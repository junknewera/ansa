//! An ephemeral manual terminal, deliberately disconnected from tools,
//! provider context and the canonical conversation journal.
use ansa_linux::pty::{CleanupOutcome, PtySession};
use ring::rand::{SecureRandom, SystemRandom};
use std::{
    collections::VecDeque,
    ffi::OsStr,
    fs::{DirBuilder, OpenOptions},
    io::{self, Write},
    num::NonZeroU16,
    os::unix::{
        ffi::OsStrExt,
        fs::{DirBuilderExt, OpenOptionsExt},
    },
    path::{Path, PathBuf},
    process::Command,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc::{self, SyncSender},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};
pub use vt100::{MouseProtocolEncoding, MouseProtocolMode};

const INPUT_LIMIT: usize = 64 * 1024;
const INPUT_CHUNK: usize = 4096;
const MAX_ROWS: u16 = 128;
const MAX_COLS: u16 = 256;

#[derive(Clone, Debug)]
pub struct TerminalSnapshot {
    /// Exactly the pane rows. Escape sequences here are generated SGR only;
    /// child OSC, cursor moves, clipboard and title commands never escape.
    pub rows: Vec<String>,
    pub cursor: Option<(u16, u16)>,
    pub cwd: PathBuf,
    pub shell_ready: bool,
    pub supports_directory_change: bool,
    pub exited: bool,
    pub error: Option<String>,
    pub revision: u64,
    pub application_cursor: bool,
    pub application_keypad: bool,
    pub bracketed_paste: bool,
    pub alternate_screen: bool,
    pub mouse_protocol: MouseProtocolMode,
    pub mouse_encoding: MouseProtocolEncoding,
}
impl TerminalSnapshot {
    fn empty(cwd: PathBuf) -> Self {
        Self {
            rows: Vec::new(),
            cursor: None,
            cwd,
            shell_ready: false,
            supports_directory_change: false,
            exited: false,
            error: None,
            revision: 0,
            application_cursor: false,
            application_keypad: false,
            bracketed_paste: false,
            alternate_screen: false,
            mouse_protocol: MouseProtocolMode::None,
            mouse_encoding: MouseProtocolEncoding::Default,
        }
    }
}
#[derive(Default)]
struct Control {
    size: Option<(u16, u16)>,
    directory: Option<PathBuf>,
    cancel_directory: bool,
}

pub struct TerminalPane {
    input: SyncSender<Vec<u8>>,
    queued: Arc<AtomicUsize>,
    interrupt: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    control: Arc<Mutex<Control>>,
    latest: Arc<Mutex<Arc<TerminalSnapshot>>>,
    worker: Option<JoinHandle<CleanupOutcome>>,
}
impl TerminalPane {
    pub fn spawn(cwd: &Path, rows: u16, cols: u16) -> io::Result<Self> {
        let selected = std::env::var_os("SHELL")
            .filter(|s| Path::new(s).is_absolute())
            .unwrap_or_else(|| "/bin/sh".into());
        Self::spawn_shell(Path::new(&selected), cwd, rows, cols)
    }

