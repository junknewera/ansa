//! Owned embedded PTY. Its slave is never the frontend's controlling terminal.
//! Only this owner reads, writes, resizes and reaps the interactive shell.
use rustix::{
    fd::OwnedFd,
    process::{Pid, Signal},
};

/// A private shell integration notifier, separate from terminal stdin.
pub fn notification_fifo(path: &Path) -> io::Result<File> {
    rustix::fs::mkfifoat(
        rustix::fs::CWD,
        path,
        rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR,
    )?;
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)?;
    crate::managed::nonblocking(&file)?;
    Ok(file)
}
use std::{
    cell::Cell,
    fs::File,
    io::{self, Read},
    os::{fd::AsRawFd, unix::process::CommandExt},
    path::{Path, PathBuf},
    process::{Child, Command, ExitStatus, Stdio},
    time::{Duration, Instant},
};

/// This describes the owned session only. A program which deliberately calls
/// setsid can escape it; absence of arbitrary detached descendants is unknown.
#[derive(Clone, Debug, Default)]
pub struct CleanupOutcome {
    pub shell_reaped: bool,
    pub session_empty: bool,
    pub error: Option<String>,
}

pub struct PtySession {
    master: OwnedFd,
    child: Child,
    leader: Pid,
    reaped: Option<ExitStatus>,
    cleanup: Option<CleanupOutcome>,
    ownership_unknown: Cell<bool>,
}

impl PtySession {
    pub fn spawn(command: Command, rows: u16, cols: u16) -> io::Result<Self> {
        Self::spawn_in(command, rows, cols, None)
    }

