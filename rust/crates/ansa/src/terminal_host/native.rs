//! Attached native PTY workers. Each fresh helper is the sole subreaper of
//! one shell tree. Losing the owning socket ends the worker's input lease.
use super::{
    Backend, Host, Journal,
    protocol::{
        Action, ApprovedScreen, Binding, Effect, Id, Intent, Launch, MAX_INPUT, Size, Writer,
        sha256,
    },
    shell::{self, Shell},
    wire,
};
use crate::frontend::terminal_pane::EscapeFilter;
use ansa_linux::{
    fs, local, managed, process as linux,
    pty::{LaunchError, PtySession},
};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, VecDeque},
    io,
    net::Shutdown,
    num::NonZeroU16,
    os::{
        fd::{AsFd, OwnedFd},
        unix::{fs::MetadataExt, net::UnixStream},
    },
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

const ARGUMENT: &str = "--internal-terminal-worker-v1";
const TICK: Duration = Duration::from_millis(4);
const WRITE_BUDGET: Duration = Duration::from_millis(200);
const RPC_BUDGET: Duration = Duration::from_secs(3);
const SCREEN_BYTES: usize = 12 * 1024;
const MAX_OUTPUT: u64 = 32 * 1024 * 1024;
const MAX_UID_TASKS: usize = 2048;
const RETAINED_OUTPUT: usize = 64 * 1024;
// Retain screen rows separately from the agent's raw output byte window.
const SHELL_HISTORY_ROWS: usize = 1024;
// A human viewport can contain 128 × 256 VT cells of up to 22 bytes each.
// Reserve room for UTF-8 sanitization and JSON escaping on this private reply.
const VIEWPORT_FRAME_BYTES: usize = 4 * 1024 * 1024;
pub const MAX_READ: usize = 8192;

#[derive(Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum Request {
    Open {
        binding: Binding,
        launch: Launch,
        workspace: (u64, u64),
    },
    Input {
        bytes: Vec<u8>,
    },
    Exec {
        operation: Id,
        command: String,
    },
    ShellStatus {
        operation: Option<Id>,
    },
    Resize {
        size: Size,
    },
    Interrupt,
    Close,
    Capture,
    Viewport {
        top: Option<usize>,
        height: usize,
    },
    Status,
    Read {
        after: u64,
        limit: usize,
    },
    Approve {
        screen: ApprovedScreen,
    },
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum Response {
    Effect { value: Effect },
    Capture { screen: Screen },
    Viewport { page: Viewport },
    Status { exit: Option<Exit> },
    Read { output: Output },
    ShellStatus { state: shell::State },
}

