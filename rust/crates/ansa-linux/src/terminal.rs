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
impl Descriptor {
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
        raw.local_modes
            .remove(rustix::termios::LocalModes::ICANON | rustix::termios::LocalModes::ECHO);
        raw.special_codes[rustix::termios::SpecialCodeIndex::VMIN] = 1;
        raw.special_codes[rustix::termios::SpecialCodeIndex::VTIME] = 0;
        rustix::termios::tcsetattr(&self.fd, rustix::termios::OptionalActions::Now, &raw)?;
        self.active = true;
        Ok(())
    }
    pub fn restore(&mut self) {
        if self.active {
            let _ = rustix::termios::tcsetattr(
                &self.fd,
                rustix::termios::OptionalActions::Now,
                &self.saved,
            );
            self.active = false;
        }
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