    pub fn spawn_in(
        mut command: Command,
        rows: u16,
        cols: u16,
        directory: Option<&File>,
    ) -> io::Result<Self> {
        require_waitable_sigchld()?;
        if rows == 0 || cols == 0 {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "pty_size"));
        }
        let master = rustix::pty::openpt(
            rustix::pty::OpenptFlags::RDWR
                | rustix::pty::OpenptFlags::NOCTTY
                | rustix::pty::OpenptFlags::CLOEXEC,
        )?;
        rustix::pty::grantpt(&master)?;
        rustix::pty::unlockpt(&master)?;
        // TIOCGPTPEER opens this exact slave without a pathname/symlink race.
        let slave = rustix::pty::ioctl_tiocgptpeer(
            &master,
            rustix::pty::OpenptFlags::RDWR
                | rustix::pty::OpenptFlags::NOCTTY
                | rustix::pty::OpenptFlags::CLOEXEC,
        )?;
        rustix::termios::tcsetwinsize(&slave, winsize(rows, cols))?;
        command.stdin(Stdio::from(File::from(slave.try_clone()?)));
        command.stdout(Stdio::from(File::from(slave.try_clone()?)));
        command.stderr(Stdio::from(File::from(slave)));
        let parent = std::process::id() as libc::pid_t;
        let directory = directory.map(AsRawFd::as_raw_fd);
        // SAFETY: initialized plain C data are prepared before fork. The child
        // callback captures only these values and integers. It calls only
        // async-signal-safe syscalls, does not allocate, lock, invoke handlers
        // or run arbitrary Rust callbacks. Stdio has already been dup'd to
        // descriptors 0..2 by Command. TIOCSCTTY therefore targets our slave,
        // never the outer tty. Error construction uses only errno integers.
        // UID/capability/NNP/close_range protections match managed::spawn.
        // A borrowed directory FD remains open until synchronous spawn returns.
        let (mut action, mut empty): (libc::sigaction, libc::sigset_t) =
            unsafe { (std::mem::zeroed(), std::mem::zeroed()) };
        unsafe {
            libc::sigemptyset(&mut empty);
            action.sa_sigaction = libc::SIG_DFL;
            action.sa_mask = empty;
            let upper = libc::SIGRTMIN();
            command.pre_exec(move || {
                if libc::getuid() == 0 || libc::getuid() != libc::geteuid() {
                    return Err(io::Error::from_raw_os_error(libc::EPERM));
                }
                for signal in 1..upper {
                    if matches!(signal, libc::SIGKILL | libc::SIGSTOP | 32 | 33) {
                        continue;
                    }
                    if libc::sigaction(signal, &action, std::ptr::null_mut()) != 0 {
                        return Err(io::Error::last_os_error());
                    }
                }
                if libc::sigprocmask(libc::SIG_SETMASK, &empty, std::ptr::null_mut()) != 0
                    || libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL, 0, 0, 0) != 0
                {
                    return Err(io::Error::last_os_error());
                }
                if libc::getppid() != parent {
                    return Err(io::Error::from_raw_os_error(libc::ESRCH));
                }
                if libc::setsid() < 0 || libc::ioctl(0, libc::TIOCSCTTY, 0) != 0 {
                    return Err(io::Error::last_os_error());
                }
                if let Some(directory) = directory
                    && libc::fchdir(directory) != 0
                {
                    return Err(io::Error::last_os_error());
                }
                let header = [0x2008_0522_u32, 0];
                let caps = [0_u32; 6];
                if libc::syscall(libc::SYS_capset, header.as_ptr(), caps.as_ptr()) != 0
                    || libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0
                    || libc::syscall(libc::SYS_close_range, 3_u32, u32::MAX, 4_u32) != 0
                {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let child = command.spawn()?;
        let leader = Pid::from_raw(child.id() as i32).expect("spawned PID");
        let mut value = Self {
            master,
            child,
            leader,
            reaped: None,
            cleanup: None,
            ownership_unknown: Cell::new(false),
        };
        if let Err(error) = value
            .wait_owned()
            .and_then(|_| crate::managed::nonblocking(&value.master))
        {
            let _ = value.shutdown();
            return Err(error);
        }
        Ok(value)
    }

    pub fn id(&self) -> u32 {
        self.child.id()
    }

    pub fn read(&self, bytes: &mut [u8]) -> io::Result<usize> {
        match rustix::io::read(&self.master, bytes) {
            Ok(size) => Ok(size),
            // Linux returns EIO after all slave descriptors have closed.
            Err(rustix::io::Errno::IO) => Ok(0),
            Err(error) => Err(error.into()),
        }
    }

    pub fn write(&self, bytes: &[u8]) -> io::Result<usize> {
        rustix::io::write(&self.master, bytes).map_err(Into::into)
    }

    pub fn resize(&self, rows: u16, cols: u16) -> io::Result<()> {
        if rows == 0 || cols == 0 {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "pty_size"));
        }
        rustix::termios::tcsetwinsize(&self.master, winsize(rows, cols))?;
        Ok(())
    }

    /// Ask the kernel to signal the slave's current foreground process group.
    /// Success reports ioctl admission, not the termination of a foreground job.
    pub fn interrupt_foreground(&self) -> io::Result<()> {
        if self.wait_owned()? {
            return Err(io::Error::other("pty_shell_exited"));
        }
        rustix::termios::tcgetpgrp(&self.master)?;
        // SAFETY: this is an owned PTY master. TIOCSIG takes a scalar signal,
        // not a pointer, and resolves the foreground group through this PTY.
        if unsafe { libc::ioctl(self.master.as_raw_fd(), libc::TIOCSIG, libc::SIGINT) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    pub fn foreground_is_shell(&self) -> bool {
        self.wait_owned().is_ok()
            && rustix::termios::tcgetpgrp(&self.master).ok() == Some(self.leader)
    }

    pub fn cwd(&self) -> io::Result<PathBuf> {
        self.wait_owned()?;
        std::fs::read_link(format!("/proc/{}/cwd", self.id()))
    }

    /// Observe without reaping: the owned zombie pins its SID/PID while
    /// session members are cleaned, avoiding signals to a recycled leader.
    pub fn exited(&self) -> io::Result<bool> {
        if self.reaped.is_some() {
            return Ok(true);
        }
        self.wait_owned()
    }

    pub fn exit_status(&self) -> Option<ExitStatus> {
        self.reaped
    }
    fn wait_owned(&self) -> io::Result<bool> {
        if self.ownership_unknown.get() {
            return Err(io::Error::other("pty_ownership_unknown"));
        }
        if self.reaped.is_some() {
            return Err(io::Error::other("pty_owner_released"));
        }
        if let Err(error) = require_waitable_sigchld() {
            self.ownership_unknown.set(true);
            return Err(error);
        }
        loop {
            match rustix::process::waitid(
                rustix::process::WaitId::Pid(self.leader),
                rustix::process::WaitIdOptions::EXITED
                    | rustix::process::WaitIdOptions::NOHANG
                    | rustix::process::WaitIdOptions::NOWAIT,
            ) {
                Ok(state) => return Ok(state.is_some()),
                Err(rustix::io::Errno::INTR) => continue,
                Err(error) => {
                    self.ownership_unknown.set(true);
                    return Err(error.into());
                }
            }
        }
    }

    pub fn shutdown(&mut self) -> CleanupOutcome {
        if let Some(outcome) = &self.cleanup {
            return outcome.clone();
        }
        let mut result = CleanupOutcome::default();
        if let Err(error) = self.wait_owned() {
            result.error = Some(error.to_string());
            return self.cache_cleanup(result);
        }
        for signal in [Signal::HUP, Signal::TERM, Signal::KILL] {
            if let Err(error) = self.signal_session(signal) {
                result.error = Some(error.to_string());
                if self.ownership_unknown.get() {
                    return self.cache_cleanup(result);
                }
            }
            let deadline = Instant::now() + Duration::from_millis(100);
            while Instant::now() < deadline {
                match self.session_members() {
                    Ok(members) if members.iter().all(|(_, state)| *state == 'Z') => break,
                    Ok(_) => std::thread::sleep(Duration::from_millis(5)),
                    Err(error) => {
                        result.error = Some(error.to_string());
                        if self.ownership_unknown.get() {
                            return self.cache_cleanup(result);
                        }
                        break;
                    }
                }
            }
        }
        // Even when pidfd enumeration was denied, Child still owns this
        // unreaped PID. kill() cannot target a recycled leader before wait.
        match self.wait_owned() {
            Ok(false) => {
                if let Err(error) = self.child.kill() {
                    result.error = Some(error.to_string());
                }
            }
            Ok(true) => {}
            Err(error) => {
                result.error = Some(error.to_string());
                return self.cache_cleanup(result);
            }
        }
        // Prove the SID has no member except its owned zombie while that
        // zombie still pins the SID. Never scan a numeric SID after wait/reap.
        let quiesced = match self.session_members() {
            Ok(members) => members
                .iter()
                .all(|(pid, state)| *pid == self.id() && *state == 'Z'),
            Err(error) => {
                result.error = Some(error.to_string());
                if self.ownership_unknown.get() {
                    return self.cache_cleanup(result);
                }
                false
            }
        };
        // SIGKILL was sent through a pidfd opened while the child was owned.
        // Polling rather than wait() keeps teardown bounded even on kernel I/O.
        let deadline = Instant::now() + Duration::from_millis(400);
        while Instant::now() < deadline {
            match self.child.try_wait() {
                Ok(Some(status)) => {
                    self.reaped = Some(status);
                    result.shell_reaped = true;
                    break;
                }
                Ok(None) => std::thread::sleep(Duration::from_millis(5)),
                Err(error) => {
                    result.error = Some(error.to_string());
                    break;
                }
            }
        }
        result.session_empty = quiesced && result.shell_reaped;
        // Never enumerate/signal again after reaping released the SID/PID.
        // A failed cleanup stays unknown, rather than retrying a recycled ID.
        self.cache_cleanup(result)
    }
    fn cache_cleanup(&mut self, result: CleanupOutcome) -> CleanupOutcome {
        self.cleanup = Some(result.clone());
        result
    }

    fn session_members(&self) -> io::Result<Vec<(u32, char)>> {
        self.wait_owned()?;
        let mut result = Vec::new();
        for (count, entry) in std::fs::read_dir("/proc")?.enumerate() {
            if count == 32_768 {
                return Err(io::Error::other("pty_proc_limit"));
            }
            let entry = entry?;
            let Some(pid) = entry
                .file_name()
                .to_str()
                .and_then(|s| s.parse::<u32>().ok())
            else {
                continue;
            };
            match proc_stat(pid) {
                Ok(stat) if stat.session == self.id() => {
                    if result.len() == 4096 {
                        return Err(io::Error::other("pty_session_limit"));
                    }
                    result.push((pid, stat.state));
                }
                Ok(_) => {}
                Err(error) if process_gone(&error) => {}
                // Unreadable/malformed membership is unknown, never proof
                // that a possibly owned process belongs to another session.
                Err(error) => return Err(error),
            }
        }
        self.wait_owned()?;
        Ok(result)
    }

    fn signal_session(&self, signal: Signal) -> io::Result<()> {
        for (raw, _) in self.session_members()? {
            self.wait_owned()?;
            let Some(pid) = Pid::from_raw(raw as i32) else {
                continue;
            };
            let fd = match rustix::process::pidfd_open(pid, rustix::process::PidfdFlags::empty()) {
                Ok(fd) => fd,
                Err(error) => {
                    // A process may have exited between enumeration and open.
                    match proc_stat(raw) {
                        Ok(stat) if stat.session == self.id() => return Err(error.into()),
                        Ok(_) => {}
                        Err(error) if process_gone(&error) => {}
                        Err(error) => return Err(error),
                    }
                    continue;
                }
            };
            // Recheck the session after opening the pidfd. Its identity stays
            // pinned even if this member exits before the signal syscall.
            self.wait_owned()?;
            match proc_stat(raw) {
                Ok(stat) if stat.session == self.id() => {
                    match rustix::process::pidfd_send_signal(&fd, signal) {
                        Ok(()) | Err(rustix::io::Errno::SRCH) => {}
                        Err(error) => return Err(error.into()),
                    }
                }
                Ok(_) => {}
                Err(error) if process_gone(&error) => {}
                Err(error) => return Err(error),
            }
        }
        Ok(())
    }
}
impl Drop for PtySession {
    fn drop(&mut self) {
        if self.cleanup.is_none() {
            let _ = self.shutdown();
        }
    }
}

fn require_waitable_sigchld() -> io::Result<()> {
    // SAFETY: sigaction only reads the parent disposition into an initialized
    // plain C structure. It installs no action and changes no process state.
    let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
    if unsafe { libc::sigaction(libc::SIGCHLD, std::ptr::null(), &mut action) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // Auto-reaping would release the leader PID/SID before cleanup. Parent
    // signal dispositions remain owned by the CLI; this API never resets one.
    if action.sa_sigaction != libc::SIG_DFL || action.sa_flags & libc::SA_NOCLDWAIT != 0 {
        return Err(io::Error::other("pty_sigchld_not_waitable"));
    }
    Ok(())
}

fn winsize(rows: u16, cols: u16) -> rustix::termios::Winsize {
    rustix::termios::Winsize {
        ws_row: rows,
        ws_col: cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    }
}
struct ProcStat {
    state: char,
    session: u32,
}
fn proc_stat(pid: u32) -> io::Result<ProcStat> {
    parse_proc_stat(&read_small(&format!("/proc/{pid}/stat"), 8192)?)
}
fn process_gone(error: &io::Error) -> bool {
    error.raw_os_error() == Some(libc::ENOENT)
}
fn parse_proc_stat(bytes: &[u8]) -> io::Result<ProcStat> {
    // comm is arbitrary Linux bytes and can itself contain ') ', newlines and
    // invalid UTF-8. The numeric tail contains no ')', so its final delimiter
    // is unambiguous. Decode only the ASCII state/session fields we actually use.
    let boundary = bytes
        .windows(2)
        .rposition(|s| s == b") ")
        .ok_or_else(|| io::Error::other("pty_proc_stat"))?;
    let mut fields = bytes[boundary + 2..]
        .split(u8::is_ascii_whitespace)
        .filter(|s| !s.is_empty());
    let state = fields
        .next()
        .filter(|s| s.len() == 1 && s[0].is_ascii_alphabetic())
        .map(|s| char::from(s[0]))
        .ok_or_else(|| io::Error::other("pty_proc_stat"))?;
    let session = fields
        .nth(2)
        .and_then(|s| std::str::from_utf8(s).ok()?.parse().ok())
        .ok_or_else(|| io::Error::other("pty_proc_stat"))?;
    Ok(ProcStat { state, session })
}
fn read_small(path: &str, limit: usize) -> io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    File::open(Path::new(path))?
        .take((limit + 1) as u64)
        .read_to_end(&mut bytes)?;
    if bytes.len() > limit {
        return Err(io::Error::other("pty_proc_limit"));
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn shell() -> Command {
        let mut command = Command::new("sh");
        command.args([
            "-c",
            "test -t 0 && test -t 1 && test -t 2 || exit 9; printf 'PTY_OK\\n'; exec sleep 30",
        ]);
        command
    }
    #[test]
    fn isolated_tty_resize_and_bounded_cleanup() {
        let outer = File::open("/dev/tty")
            .ok()
            .and_then(|f| rustix::termios::tcgetpgrp(&f).ok());
        let mut session = PtySession::spawn(shell(), 17, 61).unwrap();
        assert_eq!(
            rustix::termios::tcgetwinsize(&session.master)
                .unwrap()
                .ws_col,
            61
        );
        session.resize(23, 79).unwrap();
        assert_eq!(
            rustix::termios::tcgetwinsize(&session.master)
                .unwrap()
                .ws_row,
            23
        );
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut output = Vec::new();
        while Instant::now() < deadline && !output.windows(6).any(|v| v == b"PTY_OK") {
            let mut buffer = [0; 256];
            if let Ok(n) = session.read(&mut buffer) {
                output.extend_from_slice(&buffer[..n]);
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(output.windows(6).any(|v| v == b"PTY_OK"));
        assert_eq!(
            File::open("/dev/tty")
                .ok()
                .and_then(|f| rustix::termios::tcgetpgrp(&f).ok()),
            outer
        );
        let start = Instant::now();
        let outcome = session.shutdown();
        assert!(outcome.shell_reaped, "{outcome:?}");
        assert!(outcome.session_empty, "{outcome:?}");
        assert!(start.elapsed() < Duration::from_secs(2));
    }
    #[test]
    fn real_terminal_interrupt_reaches_child() {
        let mut session = PtySession::spawn(shell(), 10, 40).unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut buffer = [0; 256];
        while Instant::now() < deadline {
            if session.read(&mut buffer).is_ok_and(|n| n > 0) {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        session.write(&[3]).unwrap();
        while Instant::now() < deadline && !session.exited().unwrap() {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(session.exited().unwrap());
        assert!(session.shutdown().shell_reaped);
    }
    #[test]
    fn proc_stat_parses_session_identity() {
        assert_eq!(
            proc_stat(std::process::id()).unwrap().session,
            rustix::process::getsid(None)
                .unwrap()
                .as_raw_nonzero()
                .get() as u32
        );
    }
    #[test]
    fn proc_stat_ignores_arbitrary_comm_bytes_and_reports_malformed_tail() {
        let stat = parse_proc_stat(b"123 (name\xff) \n) with ) bytes) S 17 123 123 0 0\n").unwrap();
        assert_eq!(stat.state, 'S');
        assert_eq!(stat.session, 123);
        assert!(parse_proc_stat(b"123 (name\xff) S 17 123 nope").is_err());
        assert!(parse_proc_stat(b"123 no delimiter").is_err());
        assert!(!process_gone(&io::Error::from_raw_os_error(libc::EACCES)));
        assert!(!process_gone(&io::Error::other("malformed stat")));
        assert!(process_gone(&io::Error::from_raw_os_error(libc::ENOENT)));
    }
    #[test]
    fn shutdown_finds_and_kills_descendant_with_non_utf8_comm() {
        let mut command = Command::new("sh");
        command.args(["-c",r#"sh -c 'printf "\377) invalid name\n" > /proc/$$/comm; trap "" HUP TERM; printf "NONUTF8_PID=%s\n" "$$"; while :; do read ignored </dev/tty; done' & wait"#]);
        let mut session = PtySession::spawn(command, 10, 60).unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut output = Vec::new();
        let descendant = loop {
            let mut bytes = [0; 256];
            if let Ok(n) = session.read(&mut bytes) {
                output.extend_from_slice(&bytes[..n]);
            }
            assert!(output.len() < 4096);
            if let Some(pid) = output.split(|b| *b == b'\n').find_map(|line| {
                let raw = line.strip_prefix(b"NONUTF8_PID=")?;
                std::str::from_utf8(raw).ok()?.trim().parse::<u32>().ok()
            }) {
                break pid;
            }
            assert!(Instant::now() < deadline, "descendant did not announce");
            std::thread::sleep(Duration::from_millis(5));
        };
        let pid = Pid::from_raw(descendant as i32).unwrap();
        struct CleanupGuard(OwnedFd);
        impl Drop for CleanupGuard {
            fn drop(&mut self) {
                let _ = rustix::process::pidfd_send_signal(&self.0, Signal::KILL);
            }
        }
        let _guard = CleanupGuard(
            rustix::process::pidfd_open(pid, rustix::process::PidfdFlags::empty()).unwrap(),
        );
        let raw = read_small(&format!("/proc/{descendant}/stat"), 8192).unwrap();
        assert!(std::str::from_utf8(&raw).is_err());
        assert_eq!(proc_stat(descendant).unwrap().session, session.id());
        let outcome = session.shutdown();
        assert!(outcome.shell_reaped, "{outcome:?}");
        match proc_stat(descendant) {
            Ok(stat) => {
                assert_eq!(stat.state, 'Z', "owned descendant survived shutdown");
                assert!(!outcome.session_empty, "zombie session member was hidden");
            }
            Err(error) => assert!(process_gone(&error), "{error}"),
        }
    }
    #[test]
    fn child_clears_inherited_interrupt_mask() {
        // SAFETY: initialized masks affect only this test thread. The original
        // mask is restored immediately after spawn, before inspecting results.
        let (mut mask, mut old): (libc::sigset_t, libc::sigset_t) =
            unsafe { (std::mem::zeroed(), std::mem::zeroed()) };
        unsafe {
            libc::sigemptyset(&mut mask);
            libc::sigaddset(&mut mask, libc::SIGINT);
            assert_eq!(libc::pthread_sigmask(libc::SIG_BLOCK, &mask, &mut old), 0);
        }
        let mut command = Command::new("sleep");
        command.arg("30");
        let spawned = PtySession::spawn(command, 10, 40);
        // SAFETY: old was fully initialized by successful pthread_sigmask.
        unsafe {
            assert_eq!(
                libc::pthread_sigmask(libc::SIG_SETMASK, &old, std::ptr::null_mut()),
                0
            );
        }
        let mut session = spawned.unwrap();
        session.write(&[3]).unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline && !session.exited().unwrap() {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(session.exited().unwrap());
        assert!(session.shutdown().shell_reaped);
    }
    #[test]
    fn child_keeps_no_new_privs_and_empty_capabilities() {
        let mut command = Command::new("cat");
        command.arg("/proc/self/status");
        let mut session = PtySession::spawn(command, 24, 80).unwrap();
        let mut bytes = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let mut buffer = [0; 4096];
            match session.read(&mut buffer) {
                Ok(0) => break,
                Ok(n) => {
                    bytes.extend_from_slice(&buffer[..n]);
                    assert!(bytes.len() < 32 * 1024);
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(5))
                }
                Err(error) => panic!("{error}"),
            }
            assert!(Instant::now() < deadline);
        }
        let text = String::from_utf8(bytes).unwrap();
        for name in ["CapEff:", "CapPrm:", "CapInh:"] {
            let value = text
                .lines()
                .find_map(|line| line.strip_prefix(name))
                .unwrap()
                .trim();
            assert_eq!(value, "0000000000000000");
        }
        assert_eq!(
            text.lines()
                .find_map(|line| line.strip_prefix("NoNewPrivs:"))
                .unwrap()
                .trim(),
            "1"
        );
        assert!(session.shutdown().shell_reaped);
    }
    #[test]
    fn sigchld_ownership_faults_are_rejected_in_isolated_processes() {
        for mode in ["ignore", "no_cldwait", "custom_handler", "lost_child"] {
            let result = Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "pty::tests::sigchld_ownership_fixture",
                    "--nocapture",
                    "--test-threads=1",
                ])
                .env("ANSA_PTY_SIGCHLD_FIXTURE", mode)
                .output()
                .unwrap();
            assert!(
                result.status.success(),
                "{mode}: {}",
                String::from_utf8_lossy(&result.stderr)
            );
        }
    }
    #[test]
    fn sigchld_ownership_fixture() {
        let Ok(mode) = std::env::var("ANSA_PTY_SIGCHLD_FIXTURE") else {
            return;
        };
        // Signal disposition faults are process-wide, so this entry is only
        // executed by the dedicated child test binary, never parallel suites.
        if mode == "lost_child" {
            let mut command = Command::new("sleep");
            command.arg("0.01");
            let mut session = PtySession::spawn(command, 10, 40).unwrap();
            // Model an unexpected external reaper, while SIGCHLD stays DFL.
            rustix::process::waitpid(Some(session.leader), rustix::process::WaitOptions::empty())
                .unwrap();
            assert_eq!(
                session.exited().unwrap_err().raw_os_error(),
                Some(libc::ECHILD)
            );
            let outcome = session.shutdown();
            assert!(!outcome.shell_reaped && !outcome.session_empty && outcome.error.is_some());
            assert!(session.ownership_unknown.get());
            // The cached unknown remains unknown; neither call can scan a
            // released SID or send a signal to its possibly reused number.
            let again = session.shutdown();
            assert_eq!(again.error, outcome.error);
            return;
        }
        extern "C" fn handler(_: libc::c_int) {}
        // SAFETY: fully initialized C action; the only process affected is the
        // dedicated fixture child, which has no other children or signal owner.
        let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
        unsafe {
            libc::sigemptyset(&mut action.sa_mask);
            match mode.as_str() {
                "ignore" => action.sa_sigaction = libc::SIG_IGN,
                "no_cldwait" => {
                    action.sa_sigaction = libc::SIG_DFL;
                    action.sa_flags = libc::SA_NOCLDWAIT;
                }
                "custom_handler" => action.sa_sigaction = handler as *const () as usize,
                _ => panic!("invalid fixture mode"),
            }
            assert_eq!(
                libc::sigaction(libc::SIGCHLD, &action, std::ptr::null_mut()),
                0
            );
        }
        let mut command = Command::new("sleep");
        command.arg("30");
        let error = match PtySession::spawn(command, 10, 40) {
            Ok(_) => panic!("unsafe SIGCHLD disposition admitted"),
            Err(error) => error,
        };
        assert_eq!(error.to_string(), "pty_sigchld_not_waitable");
        // Preflight observed the disposition without silently changing it.
        let mut current: libc::sigaction = unsafe { std::mem::zeroed() };
        unsafe {
            assert_eq!(
                libc::sigaction(libc::SIGCHLD, std::ptr::null(), &mut current),
                0
            );
        }
        assert_eq!(current.sa_sigaction, action.sa_sigaction);
        assert_eq!(
            current.sa_flags & libc::SA_NOCLDWAIT,
            action.sa_flags & libc::SA_NOCLDWAIT
        );
    }
}
