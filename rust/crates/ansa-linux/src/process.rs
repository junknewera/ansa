pub use rustix::process::{Pid, Signal};
use std::io;
use std::path::Path;

/// Called only by private fresh-executable modes, before threads/Tokio/auth.
pub fn reset_signals() -> io::Result<()> {
    if std::fs::read_dir("/proc/self/task")?.count() != 1 {
        return Err(io::Error::other("private_process_must_be_single_threaded"));
    }
    // rustix deliberately exposes only experimental runtime-level sigaction.
    // libc preserves its reserved signals. No Rust signal handler is installed:
    // only inherited SIG_IGN dispositions are reset, then the empty mask is set.
    // SAFETY: all C structs/pointers are initialized, valid for the calls, and
    // private process entry precedes every spawned thread and child.
    unsafe {
        let mut empty: libc::sigset_t = std::mem::zeroed();
        if libc::sigemptyset(&mut empty) != 0 {
            return Err(io::Error::last_os_error());
        }
        for signal in 1..libc::SIGRTMIN() {
            if signal == libc::SIGKILL || signal == libc::SIGSTOP || signal == 32 || signal == 33 {
                continue;
            }
            let mut old: libc::sigaction = std::mem::zeroed();
            if libc::sigaction(signal, std::ptr::null(), &mut old) != 0 {
                return Err(io::Error::last_os_error());
            }
            if old.sa_sigaction == libc::SIG_IGN {
                let mut action: libc::sigaction = std::mem::zeroed();
                action.sa_sigaction = libc::SIG_DFL;
                action.sa_mask = empty;
                if libc::sigaction(signal, &action, std::ptr::null_mut()) != 0 {
                    return Err(io::Error::last_os_error());
                }
            }
        }
        let status = libc::pthread_sigmask(libc::SIG_SETMASK, &empty, std::ptr::null_mut());
        if status != 0 {
            return Err(io::Error::from_raw_os_error(status));
        }
    }
    Ok(())
}

/// Synthetic inherited-signal fault; private helper process only.
pub fn inherited_signal_fault() -> io::Result<()> {
    // SAFETY: valid initialized structs, no handler pointers, isolated process.
    unsafe {
        let mut action: libc::sigaction = std::mem::zeroed();
        action.sa_sigaction = libc::SIG_IGN;
        libc::sigemptyset(&mut action.sa_mask);
        if libc::sigaction(libc::SIGCHLD, &action, std::ptr::null_mut()) != 0 {
            return Err(io::Error::last_os_error());
        }
        let mut mask: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut mask);
        libc::sigaddset(&mut mask, libc::SIGTERM);
        let status = libc::pthread_sigmask(libc::SIG_BLOCK, &mask, std::ptr::null_mut());
        if status != 0 {
            return Err(io::Error::from_raw_os_error(status));
        }
    }
    Ok(())
}

