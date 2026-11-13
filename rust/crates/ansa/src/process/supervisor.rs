//! Isolated M1 fixture supervisor; no agent runtime or arbitrary command API.
use ansa_linux::{fs, process as linux};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread::sleep;
use std::time::{Duration, Instant};

struct OwnedPrimary {
    child: Child,
    cleaned: bool,
    reaped: bool,
}
impl OwnedPrimary {
    fn cleanup(&mut self) -> io::Result<()> {
        let primary = self.child.id();
        let primary_pid =
            linux::Pid::from_raw(primary as i32).ok_or_else(|| io::Error::other("primary_pid"))?;
        let until = Instant::now() + Duration::from_secs(2);
        if !self.reaped {
            linux::kill_group(primary)?;
            linux::kill_owned(primary_pid)?;
            while !linux::exited_owned(primary_pid)? {
                if Instant::now() >= until {
                    return Err(io::Error::other("primary_exit_unknown"));
                }
                sleep(Duration::from_millis(2));
            }
        }
        loop {
            let children = linux::children(Path::new("/proc"))?;
            let others: Vec<_> = children
                .into_iter()
                .filter(|p| p.as_raw_nonzero().get() as u32 != primary)
                .collect();
            for pid in &others {
                linux::kill_owned(*pid)?;
                linux::reap_owned(*pid)?;
            }
            if others.is_empty() {
                // Primary is still waitable: its PID/group identity cannot be reused.
                if !self.reaped {
                    self.child.wait()?;
                    self.reaped = true;
                }
                if linux::children(Path::new("/proc"))?.is_empty() {
                    self.cleaned = true;
                    return Ok(());
                }
            }
            if Instant::now() >= until {
                return Err(io::Error::other("cleanup_effect_unknown"));
            }
            sleep(Duration::from_millis(2));
        }
    }
}
impl Drop for OwnedPrimary {
    fn drop(&mut self) {
        if !self.cleaned {
            let _ = self.cleanup();
        }
    }
}

fn cancelled() -> io::Result<bool> {
    let mut byte = [0];
    match io::stdin().read(&mut byte) {
        Ok(_) => Ok(true), // Both an addressed test cancel and control EOF stop dispatch.
        Err(e) if e.kind() == io::ErrorKind::WouldBlock => Ok(false),
        Err(e) => Err(e),
    }
}

fn capability_probe(executable: &Path) -> io::Result<()> {
    let mut child = Command::new(executable)
        .arg("--m1-capability-child")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    let pid =
        linux::Pid::from_raw(child.id() as i32).ok_or_else(|| io::Error::other("probe_pid"))?;
    let result = (|| {
        linux::kill_owned(pid)?; // Exercise pidfd_open/send_signal and waitid NOWAIT before primary dispatch.
        let until = Instant::now() + Duration::from_secs(1);
        while !linux::exited_owned(pid)? {
            if Instant::now() >= until {
                return Err(io::Error::other("waitid_preflight"));
            }
            sleep(Duration::from_millis(1));
        }
        if !linux::exited_owned(pid)? {
            return Err(io::Error::other("waitid_consumed_status"));
        }
        Ok(())
    })();
    let _ = child.kill(); // The std Child is retained and waitable, including on unsupported pidfd.
    child.wait()?;
    result
}

