//! Child setup for fresh managed helpers. No allocator, locks or Rust cleanup
//! are used by the pre_exec callback; only initialized stack data and syscalls.
use std::{
    fs::File,
    io,
    os::{fd::AsRawFd, unix::process::CommandExt},
    process::{Child, Command},
};
pub fn spawn(
    mut command: Command,
    directory: Option<&File>,
    parent: Option<u32>,
) -> io::Result<Child> {
    let directory = directory.map(AsRawFd::as_raw_fd);
    // SAFETY: Command is consumed here and spawned synchronously, so the
    // borrowed directory stays open until spawn returns and callbacks cannot
    // escape. The callback has no heap operations/locks/handlers/Drop. capset
    // uses Linux v3's two contiguous three-u32 data records; all pointers and
    // arguments are initialized. No arbitrary user callback runs after fork.
    unsafe {
        command.pre_exec(move || {
            if libc::getuid() == 0 || libc::getuid() != libc::geteuid() {
                return Err(io::Error::from_raw_os_error(libc::EPERM));
            }
            if let Some(parent) = parent {
                if libc::prctl(
                    libc::PR_SET_PDEATHSIG,
                    libc::SIGKILL as libc::c_ulong,
                    0 as libc::c_ulong,
                    0 as libc::c_ulong,
                    0 as libc::c_ulong,
                ) != 0
                    || libc::getppid() != parent as libc::pid_t
                {
                    return Err(io::Error::from_raw_os_error(libc::ESRCH));
                }
                if libc::setpgid(0, 0) != 0 {
                    return Err(io::Error::last_os_error());
                }
            } else if libc::setsid() < 0 {
                return Err(io::Error::last_os_error());
            }
            let header = [0x2008_0522_u32, 0];
            let caps = [0_u32; 6];
            if libc::syscall(libc::SYS_capset, header.as_ptr(), caps.as_ptr()) != 0
                || libc::prctl(
                    libc::PR_SET_NO_NEW_PRIVS,
                    1 as libc::c_ulong,
                    0 as libc::c_ulong,
                    0 as libc::c_ulong,
                    0 as libc::c_ulong,
                ) != 0
            {
                return Err(io::Error::last_os_error());
            }
            if let Some(directory) = directory
                && libc::fchdir(directory) != 0
            {
                return Err(io::Error::last_os_error());
            }
            if libc::syscall(libc::SYS_close_range, 3_u32, u32::MAX, 4_u32) != 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    command.spawn()
}
pub fn nonblocking(file: &impl std::os::fd::AsFd) -> io::Result<()> {
    let flags = rustix::fs::fcntl_getfl(file)?;
    rustix::fs::fcntl_setfl(file, flags | rustix::fs::OFlags::NONBLOCK)?;
    Ok(())
}
pub fn continue_owned(pid: u32) -> io::Result<()> {
    let pid = rustix::process::Pid::from_raw(pid as i32).ok_or_else(|| io::Error::other("pid"))?;
    rustix::process::waitid(
        rustix::process::WaitId::Pid(pid),
        rustix::process::WaitIdOptions::EXITED
            | rustix::process::WaitIdOptions::NOHANG
            | rustix::process::WaitIdOptions::NOWAIT,
    )?;
    let fd = rustix::process::pidfd_open(pid, rustix::process::PidfdFlags::empty())?;
    rustix::process::pidfd_send_signal(&fd, rustix::process::Signal::CONT)?;
    Ok(())
}