#[derive(Serialize, Deserialize)]
pub(crate) struct Viewport {
    pub scroll: usize,
    pub history: usize,
    pub live_top: usize,
    pub height: usize,
    pub text: String,
    pub exit: Option<Exit>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Screen {
    pub size: Size,
    pub cursor: Option<(u16, u16)>,
    pub alternate: bool,
    pub through: u64,
    pub text: String,
    pub truncated: bool,
    pub exit: Option<Exit>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Output {
    pub retained_from: u64,
    pub from: u64,
    pub through: u64,
    pub available_through: u64,
    pub gap: bool,
    pub text: String,
    pub exit: Option<Exit>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EndReason {
    ProgramExited,
    Deadline,
    OutputLimit,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Exit {
    pub reason: EndReason,
    pub status: Option<i32>,
    pub cleanup_known: bool,
}

#[derive(Default)]
struct Retained {
    bytes: VecDeque<u8>,
    through: u64,
}

impl Retained {
    fn push(&mut self, bytes: &[u8]) {
        let evict = (self.bytes.len() + bytes.len()).saturating_sub(RETAINED_OUTPUT);
        self.bytes.drain(..evict);
        self.bytes.extend(bytes);
        self.through += bytes.len() as u64;
    }

    fn read(&self, after: u64, limit: usize) -> Option<Output> {
        if after > self.through || !(1..=MAX_READ).contains(&limit) {
            return None;
        }
        let retained_from = self.through - self.bytes.len() as u64;
        let from = after.max(retained_from);
        let bytes: Vec<u8> = self
            .bytes
            .iter()
            .skip((from - retained_from) as usize)
            .take(limit)
            .copied()
            .collect();
        Some(Output {
            retained_from,
            from,
            through: from + bytes.len() as u64,
            available_through: self.through,
            gap: from > after,
            text: output_text(&bytes),
            exit: None,
        })
    }
}

pub(super) fn output_text(bytes: &[u8]) -> String {
    fs::utf8_text(bytes)
        .chars()
        .map(|c| {
            if c.is_control() && !matches!(c, '\n' | '\r' | '\t') {
                '\u{fffd}'
            } else {
                c
            }
        })
        .collect()
}

impl Screen {
    pub fn approval(&self) -> Option<ApprovedScreen> {
        // The inactive screen is not part of the user's snapshot. Releasing
        // from alternate screen could expose it when the application exits.
        (!self.truncated && !self.alternate && self.exit.is_none()).then(|| ApprovedScreen {
            through: self.through,
            digest: sha256(&serde_json::to_vec(self).expect("screen contains only JSON values")),
        })
    }
}

struct Worker {
    socket: UnixStream,
    child: Child,
    sequence: u64,
    connected: bool,
    launched: bool,
}

impl Worker {
    fn start(executable: &Path) -> io::Result<Self> {
        let (socket, remote) = UnixStream::pair()?;
        socket.set_read_timeout(Some(RPC_BUDGET))?;
        socket.set_write_timeout(Some(WRITE_BUDGET))?;
        let remote: OwnedFd = remote.into();
        let mut command = Command::new(executable);
        command
            .arg(ARGUMENT)
            .stdin(Stdio::from(remote))
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let child = managed::spawn(command, None, None)?;
        Ok(Self {
            socket,
            child,
            sequence: 0,
            connected: true,
            launched: false,
        })
    }

    fn call(&mut self, request: Request) -> Option<Response> {
        if !self.connected {
            return None;
        }
        let viewport = matches!(request, Request::Viewport { .. });
        let response = wire::write(&mut self.socket, self.sequence, request).and_then(|_| {
            if viewport {
                wire::read_limited(&mut self.socket, self.sequence, VIEWPORT_FRAME_BYTES)
            } else {
                wire::read(&mut self.socket, self.sequence)
            }
        });
        self.sequence += 1;
        match response {
            Ok(response) => Some(response),
            Err(_) => {
                self.disconnect();
                None
            }
        }
    }

    fn effect(&mut self, request: Request) -> Effect {
        match self.call(request) {
            Some(Response::Effect { value }) => value,
            _ => Effect::Unknown,
        }
    }

    fn disconnect(&mut self) {
        self.connected = false;
        let _ = self.socket.shutdown(Shutdown::Both);
    }

    fn join(&mut self) -> bool {
        self.disconnect();
        let until = Instant::now() + RPC_BUDGET;
        loop {
            match self.child.try_wait() {
                Ok(Some(status)) => return status.success(),
                Ok(None) if Instant::now() < until => thread::sleep(TICK),
                _ => return false,
            }
        }
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        let _ = self.join();
    }
}

/// Backend intended for a single serialized coordinator. Client loss closes
/// this attached slice. Detached ownership requires a separate live host.
pub struct NativeBackend {
    executable: PathBuf,
    workers: BTreeMap<Id, Worker>,
    stop: Arc<Stop>,
    workspace_registry: PathBuf,
    evidence: Option<PathBuf>,
    workspace_claim: Option<super::workspace::Claim>,
}

#[derive(Default)]
pub(crate) struct Stop {
    stopped: AtomicBool,
    quarantined: AtomicBool,
    sockets: Mutex<BTreeMap<Id, UnixStream>>,
}

impl Stop {
    pub(crate) fn request(&self) {
        self.stopped.store(true, Ordering::Release);
        let sockets = self.sockets.lock().expect("socket table");
        for socket in sockets.values() {
            let _ = socket.shutdown(Shutdown::Both);
        }
    }

    pub(crate) fn quiescent(&self) -> bool {
        let sockets = self.sockets.lock().expect("socket table");
        sockets.is_empty() && !self.quarantined.load(Ordering::Acquire)
    }

    pub(crate) fn begin_turn(&self) {
        let sockets = self.sockets.lock().expect("socket table");
        if sockets.is_empty() && !self.quarantined.load(Ordering::Acquire) {
            self.stopped.store(false, Ordering::Release);
        }
    }

    fn register(&self, terminal: &Id, socket: &UnixStream) -> io::Result<()> {
        let mut sockets = self.sockets.lock().expect("socket table");
        if self.stopped.load(Ordering::Acquire) {
            return Err(io::Error::other("terminal_cancelled"));
        }
        sockets.insert(terminal.clone(), socket.try_clone()?);
        Ok(())
    }

    fn remove(&self, terminal: &Id) {
        self.sockets.lock().expect("socket table").remove(terminal);
    }
}

impl NativeBackend {
    pub fn new(executable: PathBuf, workspace_registry: PathBuf) -> Self {
        Self {
            executable,
            workers: BTreeMap::new(),
            stop: Arc::new(Stop::default()),
            workspace_registry,
            evidence: None,
            workspace_claim: None,
        }
    }

    pub(super) fn with_evidence(mut self, path: &Path) -> Self {
        self.evidence = Some(path.to_owned());
        self
    }

    pub(super) fn write_descendant(
        &self,
        root: &Path,
        relative: &Path,
    ) -> Result<Option<ansa_linux::local::WorkspaceWrite>, &'static str> {
        if self.stop.quarantined.load(Ordering::Acquire) {
            return Err("terminal_workspace_quarantined");
        }
        let Some(claim) = &self.workspace_claim else {
            return Ok(None);
        };
        if self.stop.stopped.load(Ordering::Acquire) {
            return Err("terminal_workspace_busy");
        }
        claim.write_descendant(root, relative).map(Some)
    }
}

impl Backend for NativeBackend {
    fn execute(&mut self, intent: &Intent, input: &[u8]) -> Effect {
        if let Action::Create { launch } = &intent.action {
            if self.workspace_claim.is_none() {
                self.workspace_claim = match super::workspace::Claim::acquire(
                    &self.workspace_registry,
                    &intent.binding,
                    self.evidence.as_deref(),
                ) {
                    Ok(claim) => Some(claim),
                    Err("terminal_workspace_busy" | "terminal_workspace_quarantined") => {
                        return Effect::NotStarted;
                    }
                    Err(_) => return Effect::Unknown,
                };
            }
            let Ok(mut worker) = Worker::start(&self.executable) else {
                return Effect::NotStarted;
            };
            if self
                .stop
                .register(&intent.terminal, &worker.socket)
                .is_err()
            {
                return Effect::NotStarted;
            }
            let result = worker.effect(Request::Open {
                binding: intent.binding.clone(),
                launch: launch.clone(),
                workspace: self
                    .workspace_claim
                    .as_ref()
                    .expect("owned workspace")
                    .identity,
            });
            worker.launched = result == Effect::Applied;
            if result != Effect::NotStarted {
                self.workers.insert(intent.terminal.clone(), worker);
            } else {
                self.stop.remove(&intent.terminal);
            }
            return result;
        }
        let Some(worker) = self.workers.get_mut(&intent.terminal) else {
            return Effect::Unknown;
        };
        let request = match &intent.action {
            Action::Create { .. } => unreachable!("handled create"),
            Action::Input { .. } => Request::Input {
                bytes: input.to_vec(),
            },
            Action::Exec { .. } => Request::Exec {
                operation: intent.operation.clone(),
                command: String::from_utf8(input.to_vec()).expect("admitted shell command"),
            },
            Action::Resize { size } => Request::Resize { size: *size },
            Action::Interrupt => Request::Interrupt,
            Action::Close => Request::Close,
            Action::Takeover | Action::Renew => {
                return if worker.connected {
                    Effect::Applied
                } else {
                    Effect::Unknown
                };
            }
            Action::Handback { screen } => Request::Approve {
                screen: screen.clone(),
            },
        };
        let result = worker.effect(request);
        if matches!(intent.action, Action::Close) {
            let clean = worker.join() && (worker.launched || result == Effect::Applied);
            if !clean {
                self.quarantine();
            }
            self.workers.remove(&intent.terminal);
            self.stop.remove(&intent.terminal);
            return if clean {
                Effect::Applied
            } else {
                Effect::Unknown
            };
        }
        result
    }

    fn stop(&mut self) {
        self.quarantine();
        self.stop.request();
        for worker in self.workers.values_mut() {
            worker.disconnect();
        }
        self.workers.clear();
    }

    fn quarantine(&mut self) {
        self.stop.quarantined.store(true, Ordering::Release);
    }

    fn receipt_committed(&mut self) {
        if self.workers.is_empty()
            && self.stop.quiescent()
            && let Some(claim) = self.workspace_claim.take()
            && claim.release().is_err()
        {
            self.quarantine();
        }
    }
}

impl<J: Journal> Host<J, NativeBackend> {
    pub(crate) fn stop_handle(&self) -> Arc<Stop> {
        self.backend.stop.clone()
    }

    pub(super) fn shell_status(
        &mut self,
        terminal: &Id,
        epoch: u64,
        operation: Option<Id>,
    ) -> Result<shell::State, &'static str> {
        let after = self
            .terminals
            .get(terminal)
            .ok_or("terminal_unknown")?
            .visible_after;
        self.read_permit(Writer::Agent, terminal, epoch, after)?;
        let response = self
            .backend
            .workers
            .get_mut(terminal)
            .and_then(|worker| worker.call(Request::ShellStatus { operation }));
        match response {
            Some(Response::ShellStatus { state }) => {
                if matches!(
                    state,
                    shell::State::Exited {
                        cleanup_known: false
                    }
                ) {
                    self.backend.stop.request();
                }
                Ok(state)
            }
            _ => {
                self.backend.stop.request();
                Err("terminal_worker_lost")
            }
        }
    }

    pub fn worker_status(&mut self, terminal: &Id) -> Result<Option<Exit>, &'static str> {
        let worker = self
            .backend
            .workers
            .get_mut(terminal)
            .ok_or("terminal_worker_lost")?;
        match worker.call(Request::Status) {
            Some(Response::Status { exit }) => Ok(exit),
            _ => Err("terminal_worker_lost"),
        }
    }
    pub fn capture(
        &mut self,
        writer: Writer,
        terminal: &Id,
        epoch: u64,
        after: u64,
    ) -> Result<Screen, &'static str> {
        let permit = self.read_permit(writer, terminal, epoch, after)?;
        let worker = self
            .backend
            .workers
            .get_mut(terminal)
            .ok_or("terminal_worker_lost")?;
        let Some(Response::Capture { screen }) = worker.call(Request::Capture) else {
            return Err("terminal_worker_lost");
        };
        if !self.recheck_read(&permit) || screen.through < after {
            return Err("terminal_read_lease");
        }
        if writer == Writer::Agent && screen.alternate && self.terminals[terminal].visible_after > 0
        {
            return Err("terminal_private_alternate_screen");
        }
        Ok(screen)
    }

