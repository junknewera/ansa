//! Command integration for the session's interactive Bash. Command bytes and
//! prompt notifications use different FIFOs, so output cannot become a receipt.
use super::protocol::Id;
use ring::rand::{SecureRandom, SystemRandom};
use std::{
    fs::{self, DirBuilder, File, OpenOptions},
    io::{self, Read, Write},
    os::unix::fs::{DirBuilderExt, OpenOptionsExt},
    path::{Path, PathBuf},
    process::Command,
};

pub(super) const ARGV: [&str; 5] = ["bash", "--noprofile", "--norc", "--noediting", "-i"];
const REPORT_BYTES: usize = 256;
const OUTPUT_BYTES: usize = super::native::MAX_READ;

fn quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

pub(super) fn supported(argv: &[String]) -> bool {
    argv == ARGV
        || argv.split_first().is_some_and(|(bash, args)| {
            Path::new(bash)
                .file_name()
                .is_some_and(|name| name == "bash")
                && (args.is_empty() || args == ["-i"] || args == ["--noprofile", "--norc", "-i"])
        })
}

pub(super) struct Shell {
    directory: PathBuf,
    reports: File,
    report: Vec<u8>,
    ready: bool,
    raw_unstable: bool,
    raw_pending: usize,
    run: Option<Run>,
    background: Vec<File>,
}

struct Run {
    operation: Id,
    pipe: Option<File>,
    output: Vec<u8>,
    truncated: bool,
    started: bool,
    exit_code: Option<i32>,
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub(super) enum State {
    Unavailable,
    Busy,
    Ready,
    Starting,
    Running,
    Finished {
        exit_code: i32,
        output: String,
        truncated: bool,
    },
}

impl Shell {
    pub(super) fn prepare(command: &mut Command, user_rc: bool) -> io::Result<Self> {
        let mut random = [0; 16];
        SystemRandom::new()
            .fill(&mut random)
            .map_err(|_| io::Error::other("terminal_random"))?;
        let token: String = random.iter().map(|b| format!("{b:02x}")).collect();
        let directory = std::env::temp_dir().join(format!("ansa-shell-{token}"));
        let path = directory
            .to_str()
            .ok_or_else(|| io::Error::other("terminal_shell_path"))?;
        DirBuilder::new().mode(0o700).create(&directory)?;
        let prepared = (|| {
            let reports = ansa_linux::pty::notification_fifo(&directory.join("status"))?;
            let user_rc = if user_rc {
                r#"if [[ -r ~/.bashrc ]]; then source ~/.bashrc; fi
"#
            } else {
                "HISTFILE=/dev/null\n"
            };
            let rc = format!(
                "{user_rc}__ansa_directory={}\n{}",
                quote(path),
                r#"__ansa_begin() {
    [[ -r "$__ansa_directory/pending" ]] || return 1
    {
        IFS= builtin read -r __ansa_operation &&
        IFS= builtin read -r -d '' __ansa_source
    } < "$__ansa_directory/pending" || return 1
    [[ "$__ansa_operation" != "${__ansa_previous-}" ]] || return 1
    __ansa_previous=$__ansa_operation
    builtin printf '%s start\n' "$__ansa_operation" > "$__ansa_directory/status"
}
__ansa_finish() {
    builtin trap "$__ansa_dispatch" URG
    builtin printf '%s %d\n' "$__ansa_operation" "$1" > "$__ansa_directory/status"
}
readonly __ansa_dispatch='if __ansa_begin; then builtin eval -- "$__ansa_source" </dev/null > "$__ansa_directory/out-$__ansa_operation" 2>&1; __ansa_finish "$?"; fi'
builtin trap "$__ansa_dispatch" URG
PROMPT_COMMAND+=('builtin trap "$__ansa_dispatch" URG; builtin printf "ready\n" > "$__ansa_directory/status"')
"#,
            );
            let init = directory.join("bashrc");
            fs::write(&init, rc)?;
            command.args(["--noprofile", "--rcfile"]);
            command.arg(init).args(["--noediting", "-i"]);
            Ok(reports)
        })();
        let reports = match prepared {
            Ok(reports) => reports,
            Err(error) => {
                let _ = fs::remove_dir_all(&directory);
                return Err(error);
            }
        };
        Ok(Self {
            directory,
            reports,
            report: Vec::new(),
            ready: false,
            raw_unstable: false,
            raw_pending: 0,
            run: None,
            background: Vec::new(),
        })
    }

    pub(super) fn begin(&mut self, operation: Id, command: &str) -> io::Result<()> {
        // Retain a bounded number of output streams left open by descendants.
        if !self.ready || self.background.len() >= super::MAX_TERMINAL_IDS {
            return Err(io::Error::other("terminal_shell_busy"));
        }
        let path = self.directory.join(format!("out-{}", operation.as_str()));
        let pipe = ansa_linux::pty::output_fifo(&path)?;
        self.release();
        let prepared = (|| {
            let mut pending = OpenOptions::new()
                .create_new(true)
                .write(true)
                .mode(0o600)
                .open(self.directory.join("pending"))?;
            writeln!(pending, "{}", operation.as_str())?;
            pending.write_all(command.as_bytes())?;
            pending.write_all(&[0])
        })();
        if let Err(error) = prepared {
            let _ = fs::remove_file(&path);
            let _ = fs::remove_file(self.directory.join("pending"));
            return Err(error);
        }
        self.ready = false;
        self.run = Some(Run {
            operation,
            pipe: Some(pipe),
            output: Vec::new(),
            truncated: false,
            started: false,
            exit_code: None,
        });
        Ok(())
    }