fn worker(path: &Path, fault: &str) -> io::Result<()> {
    linux::reset_signals()?;
    let directory = fs::private_directory(path)?;
    let proc_root = if fault == "missing-proc" {
        path.join("unavailable-proc")
    } else {
        PathBuf::from("/proc")
    };
    linux::preflight(&proc_root)?; // Must fail before consume/primary exec.
    let executable = std::env::current_exe()?;
    if fault == "missing-pidfd" {
        linux::pidfd_denial_fault()?;
    }
    capability_probe(&executable)
        .map_err(|error| io::Error::other(format!("pidfd_capability_probe: {error}")))?;
    linux::nonblocking_stdin()?;
    if cancelled()? {
        return Err(io::Error::other("cancelled_before_dispatch"));
    }
    fs::consume(&directory)?; // Durable consume-once intent before synthetic side effects.
    if fault == "intent-pause" {
        println!("{}", serde_json::json!({"state":"intent"}));
        io::stdout().flush()?;
        sleep(Duration::from_millis(150));
    }
    if cancelled()? {
        return Err(io::Error::other("cancelled_before_dispatch"));
    }
    let child = Command::new(&executable)
        .arg("--m1-primary")
        .arg(path)
        .arg(std::process::id().to_string())
        .arg(fs::identity(&directory)?)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    let mut primary = OwnedPrimary {
        child,
        cleaned: false,
        reaped: false,
    };
    let until = Instant::now() + Duration::from_secs(2);
    let detached = loop {
        if let (Some(_), Some(pid)) = (
            fs::read_record(&directory, "primary.started")?,
            fs::read_record(&directory, "detached.pid")?,
        ) {
            break pid.trim().parse::<u32>().map_err(io::Error::other)?;
        }
        if cancelled()? || Instant::now() >= until {
            return Err(io::Error::other("fixture_start_cancelled"));
        }
        sleep(Duration::from_millis(2));
    };
    let threads = std::fs::read_dir("/proc/self/task")?
        .collect::<Result<Vec<_>, _>>()?
        .len();
    println!(
        "{}",
        serde_json::json!({"schema_version":1,"state":"ready","primary":primary.child.id(),"detached":detached,"threads":threads})
    );
    io::stdout().flush()?;
    let until = Instant::now() + Duration::from_secs(2);
    while !cancelled()? && Instant::now() < until {
        sleep(Duration::from_millis(2));
    }
    primary.cleanup()?;
    fs::record(&directory, "outcome", "known_cancelled\n")?;
    println!(
        "{}",
        serde_json::json!({"schema_version":1,"state":"cancelled","effect":"known_result","reaped":true})
    );
    Ok(())
}

fn primary(path: &Path, parent: u32, identity: &str) -> io::Result<()> {
    linux::reset_signals()?;
    linux::primary_setup(parent)?;
    let directory = fs::private_directory(path)?;
    fs::authorize_helper(&directory, identity, "primary.claim")?;
    fs::record(
        &directory,
        "primary.started",
        &std::process::id().to_string(),
    )?;
    let _child = Command::new(std::env::current_exe()?)
        .arg("--m1-detached")
        .arg(path)
        .arg(identity)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    sleep(Duration::from_secs(10));
    Ok(())
}
fn detached(path: &Path, identity: &str) -> io::Result<()> {
    linux::reset_signals()?;
    linux::detached_setup()?;
    let directory = fs::private_directory(path)?;
    fs::authorize_helper(&directory, identity, "detached.claim")?;
    fs::record(&directory, "detached.pid", &std::process::id().to_string())?;
    sleep(Duration::from_secs(10));
    Ok(())
}

/// Called from main before any Tokio/config/auth bootstrap.
pub fn private_entry(args: &[String]) -> Option<io::Result<()>> {
    let mode = args.get(1)?.as_str();
    if !matches!(
        mode,
        "--m1-supervisor"
            | "--m1-missing-proc"
            | "--m1-missing-pidfd"
            | "--m1-intent-pause"
            | "--m1-primary"
            | "--m1-detached"
            | "--m1-inherited-supervisor"
            | "--m1-capability-child"
    ) {
        return None;
    }
    Some((|| {
        if mode == "--m1-capability-child" && args.len() == 2 {
            sleep(Duration::from_millis(100));
            return Ok(());
        }
        if args.len()
            != match mode {
                "--m1-primary" => 5,
                "--m1-detached" => 4,
                _ => 3,
            }
        {
            return Err(io::Error::other("private_mode_arguments"));
        }
        let path = Path::new(
            args.get(2)
                .ok_or_else(|| io::Error::other("private_mode_arguments"))?,
        );
        match mode {
            "--m1-supervisor" => worker(path, ""),
            "--m1-missing-proc" => worker(path, "missing-proc"),
            "--m1-missing-pidfd" => worker(path, "missing-pidfd"),
            "--m1-intent-pause" => worker(path, "intent-pause"),
            "--m1-primary" => primary(path, args[3].parse().map_err(io::Error::other)?, &args[4]),
            "--m1-detached" => detached(path, &args[3]),
            "--m1-inherited-supervisor" => {
                use std::os::unix::process::CommandExt;
                linux::inherited_signal_fault()?;
                Err(Command::new(std::env::current_exe()?)
                    .arg("--m1-supervisor")
                    .arg(path)
                    .exec())
            }
            _ => unreachable!(),
        }
    })())
}