    pub(crate) fn human_viewport(
        &mut self,
        terminal: &Id,
        epoch: u64,
        top: Option<usize>,
        height: usize,
    ) -> Result<Viewport, &'static str> {
        let permit = self.read_permit(Writer::Human, terminal, epoch, 0)?;
        let worker = self
            .backend
            .workers
            .get_mut(terminal)
            .ok_or("terminal_worker_lost")?;
        let Some(Response::Viewport { page }) = worker.call(Request::Viewport { top, height })
        else {
            return Err("terminal_worker_lost");
        };
        if !self.recheck_read(&permit) {
            return Err("terminal_read_lease");
        }
        Ok(page)
    }

    pub fn read_output(
        &mut self,
        writer: Writer,
        terminal: &Id,
        epoch: u64,
        after: u64,
        limit: usize,
    ) -> Result<Output, &'static str> {
        if !(1..=MAX_READ).contains(&limit) {
            return Err("terminal_read_limit");
        }
        let permit = self.read_permit(writer, terminal, epoch, after)?;
        let worker = self
            .backend
            .workers
            .get_mut(terminal)
            .ok_or("terminal_worker_lost")?;
        let output = match worker.call(Request::Read { after, limit }) {
            Some(Response::Read { output }) => output,
            Some(Response::Effect {
                value: Effect::NotStarted,
            }) => return Err("terminal_read_cursor"),
            _ => return Err("terminal_worker_lost"),
        };
        if !self.recheck_read(&permit) || output.from < after {
            return Err("terminal_read_lease");
        }
        Ok(output)
    }
}

