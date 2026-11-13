//! Fresh-executable process supervisor. The runtime never forks directly.
use crate::{
    config::{n, s},
    protocol::{json::Json, legacy_json_v1::sha256},
    provider::replay::{Effects, Execution},
    tools::{command::ProcessOptions, evidence as v, invocation::Invocation, registry::Outcome},
};
use ansa_linux::{fs, process as linux};
use std::{
    fs::File,
    io::{ErrorKind, Read},
    os::unix::process::ExitStatusExt,
    path::Path,
    process::{Child, Command, ExitStatus, Stdio},
    thread::sleep,
    time::{Duration, Instant},
};
type Result<T> = v::Result<T>;
struct Primary {
    child: Child,
    cleaned: bool,
    reaped: bool,
    status: Option<ExitStatus>,
}
impl Primary {
    fn cleanup(&mut self) -> std::io::Result<ExitStatus> {
        let raw = self.child.id();
        let pid = linux::Pid::from_raw(raw as i32).ok_or_else(|| std::io::Error::other("pid"))?;
        let deadline = Instant::now() + Duration::from_millis(1500);
        if !self.reaped {
            linux::kill_group(raw)?;
            if !linux::exited_owned(pid)? {
                linux::kill_owned(pid)?;
            }
        }
        loop {
            let children = linux::children(Path::new("/proc"))?;
            let others: Vec<_> = children
                .into_iter()
                .filter(|v| self.reaped || v.as_raw_nonzero().get() as u32 != raw)
                .collect();
            for child in &others {
                linux::kill_owned(*child)?;
                linux::reap_owned(*child)?;
            }
            if !self.reaped && others.is_empty() && linux::exited_owned(pid)? {
                let status = self.child.wait()?;
                self.reaped = true;
                self.status = Some(status);
            }
            // Reparenting can happen between the first children snapshot and
            // primary exit. Continue draining after reap, without using the
            // primary PID/PGID again (its identity can now be recycled).
            if self.reaped && linux::children(Path::new("/proc"))?.is_empty() {
                self.cleaned = true;
                return self
                    .status
                    .ok_or_else(|| std::io::Error::other("primary_status"));
            }
            if Instant::now() >= deadline {
                return Err(std::io::Error::other("cleanup_effect_unknown"));
            }
            sleep(Duration::from_millis(5));
        }
    }
}
impl Drop for Primary {
    fn drop(&mut self) {
        if !self.cleaned {
            let _ = self.cleanup();
        }
    }
}
fn cancelled() -> std::io::Result<bool> {
    let mut byte = [0];
    match std::io::stdin().read(&mut byte) {
        Ok(_) => Ok(true),
        Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::Interrupted) => Ok(false),
        Err(e) => Err(e),
    }
}
struct Output {
    saved: Vec<u8>,
    seen: usize,
}
impl Output {
    fn drain(
        &mut self,
        reader: &mut impl Read,
        other: usize,
        budget: usize,
    ) -> std::io::Result<()> {
        let mut chunk = [0; 4096];
        for _ in 0..16 {
            match reader.read(&mut chunk) {
                Ok(0) => break,
                Ok(count) => {
                    self.seen = self.seen.saturating_add(count);
                    let keep = count.min(budget - self.saved.len() - other);
                    self.saved.extend_from_slice(&chunk[..keep]);
                }
                Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }
}
fn output(
    status: Option<ExitStatus>,
    cancelled: bool,
    timed_out: bool,
    out: Output,
    err: Output,
    spawn_errno: usize,
    clean: bool,
) -> Result<Outcome> {
    let exit = status.map_or(127, |s| s.code().unwrap_or(-1));
    let signal = status.and_then(|s| s.signal()).unwrap_or(0);
    let body = Json::object(vec![
        ("exit_code", Json::parse(&exit.to_string())?),
        ("signal", n(signal as usize)),
        (
            "cancelled",
            Json::parse(if cancelled { "true" } else { "false" })?,
        ),
        (
            "timed_out",
            Json::parse(if timed_out { "true" } else { "false" })?,
        ),
        (
            "truncated",
            Json::parse(if out.seen + err.seen > out.saved.len() + err.saved.len() {
                "true"
            } else {
                "false"
            })?,
        ),
        ("stdout", s(&fs::utf8_text(&out.saved))?),
        ("stderr", s(&fs::utf8_text(&err.saved))?),
        ("stdout_bytes", n(out.seen)),
        ("stderr_bytes", n(err.seen)),
        ("spawn_errno", n(spawn_errno)),
        (
            "cleanup_complete",
            Json::parse(if clean { "true" } else { "false" })?,
        ),
    ])?;
    Ok(Outcome::from_bytes(
        if clean {
            Execution::Succeeded
        } else {
            Execution::Failed
        },
        if clean {
            Effects::KnownResult
        } else {
            Effects::Unknown
        },
        body.dump().into_bytes(),
    ))
}
fn supervise(spec: &Json) -> Result<Outcome> {
    linux::preflight(Path::new("/proc")).map_err(|_| "process_privileges")?;
    super::supervisor::capability_probe(
        &std::env::current_exe().map_err(|_| "process_executable")?,
    )
    .map_err(|_| "process_capabilities")?;
    let options = ProcessOptions::load(spec)?;
    if Path::new(&options.argv[0])
        .file_name()
        .is_some_and(|v| v == "sudo")
    {
        return Ok(Outcome::succeeded(
            r#"{"error":"sudo_disabled","exit_code":126}"#.into(),
        ));
    }
    let directory = File::open(&options.cwd).map_err(|_| "process_cwd")?;
    if !directory.metadata().map_err(|_| "process_cwd")?.is_dir() {
        return Err("process_cwd");
    }
    linux::nonblocking_stdin().map_err(|_| "process_nonblocking")?;
    let mut command = Command::new(&options.argv[0]);
    command
        .args(&options.argv[1..])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let child =
        match ansa_linux::managed::spawn(command, Some(&directory), Some(std::process::id())) {
            Ok(child) => child,
            Err(e) => {
                return output(
                    None,
                    false,
                    false,
                    Output {
                        saved: vec![],
                        seen: 0,
                    },
                    Output {
                        saved: vec![],
                        seen: 0,
                    },
                    e.raw_os_error().unwrap_or(5) as usize,
                    true,
                );
            }
        };
    let mut primary = Primary {
        child,
        cleaned: false,
        reaped: false,
        status: None,
    };
    let mut stdout = primary.child.stdout.take().ok_or("process_output")?;
    let mut stderr = primary.child.stderr.take().ok_or("process_output")?;
    ansa_linux::managed::nonblocking(&stdout).map_err(|_| "process_nonblocking")?;
    ansa_linux::managed::nonblocking(&stderr).map_err(|_| "process_nonblocking")?;
    let mut out = Output {
        saved: Vec::with_capacity(options.output_bytes),
        seen: 0,
    };
    let mut err = Output {
        saved: Vec::with_capacity(options.output_bytes),
        seen: 0,
    };
    let deadline = Instant::now() + Duration::from_millis(options.timeout_ms as u64);
    let pid = linux::Pid::from_raw(primary.child.id() as i32).ok_or("process_pid")?;
    let (cancelled, timed_out) = loop {
        out.drain(&mut stdout, err.saved.len(), options.output_bytes)
            .map_err(|_| "process_output")?;
        err.drain(&mut stderr, out.saved.len(), options.output_bytes)
            .map_err(|_| "process_output")?;
        let cancelled = cancelled().map_err(|_| "process_control")?;
        let timed_out = Instant::now() >= deadline;
        if cancelled || timed_out || linux::exited_owned(pid).map_err(|_| "process_wait")? {
            break (cancelled, timed_out);
        }
        sleep(Duration::from_millis(10));
    };
    let status = primary.cleanup();
    let clean = status.is_ok();
    out.drain(&mut stdout, err.saved.len(), options.output_bytes)
        .map_err(|_| "process_output")?;
    err.drain(&mut stderr, out.saved.len(), options.output_bytes)
        .map_err(|_| "process_output")?;
    output(status.ok(), cancelled, timed_out, out, err, 0, clean)
}
pub fn process_worker(job: &Path) -> Result<()> {
    linux::reset_signals().map_err(|_| "process_signal_reset")?;
    let directory = fs::private_directory(job).map_err(|_| "local_directory_permissions")?;
    let spec = v::read(&directory, "request.json")?.ok_or("process_job_request")?;
    if spec.usize_field("version")? != 1 {
        return Err("process_job_request");
    }
    let _lock = ansa_linux::local::lock(&directory).map_err(|_| "process_job_lock")?;
    let invocation = Invocation::load(spec.field("invocation")?)?;
    if v::read(&directory, "result.json")?.is_some() {
        v::admit(&directory, &invocation)?;
        return Ok(());
    }
    if v::read(&directory, "started.json")?.is_some() {
        return Err("process_consumed");
    }
    v::create(
        &directory,
        "started.json",
        &Json::object(vec![
            ("version", n(1)),
            ("request_hash", s(&sha256(spec.dump().as_bytes()))?),
        ])?,
    )?;
    let outcome = supervise(&spec).unwrap_or_else(|_| v::unknown());
    v::publish(&directory, &spec, &outcome)
}
/// Dispatch before Tokio, configuration, auth or any user-facing frontend.
pub fn private_entry(args: &[String]) -> Option<Result<()>> {
    if args.get(1)?.as_str() != "--process-worker" {
        return None;
    }
    Some(if args.len() == 3 {
        process_worker(Path::new(&args[2]))
    } else {
        Err("private_mode_arguments")
    })
}