    pub fn spawn_shell(shell: &Path, cwd: &Path, rows: u16, cols: u16) -> io::Result<Self> {
        Self::spawn_configured(shell, cwd, rows, cols, None)
    }
    fn spawn_configured(
        shell: &Path,
        cwd: &Path,
        rows: u16,
        cols: u16,
        home: Option<PathBuf>,
    ) -> io::Result<Self> {
        let size = checked_size(rows, cols)?;
        let shell = shell.to_owned();
        let cwd = cwd.to_owned();
        let (input, receiver) = mpsc::sync_channel::<Vec<u8>>(32);
        let queued = Arc::new(AtomicUsize::new(0));
        let interrupt = Arc::new(AtomicBool::new(false));
        let stop = Arc::new(AtomicBool::new(false));
        let control = Arc::new(Mutex::new(Control::default()));
        let latest = Arc::new(Mutex::new(Arc::new(TerminalSnapshot::empty(cwd.clone()))));
        let worker = {
            let queued = queued.clone();
            let interrupt = interrupt.clone();
            let stop = stop.clone();
            let control = control.clone();
            let latest = latest.clone();
            thread::Builder::new()
                .name("ansa-terminal".into())
                .spawn(move || {
                    let mut init = match ShellInit::new(&shell, home.as_deref()) {
                        Ok(init) => init,
                        Err(error) => {
                            startup_failure(&latest, error);
                            return CleanupOutcome::default();
                        }
                    };
                    let mut command = init.command(&shell);
                    if let Some(home) = home {
                        command
                            .env("HOME", &home)
                            .env("XDG_CONFIG_HOME", home.join(".config"));
                    }
                    command.current_dir(&cwd).env("TERM", "xterm-256color");
                    let mut pty = match PtySession::spawn(command, size.0, size.1) {
                        Ok(pty) => pty,
                        Err(error) => {
                            startup_failure(&latest, error);
                            return CleanupOutcome::default();
                        }
                    };
                    let mut parser = vt100::Parser::new_with_callbacks(
                        nz(size.0),
                        nz(size.1),
                        0,
                        Replies::default(),
                    );
                    let mut filter = EscapeFilter::new(init.marker.clone());
                    let mut state = TerminalSnapshot::empty(cwd);
                    state.supports_directory_change = init.zsh;
                    let mut pending = VecDeque::new();
                    let mut pending_accounted = 0;
                    let mut prompt = false;
                    let mut eof = false;
                    let mut last_publish = Instant::now() - Duration::from_secs(1);
                    let mut unpublished = false;
                    while !stop.load(Ordering::Acquire) {
                        let mut dirty = false;
                        if let Ok(mut control) = control.lock() {
                            if control.cancel_directory {
                                init.cancel_directory();
                                control.cancel_directory = false;
                            }
                            if let Some((rows, cols)) = control.size.take() {
                                match pty.resize(rows, cols) {
                                    Ok(()) => parser.set_size(nz(rows), nz(cols)),
                                    Err(error) => state.error = Some(error.to_string()),
                                }
                                dirty = true;
                            }
                            if let Some(directory) = control.directory.take() {
                                if prompt
                                    && pending.is_empty()
                                    && queued.load(Ordering::Acquire) == 0
                                    && pty.foreground_is_shell()
                                    && !parser.screen().alternate_screen()
                                {
                                    match init.request_directory(&directory) {
                                        Ok(()) => {
                                            prompt = false;
                                        }
                                        Err(error) => state.error = Some(error.to_string()),
                                    }
                                } else {
                                    state.error = Some("terminal_shell_busy".into());
                                }
                                dirty = true;
                            }
                        }
                        if interrupt.swap(false, Ordering::AcqRel) {
                            init.cancel_directory();
                            prompt = false;
                            filter.prompt = None;
                            queued.fetch_sub(pending_accounted, Ordering::AcqRel);
                            pending_accounted = 0;
                            pending.clear();
                            while let Ok(bytes) = receiver.try_recv() {
                                queued.fetch_sub(bytes.len(), Ordering::AcqRel);
                            }
                            pending.push_back(3);
                            dirty = true;
                        }
                        // One bounded chunk at a time. Requests remain FIFO; bytes
                        // cannot grow beyond INPUT_LIMIT even under blocked slave.
                        if pending.is_empty()
                            && let Ok(bytes) = receiver.try_recv()
                        {
                            prompt = false;
                            filter.prompt = None;
                            init.cancel_directory();
                            pending_accounted = bytes.len();
                            pending.extend(bytes);
                            dirty = true;
                        }
                        if !pending.is_empty() {
                            let bytes = pending.make_contiguous();
                            match pty.write(bytes) {
                                Ok(n) => {
                                    pending.drain(..n);
                                    let accounted = n.min(pending_accounted);
                                    queued.fetch_sub(accounted, Ordering::AcqRel);
                                    pending_accounted -= accounted;
                                }
                                Err(error)
                                    if error.kind() == io::ErrorKind::WouldBlock
                                        || error.kind() == io::ErrorKind::Interrupted => {}
                                Err(error) => {
                                    state.error = Some(error.to_string());
                                    eof = true;
                                }
                            }
                        }
                        // Bound work per turn so control and shutdown remain
                        // responsive during a continuous child output flood.
                        for _ in 0..4 {
                            let mut buffer = [0; 4096];
                            match pty.read(&mut buffer) {
                                Ok(0) => {
                                    eof = true;
                                    break;
                                }
                                Ok(n) => {
                                    filter.process(&buffer[..n], &mut parser);
                                    if let Some(ready) = filter.prompt.take() {
                                        prompt = ready;
                                    }
                                    dirty = true;
                                }
                                Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                                Err(error) if error.kind() == io::ErrorKind::Interrupted => {
                                    continue;
                                }
                                Err(error) => {
                                    state.error = Some(error.to_string());
                                    eof = true;
                                    break;
                                }
                            }
                        }
                        // Terminal query replies go only to this PTY, never the
                        // outer tty. Drop excess queries instead of growing a queue.
                        if pending.is_empty() {
                            pending.extend(parser.callbacks_mut().bytes.drain(..));
                        } else {
                            parser.callbacks_mut().bytes.clear();
                        }
                        let ready = prompt
                            && pending.is_empty()
                            && queued.load(Ordering::Acquire) == 0
                            && pty.foreground_is_shell()
                            && !parser.screen().alternate_screen();
                        dirty |= state.shell_ready != ready;
                        state.shell_ready = ready;
                        if let Ok(cwd) = pty.cwd() {
                            dirty |= state.cwd != cwd;
                            state.cwd = cwd;
                        }
                        let exited = pty.exited().unwrap_or_else(|error| {
                            state.error = Some(error.to_string());
                            true
                        });
                        unpublished |= dirty;
                        if unpublished && last_publish.elapsed() >= Duration::from_millis(16)
                            || exited
                            || eof
                        {
                            state.revision = state.revision.saturating_add(1);
                            update_screen(&mut state, parser.screen());
                            state.exited = exited || eof;
                            if state.exited {
                                state.shell_ready = false;
                                state.cursor = None;
                            }
                            if let Ok(mut latest) = latest.lock() {
                                *latest = Arc::new(state.clone());
                            }
                            last_publish = Instant::now();
                            unpublished = false;
                        }
                        if exited || eof {
                            break;
                        }
                        thread::sleep(Duration::from_millis(5));
                    }
                    let outcome = pty.shutdown();
                    if let Ok(mut latest) = latest.lock() {
                        let mut final_state = (**latest).clone();
                        final_state.exited = true;
                        final_state.shell_ready = false;
                        final_state.cursor = None;
                        final_state.revision = final_state.revision.saturating_add(1);
                        if !outcome.shell_reaped || !outcome.session_empty {
                            final_state.error = Some("terminal_cleanup_unknown".into());
                        }
                        *latest = Arc::new(final_state);
                    }
                    drop(init);
                    outcome
                })?
        };
        Ok(Self {
            input,
            queued,
            interrupt,
            stop,
            control,
            latest,
            worker: Some(worker),
        })
    }