/// Private helper entry point. The only accepted authority is the inherited
/// socket; no user session path, credential store or worktree is opened here.
pub fn private_entry(args: &[String]) -> Option<io::Result<()>> {
    (args.get(1).map(String::as_str) == Some(ARGUMENT)).then(|| {
        if args.len() != 2 {
            return Err(io::Error::other("terminal_worker_args"));
        }
        worker()
    })
}

fn worker() -> io::Result<()> {
    linux::reset_signals()?;
    linux::preflight(Path::new("/proc"))?;
    linux::limit_processes(MAX_UID_TASKS as u64)?;
    let mut socket = UnixStream::from(std::io::stdin().as_fd().try_clone_to_owned()?);
    socket.peer_addr()?;
    socket.set_nonblocking(true)?;
    socket.set_write_timeout(Some(WRITE_BUDGET))?;
    let mut reader = wire::Reader::default();
    let deadline = Instant::now() + RPC_BUDGET;
    let (binding, launch, workspace) = loop {
        if Instant::now() >= deadline {
            return Err(io::Error::other("terminal_open_timeout"));
        }
        match reader.poll(&mut socket, 0)? {
            Some(Request::Open {
                binding,
                launch,
                workspace,
            }) => break (binding, launch, workspace),
            Some(_) => return Err(io::Error::other("terminal_open_expected")),
            None => thread::sleep(TICK),
        }
    };
    let directory = if binding.validate()
        && launch.validate()
        && Path::new(&launch.cwd).starts_with(&binding.workspace)
        && !fs::protected_path(Path::new(&launch.cwd))
    {
        let root = Path::new(&binding.workspace);
        let relative = Path::new(&launch.cwd)
            .strip_prefix(root)
            .expect("checked workspace");
        fs::open_nofollow(root, local::read_flags())
            .ok()
            .and_then(|directory| {
                let meta = directory.metadata().ok()?;
                if !meta.is_dir() || (meta.dev(), meta.ino()) != workspace {
                    return None;
                }
                fs::open_relative_directory(&directory, relative).ok()
            })
    } else {
        None
    };
    let Some(directory) = directory else {
        return wire::write(
            &mut socket,
            0,
            Response::Effect {
                value: Effect::NotStarted,
            },
        );
    };
    let mut command = Command::new(&launch.argv[0]);
    command.env("TERM", "xterm-256color");
    let shell = if launch.session_lifetime() && shell::supported(&launch.argv) {
        match Shell::prepare(&mut command, launch.argv.len() <= 2) {
            Ok(shell) => Some(shell),
            Err(_) => {
                return wire::write(
                    &mut socket,
                    0,
                    Response::Effect {
                        value: Effect::NotStarted,
                    },
                );
            }
        }
    } else {
        command.args(&launch.argv[1..]);
        None
    };
    let mut pty = match PtySession::launch_in(
        command,
        launch.size.rows,
        launch.size.columns,
        Some(&directory),
    ) {
        Ok(pty) => pty,
        Err(LaunchError::NotStarted(_)) => {
            return wire::write(
                &mut socket,
                0,
                Response::Effect {
                    value: Effect::NotStarted,
                },
            );
        }
        Err(LaunchError::AfterSpawn(_)) => {
            let _ = wire::write(
                &mut socket,
                0,
                Response::Effect {
                    value: Effect::Unknown,
                },
            );
            let _ = reap_descendants();
            return Err(io::Error::other("terminal_launch_unknown"));
        }
    };
    drop(directory);
    let mut terminal = WorkerTerminal::new(reader, launch.size, launch.session_lifetime());
    terminal.shell = shell;
    let handling = (|| {
        wire::write(
            &mut socket,
            0,
            Response::Effect {
                value: Effect::Applied,
            },
        )?;
        worker_loop(&mut socket, &mut terminal, &mut pty, &launch)
    })();
    let naturally_exited = terminal.shell.is_some()
        && matches!(&handling, Ok(End::Automatic(EndReason::ProgramExited)))
        && observe_shell_exit(&pty);
    let outcome = pty.shutdown();
    let clean = outcome.shell_reaped && reap_descendants().is_ok();
    match handling {
        Ok(End::Requested) => terminal.reply(&mut socket, close_effect(clean))?,
        Ok(End::Automatic(reason)) => {
            // Pipe bytes already produced by an exited shell remain readable
            // from the master. Retain a final bounded tail before closing it.
            for _ in 0..4 {
                if !matches!(terminal.drain(&pty), Ok(Drain::Busy)) {
                    break;
                }
            }
            let status = pty.exit_status().and_then(|status| status.code());
            // A command such as `exit "$rc"` bypasses the Bash completion
            // hook. An observed normal exit can finish a started invocation
            // after cleanup. An EOF followed by shutdown or a signal cannot.
            terminal.poll_shell()?;
            if naturally_exited
                && clean
                && let Some(status) = status
            {
                if let Some(shell) = &mut terminal.shell {
                    shell.finish_from_exit(status);
                }
                terminal.poll_shell()?;
            }
            terminal.exit = Some(Exit {
                reason,
                cleanup_known: clean,
                status,
            });
            drop(pty);
            // The shell and all owned descendants are finished. Retain the
            // bounded final view while its host connection still owns it.
            loop {
                match terminal.reader.poll(&mut socket, terminal.sequence) {
                    Ok(Some(Request::Close)) => {
                        terminal.reply(&mut socket, close_effect(clean))?;
                        break;
                    }
                    Ok(Some(request)) => {
                        let response = terminal.observe(request);
                        terminal.reply(&mut socket, response)?;
                    }
                    Ok(None) => thread::sleep(TICK),
                    Err(_) => break,
                }
            }
        }
        Err(_) => {}
    }
    if clean {
        Ok(())
    } else {
        Err(io::Error::other("terminal_cleanup_unknown"))
    }
}