/// Deny pidfd syscalls with real EPERM in an isolated, no_new_privs M1 child.
/// No global policy is changed. The filter is inherited only by its children.
pub fn pidfd_denial_fault() -> io::Result<()> {
    #[cfg(target_arch = "aarch64")]
    const ARCH: u32 = 0xc00000b7;
    #[cfg(target_arch = "x86_64")]
    const ARCH: u32 = 0xc000003e;
    #[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
    compile_error!("M1 seccomp fault needs an explicit native audit architecture");
    let instruction = |code, jt, jf, k| libc::sock_filter { code, jt, jf, k };
    let mut filter = [
        instruction(0x20, 0, 0, 4), // LD arch from struct seccomp_data.
        instruction(0x15, 1, 0, ARCH),
        instruction(0x06, 0, 0, 0x80000000), // KILL_PROCESS on foreign ABI.
        instruction(0x20, 0, 0, 0),          // LD syscall nr.
        instruction(0x15, 0, 1, libc::SYS_pidfd_open as u32),
        instruction(0x06, 0, 0, 0x00050000 | libc::EPERM as u32),
        instruction(0x15, 0, 1, libc::SYS_pidfd_send_signal as u32),
        instruction(0x06, 0, 0, 0x00050000 | libc::EPERM as u32),
        instruction(0x06, 0, 0, 0x7fff0000), // ALLOW everything else.
    ];
    let program = libc::sock_fprog {
        len: filter.len() as u16,
        filter: filter.as_mut_ptr(),
    };
    // SAFETY: initialized classic BPF array remains alive during the copying
    // syscall. Fresh single-threaded process, NNP already set by preflight.
    if unsafe {
        libc::prctl(
            libc::PR_SET_SECCOMP,
            2 as libc::c_ulong,
            &program as *const libc::sock_fprog,
            0 as libc::c_ulong,
            0 as libc::c_ulong,
        )
    } != 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

pub fn preflight(proc_root: &Path) -> io::Result<()> {
    if rustix::process::getuid().is_root()
        || rustix::process::getuid() != rustix::process::geteuid()
    {
        return Err(io::Error::other("non_root_identity_required"));
    }
    children(proc_root)?;
    subreaper()?;
    if rustix::process::child_subreaper()?.is_none() {
        return Err(io::Error::other("subreaper_unavailable"));
    }
    rustix::thread::set_no_new_privs(true)?;
    if !rustix::thread::no_new_privs()? {
        return Err(io::Error::other("no_new_privs_unavailable"));
    }
    rustix::thread::set_capabilities(
        None,
        rustix::thread::CapabilitySets {
            effective: rustix::thread::CapabilitySet::empty(),
            permitted: rustix::thread::CapabilitySet::empty(),
            inheritable: rustix::thread::CapabilitySet::empty(),
        },
    )?;
    rustix::thread::clear_ambient_capability_set()?;
    let status = std::fs::read_to_string("/proc/self/status")?;
    for field in ["CapInh:", "CapPrm:", "CapEff:"] {
        let value = status
            .lines()
            .find_map(|line| line.strip_prefix(field))
            .ok_or_else(|| io::Error::other("capability_status"))?;
        if u64::from_str_radix(value.trim(), 16).map_err(io::Error::other)? != 0 {
            return Err(io::Error::other("fixture_requires_zero_capabilities"));
        }
    }
    Ok(())
}

pub fn children(proc_root: &Path) -> io::Result<Vec<Pid>> {
    let id = std::process::id().to_string();
    let bytes = std::fs::read(proc_root.join(&id).join("task").join(&id).join("children"))?;
    if bytes.len() > 4096 {
        return Err(io::Error::other("children_limit"));
    }
    let text = std::str::from_utf8(&bytes).map_err(io::Error::other)?;
    text.split_whitespace()
        .map(|part| {
            let value: u32 = part.parse().map_err(io::Error::other)?;
            Pid::from_raw(value as i32).ok_or_else(|| io::Error::other("child_pid"))
        })
        .collect()
}

pub fn subreaper() -> io::Result<()> {
    // rustix 1.1.6 models this kernel boolean as Option<Pid>; raw 1 enables it.
    Ok(rustix::process::set_child_subreaper(Pid::from_raw(1))?)
}
pub fn primary_setup(parent: u32) -> io::Result<()> {
    rustix::process::setsid()?;
    rustix::thread::set_no_new_privs(true)?;
    rustix::process::set_parent_process_death_signal(Some(Signal::KILL))?;
    if rustix::process::getppid().map(|p| p.as_raw_nonzero().get() as u32) != Some(parent) {
        return Err(io::Error::other("parent_changed"));
    }
    Ok(())
}
pub fn detached_setup() -> io::Result<()> {
    rustix::process::setsid()?;
    Ok(())
}
pub fn nonblocking_stdin() -> io::Result<()> {
    let input = std::io::stdin();
    let flags = rustix::fs::fcntl_getfl(&input)?;
    rustix::fs::fcntl_setfl(&input, flags | rustix::fs::OFlags::NONBLOCK)?;
    Ok(())
}
pub fn kill_group(pid: u32) -> io::Result<()> {
    let pid = Pid::from_raw(pid as i32).ok_or_else(|| io::Error::other("pid"))?;
    match rustix::process::kill_process_group(pid, Signal::KILL) {
        Ok(()) | Err(rustix::io::Errno::SRCH) => Ok(()),
        Err(e) => Err(e.into()),
    }
}
pub fn kill_owned(pid: Pid) -> io::Result<()> {
    // ECHILD rejects an arbitrary/recycled PID. A zombie is retained until reap.
    rustix::process::waitid(
        rustix::process::WaitId::Pid(pid),
        rustix::process::WaitIdOptions::EXITED
            | rustix::process::WaitIdOptions::NOHANG
            | rustix::process::WaitIdOptions::NOWAIT,
    )?;
    let fd = rustix::process::pidfd_open(pid, rustix::process::PidfdFlags::empty())?;
    match rustix::process::pidfd_send_signal(&fd, Signal::KILL) {
        Ok(()) | Err(rustix::io::Errno::SRCH) => Ok(()),
        Err(e) => Err(e.into()),
    }
}
pub fn reap_owned(pid: Pid) -> io::Result<bool> {
    Ok(rustix::process::waitpid(Some(pid), rustix::process::WaitOptions::NOHANG)?.is_some())
}
pub fn exited_owned(pid: Pid) -> io::Result<bool> {
    Ok(rustix::process::waitid(
        rustix::process::WaitId::Pid(pid),
        rustix::process::WaitIdOptions::EXITED
            | rustix::process::WaitIdOptions::NOHANG
            | rustix::process::WaitIdOptions::NOWAIT,
    )?
    .is_some())
}
