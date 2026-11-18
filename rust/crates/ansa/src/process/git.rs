//! Bounded Git invocations for local repository metadata and worktrees.
use crate::auth::Cancel;
use std::{
    fs::File,
    io::{ErrorKind, Read},
    path::Path,
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant},
};

struct GitChild(Child);
impl Drop for GitChild {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
pub(crate) enum GitOutput {
    Success(Vec<u8>),
    Failure,
    Missing,
    TooLarge,
}
pub(crate) fn run(
    cwd: &Path,
    args: &[&str],
    input: Option<File>,
    limit: usize,
    timeout: Duration,
    stop: &Cancel,
) -> Result<GitOutput, &'static str> {
    stop.check().map_err(|_| "workspace_cancelled")?;
    let child = Command::new("git")
        .args([
            "--no-pager",
            "--no-optional-locks",
            "-c",
            "core.fsmonitor=false",
            "-c",
            "core.hooksPath=/dev/null",
        ])
        .args(args)
        .current_dir(cwd)
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("LC_ALL", "C")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_NO_LAZY_FETCH", "1")
        .env("GIT_NO_REPLACE_OBJECTS", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(input.map_or_else(Stdio::null, Stdio::from))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn();
    let mut child = match child {
        Ok(child) => GitChild(child),
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(GitOutput::Missing),
        Err(_) => return Err("workspace_git_spawn"),
    };
    let mut stdout = child.0.stdout.take().ok_or("workspace_git_output")?;
    let mut stderr = child.0.stderr.take().ok_or("workspace_git_output")?;
    ansa_linux::managed::nonblocking(&stdout).map_err(|_| "workspace_git_output")?;
    ansa_linux::managed::nonblocking(&stderr).map_err(|_| "workspace_git_output")?;
    let deadline = Instant::now() + timeout;
    let mut bytes = Vec::new();
    loop {
        stop.check().map_err(|_| "workspace_cancelled")?;
        if Instant::now() >= deadline {
            return Err("workspace_git_timeout");
        }
        let mut buffer = [0; 8192];
        for _ in 0..16 {
            match stdout.read(&mut buffer) {
                Ok(0) => break,
                Ok(count) => {
                    if count > limit - bytes.len() {
                        return Ok(GitOutput::TooLarge);
                    }
                    bytes.extend_from_slice(&buffer[..count]);
                }
                Err(error) if error.kind() == ErrorKind::WouldBlock => break,
                Err(error) if error.kind() == ErrorKind::Interrupted => continue,
                Err(_) => return Err("workspace_git_output"),
            }
        }
        // Diagnostics are deliberately drained and discarded, never exposed as
        // terminal control sequences or copied into the model context.
        for _ in 0..4 {
            match stderr.read(&mut buffer) {
                Ok(0) => break,
                Ok(_) => {}
                Err(error) if error.kind() == ErrorKind::WouldBlock => break,
                Err(error) if error.kind() == ErrorKind::Interrupted => continue,
                Err(_) => return Err("workspace_git_output"),
            }
        }
        if let Some(status) = child.0.try_wait().map_err(|_| "workspace_git_wait")? {
            if !status.success() {
                return Ok(GitOutput::Failure);
            }
            // A successful exit can race the last stdout drain.
            loop {
                match stdout.read(&mut buffer) {
                    Ok(0) => break,
                    Ok(count) => {
                        if count > limit - bytes.len() {
                            return Ok(GitOutput::TooLarge);
                        }
                        bytes.extend_from_slice(&buffer[..count]);
                    }
                    Err(error) if error.kind() == ErrorKind::Interrupted => continue,
                    Err(error) if error.kind() == ErrorKind::WouldBlock => break,
                    Err(_) => return Err("workspace_git_output"),
                }
            }
            return Ok(GitOutput::Success(bytes));
        }
        thread::sleep(Duration::from_millis(5));
    }
}