fn observe_shell_exit(pty: &PtySession) -> bool {
    // The slave can close just before the leader becomes waitable. Give
    // that exit a short window before shutdown signals could change it.
    let until = Instant::now() + 16 * TICK;
    loop {
        match pty.exited() {
            Ok(true) => return true,
            Ok(false) if Instant::now() < until => thread::sleep(TICK),
            _ => return false,
        }
    }
}

fn close_effect(clean: bool) -> Response {
    Response::Effect {
        value: if clean {
            Effect::Applied
        } else {
            Effect::Unknown
        },
    }
}

fn reap_descendants() -> io::Result<()> {
    let until = Instant::now() + Duration::from_millis(900);
    loop {
        let children = linux::children_with_limit(Path::new("/proc"), MAX_UID_TASKS)?;
        if children.is_empty() {
            return Ok(());
        }
        for pid in children {
            linux::kill_owned(pid)?;
            linux::reap_owned(pid)?;
        }
        if Instant::now() >= until {
            return Err(io::Error::other("terminal_cleanup_timeout"));
        }
        thread::sleep(TICK);
    }
}

enum End {
    Requested,
    Automatic(EndReason),
}

enum Drain {
    Idle,
    Busy,
    Eof,
    Limit,
}

struct WorkerTerminal {
    reader: wire::Reader,
    parser: vt100::Parser<History>,
    filter: EscapeFilter,
    output: Retained,
    sequence: u64,
    exit: Option<Exit>,
    continuous: bool,
    shell: Option<Shell>,
}