    pub fn snapshot(&self) -> Arc<TerminalSnapshot> {
        self.latest
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }
    pub fn input(&self, bytes: &[u8]) -> io::Result<()> {
        if bytes.is_empty() {
            return Ok(());
        }
        if self.snapshot().exited {
            return Err(io::Error::new(io::ErrorKind::BrokenPipe, "terminal_exited"));
        }
        if bytes == [3] {
            self.interrupt.store(true, Ordering::Release);
            return Ok(());
        }
        if bytes.len() > INPUT_CHUNK {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "terminal_input_chunk",
            ));
        }
        self.queued
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| {
                (n + bytes.len() <= INPUT_LIMIT).then_some(n + bytes.len())
            })
            .map_err(|_| io::Error::new(io::ErrorKind::WouldBlock, "terminal_input_full"))?;
        if self.input.try_send(bytes.to_vec()).is_err() {
            self.queued.fetch_sub(bytes.len(), Ordering::AcqRel);
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "terminal_input_full",
            ));
        }
        Ok(())
    }
    pub fn resize(&self, rows: u16, cols: u16) -> io::Result<()> {
        let size = checked_size(rows, cols)?;
        self.control
            .try_lock()
            .map_err(|_| io::Error::new(io::ErrorKind::WouldBlock, "terminal_control_busy"))?
            .size = Some(size);
        Ok(())
    }
    pub fn change_directory(&self, path: &Path) -> io::Result<()> {
        if path.as_os_str().as_bytes().len() > 4096
            || path.as_os_str().as_bytes().contains(&0)
            || path.to_str().is_none()
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "terminal_directory",
            ));
        }
        if !self.snapshot().shell_ready || self.queued.load(Ordering::Acquire) != 0 {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "terminal_shell_busy",
            ));
        }
        if !self.snapshot().supports_directory_change {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "terminal_manual_cd_required",
            ));
        }
        self.control
            .try_lock()
            .map_err(|_| io::Error::new(io::ErrorKind::WouldBlock, "terminal_control_busy"))?
            .directory = Some(path.to_owned());
        Ok(())
    }
    pub fn shutdown(&mut self) -> CleanupOutcome {
        self.stop.store(true, Ordering::Release);
        self.worker
            .take()
            .map(|worker| {
                worker.join().unwrap_or_else(|_| CleanupOutcome {
                    error: Some("terminal_worker_panicked".into()),
                    ..CleanupOutcome::default()
                })
            })
            .unwrap_or_default()
    }
    pub fn cancel_directory_change(&self) {
        if let Ok(mut control) = self.control.try_lock() {
            control.directory = None;
            control.cancel_directory = true;
        }
    }
}
fn startup_failure(latest: &Mutex<Arc<TerminalSnapshot>>, error: io::Error) {
    if let Ok(mut latest) = latest.lock() {
        let mut state = (**latest).clone();
        state.error = Some(error.to_string());
        state.exited = true;
        state.revision += 1;
        *latest = Arc::new(state);
    }
}
impl Drop for TerminalPane {
    fn drop(&mut self) {
        let _ = self.shutdown();
    }
}

fn checked_size(rows: u16, cols: u16) -> io::Result<(u16, u16)> {
    if rows == 0 || cols == 0 {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "terminal_size"));
    }
    Ok((rows.min(MAX_ROWS), cols.min(MAX_COLS)))
}
fn nz(n: u16) -> NonZeroU16 {
    NonZeroU16::new(n).expect("validated terminal size")
}