    fn release(&mut self) {
        if let Some(run) = self.run.take() {
            let _ = fs::remove_file(
                self.directory
                    .join(format!("out-{}", run.operation.as_str())),
            );
        }
    }

    pub(super) fn not_delivered(&mut self) {
        self.release();
        let _ = fs::remove_file(self.directory.join("pending"));
        self.ready = true;
    }

    pub(super) fn raw_input(&mut self, bytes: &[u8], foreground_is_shell: bool) {
        if foreground_is_shell {
            // Consume one prompt per complete line accepted at the shell. A
            // delayed prompt from an earlier input must not release a later one.
            self.raw_pending = self.raw_pending.saturating_add(
                bytes
                    .iter()
                    .filter(|&&byte| matches!(byte, b'\r' | b'\n'))
                    .count(),
            );
            self.raw_unstable = !bytes
                .strip_suffix(b"\r")
                .is_some_and(|line| !line.iter().any(|byte| byte.is_ascii_control()));
        }
        self.ready = false;
    }

    pub(super) fn running(&self) -> bool {
        self.run.as_ref().is_some_and(|run| run.pipe.is_some())
    }

    pub(super) fn state(&self, operation: Option<&Id>) -> State {
        match (operation, &self.run) {
            (None, _) if self.ready => State::Ready,
            (None, _) => State::Busy,
            (Some(operation), Some(run)) if *operation == run.operation => {
                if run.pipe.is_none()
                    && let Some(exit_code) = run.exit_code
                {
                    State::Finished {
                        exit_code,
                        output: super::native::output_text(&run.output),
                        truncated: run.truncated,
                    }
                } else if run.started {
                    State::Running
                } else {
                    State::Starting
                }
            }
            _ => State::Unavailable,
        }
    }

    pub(super) fn poll(&mut self, mut display: impl FnMut(&[u8])) -> io::Result<()> {
        let mut reports = [0; REPORT_BYTES];
        match self.reports.read(&mut reports) {
            Ok(count) => {
                for &byte in &reports[..count] {
                    if byte == b'\n' {
                        self.accept_report();
                        self.report.clear();
                    } else if self.report.len() < REPORT_BYTES {
                        self.report.push(byte);
                    } else {
                        return Err(io::Error::other("terminal_shell_report"));
                    }
                }
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
            Err(error) => return Err(error),
        }
        if let Some(run) = self.run.as_mut()
            && let Some(pipe) = &mut run.pipe
        {
            let mut bytes = [0; 4096];
            let mut finish = None;
            for _ in 0..4 {
                match pipe.read(&mut bytes) {
                    Ok(0) => {
                        if run.exit_code.is_some() {
                            finish = Some(false);
                        }
                        break;
                    }
                    Ok(count) => {
                        display(&bytes[..count]);
                        let keep = count.min(OUTPUT_BYTES - run.output.len());
                        run.output.extend_from_slice(&bytes[..keep]);
                        run.truncated |= keep < count;
                    }
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                        if run.exit_code.is_some() {
                            finish = Some(true);
                        }
                        break;
                    }
                    Err(error) => return Err(error),
                }
            }
            // All foreground output precedes its exit report. A full read
            // budget already exceeds the capture limit. Finish even when
            // descendants keep refilling the FIFO without reaching EAGAIN.
            if let Some(background) = finish.or_else(|| run.exit_code.map(|_| true)) {
                if let Some(pipe) = run.pipe.take()
                    && background
                {
                    self.background.push(pipe);
                }
                let _ = fs::remove_file(
                    self.directory
                        .join(format!("out-{}", run.operation.as_str())),
                );
                self.ready = true;
            }
        }
        self.background.retain_mut(|pipe| {
            let mut bytes = [0; 4096];
            match pipe.read(&mut bytes) {
                Ok(0) => false,
                Ok(count) => {
                    display(&bytes[..count]);
                    true
                }
                Err(error) => matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                ),
            }
        });
        Ok(())
    }

    fn accept_report(&mut self) {
        if self.report == b"ready" && !self.running() {
            self.raw_pending = self.raw_pending.saturating_sub(1);
            self.ready = self.raw_pending == 0 && !self.raw_unstable;
        } else if let Some(run) = self.run.as_mut().filter(|run| run.pipe.is_some())
            && let Ok(report) = std::str::from_utf8(&self.report)
            && let Some((operation, exit_code)) = report.split_once(' ')
            && operation == run.operation.as_str()
        {
            if exit_code == "start" {
                run.started = true;
            } else if run.started
                && let Ok(exit_code @ 0..=255) = exit_code.parse::<i32>()
            {
                run.exit_code.get_or_insert(exit_code);
                let _ = fs::remove_file(self.directory.join("pending"));
            }
        }
    }
}

impl Drop for Shell {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.directory);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_earlier_prompt_does_not_release_the_last_raw_command() {
        let mut shell = Shell::prepare(&mut Command::new("bash"), false).unwrap();
        shell.report = b"ready".to_vec();
        shell.accept_report();
        assert!(matches!(shell.state(None), State::Ready));
        shell.raw_input(b"cd ke", true);
        shell.raw_input(b"pt\r", true);
        shell.raw_input(b"wait $!\r", true);
        shell.accept_report();
        assert!(matches!(shell.state(None), State::Busy));
        shell.accept_report();
        assert!(matches!(shell.state(None), State::Ready));
    }
}
