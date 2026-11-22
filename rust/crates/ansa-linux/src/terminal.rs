//! CLI-only terminal/signal ownership. Construct Signals before starting threads.
pub use libc::{SIGCONT, SIGHUP, SIGINT, SIGTERM, SIGTSTP, SIGWINCH};
use std::{
    io,
    marker::PhantomData,
    os::fd::{AsRawFd, FromRawFd, OwnedFd},
    rc::Rc,
};
pub struct Descriptor {
    fd: OwnedFd,
}
impl AsRawFd for Descriptor {
    fn as_raw_fd(&self) -> i32 {
        self.fd.as_raw_fd()
    }
}
impl Descriptor {
    pub fn duplicate(&self) -> io::Result<Self> {
        Ok(Self {
            fd: rustix::io::fcntl_dupfd_cloexec(&self.fd, 3)?,
        })
    }
    pub fn new(raw: i32) -> io::Result<Self> {
        // SAFETY: fcntl accepts a raw descriptor; errors are checked, and a
        // successful duplicate transfers a new owned CLOEXEC descriptor once.
        let fd = unsafe { libc::fcntl(raw, libc::F_DUPFD_CLOEXEC, 3) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };
        Ok(Self { fd })
    }
    pub fn terminal(&self) -> bool {
        rustix::termios::isatty(&self.fd)
    }
    pub fn ready(&self, write: bool) -> io::Result<bool> {
        let mut fds = [rustix::event::PollFd::new(
            &self.fd,
            if write {
                rustix::event::PollFlags::OUT
            } else {
                rustix::event::PollFlags::IN
            },
        )];
        match rustix::event::poll(
            &mut fds,
            Some(&rustix::event::Timespec {
                tv_sec: 0,
                tv_nsec: 0,
            }),
        ) {
            Ok(n) => Ok(n > 0),
            Err(rustix::io::Errno::INTR) => Ok(false),
            Err(e) => Err(e.into()),
        }
    }
    pub fn read(&self, bytes: &mut [u8]) -> io::Result<usize> {
        rustix::io::read(&self.fd, bytes).map_err(Into::into)
    }
    pub fn write(&self, bytes: &[u8]) -> io::Result<usize> {
        rustix::io::write(&self.fd, bytes).map_err(Into::into)
    }
    pub fn size(&self) -> (usize, usize) {
        let Ok(size) = rustix::termios::tcgetwinsize(&self.fd) else {
            return (80, 16);
        };
        (
            if size.ws_col == 0 {
                80
            } else {
                size.ws_col as usize
            },
            if size.ws_row == 0 {
                16
            } else {
                size.ws_row as usize
            },
        )
    }
    pub fn cell_pixels(&self) -> Option<(f64, f64)> {
        let size = rustix::termios::tcgetwinsize(&self.fd).ok()?;
        if [size.ws_col, size.ws_row, size.ws_xpixel, size.ws_ypixel].contains(&0) {
            return None;
        }
        Some((
            f64::from(size.ws_xpixel) / f64::from(size.ws_col),
            f64::from(size.ws_ypixel) / f64::from(size.ws_row),
        ))
    }
}
/// Capture every original open-file flag before changing any aliased stdio.
/// Retained duplicates keep identities alive until all writers are joined.
pub struct Nonblocking {
    entries: Vec<(OwnedFd, rustix::fs::OFlags)>,
}
impl Nonblocking {
    pub fn new(descriptors: &[&Descriptor]) -> io::Result<Self> {
        let mut entries = vec![];
        for descriptor in descriptors {
            let fd = rustix::io::fcntl_dupfd_cloexec(&descriptor.fd, 3)?;
            let flags = rustix::fs::fcntl_getfl(&fd)?;
            entries.push((fd, flags));
        }
        let value = Self { entries };
        for (fd, flags) in &value.entries {
            rustix::fs::fcntl_setfl(fd, *flags | rustix::fs::OFlags::NONBLOCK)?;
        }
        Ok(value)
    }
    pub fn restore(&self) -> io::Result<()> {
        for (fd, flags) in &self.entries {
            rustix::fs::fcntl_setfl(fd, *flags)?;
        }
        Ok(())
    }
    pub fn resume(&self) -> io::Result<()> {
        for (fd, flags) in &self.entries {
            rustix::fs::fcntl_setfl(fd, *flags | rustix::fs::OFlags::NONBLOCK)?;
        }
        Ok(())
    }
}
impl Drop for Nonblocking {
    fn drop(&mut self) {
        for (fd, flags) in &self.entries {
            let _ = rustix::fs::fcntl_setfl(fd, *flags);
        }
    }
}
pub struct RawMode {
    fd: OwnedFd,
    saved: rustix::termios::Termios,
    active: bool,
}
impl RawMode {
    pub fn new() -> io::Result<Self> {
        let fd = Descriptor::new(0)?.fd;
        let saved = rustix::termios::tcgetattr(&fd)?;
        let mut value = Self {
            fd,
            saved,
            active: false,
        };
        value.resume()?;
        Ok(value)
    }
    pub fn resume(&mut self) -> io::Result<()> {
        let mut raw = self.saved.clone();
        // Keep CR (Enter) distinct from LF (Ctrl+J / multiline input). The
        // caller's original mappings are retained in `saved` for handoff/exit.
        // Disable software flow control so Ctrl+S cannot freeze the editor UI.
        raw.input_modes.remove(
            rustix::termios::InputModes::ICRNL
                | rustix::termios::InputModes::INLCR
                | rustix::termios::InputModes::IGNCR
                | rustix::termios::InputModes::IXON,
        );
        raw.local_modes
            .remove(rustix::termios::LocalModes::ICANON | rustix::termios::LocalModes::ECHO);
        raw.special_codes[rustix::termios::SpecialCodeIndex::VMIN] = 1;
        raw.special_codes[rustix::termios::SpecialCodeIndex::VTIME] = 0;
        rustix::termios::tcsetattr(&self.fd, rustix::termios::OptionalActions::Now, &raw)?;
        self.active = true;
        Ok(())
    }
    pub fn restore(&mut self) {
        let _ = self.restore_checked();
    }
    pub fn restore_checked(&mut self) -> io::Result<()> {
        if self.active {
            rustix::termios::tcsetattr(
                &self.fd,
                rustix::termios::OptionalActions::Now,
                &self.saved,
            )?;
            self.active = false;
        }
        Ok(())
    }
}
impl Drop for RawMode {
    fn drop(&mut self) {
        self.restore();
    }
}
pub struct Signals {
    fd: OwnedFd,
    old: libc::sigset_t,
    _thread: PhantomData<Rc<()>>,
}
impl Signals {
    /// Readiness registration owns a separate CLOEXEC FD, not another reader.
    pub fn readiness_descriptor(&self) -> io::Result<Descriptor> {
        Ok(Descriptor {
            fd: rustix::io::fcntl_dupfd_cloexec(&self.fd, 3)?,
        })
    }
    pub fn new() -> io::Result<Self> {
        if std::fs::read_dir("/proc/self/task")?.count() != 1 {
            return Err(io::Error::other("cli_signal_threads"));
        }
        // SAFETY: initialized sigsets, before threads. All runtime/writer
        // threads inherit the blocked set; signalfd is the only signal reader.
        // No async handler, pointer lifetime, allocation or shared-FD race.
        unsafe {
            let mut set = std::mem::zeroed();
            libc::sigemptyset(&mut set);
            for signal in [SIGINT, SIGTERM, SIGHUP, SIGTSTP, SIGCONT, SIGWINCH] {
                libc::sigaddset(&mut set, signal);
            }
            let mut old = std::mem::zeroed();
            let status = libc::pthread_sigmask(libc::SIG_BLOCK, &set, &mut old);
            if status != 0 {
                return Err(io::Error::from_raw_os_error(status));
            }
            let fd = libc::signalfd(-1, &set, libc::SFD_CLOEXEC | libc::SFD_NONBLOCK);
            if fd < 0 {
                let error = io::Error::last_os_error();
                libc::pthread_sigmask(libc::SIG_SETMASK, &old, std::ptr::null_mut());
                return Err(error);
            }
            Ok(Self {
                fd: OwnedFd::from_raw_fd(fd),
                old,
                _thread: PhantomData,
            })
        }
    }
    pub fn take(&self) -> io::Result<Option<i32>> {
        // SAFETY: initialized signalfd_siginfo with exact kernel size.
        let mut info: libc::signalfd_siginfo = unsafe { std::mem::zeroed() };
        let n = unsafe {
            libc::read(
                self.fd.as_raw_fd(),
                (&mut info as *mut libc::signalfd_siginfo).cast(),
                std::mem::size_of_val(&info),
            )
        };
        if n < 0 {
            let e = io::Error::last_os_error();
            if matches!(
                e.kind(),
                io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
            ) {
                return Ok(None);
            }
            return Err(e);
        }
        if n as usize != std::mem::size_of_val(&info) {
            return Err(io::Error::other("cli_signal_read"));
        }
        Ok(Some(info.ssi_signo as i32))
    }
    pub fn suspend(&self) -> io::Result<()> {
        // SAFETY: signal this process only. SIGSTOP is unmaskable; callers
        // restore termios before stopping and resume it after SIGCONT.
        if unsafe { libc::kill(libc::getpid(), libc::SIGSTOP) } < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}
impl Drop for Signals {
    fn drop(&mut self) {
        // All owned threads must already be joined. !Send enforces original-thread restoration.
        unsafe { libc::pthread_sigmask(libc::SIG_SETMASK, &self.old, std::ptr::null_mut()) };
    }
}
/// A private per-thread UTF-8 locale; never changes the process locale.
pub struct Width {
    locale: libc::locale_t,
    _thread: PhantomData<Rc<()>>,
}
impl Default for Width {
    fn default() -> Self {
        // SAFETY: NUL-terminated static locale, live locale retained until Drop.
        // Creating an owned locale does not activate it or require LIFO Drop.
        unsafe {
            let locale = libc::newlocale(
                libc::LC_CTYPE_MASK,
                c"C.UTF-8".as_ptr(),
                std::ptr::null_mut(),
            );
            Self {
                locale,
                _thread: PhantomData,
            }
        }
    }
}
impl Width {
    pub fn width(&self, cp: char) -> Option<usize> {
        if self.locale.is_null() {
            return None;
        }
        // SAFETY: owned locale is live; no external callback/unwind point occurs
        // between switch and restoration. !Send pins this operation to its thread.
        let n = unsafe {
            let old = libc::uselocale(self.locale);
            if old.is_null() {
                return None;
            }
            let n = wcwidth(cp as libc::wchar_t);
            libc::uselocale(old);
            n
        };
        if n < 0 { None } else { Some(n as usize) }
    }
}
unsafe extern "C" {
    fn wcwidth(c: libc::wchar_t) -> libc::c_int;
}
impl Drop for Width {
    fn drop(&mut self) {
        // SAFETY: this locale is never active across a public call.
        unsafe {
            if !self.locale.is_null() {
                libc::freelocale(self.locale);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn locale_ownership_allows_non_lifo_drop() {
        for _ in 0..100 {
            let a = Width::default();
            let b = Width::default();
            assert_eq!(a.width('Ж'), Some(1));
            drop(a);
            assert_eq!(b.width('界'), Some(2));
            drop(b);
            assert_eq!(Width::default().width('😀'), Some(2));
        }
    }
    #[test]
    fn aliased_open_file_flags_restore_after_all_owners() {
        let (read, _) = rustix::pipe::pipe().unwrap();
        let initial = rustix::fs::fcntl_getfl(&read).unwrap();
        let a = Descriptor::new(read.as_raw_fd()).unwrap();
        let b = Descriptor::new(read.as_raw_fd()).unwrap();
        let c = Descriptor::new(read.as_raw_fd()).unwrap();
        let flags = Nonblocking::new(&[&a, &b, &c]).unwrap();
        assert!(
            rustix::fs::fcntl_getfl(&read)
                .unwrap()
                .contains(rustix::fs::OFlags::NONBLOCK)
        );
        drop(b);
        drop(a);
        drop(c);
        drop(flags);
        assert_eq!(rustix::fs::fcntl_getfl(&read).unwrap(), initial);
    }
}

/// Foreground handoff retains a waitable leader until its process group has
/// been signalled. It never targets the harness's own process group.
/// Detached descendants are outside this boundary and remain cleanup-unknown.
pub struct ForegroundEditor {
    child: Option<std::process::Child>,
    terminal: Descriptor,
    parent_group: rustix::process::Pid,
    group: rustix::process::Pid,
    foreground: bool,
    startup_read_pending: bool,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EditorExit {
    pub code: i32,
    pub cleanup_unknown: bool,
}
fn foreground(terminal: &Descriptor, group: rustix::process::Pid) -> io::Result<()> {
    // SAFETY: initialized per-thread masks stay live until restoration. The
    // temporary mask permits tcsetpgrp while this thread is in the background;
    // it does not change another thread's mask or install a signal handler.
    unsafe {
        let mut mask: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut mask);
        libc::sigaddset(&mut mask, libc::SIGTTOU);
        let mut old: libc::sigset_t = std::mem::zeroed();
        let status = libc::pthread_sigmask(libc::SIG_BLOCK, &mask, &mut old);
        if status != 0 {
            return Err(io::Error::from_raw_os_error(status));
        }
        let result = rustix::termios::tcsetpgrp(&terminal.fd, group).map_err(io::Error::from);
        let restore = libc::pthread_sigmask(libc::SIG_SETMASK, &old, std::ptr::null_mut());
        if restore != 0 {
            return Err(io::Error::from_raw_os_error(restore));
        }
        result
    }
}
fn spawn_editor_command(mut command: std::process::Command) -> io::Result<std::process::Child> {
    use std::os::unix::process::CommandExt;
    // SAFETY: both libc plain-data structures permit all-zero initialization;
    // their signal set/action fields are fully populated before any syscall.
    let (mut action, mut empty): (libc::sigaction, libc::sigset_t) =
        unsafe { (std::mem::zeroed(), std::mem::zeroed()) };
    // SAFETY: these initialized masks/actions are prepared before fork and
    // captured by value. They contain no borrowed pointers or owned allocator
    // state; only libc signal syscalls and checked integer arithmetic run in
    // the child callback. managed::spawn then installs group/PDEATHSIG/NNP,
    // clears capabilities and marks inherited non-stdio descriptors CLOEXEC.
    unsafe {
        libc::sigemptyset(&mut empty);
        action.sa_sigaction = libc::SIG_DFL;
        action.sa_mask = empty;
        let upper = libc::SIGRTMIN();
        command.pre_exec(move || {
            for signal in 1..upper {
                if matches!(signal, libc::SIGKILL | libc::SIGSTOP | 32 | 33) {
                    continue;
                }
                if libc::sigaction(signal, &action, std::ptr::null_mut()) != 0 {
                    return Err(io::Error::last_os_error());
                }
            }
            if libc::sigprocmask(libc::SIG_SETMASK, &empty, std::ptr::null_mut()) != 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    crate::managed::spawn(command, None, Some(std::process::id()))
}
impl ForegroundEditor {
    pub fn spawn(command: std::process::Command, terminal: &Descriptor) -> io::Result<Self> {
        let terminal = terminal.duplicate()?;
        let parent_group = rustix::termios::tcgetpgrp(&terminal.fd)?;
        if parent_group != rustix::process::getpgrp() {
            return Err(io::Error::other("editor_parent_not_foreground"));
        }
        let mut child = spawn_editor_command(command)?;
        let Some(group) = rustix::process::Pid::from_raw(child.id() as i32) else {
            let _ = child.kill();
            let _ = child.wait();
            return Err(io::Error::other("editor_pid"));
        };
        let mut value = Self {
            child: Some(child),
            terminal,
            parent_group,
            group,
            foreground: false,
            startup_read_pending: true,
        };
        foreground(&value.terminal, group)?;
        value.foreground = true;
        value.handle_stop()?;
        Ok(value)
    }
    fn handle_stop(&mut self) -> io::Result<()> {
        let Some(child) = &self.child else {
            return Ok(());
        };
        let pid = rustix::process::Pid::from_raw(child.id() as i32)
            .ok_or_else(|| io::Error::other("editor_pid"))?;
        // Observe exits without reaping the leader before group cleanup.
        // WSTOPPED alone can report ECHILD once the child is already exited.
        let status = rustix::process::waitid(
            rustix::process::WaitId::Pid(pid),
            rustix::process::WaitIdOptions::EXITED
                | rustix::process::WaitIdOptions::STOPPED
                | rustix::process::WaitIdOptions::NOHANG
                | rustix::process::WaitIdOptions::NOWAIT,
        )?;
        if status.is_some_and(|status| status.stopped()) {
            // Consume only the stop, never an exit. If it was killed between
            // observations, leave the exited leader for poll's owned cleanup.
            let status = match rustix::process::waitid(
                rustix::process::WaitId::Pid(pid),
                rustix::process::WaitIdOptions::STOPPED | rustix::process::WaitIdOptions::NOHANG,
            ) {
                Ok(Some(status)) => status,
                Ok(None) | Err(rustix::io::Errno::CHILD) => return Ok(()),
                Err(error) => return Err(error.into()),
            };
            // A startup read may stop on TTIN before tcsetpgrp; STOP/TSTP
            // must never resume, even when they happen before the handoff.
            let recover_read =
                self.startup_read_pending && status.stopping_signal() == Some(libc::SIGTTIN);
            self.startup_read_pending = false;
            if recover_read {
                match rustix::process::kill_process_group(self.group, rustix::process::Signal::CONT)
                {
                    Ok(()) | Err(rustix::io::Errno::SRCH) => {}
                    Err(error) => return Err(error.into()),
                }
            } else {
                self.cancel()?;
            }
        }
        Ok(())
    }
    fn restore_foreground(&mut self) -> io::Result<()> {
        if self.foreground {
            foreground(&self.terminal, self.parent_group)?;
            self.foreground = false;
        }
        Ok(())
    }
    pub fn cancel(&mut self) -> io::Result<()> {
        let Some(child) = &self.child else {
            return Ok(());
        };
        let pid = rustix::process::Pid::from_raw(child.id() as i32)
            .ok_or_else(|| io::Error::other("editor_pid"))?;
        // Prove the leader is still our waitable child before group signalling;
        // keeping it unreaped prevents the group identity from being recycled.
        rustix::process::waitid(
            rustix::process::WaitId::Pid(pid),
            rustix::process::WaitIdOptions::EXITED
                | rustix::process::WaitIdOptions::NOHANG
                | rustix::process::WaitIdOptions::NOWAIT,
        )?;
        if self.group == rustix::process::getpgrp() {
            return Err(io::Error::other("editor_group_identity"));
        }
        crate::process::kill_group(self.group.as_raw_nonzero().get() as u32)?;
        crate::process::kill_owned(pid)
    }
    /// Nonblocking: stopped editors are cancelled rather than left detached.
    pub fn poll(&mut self) -> io::Result<Option<EditorExit>> {
        use std::os::unix::process::ExitStatusExt;
        self.handle_stop()?;
        let Some(child) = &self.child else {
            return Ok(None);
        };
        let pid = rustix::process::Pid::from_raw(child.id() as i32)
            .ok_or_else(|| io::Error::other("editor_pid"))?;
        let status = rustix::process::waitid(
            rustix::process::WaitId::Pid(pid),
            rustix::process::WaitIdOptions::EXITED
                | rustix::process::WaitIdOptions::NOHANG
                | rustix::process::WaitIdOptions::NOWAIT,
        )?;
        let Some(_) = status else {
            return Ok(None);
        };
        self.cancel()?;
        self.restore_foreground()?;
        let mut child = self.child.take().expect("owned child");
        // waitid observed the terminal status with NOWAIT, so this wait only
        // reaps the already exited leader and cannot wait on a running editor.
        let status = child.wait()?;
        Ok(Some(EditorExit {
            code: status
                .code()
                .unwrap_or_else(|| 128 + status.signal().unwrap_or(0)),
            cleanup_unknown: true,
        }))
    }
    /// Emergency shutdown only. After the caller's finite cancel budget,
    /// reclaim the terminal and attempt a nonblocking reap. An unreapable
    /// primary is deliberately removed from Drop's wait path: the harness
    /// MUST terminate next, cannot resume dispatch, and cannot claim cleanup.
    pub fn abandon_for_exit(&mut self) -> io::Result<EditorExit> {
        use std::os::unix::process::ExitStatusExt;
        let mut error = self.cancel().err();
        if let Err(restore) = self.restore_foreground() {
            error.get_or_insert(restore);
        }
        let mut status = None;
        if let Some(mut child) = self.child.take() {
            match child.try_wait() {
                Ok(value) => status = value,
                Err(wait) => {
                    error.get_or_insert(wait);
                }
            }
            // Child::drop never waits. If the kernel has not delivered a
            // terminal status, imminent harness exit/PDEATHSIG and eventual
            // kernel reparenting own the remaining unknown cleanup.
        }
        if let Some(error) = error {
            return Err(error);
        }
        Ok(EditorExit {
            code: status.map_or(1, |status| {
                status
                    .code()
                    .unwrap_or_else(|| 128 + status.signal().unwrap_or(0))
            }),
            cleanup_unknown: true,
        })
    }
}
impl Drop for ForegroundEditor {
    fn drop(&mut self) {
        let _ = self.cancel();
        let _ = self.restore_foreground();
        if let Some(mut child) = self.child.take() {
            // Error/owner-drop cleanup still owns and reaps the primary. The
            // ordinary UI path uses nonblocking poll until terminal completion.
            let _ = child.wait();
        }
    }
}

#[cfg(test)]
mod editor_tests {
    use super::*;
    use std::{
        os::unix::process::ExitStatusExt,
        process::{Command, Stdio},
    };
    struct Mask(libc::sigset_t);
    impl Mask {
        fn blocked() -> Self {
            // SAFETY: synthetic tests affect only this calling test thread;
            // initialized masks are restored by this thread's local guard.
            unsafe {
                let mut mask = std::mem::zeroed();
                libc::sigemptyset(&mut mask);
                for signal in [SIGINT, SIGTERM, SIGHUP, SIGTSTP] {
                    libc::sigaddset(&mut mask, signal);
                }
                let mut old = std::mem::zeroed();
                assert_eq!(libc::pthread_sigmask(libc::SIG_BLOCK, &mask, &mut old), 0);
                Self(old)
            }
        }
    }
    impl Drop for Mask {
        fn drop(&mut self) {
            // SAFETY: original initialized mask remains owned by this thread.
            unsafe {
                libc::pthread_sigmask(libc::SIG_SETMASK, &self.0, std::ptr::null_mut());
            }
        }
    }
    #[test]
    fn editor_child_signal_fixture() {
        if !std::env::args().any(|arg| arg == "--exact") {
            return;
        }
        assert_eq!(rustix::process::getpgrp(), rustix::process::getpid());
        // SAFETY: querying the current test thread's mask writes an initialized
        // sigset only; no disposition or other thread is changed.
        unsafe {
            let mut mask: libc::sigset_t = std::mem::zeroed();
            assert_eq!(
                libc::pthread_sigmask(libc::SIG_SETMASK, std::ptr::null(), &mut mask),
                0
            );
            for signal in [SIGINT, SIGTERM, SIGHUP, SIGTSTP] {
                assert_eq!(libc::sigismember(&mask, signal), 0);
            }
        }
    }
    #[test]
    fn editor_exec_resets_blocked_signals_and_uses_a_distinct_group() {
        let _mask = Mask::blocked();
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--exact",
                "terminal::editor_tests::editor_child_signal_fixture",
            ])
            .stdout(Stdio::null());
        let status = spawn_editor_command(command).unwrap().wait().unwrap();
        assert!(status.success());
        let mut command = Command::new("sh");
        command.args(["-c", "kill -TERM $$"]);
        let status = spawn_editor_command(command).unwrap().wait().unwrap();
        assert_eq!(status.signal(), Some(SIGTERM));
    }
    #[test]
    fn editor_open_file_flags_can_be_restored_and_reentered() {
        let (read, _) = rustix::pipe::pipe().unwrap();
        let original = rustix::fs::fcntl_getfl(&read).unwrap();
        let descriptor = Descriptor::new(read.as_raw_fd()).unwrap();
        let flags = Nonblocking::new(&[&descriptor]).unwrap();
        flags.restore().unwrap();
        assert_eq!(rustix::fs::fcntl_getfl(&read).unwrap(), original);
        flags.resume().unwrap();
        assert!(
            rustix::fs::fcntl_getfl(&read)
                .unwrap()
                .contains(rustix::fs::OFlags::NONBLOCK)
        );
        drop(flags);
        assert_eq!(rustix::fs::fcntl_getfl(&read).unwrap(), original);
    }
    #[test]
    fn editor_startup_only_resumes_observed_background_reads() {
        use std::{
            io::Read,
            time::{Duration, Instant},
        };
        for (name, signal, resumed) in [
            ("STOP", libc::SIGSTOP, false),
            ("TSTP", libc::SIGTSTP, false),
            ("TTIN", libc::SIGTTIN, true),
        ] {
            let mut command = Command::new("sh");
            command
                .args(["-c", &format!("kill -{name} $$; printf resumed")])
                .stdout(Stdio::piped());
            let mut child = spawn_editor_command(command).unwrap();
            let mut stdout = child.stdout.take().unwrap();
            let group = rustix::process::Pid::from_raw(child.id() as i32).unwrap();
            let (read, _) = rustix::pipe::pipe().unwrap();
            // Exercise the production handoff observer after a real child stop,
            // deterministically before any continuation. This owner never takes
            // the test runner's tty; foreground=false makes restoration a no-op.
            let mut owner = ForegroundEditor {
                child: Some(child),
                terminal: Descriptor::new(read.as_raw_fd()).unwrap(),
                parent_group: rustix::process::getpgrp(),
                group,
                foreground: false,
                startup_read_pending: true,
            };
            let deadline = Instant::now() + Duration::from_secs(4);
            loop {
                if let Some(status) = rustix::process::waitid(
                    rustix::process::WaitId::Pid(group),
                    rustix::process::WaitIdOptions::STOPPED
                        | rustix::process::WaitIdOptions::NOHANG
                        | rustix::process::WaitIdOptions::NOWAIT,
                )
                .unwrap()
                {
                    assert_eq!(status.stopping_signal(), Some(signal));
                    break;
                }
                assert!(Instant::now() < deadline, "{name}: child did not stop");
                std::thread::sleep(Duration::from_millis(1));
            }
            owner.handle_stop().unwrap();
            let exit = loop {
                if let Some(exit) = owner.poll().unwrap() {
                    break exit;
                }
                assert!(
                    Instant::now() < deadline,
                    "{name}: stopped editor cleanup stalled"
                );
                std::thread::sleep(Duration::from_millis(1));
            };
            let mut output = String::new();
            stdout.read_to_string(&mut output).unwrap();
            assert_eq!(output, if resumed { "resumed" } else { "" }, "{name}");
            assert_eq!(exit.code, if resumed { 0 } else { 128 + libc::SIGKILL });
        }
    }
}