#[derive(Default)]
struct Replies {
    bytes: Vec<u8>,
}
impl vt100::Callbacks for Replies {
    fn unhandled_csi(
        &mut self,
        screen: &mut vt100::Screen,
        i1: Option<u8>,
        _: Option<u8>,
        params: &[&[u16]],
        c: char,
    ) {
        let param = params.first().and_then(|p| p.first()).copied().unwrap_or(0);
        let reply = match (i1, c, param) {
            (None, 'n', 5) => "\x1b[0n".into(),
            (None, 'n', 6) => {
                let (r, c) = screen.cursor_position();
                format!("\x1b[{};{}R", r + 1, c + 1)
            }
            (None, 'c', 0) => "\x1b[?1;2c".into(),
            (Some(b'>'), 'c', _) => "\x1b[>0;0;0c".into(),
            _ => String::new(),
        };
        if self.bytes.len() + reply.len() <= 4096 {
            self.bytes.extend(reply.bytes());
        }
    }
}

fn update_screen(state: &mut TerminalSnapshot, screen: &vt100::Screen) {
    let (rows, cols) = screen.size();
    state.rows = (0..rows.get())
        .map(|row| {
            let mut text = String::new();
            let mut previous = String::new();
            for col in 0..cols.get() {
                let Some(cell) = screen.cell(row, col) else {
                    continue;
                };
                if cell.is_wide_continuation() {
                    continue;
                }
                let attrs = attributes(cell);
                if attrs != previous {
                    text.push_str(&attrs);
                    previous = attrs;
                }
                if cell.has_contents() {
                    for c in cell.contents().chars() {
                        text.push(if c.is_control() { '\u{fffd}' } else { c });
                    }
                } else {
                    text.push(' ');
                }
            }
            text.push_str("\x1b[0m");
            text
        })
        .collect();
    let (row, col) = screen.cursor_position();
    state.cursor =
        (!screen.hide_cursor()).then_some((row.min(rows.get() - 1), col.min(cols.get() - 1)));
    state.application_cursor = screen.application_cursor();
    state.application_keypad = screen.application_keypad();
    state.bracketed_paste = screen.bracketed_paste();
    state.alternate_screen = screen.alternate_screen();
    state.mouse_protocol = screen.mouse_protocol_mode();
    state.mouse_encoding = screen.mouse_protocol_encoding();
}
fn attributes(cell: &vt100::Cell) -> String {
    let mut codes = vec!["0".to_owned()];
    for (enabled, code) in [
        (cell.bold(), "1"),
        (cell.dim(), "2"),
        (cell.italic(), "3"),
        (cell.underline(), "4"),
        (cell.inverse(), "7"),
    ] {
        if enabled {
            codes.push(code.into());
        }
    }
    for (color, base) in [(cell.fgcolor(), 38), (cell.bgcolor(), 48)] {
        match color {
            vt100::Color::Default => {}
            vt100::Color::Idx(n) => codes.push(format!("{base};5;{n}")),
            vt100::Color::Rgb(r, g, b) => codes.push(format!("{base};2;{r};{g};{b}")),
        }
    }
    format!("\x1b[{}m", codes.join(";"))
}