#[derive(Default)]
struct History {
    discarded: usize,
}

impl vt100::Callbacks for History {
    fn on_scroll(&mut self, _: vt100::capture::RowContents<'_>, alternate_screen: bool) {
        if !alternate_screen {
            self.discarded = self.discarded.saturating_add(1);
        }
    }
}

impl WorkerTerminal {
    fn new(reader: wire::Reader, size: Size, continuous: bool) -> Self {
        Self {
            reader,
            parser: vt100::Parser::new_with_callbacks(
                nonzero(size.rows),
                nonzero(size.columns),
                if continuous { SHELL_HISTORY_ROWS } else { 0 },
                History::default(),
            ),
            filter: EscapeFilter::new(vec![]),
            output: Retained::default(),
            sequence: 1,
            exit: None,
            continuous,
            shell: None,
        }
    }

    fn drain(&mut self, pty: &PtySession) -> io::Result<Drain> {
        let mut chunk = [0; 4096];
        for _ in 0..4 {
            let available = if self.continuous {
                chunk.len()
            } else {
                (MAX_OUTPUT - self.output.through).min(chunk.len() as u64) as usize
            };
            if available == 0 {
                return Ok(Drain::Limit);
            }
            match pty.read(&mut chunk[..available]) {
                Ok(0) => return Ok(Drain::Eof),
                Ok(count) => {
                    self.filter.process(&chunk[..count], &mut self.parser);
                    self.output.push(&chunk[..count]);
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(Drain::Idle),
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error),
            }
        }
        Ok(Drain::Busy)
    }

    fn poll_shell(&mut self) -> io::Result<()> {
        if let Some(shell) = &mut self.shell {
            shell.poll(|bytes| {
                let display: Vec<_> = bytes
                    .iter()
                    .flat_map(|byte| {
                        if *byte == b'\n' {
                            [Some(b'\r'), Some(*byte)]
                        } else {
                            [None, Some(*byte)]
                        }
                    })
                    .flatten()
                    .collect();
                self.filter.process(&display, &mut self.parser);
                self.output.push(&display);
            })?;
        }
        Ok(())
    }

    fn capture(&self) -> Screen {
        let mut screen = snapshot(self.parser.screen(), self.output.through);
        screen.exit = self.exit.clone();
        screen
    }

    fn viewport(&mut self, top: Option<usize>, height: usize) -> Viewport {
        let (screen, counters) = self.parser.screen_and_callbacks_mut();
        let (rows, columns) = screen.size();
        let height = height.clamp(1, usize::from(rows.get()));
        let columns = columns.get();
        let live_start = if screen.alternate_screen() {
            0
        } else {
            screen
                .rows(0, columns)
                .enumerate()
                .filter(|(_, line)| !line.is_empty())
                .last()
                .map_or(0, |(row, _)| (row + 1).saturating_sub(height))
        };
        let history = live_start + screen.scrollback_len();
        let live_top = counters.discarded.saturating_add(history);
        let scroll = top.map_or(0, |top| live_top.saturating_sub(top).min(history));
        screen.set_scrollback(scroll.saturating_sub(live_start));
        let text = screen
            .rows(0, columns)
            .skip(live_start.saturating_sub(scroll))
            .take(height)
            .map(|line| line.replace(char::is_control, "\u{fffd}"))
            .collect::<Vec<_>>()
            .join("\n");
        screen.set_scrollback(0);
        Viewport {
            scroll,
            history,
            live_top,
            height,
            text,
            exit: self.exit.clone(),
        }
    }

    fn observe(&mut self, request: Request) -> Response {
        match request {
            Request::Capture => Response::Capture {
                screen: self.capture(),
            },
            Request::Viewport { top, height } => Response::Viewport {
                page: self.viewport(top, height),
            },
            Request::Status => Response::Status {
                exit: self.exit.clone(),
            },
            Request::ShellStatus { operation } => {
                let state = self
                    .shell
                    .as_ref()
                    .map_or(shell::State::Unavailable, |shell| {
                        shell.state(operation.as_ref())
                    });
                let state = match (&self.exit, state) {
                    (Some(exit), state) if !matches!(state, shell::State::Finished { .. }) => {
                        shell::State::Exited {
                            cleanup_known: exit.cleanup_known,
                        }
                    }
                    (_, state) => state,
                };
                Response::ShellStatus { state }
            }
            Request::Read { after, limit } => self.output.read(after, limit).map_or(
                Response::Effect {
                    value: Effect::NotStarted,
                },
                |mut output| {
                    output.exit = self.exit.clone();
                    Response::Read { output }
                },
            ),
            _ => Response::Effect {
                value: Effect::NotStarted,
            },
        }
    }

