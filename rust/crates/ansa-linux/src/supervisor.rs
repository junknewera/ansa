//! Atomic pidfd ownership for a reexec supervisor, including SIGCHLD auto-reap.
use rustix::{
    fd::OwnedFd,
    process::{Pid, Signal},
};
use std::{
    ffi::CString,
    fs::File,
    io,
    os::{
        fd::{AsFd, AsRawFd, FromRawFd},
        unix::process::ExitStatusExt,
    },
    path::Path,
    process::ExitStatus,
    thread::sleep,
    time::{Duration, Instant},
};
#[repr(C)]
#[derive(Default)]
struct CloneArgs {
    flags: u64,
    pidfd: u64,
    child_tid: u64,
    parent_tid: u64,
    exit_signal: u64,
    stack: u64,
    stack_size: u64,
    tls: u64,
    set_tid: u64,
    set_tid_size: u64,
    cgroup: u64,
}
pub struct Supervisor {
    pub control: Option<File>,
    pid: Pid,
    pidfd: OwnedFd,
    terminal: bool,
    status: Option<ExitStatus>,
    #[cfg(test)]
    fail_wait: bool,
}
impl Supervisor {
    pub fn id(&self) -> u32 {
        self.pid.as_raw_nonzero().get() as u32
    }
    pub fn spawn(executable: &Path, job: &Path) -> io::Result<Self> {
        let argv = [
            cstring(executable.as_os_str().as_encoded_bytes())?,
            CString::new("--process-worker").expect("literal"),
            cstring(job.as_os_str().as_encoded_bytes())?,
        ];
        Self::spawn_prepared(&argv)
    }
    fn spawn_prepared(argv: &[CString]) -> io::Result<Self> {
        if argv.is_empty() {
            return Err(io::Error::other("empty_argv"));
        }
        let mut argv_ptr: Vec<_> = argv.iter().map(|v| v.as_ptr()).collect();
        argv_ptr.push(std::ptr::null());
        // Copy through std's environment access before clone; no environment
        // pointers or allocator/locks are touched in the child branch.
        let environment = std::env::vars_os()
            .map(|(key, value)| {
                let mut item = key.into_encoded_bytes();
                item.push(b'=');
                item.extend(value.into_encoded_bytes());
                cstring(&item)
            })
            .collect::<io::Result<Vec<_>>>()?;
        let mut env_ptr: Vec<_> = environment.iter().map(|v| v.as_ptr()).collect();
        env_ptr.push(std::ptr::null());
        let (read, write) = rustix::pipe::pipe_with(rustix::pipe::PipeFlags::CLOEXEC)?;
        // dup2(fd,fd) preserves CLOEXEC. Reserve sources above stdio even when
        // the caller entered with 0/1/2 closed, so every dup2 clears the flag.
        let read = rustix::io::fcntl_dupfd_cloexec(&read, 3)?;
        let null = File::options().write(true).open("/dev/null")?;
        let null = rustix::io::fcntl_dupfd_cloexec(&null, 3)?;
        let mut descriptor = -1_i32;
        let args = CloneArgs {
            flags: libc::CLONE_PIDFD as u64,
            pidfd: (&mut descriptor as *mut i32) as u64,
            exit_signal: libc::SIGCHLD as u64,
            ..CloneArgs::default()
        };
        // SAFETY: clone3 has fork semantics (no shared VM/stack/TLS flags).
        // The kernel writes one i32 pidfd into live parent stack storage.
        // CString buffers/pointer arrays and FDs are prepared and remain alive
        // through execve. The child runs only initialized stack operations and
        // libc syscalls, then execve/_exit; it never unwinds or drops Rust data.
        // CLONE_PIDFD obtains identity atomically, even when SIGCHLD is ignored.
        let child = unsafe {
            let child = libc::syscall(
                libc::SYS_clone3,
                &args as *const CloneArgs,
                std::mem::size_of::<CloneArgs>(),
            );
            if child == 0 {
                let header = [0x2008_0522_u32, 0];
                let caps = [0_u32; 6];
                if libc::getuid() == 0
                    || libc::getuid() != libc::geteuid()
                    || libc::setsid() < 0
                    || libc::syscall(libc::SYS_capset, header.as_ptr(), caps.as_ptr()) != 0
                    || libc::prctl(
                        libc::PR_SET_NO_NEW_PRIVS,
                        1 as libc::c_ulong,
                        0 as libc::c_ulong,
                        0 as libc::c_ulong,
                        0 as libc::c_ulong,
                    ) != 0
                    || libc::dup2(read.as_raw_fd(), 0) < 0
                    || libc::dup2(null.as_raw_fd(), 1) < 0
                    || libc::dup2(null.as_raw_fd(), 2) < 0
                    || libc::syscall(libc::SYS_close_range, 3_u32, u32::MAX, 4_u32) != 0
                {
                    libc::_exit(125);
                }
                libc::execve(argv[0].as_ptr(), argv_ptr.as_ptr(), env_ptr.as_ptr());
                libc::_exit(125);
            }
            child
        };
        if child < 0 {
            return Err(io::Error::last_os_error());
        }
        if descriptor < 0 {
            return Err(io::Error::other("clone_pidfd_missing"));
        }
        // SAFETY: successful CLONE_PIDFD returned a new owned fd, exactly once.
        let pidfd = unsafe { OwnedFd::from_raw_fd(descriptor) };
        Ok(Self {
            control: Some(File::from(write)),
            pid: Pid::from_raw(child as i32).ok_or_else(|| io::Error::other("pid"))?,
            pidfd,
            terminal: false,
            status: None,
            #[cfg(test)]
            fail_wait: false,
        })
    }
    fn dead(&self) -> io::Result<bool> {
        let mut fds = [rustix::event::PollFd::new(
            &self.pidfd,
            rustix::event::PollFlags::IN,
        )];
        rustix::event::poll(
            &mut fds,
            Some(&rustix::event::Timespec {
                tv_sec: 0,
                tv_nsec: 0,
            }),
        )?;
        Ok(!fds[0].revents().is_empty())
    }
    pub fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        if self.terminal {
            return self
                .status
                .map(Some)
                .ok_or_else(|| io::Error::from_raw_os_error(libc::ECHILD));
        }
        #[cfg(test)]
        if self.fail_wait {
            return Err(io::Error::from_raw_os_error(libc::EIO));
        }
        match rustix::process::waitid(
            rustix::process::WaitId::PidFd(self.pidfd.as_fd()),
            rustix::process::WaitIdOptions::EXITED | rustix::process::WaitIdOptions::NOHANG,
        ) {
            Ok(Some(status)) => {
                self.terminal = true;
                let status = ExitStatus::from_raw(
                    status
                        .exit_status()
                        .map(|v| v << 8)
                        .unwrap_or_else(|| status.terminating_signal().unwrap_or(0)),
                );
                self.status = Some(status);
                Ok(Some(status))
            }
            Ok(None) => Ok(None),
            Err(rustix::io::Errno::CHILD) => {
                if self.dead()? {
                    self.terminal = true;
                }
                Err(io::Error::from_raw_os_error(libc::ECHILD))
            }
            Err(e) => Err(e.into()),
        }
    }
    pub fn signal(&self, signal: Signal) -> io::Result<()> {
        match rustix::process::pidfd_send_signal(&self.pidfd, signal) {
            Ok(()) | Err(rustix::io::Errno::SRCH) => Ok(()),
            Err(e) => Err(e.into()),
        }
    }
    pub fn finish(&mut self) -> io::Result<()> {
        self.control.take();
        if self.terminal {
            return Ok(());
        }
        let deadline = Instant::now() + Duration::from_millis(1700);
        loop {
            match self.try_wait() {
                Ok(Some(_)) => return Ok(()),
                Err(e) if e.raw_os_error() == Some(libc::ECHILD) && self.terminal => return Ok(()),
                _ => {}
            }
            if Instant::now() >= deadline {
                break;
            }
            sleep(Duration::from_millis(5));
        }
        self.signal(Signal::KILL)?;
        // A different wait syscall still reaps when waitpid was interrupted or
        // failed. Auto-reap is terminal only after this exact pidfd is readable.
        loop {
            match rustix::process::waitid(
                rustix::process::WaitId::PidFd(self.pidfd.as_fd()),
                rustix::process::WaitIdOptions::EXITED,
            ) {
                Ok(_) => {
                    self.terminal = true;
                    return Ok(());
                }
                Err(rustix::io::Errno::INTR) => continue,
                Err(rustix::io::Errno::CHILD) if self.dead()? => {
                    self.terminal = true;
                    return Ok(());
                }
                Err(e) => return Err(e.into()),
            }
        }
    }
}
impl Drop for Supervisor {
    fn drop(&mut self) {
        if !self.terminal {
            let _ = self.finish();
        }
    }
}
fn cstring(bytes: &[u8]) -> io::Result<CString> {
    CString::new(bytes).map_err(|_| io::Error::other("process_arguments"))
}
#[cfg(test)]
mod tests {
    use super::*;
    fn sleeper() -> Supervisor {
        let shell = std::env::split_paths(&std::env::var_os("PATH").expect("pinned test PATH"))
            .map(|p| p.join("sh"))
            .find(|p| p.is_file())
            .expect("pinned test shell");
        Supervisor::spawn_prepared(&[
            cstring(shell.as_os_str().as_encoded_bytes()).unwrap(),
            CString::new("-c").unwrap(),
            CString::new("exec sleep 10").unwrap(),
        ])
        .unwrap()
    }
    #[test]
    fn wait_error_keeps_handle_until_reap() {
        let mut child = sleeper();
        let pid = child.pid;
        child.fail_wait = true;
        assert_eq!(
            child.try_wait().unwrap_err().raw_os_error(),
            Some(libc::EIO)
        );
        child.finish().unwrap();
        assert!(child.terminal);
        assert!(
            !Path::new("/proc")
                .join(pid.as_raw_nonzero().get().to_string())
                .exists()
        );
    }
    #[test]
    fn drop_reaps_live_supervisor_after_wait_error() {
        let mut child = sleeper();
        let pid = child.pid;
        child.fail_wait = true;
        drop(child);
        assert!(
            !Path::new("/proc")
                .join(pid.as_raw_nonzero().get().to_string())
                .exists()
        );
    }
    #[test]
    fn closed_stdio_keeps_control_pipe() {
        use std::{
            os::unix::fs::DirBuilderExt,
            process::{Command, Stdio},
        };
        let root = std::env::temp_dir().join(format!("ansa-m6-stdio-{}", std::process::id()));
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&root)
            .unwrap();
        for all in [false, true] {
            let marker = root.join(if all { "all" } else { "stdin" });
            let status = Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "supervisor::tests::private_closed_stdio_helper",
                    "--nocapture",
                ])
                .env("ANSA_M6_STDIO_FIXTURE", &marker)
                .env("ANSA_M6_STDIO_ALL", if all { "1" } else { "0" })
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .unwrap();
            assert!(status.success());
            assert_eq!(std::fs::read(marker).unwrap(), b"PASS");
        }
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn private_closed_stdio_helper() {
        use std::io::Write;
        let Some(marker) = std::env::var_os("ANSA_M6_STDIO_FIXTURE") else {
            return;
        };
        let all = std::env::var_os("ANSA_M6_STDIO_ALL").is_some_and(|v| v == "1");
        let shell = std::env::split_paths(&std::env::var_os("PATH").unwrap())
            .map(|p| p.join("sh"))
            .find(|p| p.is_file())
            .unwrap();
        // SAFETY: this function runs only in its fresh disposable test process.
        // No borrowed stdio descriptors exist in the fixture. Exit bypasses
        // the test harness's normal output after descriptors are closed.
        unsafe {
            libc::close(0);
            if all {
                libc::close(1);
                libc::close(2);
            }
        }
        let mut child = Supervisor::spawn_prepared(&[
            cstring(shell.as_os_str().as_encoded_bytes()).unwrap(),
            CString::new("-c").unwrap(),
            CString::new("read line; test \"$line\" = resume").unwrap(),
        ])
        .unwrap();
        child
            .control
            .as_mut()
            .unwrap()
            .write_all(b"resume\n")
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            match child.try_wait() {
                Ok(Some(status)) => {
                    if status.success() {
                        std::fs::write(marker, b"PASS").unwrap();
                        std::process::exit(0);
                    }
                    std::process::exit(1);
                }
                Ok(None) if Instant::now() < deadline => sleep(Duration::from_millis(5)),
                _ => std::process::exit(1),
            }
        }
    }
}