/// All terminal strings are discarded before reaching the VT parser. Only our
/// exact prompt marker is consumed; even an unterminated OSC/DCS stays bounded.
pub(crate) struct EscapeFilter {
    state: FilterState,
    osc: Vec<u8>,
    escape: Vec<u8>,
    csi: Vec<u8>,
    marker: Vec<u8>,
    prompt: Option<bool>,
}
#[derive(Clone, Copy)]
enum FilterState {
    Ground,
    Escape { intermediate: bool, ignored: bool },
    Csi { ignored: bool },
    String { osc: bool, escaped: bool },
}
impl EscapeFilter {
    pub(crate) fn new(marker: Vec<u8>) -> Self {
        Self {
            state: FilterState::Ground,
            osc: Vec::new(),
            escape: Vec::new(),
            csi: Vec::new(),
            marker,
            prompt: None,
        }
    }
    pub(crate) fn process(
        &mut self,
        bytes: &[u8],
        parser: &mut vt100::Parser<impl vt100::Callbacks>,
    ) {
        for &byte in bytes {
            match self.state {
                FilterState::Ground if byte == 0x1b => self.start_escape(),
                FilterState::Ground => parser.process(&[byte]),
                FilterState::Escape { .. } | FilterState::Csi { .. } if byte == 0x1b => {
                    self.start_escape()
                }
                FilterState::Escape { .. } | FilterState::Csi { .. }
                    if matches!(byte, 0x18 | 0x1a) =>
                {
                    self.state = FilterState::Ground;
                }
                FilterState::Escape { .. } | FilterState::Csi { .. } if byte < 0x20 => {
                    // C0 executes without completing or cancelling the pending
                    // escape. Never feed its incomplete prefix to the parser.
                    parser.process(&[byte]);
                }
                FilterState::Escape { .. } | FilterState::Csi { .. }
                    if byte == 0x7f || byte >= 0x80 => {}
                FilterState::Escape {
                    intermediate: false,
                    ..
                } if matches!(byte, b']' | b'P' | b'X' | b'^' | b'_') => {
                    self.osc.clear();
                    self.state = FilterState::String {
                        osc: byte == b']',
                        escaped: false,
                    };
                }
                FilterState::Escape {
                    intermediate: false,
                    ..
                } if byte == b'[' => {
                    self.csi.clear();
                    self.csi.extend_from_slice(b"\x1b[");
                    self.state = FilterState::Csi { ignored: false };
                }
                FilterState::Escape {
                    intermediate,
                    ignored,
                } => {
                    let ignored = ignored || self.escape.len() == 128;
                    if !ignored {
                        self.escape.push(byte);
                    }
                    if (0x20..=0x2f).contains(&byte) {
                        self.state = FilterState::Escape {
                            intermediate: true,
                            ignored,
                        };
                    } else if (0x30..=0x7e).contains(&byte) {
                        // A complete ESC intermediate/final sequence always
                        // returns vte to Ground. No parser Escape state leaks
                        // into later bytes which could bypass the CSI guard.
                        if !ignored {
                            parser.process(&self.escape);
                        }
                        self.state = FilterState::Ground;
                    } else {
                        self.state = FilterState::Escape {
                            intermediate,
                            ignored,
                        };
                    }
                }
                FilterState::Csi { ignored } => {
                    let ignored = ignored || self.csi.len() == 128;
                    if !ignored {
                        self.csi.push(byte);
                    }
                    if (0x40..=0x7e).contains(&byte) {
                        // vt100's REP loops over its raw u16 count rather than
                        // clamping to screen dimensions. Bound this CPU work,
                        // as well as malformed arbitrarily long CSI strings.
                        let count = self.csi[2..self.csi.len().saturating_sub(1)]
                            .split(|b| matches!(b, b';' | b':'))
                            .next()
                            .unwrap_or_default();
                        let count = count
                            .iter()
                            .try_fold(0_u32, |n, b| {
                                if !b.is_ascii_digit() {
                                    return None;
                                }
                                n.checked_mul(10)?.checked_add(u32::from(b - b'0'))
                            })
                            .unwrap_or(u32::MAX);
                        if !ignored && (byte != b'b' || count <= 1024) {
                            parser.process(&self.csi);
                        }
                        self.state = FilterState::Ground;
                    } else {
                        self.state = FilterState::Csi { ignored };
                    }
                }
                FilterState::String { osc, escaped } => {
                    if matches!(byte, 0x18 | 0x1a) {
                        self.osc.clear();
                        self.state = FilterState::Ground;
                    } else if (osc && byte == 7) || (escaped && byte == b'\\') {
                        if osc && !self.marker.is_empty() {
                            if self.osc == self.marker {
                                self.prompt = Some(true);
                            } else if self.osc.len() == self.marker.len()
                                && self
                                    .osc
                                    .iter()
                                    .zip(&self.marker)
                                    .enumerate()
                                    .all(|(i, (a, b))| if i == 4 { *a == b'B' } else { a == b })
                            {
                                self.prompt = Some(false);
                            }
                        }
                        self.osc.clear();
                        self.state = FilterState::Ground;
                    } else {
                        if osc && byte != 0x1b && self.osc.len() <= 128 {
                            self.osc.push(byte);
                        }
                        self.state = FilterState::String {
                            osc,
                            escaped: byte == 0x1b,
                        };
                    }
                }
            }
        }
    }
    fn start_escape(&mut self) {
        self.escape.clear();
        self.escape.push(0x1b);
        self.state = FilterState::Escape {
            intermediate: false,
            ignored: false,
        };
    }
}