    fn reply(&mut self, socket: &mut UnixStream, response: Response) -> io::Result<()> {
        if matches!(response, Response::Viewport { .. }) {
            socket.set_nonblocking(false)?;
            let sent = wire::write_limited(socket, self.sequence, response, VIEWPORT_FRAME_BYTES);
            socket.set_nonblocking(true)?;
            sent?;
        } else {
            wire::write(socket, self.sequence, response)?;
        }
        self.sequence += 1;
        Ok(())
    }
}

fn worker_loop(
    socket: &mut UnixStream,
    terminal: &mut WorkerTerminal,
    pty: &mut PtySession,
    launch: &Launch,
) -> io::Result<End> {
    let mut input_bytes = 0;
    let started = Instant::now();
    let lifetime = Duration::from_secs(u64::from(launch.lifetime_seconds));
    let idle_duration = Duration::from_secs(u64::from(launch.idle_seconds));
    let mut last_input = started;
    loop {
        terminal.poll_shell()?;
        let drain = terminal.drain(pty)?;
        if matches!(drain, Drain::Limit) {
            return Ok(End::Automatic(EndReason::OutputLimit));
        }
        if matches!(drain, Drain::Eof) || pty.exited()? {
            return Ok(End::Automatic(EndReason::ProgramExited));
        }
        if !launch.session_lifetime()
            && (started.elapsed() >= lifetime || last_input.elapsed() >= idle_duration)
        {
            return Ok(End::Automatic(EndReason::Deadline));
        }
        let Some(request) = terminal.reader.poll(socket, terminal.sequence)? else {
            thread::sleep(TICK);
            continue;
        };
        let value = match request {
            Request::Open { .. } => return Err(io::Error::other("terminal_already_open")),
            Request::Close => return Ok(End::Requested),
            Request::Capture
            | Request::Viewport { .. }
            | Request::Read { .. }
            | Request::Status
            | Request::ShellStatus { .. } => {
                let response = terminal.observe(request);
                terminal.reply(socket, response)?;
                continue;
            }
            Request::Approve { screen } => {
                if matches!(drain, Drain::Idle)
                    && terminal.capture().approval().as_ref() == Some(&screen)
                {
                    Effect::Applied
                } else {
                    Effect::NotStarted
                }
            }
            Request::Input { bytes } => {
                if bytes.is_empty()
                    || bytes.len() > MAX_INPUT
                    || terminal.shell.as_ref().is_some_and(Shell::running)
                    || !launch.session_lifetime()
                        && input_bytes + bytes.len() > super::MAX_TERMINAL_INPUT
                {
                    Effect::NotStarted
                } else {
                    input_bytes = input_bytes.saturating_add(bytes.len());
                    last_input = Instant::now();
                    let deadline = (!launch.session_lifetime())
                        .then(|| (started + lifetime).min(last_input + idle_duration));
                    let at_shell = terminal.shell.is_some() && pty.foreground_is_shell();
                    let effect = deliver(pty, &bytes, deadline);
                    if let Effect::Delivered { count } | Effect::Partial { count } = effect
                        && let Some(shell) = &mut terminal.shell
                    {
                        shell.raw_input(&bytes[..count], at_shell);
                    }
                    effect
                }
            }
            Request::Exec { operation, command } => {
                if Action::exec(command.as_bytes()).is_none() || !pty.foreground_is_shell() {
                    Effect::NotStarted
                } else if let Some(shell) = &mut terminal.shell
                    && shell.begin(operation, &command).is_ok()
                {
                    if pty.notify_foreground_shell().is_ok() {
                        let line = format!("$ {}\r\n", command.replace('\n', "\r\n"));
                        terminal
                            .filter
                            .process(line.as_bytes(), &mut terminal.parser);
                        terminal.output.push(line.as_bytes());
                        Effect::Delivered {
                            count: command.len(),
                        }
                    } else {
                        shell.not_delivered();
                        Effect::NotStarted
                    }
                } else {
                    Effect::NotStarted
                }
            }
            Request::Resize { size } => {
                if !size.validate() || pty.resize(size.rows, size.columns).is_err() {
                    Effect::NotStarted
                } else {
                    terminal
                        .parser
                        .set_size(nonzero(size.rows), nonzero(size.columns));
                    Effect::Applied
                }
            }
            Request::Interrupt => {
                if pty.interrupt_foreground().is_ok() {
                    Effect::Applied
                } else {
                    Effect::NotStarted
                }
            }
        };
        terminal.reply(socket, Response::Effect { value })?;
    }
}

fn deliver(pty: &PtySession, input: &[u8], deadline: Option<Instant>) -> Effect {
    let budget = Instant::now() + WRITE_BUDGET;
    let until = deadline.map_or(budget, |deadline| budget.min(deadline));
    let mut count = 0;
    while count < input.len() && Instant::now() < until {
        match pty.write(&input[count..]) {
            Ok(0) => break,
            Ok(written) => count += written,
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => thread::sleep(TICK),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        }
    }
    if count == input.len() {
        Effect::Delivered { count }
    } else if count == 0 {
        Effect::NotStarted
    } else {
        Effect::Partial { count }
    }
}

