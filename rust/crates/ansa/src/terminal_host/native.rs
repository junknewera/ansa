//! Attached native PTY workers. Each fresh helper is the sole subreaper of
//! one shell tree. Losing the owning socket ends the worker's input lease.
use super::{
    Backend, Host, Journal,
    protocol::{
        Action, ApprovedScreen, Binding, Effect, Id, Intent, Launch, MAX_INPUT, Size, Writer,
        sha256,
    },
    wire,
};
use crate::frontend::terminal_pane::EscapeFilter;
use ansa_linux::{fs, local, managed, process as linux, pty::PtySession};
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
    Resize {
        size: Size,
    },
    Interrupt,
    Close,
    Capture,
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
    Status { exit: Option<Exit> },
    Read { output: Output },
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
        let text = ansa_linux::fs::utf8_text(&bytes)
            .chars()
            .map(|c| {
                if c.is_control() && !matches!(c, '\n' | '\r' | '\t') {
                    '\u{fffd}'
                } else {
                    c
                }
            })
            .collect();
        Some(Output {
            retained_from,
            from,
            through: from + bytes.len() as u64,
            available_through: self.through,
            gap: from > after,
            text,
            exit: None,
        })
    }
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
        let response = wire::write(&mut self.socket, self.sequence, request)
            .and_then(|_| wire::read(&mut self.socket, self.sequence));
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
            workspace_claim: None,
        }
    }
}

impl Backend for NativeBackend {
    fn execute(&mut self, intent: &Intent, input: &[u8]) -> Effect {
        if let Action::Create { launch } = &intent.action {
            if self.workspace_claim.is_none() {
                self.workspace_claim = match super::workspace::Claim::acquire(
                    &self.workspace_registry,
                    &intent.binding,
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
    command
        .args(&launch.argv[1..])
        .env("TERM", "xterm-256color");
    let mut pty = match PtySession::spawn_in(
        command,
        launch.size.rows,
        launch.size.columns,
        Some(&directory),
    ) {
        Ok(pty) => pty,
        Err(_) => {
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
    let mut terminal = WorkerTerminal::new(reader, launch.size);
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
            terminal.exit = Some(Exit {
                reason,
                cleanup_known: clean,
                status: pty.exit_status().and_then(|status| status.code()),
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
                    Ok(Some(request)) => terminal.reply(&mut socket, terminal.observe(request))?,
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
    parser: vt100::Parser,
    filter: EscapeFilter,
    output: Retained,
    sequence: u64,
    exit: Option<Exit>,
}

impl WorkerTerminal {
    fn new(reader: wire::Reader, size: Size) -> Self {
        Self {
            reader,
            parser: vt100::Parser::new(nonzero(size.rows), nonzero(size.columns), 0),
            filter: EscapeFilter::new(vec![]),
            output: Retained::default(),
            sequence: 1,
            exit: None,
        }
    }

    fn drain(&mut self, pty: &PtySession) -> io::Result<Drain> {
        let mut chunk = [0; 4096];
        for _ in 0..4 {
            let available = (MAX_OUTPUT - self.output.through).min(chunk.len() as u64) as usize;
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

    fn capture(&self) -> Screen {
        let mut screen = snapshot(self.parser.screen(), self.output.through);
        screen.exit = self.exit.clone();
        screen
    }

    fn observe(&self, request: Request) -> Response {
        match request {
            Request::Capture => Response::Capture {
                screen: self.capture(),
            },
            Request::Status => Response::Status {
                exit: self.exit.clone(),
            },
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
        wire::write(socket, self.sequence, response)?;
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
    let lifetime = Instant::now() + Duration::from_secs(u64::from(launch.lifetime_seconds));
    let idle_duration = Duration::from_secs(u64::from(launch.idle_seconds));
    let mut idle = Instant::now() + idle_duration;
    loop {
        let drain = terminal.drain(pty)?;
        if matches!(drain, Drain::Limit) {
            return Ok(End::Automatic(EndReason::OutputLimit));
        }
        if matches!(drain, Drain::Eof) || pty.exited()? {
            return Ok(End::Automatic(EndReason::ProgramExited));
        }
        if Instant::now() >= lifetime.min(idle) {
            return Ok(End::Automatic(EndReason::Deadline));
        }
        let Some(request) = terminal.reader.poll(socket, terminal.sequence)? else {
            thread::sleep(TICK);
            continue;
        };
        let value = match request {
            Request::Open { .. } => return Err(io::Error::other("terminal_already_open")),
            Request::Close => return Ok(End::Requested),
            Request::Capture | Request::Read { .. } | Request::Status => {
                terminal.reply(socket, terminal.observe(request))?;
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
                    || input_bytes + bytes.len() > super::MAX_TERMINAL_INPUT
                {
                    Effect::NotStarted
                } else {
                    input_bytes += bytes.len();
                    idle = Instant::now() + idle_duration;
                    deliver(pty, &bytes, lifetime.min(idle))
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

fn deliver(pty: &PtySession, input: &[u8], deadline: Instant) -> Effect {
    let until = (Instant::now() + WRITE_BUDGET).min(deadline);
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