struct ShellInit {
    directory: Option<PathBuf>,
    marker: Vec<u8>,
    zsh: bool,
    user_zdotdir: Option<std::ffi::OsString>,
    notify: Option<std::fs::File>,
    request: Option<PathBuf>,
    next_request: u64,
}
impl ShellInit {
    fn new(shell: &Path, home: Option<&Path>) -> io::Result<Self> {
        let name = shell.file_name().and_then(OsStr::to_str).unwrap_or("");
        if !matches!(name, "bash" | "zsh") {
            return Ok(Self {
                directory: None,
                marker: Vec::new(),
                zsh: false,
                user_zdotdir: None,
                notify: None,
                request: None,
                next_request: 0,
            });
        }
        let mut random = [0; 16];
        SystemRandom::new()
            .fill(&mut random)
            .map_err(|_| io::Error::other("terminal_random"))?;
        let token = random
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>();
        let directory =
            std::env::temp_dir().join(format!("ansa-terminal-{}-{token}", std::process::id()));
        DirBuilder::new().mode(0o700).create(&directory)?;
        let marker = format!("133;A;ansa={token}");
        let mut value = Self {
            directory: Some(directory.clone()),
            marker: marker.as_bytes().to_vec(),
            zsh: name == "zsh",
            user_zdotdir: None,
            notify: None,
            request: None,
            next_request: 0,
        };
        let hook = format!("__ansa_prompt_marker() {{ builtin printf '\\033]{marker}\\007'; }}\n");
        if value.zsh {
            value.notify = Some(ansa_linux::pty::notification_fifo(
                &directory.join("notify"),
            )?);
            value.user_zdotdir =
                Some(home.map(|p| p.as_os_str().to_owned()).unwrap_or_else(|| {
                    std::env::var_os("ZDOTDIR")
                        .unwrap_or_else(|| std::env::var_os("HOME").unwrap_or_default())
                }));
            value.write(".zshenv", "ZDOTDIR=$ANSA_USER_ZDOTDIR\n[[ -r $ZDOTDIR/.zshenv ]] && source \"$ZDOTDIR/.zshenv\"\n__ansa_user_zdotdir=${ZDOTDIR:-$HOME}\nZDOTDIR=$ANSA_INIT_DIR\n")?;
            value.write(".zshrc", &format!("ZDOTDIR=$__ansa_user_zdotdir\n[[ -r $ZDOTDIR/.zshrc ]] && source \"$ZDOTDIR/.zshrc\"\n__ansa_init_dir=$ANSA_INIT_DIR\n{hook}\
__ansa_preexec_marker() {{ builtin printf '\\033]{}\\007'; }}\n\
__ansa_cwd_notification() {{\n\
  local __ansa_id __ansa_target\n\
  IFS= read -r -u $__ansa_notify_fd __ansa_id || return\n\
  [[ $__ansa_id == <-> ]] || return\n\
  [[ -f $__ansa_init_dir/request-$__ansa_id ]] || return\n\
  IFS= read -r -d '' __ansa_target < \"$__ansa_init_dir/request-$__ansa_id\" || return\n\
  builtin cd -- \"$__ansa_target\"\n\
  zle reset-prompt\n\
  __ansa_prompt_marker\n\
}}\n\
exec {{__ansa_notify_fd}}<> \"$__ansa_init_dir/notify\"\n\
zle -F $__ansa_notify_fd __ansa_cwd_notification\n\
precmd_functions+=(__ansa_prompt_marker)\n\
preexec_functions+=(__ansa_preexec_marker)\n\
autoload -Uz add-zle-hook-widget\n\
add-zle-hook-widget line-init __ansa_prompt_marker\n\
add-zle-hook-widget line-pre-redraw __ansa_prompt_marker\n\
unset ANSA_INIT_DIR ANSA_USER_ZDOTDIR __ansa_user_zdotdir\n", marker.replace(";A;", ";B;")))?;
        } else {
            value.write("bashrc", &format!("[[ -r $HOME/.bashrc ]] && source \"$HOME/.bashrc\"\n{hook}if [[ $(declare -p PROMPT_COMMAND 2>/dev/null) == 'declare -a'* ]]; then\n  PROMPT_COMMAND+=(__ansa_prompt_marker)\nelse\n  PROMPT_COMMAND=\"${{PROMPT_COMMAND:+${{PROMPT_COMMAND}}; }}__ansa_prompt_marker\"\nfi\nPS0=\"${{PS0}}\"$'\\e]{}\\a'\n",marker.replace(";A;", ";B;")))?;
        }
        Ok(value)
    }
    fn write(&self, name: &str, contents: &str) -> io::Result<()> {
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(self.directory.as_ref().expect("owned init").join(name))?
            .write_all(contents.as_bytes())
    }
    fn request_directory(&mut self, path: &Path) -> io::Result<()> {
        if !self.zsh || !path.is_absolute() {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "terminal_manual_cd_required",
            ));
        }
        self.cancel_directory();
        self.next_request = self
            .next_request
            .checked_add(1)
            .ok_or_else(|| io::Error::other("terminal_request_limit"))?;
        let name = format!("request-{}", self.next_request);
        self.write(
            &name,
            &format!(
                "{}\0",
                path.to_str()
                    .ok_or_else(|| io::Error::other("terminal_directory_encoding"))?
            ),
        )?;
        self.request = Some(self.directory.as_ref().expect("owned init").join(name));
        let result = self
            .notify
            .as_mut()
            .expect("zsh notifier")
            .write_all(format!("{}\n", self.next_request).as_bytes());
        if result.is_err() {
            self.cancel_directory();
        }
        result
    }
    fn cancel_directory(&mut self) {
        if let Some(path) = self.request.take() {
            let _ = std::fs::remove_file(path);
        }
    }
    fn command(&self, shell: &Path) -> Command {
        let mut command = Command::new(shell);
        if let Some(directory) = &self.directory {
            if self.zsh {
                command
                    .env("ZDOTDIR", directory)
                    .env("ANSA_INIT_DIR", directory)
                    .env(
                        "ANSA_USER_ZDOTDIR",
                        self.user_zdotdir.as_deref().unwrap_or_default(),
                    );
            } else {
                command.arg("--rcfile").arg(directory.join("bashrc"));
            }
        }
        command.arg("-i");
        command
    }
}
impl Drop for ShellInit {
    fn drop(&mut self) {
        self.cancel_directory();
        if let Some(path) = &self.directory {
            for name in [".zshenv", ".zshrc", "bashrc", "notify"] {
                let _ = std::fs::remove_file(path.join(name));
            }
            let _ = std::fs::remove_dir(path);
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            let mut token = [0; 16];
            SystemRandom::new().fill(&mut token).unwrap();
            let token = token.iter().map(|b| format!("{b:02x}")).collect::<String>();
            let path = std::env::temp_dir().join(format!("ansa-terminal-fixture-{token}"));
            DirBuilder::new().mode(0o700).create(&path).unwrap();
            std::fs::write(
                path.join(".bashrc"),
                "PS1='fixture> '\nHISTFILE=/dev/null\nalias fixture_alias='printf ALIAS_OK'\n",
            )
            .unwrap();
            std::fs::write(
                path.join(".zshrc"),
                "PROMPT='fixture> '\nHISTFILE=/dev/null\nalias fixture_alias='printf ALIAS_OK'\n",
            )
            .unwrap();
            Self(path)
        }
        fn spawn(&self, shell: &str) -> TerminalPane {
            TerminalPane::spawn_configured(Path::new(shell), &self.0, 14, 64, Some(self.0.clone()))
                .unwrap()
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    fn wait_for(
        pane: &TerminalPane,
        predicate: impl Fn(&TerminalSnapshot) -> bool,
    ) -> Arc<TerminalSnapshot> {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let snapshot = pane.snapshot();
            if predicate(&snapshot) {
                return snapshot;
            }
            assert!(!snapshot.exited, "terminal exited: {:?}", snapshot.error);
            assert!(
                Instant::now() < deadline,
                "terminal condition timed out: {snapshot:?}"
            );
            thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn bash_prompt_real_foreground_interrupt_and_no_cwd_injection() {
        let fixture = Fixture::new();
        let mut pane = fixture.spawn("bash");
        wait_for(&pane, |s| s.shell_ready);
        assert_eq!(
            pane.change_directory(&fixture.0).unwrap_err().kind(),
            io::ErrorKind::Unsupported
        );
        pane.input(b"fixture_alias\r").unwrap();
        wait_for(&pane, |s| {
            s.shell_ready && s.rows.join("").contains("ALIAS_OK")
        });
        pane.input(b"sleep 30\r").unwrap();
        wait_for(&pane, |s| !s.shell_ready);
        pane.input(&[3]).unwrap();
        wait_for(&pane, |s| s.shell_ready);
        pane.resize(9, 37).unwrap();
        wait_for(&pane, |s| s.rows.len() == 9);
        pane.input(b"stty size\r").unwrap();
        wait_for(&pane, |s| s.shell_ready && s.rows.join("").contains("9 37"));
        pane.input(b"exit\r").unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while !pane.snapshot().exited && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        assert!(pane.snapshot().exited);
        let cleanup = pane.shutdown();
        assert!(cleanup.shell_reaped, "{cleanup:?}");
    }

    #[test]
    fn zsh_directory_sidechannel_preserves_prompt_and_builtin_read() {
        let fixture = Fixture::new();
        let first = fixture.0.join("first ' каталог");
        let second = fixture.0.join("second");
        std::fs::create_dir(&first).unwrap();
        std::fs::create_dir(&second).unwrap();
        let mut pane = fixture.spawn("zsh");
        wait_for(&pane, |s| s.shell_ready);
        pane.input(b"unfinished_text").unwrap();
        wait_for(&pane, |s| {
            s.shell_ready && s.rows.join("").contains("unfinished_text")
        });
        pane.input(&[21]).unwrap();
        wait_for(&pane, |s| {
            s.shell_ready && !s.rows.join("").contains("unfinished_text")
        });
        pane.change_directory(&first).unwrap();
        wait_for(&pane, |s| s.shell_ready && s.cwd == first);
        pane.input(b"read answer; printf 'READ_%s_DONE\\n' \"$answer\"\r")
            .unwrap();
        wait_for(&pane, |s| !s.shell_ready);
        assert_eq!(
            pane.change_directory(&second).unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
        pane.input(b"safe\r").unwrap();
        wait_for(&pane, |s| {
            s.shell_ready && s.rows.join("").contains("READ_safe_DONE")
        });
        assert_eq!(pane.snapshot().cwd, first);
        pane.change_directory(&second).unwrap();
        wait_for(&pane, |s| s.shell_ready && s.cwd == second);
        let cleanup = pane.shutdown();
        assert!(cleanup.shell_reaped, "{cleanup:?}");
    }

    #[test]
    fn bounded_input_remains_nonblocking_under_stopped_reader() {
        let fixture = Fixture::new();
        let mut pane = fixture.spawn("bash");
        wait_for(&pane, |s| s.shell_ready);
        pane.input(b"stty -echo; sleep 30\r").unwrap();
        wait_for(&pane, |s| !s.shell_ready);
        let began = Instant::now();
        let mut full = false;
        for _ in 0..4096 {
            if pane.input(&[b'x'; INPUT_CHUNK]).is_err() {
                full = true;
                break;
            }
        }
        assert!(full);
        assert!(began.elapsed() < Duration::from_secs(1));
        assert!(pane.queued.load(Ordering::Acquire) <= INPUT_LIMIT);
        pane.input(&[3]).unwrap();
        wait_for(&pane, |s| s.shell_ready);
        assert_eq!(pane.queued.load(Ordering::Acquire), 0);
        let cleanup = pane.shutdown();
        assert!(cleanup.shell_reaped, "{cleanup:?}");
    }
    #[test]
    fn output_is_bounded_and_contains_only_painted_sgr() {
        let mut parser = vt100::Parser::new_with_callbacks(nz(4), nz(16), 0, Replies::default());
        let mut filter = EscapeFilter::new(b"133;A;ansa=test".to_vec());
        filter.process(
            b"hello\x1b]52;c;clipboard\x07\x1b]2;title\x07\x1b[31mRED\x1b[0m",
            &mut parser,
        );
        filter.process(b"\x1b]133;A;ansa=test\x07", &mut parser);
        assert_eq!(filter.prompt, Some(true));
        let mut snapshot = TerminalSnapshot::empty(PathBuf::new());
        update_screen(&mut snapshot, parser.screen());
        assert!(snapshot.rows[0].contains("hello"));
        assert!(snapshot.rows[0].contains("38;5;1"));
        assert!(!snapshot.rows.join("").contains("clipboard"));
        assert!(!snapshot.rows.join("").contains("\x1b]"));
        filter.process(b"\x1b]", &mut parser);
        for _ in 0..1024 {
            filter.process(&[b'x'; 1024], &mut parser);
        }
        assert!(filter.osc.len() <= 129);
        filter.process(b"\x07\x1b[?1049h\x1b[2;3HX\x1b[?1h\x1b[?2004h", &mut parser);
        update_screen(&mut snapshot, parser.screen());
        assert!(
            snapshot.alternate_screen && snapshot.application_cursor && snapshot.bracketed_paste
        );
        assert_eq!(snapshot.cursor, Some((1, 3)));
        filter.process("\x1b[Hа".as_bytes(), &mut parser);
        for _ in 0..1024 {
            filter.process("\u{301}".as_bytes(), &mut parser);
        }
        assert!(parser.screen().cell(0, 0).unwrap().contents().len() <= 22);
        let before = parser.screen().contents();
        filter.process(b"\x1b[65535b", &mut parser);
        assert_eq!(parser.screen().contents(), before);
    }
    #[test]
    fn resize_has_bounded_dimensions_and_no_zero_size() {
        assert_eq!(
            checked_size(u16::MAX, u16::MAX).unwrap(),
            (MAX_ROWS, MAX_COLS)
        );
        assert!(checked_size(0, 10).is_err());
    }
    #[test]
    fn escape_filter_never_exposes_incomplete_parser_state_or_repeat_bypass() {
        let attacks: &[&[u8]] = &[
            b"A\x1b\x1b[65535b",
            b"A\x1b\0[65535b",
            b"A\x1b\x7f[65535b",
            b"A\x1b#\x1b[65535b",
            b"A\x1b[1;\x1b[65535b",
            b"A\x1b[65\0\x7f535b",
            b"A\x1b[65535\x18\x1b[65535b",
            b"A\x1b[65535\x1a\x1b[65535b",
            b"A\x1b(0\x1b[65535b",
            b"A\x1b)0\x1b\x1b\x1b[65535b",
        ];
        for &attack in attacks {
            for split in 0..=attack.len() {
                let mut parser =
                    vt100::Parser::new_with_callbacks(nz(1), nz(16), 0, Replies::default());
                let mut filter = EscapeFilter::new(Vec::new());
                filter.process(&attack[..split], &mut parser);
                filter.process(&attack[split..], &mut parser);
                assert_eq!(
                    parser.screen().contents(),
                    "A",
                    "split={split}, attack={attack:?}"
                );
                filter.process(b"\x1b[31mZ", &mut parser);
                assert_eq!(parser.screen().contents(), "AZ");
                assert_eq!(
                    parser.screen().cell(0, 1).unwrap().fgcolor(),
                    vt100::Color::Idx(1)
                );
            }
            let mut parser =
                vt100::Parser::new_with_callbacks(nz(1), nz(16), 0, Replies::default());
            let mut filter = EscapeFilter::new(Vec::new());
            for byte in attack {
                filter.process(&[*byte], &mut parser);
            }
            assert_eq!(parser.screen().contents(), "A", "byte chunks: {attack:?}");
        }
        let mut parser = vt100::Parser::new_with_callbacks(nz(1), nz(32), 0, Replies::default());
        let mut filter = EscapeFilter::new(Vec::new());
        filter.process(b"A\x1b(0[65535b", &mut parser);
        assert_eq!(parser.screen().contents(), "A[65535b");
        filter.process(b"\x1b[3b", &mut parser);
        assert_eq!(parser.screen().contents(), "A[65535bbbb");
    }
}