fn snapshot(screen: &vt100::Screen, through: u64) -> Screen {
    let (rows, columns) = screen.size();
    let mut text = String::new();
    let mut truncated = false;
    for character in screen.contents().chars() {
        let character = if character != '\n' && character.is_control() {
            '\u{fffd}'
        } else {
            character
        };
        if text.len() + character.len_utf8() > SCREEN_BYTES {
            truncated = true;
            break;
        }
        text.push(character);
    }
    Screen {
        size: Size {
            rows: rows.get(),
            columns: columns.get(),
        },
        cursor: (!screen.hide_cursor()).then(|| screen.cursor_position()),
        alternate: screen.alternate_screen(),
        through,
        text,
        truncated,
        exit: None,
    }
}

fn nonzero(value: u16) -> NonZeroU16 {
    NonZeroU16::new(value).expect("validated terminal size")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stopped_terminal_keeps_workspace_fenced_until_close_receipt() {
        let base = std::env::temp_dir().join(format!("ansa-stop-claim-{}", std::process::id()));
        fs::create_private_leaf(&base).unwrap();
        let base = base.canonicalize().unwrap();
        let workspace = base.join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        let id = Id::try_from("stop-fixture".to_owned()).unwrap();
        let binding = Binding::attached(id.clone(), workspace.to_str().unwrap().into());
        let registry = base.join("registry");
        let mut backend = NativeBackend::new(PathBuf::new(), registry.clone());
        backend.workspace_claim =
            Some(super::super::workspace::Claim::acquire(&registry, &binding, None).unwrap());
        let (worker, _peer) = UnixStream::pair().unwrap();
        backend.stop.register(&id, &worker).unwrap();
        assert!(
            backend
                .write_descendant(&workspace, Path::new(""))
                .unwrap()
                .is_some()
        );
        backend.stop.request();
        assert!(!backend.stop.quiescent());
        assert!(matches!(
            backend.write_descendant(&workspace, Path::new("")),
            Err("terminal_workspace_busy")
        ));
        // The normal Close path unregisters the worker and releases the
        // claim after persisting the cleanup result.
        backend.stop.remove(&id);
        backend.receipt_committed();
        assert!(
            backend
                .write_descendant(&workspace, Path::new(""))
                .unwrap()
                .is_none()
        );
        std::fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn viewport_keeps_its_top_row_across_output_and_resize() {
        let mut terminal = WorkerTerminal::new(
            wire::Reader::default(),
            Size {
                rows: 24,
                columns: 80,
            },
            true,
        );
        for line in 0..SHELL_HISTORY_ROWS {
            terminal.parser.process(format!("{line:04}\r\n").as_bytes());
        }
        let live = terminal.viewport(None, 10);
        let top = live.live_top - 10;
        let before = terminal.viewport(Some(top), 10);
        for line in SHELL_HISTORY_ROWS..SHELL_HISTORY_ROWS + 90 {
            terminal.parser.process(format!("{line:04}\r\n").as_bytes());
        }
        let after = terminal.viewport(Some(top), 80);
        assert_eq!(after.height, 24);
        assert_eq!(after.live_top - after.scroll, top);
        assert_eq!(after.text.lines().next(), before.text.lines().next());
        assert_eq!(terminal.capture().text.lines().last(), Some("1113"));
        let oldest = terminal.viewport(Some(0), 10);
        assert!(oldest.live_top > oldest.history);
        assert!(oldest.text.starts_with("0067\n"));
    }

    #[test]
    fn viewport_includes_the_live_tail_of_a_dense_screen() {
        let mut terminal = WorkerTerminal::new(
            wire::Reader::default(),
            Size {
                rows: 128,
                columns: 256,
            },
            true,
        );
        let fill = "∑".repeat(248);
        for line in 0..128 {
            terminal
                .parser
                .process(format!("{line:03} {fill}\r\n").as_bytes());
        }
        let page = terminal.viewport(None, 128);
        assert!(page.text.len() > 64 * 1024);
        assert!(
            page.text
                .trim_end()
                .lines()
                .last()
                .unwrap()
                .starts_with("127 ")
        );
        let mut bytes = Vec::new();
        wire::write_limited(
            &mut bytes,
            1,
            Response::Viewport { page },
            VIEWPORT_FRAME_BYTES,
        )
        .unwrap();
        assert!(wire::read::<Response>(&mut bytes.as_slice(), 1).is_err());
        assert!(matches!(
            wire::read_limited(&mut bytes.as_slice(), 1, VIEWPORT_FRAME_BYTES).unwrap(),
            Response::Viewport { page } if page.height == 128
        ));
    }
}
