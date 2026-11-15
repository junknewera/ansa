//! Bounded output worker. Blocking stdout never owns runtime control.
use super::{Result, carbon::Frame};
use ansa_linux::terminal::{Descriptor, Nonblocking, RawMode, Signals};
use std::{
    collections::VecDeque,
    path::{Path, PathBuf},
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};
const TERMINAL_ENABLE: &str = "\x1b[?2004h\x1b[?1002h\x1b[?1006h";
const TERMINAL_DISABLE: &str = "\x1b[?1006l\x1b[?1002l\x1b[?2004l";
enum Item {
    Text(String),
    Frame(u64, Frame),
}
struct Pending {
    queue: VecDeque<Item>,
    bytes: usize,
    frame: Option<(u64, Frame)>,
    stop: bool,
}
struct Shared {
    pending: Mutex<Pending>,
    wake: Condvar,
    abort: AtomicBool,
    failed: AtomicBool,
    sequence: AtomicU64,
    displayed: AtomicU64,
    inflight: AtomicBool,
    changed: tokio::sync::Notify,
    repaint: AtomicBool,
    written: AtomicU64,
}
pub struct Writer {
    shared: Arc<Shared>,
    join: Option<JoinHandle<()>>,
}
#[derive(Default)]
pub(super) struct Drain {
    written: u64,
    deadline: Option<Instant>,
}
impl Drain {
    pub fn stalled(&mut self, written: u64, idle: bool, now: Instant) -> bool {
        if idle {
            self.deadline = None;
        } else if self.deadline.is_none() || written != self.written {
            self.deadline = Some(now + Duration::from_millis(250));
        }
        self.written = written;
        self.deadline.is_some_and(|deadline| now >= deadline)
    }
}
impl Writer {
    pub fn new(output: Descriptor) -> Result<Self> {
        let shared = Arc::new(Shared {
            pending: Mutex::new(Pending {
                queue: VecDeque::new(),
                bytes: 0,
                frame: None,
                stop: false,
            }),
            wake: Condvar::new(),
            abort: AtomicBool::new(false),
            failed: AtomicBool::new(false),
            sequence: AtomicU64::new(0),
            displayed: AtomicU64::new(0),
            inflight: AtomicBool::new(false),
            changed: tokio::sync::Notify::new(),
            repaint: AtomicBool::new(false),
            written: AtomicU64::new(0),
        });
        let state = shared.clone();
        let join = thread::Builder::new()
            .name("ansa-output".into())
            .spawn(move || {
                let mut previous: Option<Frame> = None;
                loop {
                    let item = {
                        let mut p = state.pending.lock().unwrap_or_else(|e| e.into_inner());
                        while p.queue.is_empty() && p.frame.is_none() && !p.stop {
                            p = state.wake.wait(p).unwrap_or_else(|e| e.into_inner());
                        }
                        if state.abort.load(Ordering::Acquire) {
                            break;
                        }
                        if !p.queue.is_empty() || p.frame.is_some() {
                            state.inflight.store(true, Ordering::Release);
                        }
                        if let Some(item) = p.queue.pop_front() {
                            if let Item::Text(v) = &item {
                                p.bytes -= v.len();
                            }
                            Some(item)
                        } else if let Some((sequence, frame)) = p.frame.take() {
                            Some(Item::Frame(sequence, frame))
                        } else {
                            None
                        }
                    };
                    let Some(item) = item else {
                        break;
                    };
                    let (sequence, text) = match item {
                        Item::Text(v) => {
                            previous = None;
                            (0, v)
                        }
                        Item::Frame(sequence, v) => {
                            if state.repaint.swap(false, Ordering::AcqRel) {
                                previous = None;
                            }
                            let result = previous
                                .as_ref()
                                .map_or_else(|| v.paint(0), |old| v.paint_changes(old));
                            previous = Some(v);
                            (sequence, result)
                        }
                    };
                    let mut remaining = text.as_bytes();
                    while !remaining.is_empty() && !state.abort.load(Ordering::Acquire) {
                        match output.write(remaining) {
                            Ok(0) => {
                                state.failed.store(true, Ordering::Release);
                                break;
                            }
                            Ok(n) => {
                                state.written.fetch_add(n as u64, Ordering::Relaxed);
                                remaining = &remaining[n..];
                            }
                            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                                thread::sleep(Duration::from_millis(5))
                            }
                            Err(_) => {
                                state.failed.store(true, Ordering::Release);
                                break;
                            }
                        }
                    }
                    if state.failed.load(Ordering::Acquire) {
                        state.changed.notify_one();
                        break;
                    }
                    state.inflight.store(false, Ordering::Release);
                    if remaining.is_empty() && sequence > 0 {
                        state.displayed.store(sequence, Ordering::Release);
                    }
                    state.changed.notify_one();
                }
            })
            .map_err(|_| "cli_output_thread")?;
        Ok(Self {
            shared,
            join: Some(join),
        })
    }
    pub fn capacity(&self) -> bool {
        let p = self
            .shared
            .pending
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        p.queue.len() < 32 && p.bytes <= 16 * 1024 * 1024
    }
    pub async fn changed(&self) {
        self.shared.changed.notified().await;
    }
    pub fn text(&self, text: String) -> bool {
        if text.len() > 16 * 1024 * 1024 + 1 {
            self.shared.failed.store(true, Ordering::Release);
            return false;
        }
        let mut p = self
            .shared
            .pending
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if p.stop
            || self.failed()
            || p.queue.len() >= 32
            || p.bytes + text.len() > 32 * 1024 * 1024 + 1
        {
            return false;
        }
        p.bytes += text.len();
        p.queue.push_back(Item::Text(text));
        self.shared.wake.notify_one();
        true
    }
    pub fn frame(&self, frame: Frame) -> u64 {
        let sequence = self.shared.sequence.fetch_add(1, Ordering::Relaxed) + 1;
        let mut p = self
            .shared
            .pending
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        p.frame = Some((sequence, frame));
        self.shared.wake.notify_one();
        sequence
    }
    pub fn clipboard(&self, text: &str) -> bool {
        use base64::Engine;
        if text.len() > super::copy::LIMIT {
            return false;
        }
        self.text(format!(
            "\x1b]52;c;{}\x07",
            base64::engine::general_purpose::STANDARD.encode(text.as_bytes())
        ))
    }
    pub fn invalidate(&self) {
        self.shared.repaint.store(true, Ordering::Release);
    }
    pub fn displayed(&self, sequence: u64) -> bool {
        self.shared.displayed.load(Ordering::Acquire) >= sequence
    }
    pub fn failed(&self) -> bool {
        self.shared.failed.load(Ordering::Acquire)
    }
    pub fn written(&self) -> u64 {
        self.shared.written.load(Ordering::Relaxed)
    }
    pub fn idle(&self) -> bool {
        let p = self
            .shared
            .pending
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        self.failed()
            || (p.queue.is_empty()
                && p.frame.is_none()
                && !self.shared.inflight.load(Ordering::Acquire))
    }
    pub fn abort(&self) {
        self.shared.failed.store(true, Ordering::Release);
        self.shared.abort.store(true, Ordering::Release);
        self.shared.wake.notify_one();
    }
    /// Normal drain belongs to the async controller; Drop/error cleanup is bounded.
    pub fn finish(&mut self) -> bool {
        {
            let mut p = self
                .shared
                .pending
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            p.stop = true;
            self.shared.wake.notify_one();
        }
        let deadline = Instant::now() + Duration::from_millis(250);
        if let Some(join) = self.join.take() {
            while !join.is_finished() && Instant::now() < deadline {
                thread::sleep(Duration::from_millis(5));
            }
            if !join.is_finished() {
                self.shared.failed.store(true, Ordering::Release);
                self.shared.abort.store(true, Ordering::Release);
                self.shared.wake.notify_one();
            }
            if join.join().is_err() {
                self.shared.failed.store(true, Ordering::Release);
            }
        }
        !self.failed()
    }
}
impl Drop for Writer {
    fn drop(&mut self) {
        self.finish();
    }
}
pub struct Io {
    pub input: Descriptor,
    pub diagnostics: Descriptor,
    pub writer: Writer,
    pub raw: Option<RawMode>,
    pub size: (usize, usize),
    pub color: bool,
    _nonblocking: Nonblocking,
    editor_active: bool,
    reporting_enabled: bool,
    pub signals: Signals,
}
impl Io {
    pub fn open(interactive: bool) -> Result<Self> {
        // Install/block signals before any runtime/output thread exists.
        let signals = Signals::new().map_err(|_| "cli_signal_handler")?;
        let input = Descriptor::new(0).map_err(|_| "cli_input_read")?;
        let output = Descriptor::new(1).map_err(|_| "cli_output")?;
        let diagnostics = Descriptor::new(2).map_err(|_| "cli_output")?;
        let nonblocking =
            Nonblocking::new(&[&input, &output, &diagnostics]).map_err(|_| "cli_output")?;
        let terminal = interactive && input.terminal() && output.terminal();
        let size = output.size();
        let raw = if terminal {
            Some(RawMode::new().map_err(|_| "cli_termios")?)
        } else {
            None
        };
        let color = std::env::var_os("NO_COLOR").is_none()
            && std::env::var("TERM").ok().as_deref() != Some("dumb");
        // Keep guards in local reverse-drop order until thread creation succeeds.
        let writer = Writer::new(output)?;
        if terminal && !writer.text(TERMINAL_ENABLE.into()) {
            return Err("cli_output");
        }
        Ok(Self {
            input,
            diagnostics,
            writer,
            raw,
            size,
            color,
            _nonblocking: nonblocking,
            editor_active: false,
            reporting_enabled: terminal,
            signals,
        })
    }
    pub fn status(&self, text: &str) {
        let _ = self.diagnostics.write(text.as_bytes());
    }
    pub fn resize(&mut self) {
        // query fd1 without changing its open-file flags
        if let Ok(output) = Descriptor::new(1) {
            self.size = output.size();
        }
    }
    pub fn editor_active(&self) -> bool {
        self.editor_active
    }
    /// Queue reporting cleanup before the controller's final output drain and
    /// Writer::finish. A finished writer cannot deliver cleanup from Io::drop.
    pub fn stop_reporting(&mut self) -> Result<()> {
        if self.raw.is_some() && self.reporting_enabled {
            if !self.writer.text(TERMINAL_DISABLE.into()) {
                return Err("cli_output");
            }
            self.reporting_enabled = false;
        }
        Ok(())
    }
    pub fn resume_terminal(&mut self) -> Result<()> {
        if self.editor_active {
            return Ok(());
        }
        self._nonblocking.resume().map_err(|_| "cli_output")?;
        if let Some(raw) = &mut self.raw {
            raw.resume().map_err(|_| "cli_termios")?;
            if !self.writer.text(TERMINAL_ENABLE.into()) {
                return Err("cli_output");
            }
            self.reporting_enabled = true;
        }
        self.writer.invalidate();
        self.resize();
        Ok(())
    }
    /// Return modes and aliased file flags before shell job control, then
    /// re-enter the frontend after SIGCONT. Waiting yields to the reactor.
    pub async fn suspend(&mut self) -> Result<()> {
        if self.editor_active {
            return Err("editor_active");
        }
        self.stop_reporting()?;
        let deadline = Instant::now() + Duration::from_millis(250);
        while !self.writer.idle() {
            if Instant::now() >= deadline {
                return Err("cli_suspend_output_stalled");
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        if let Some(raw) = &mut self.raw {
            raw.restore_checked().map_err(|_| "cli_termios")?;
        }
        self._nonblocking.restore().map_err(|_| "cli_output")?;
        let result = self.signals.suspend().map_err(|_| "cli_suspend");
        let restored = self.resume_terminal();
        result.and(restored)
    }
    pub fn start_editor(&mut self, argv: &[String], directory: &Path) -> Result<EditorJob> {
        if self.raw.is_none() {
            return Err("editor_requires_terminal");
        }
        if self.editor_active {
            return Err("editor_active");
        }
        if !self.writer.idle() || self.writer.failed() {
            return Err("editor_output_pending");
        }
        if argv.is_empty() || argv[0].is_empty() || argv.iter().any(|arg| arg.contains('\0')) {
            return Err("workspace_editor_command");
        }
        self.stop_reporting()?;
        self.editor_active = true;
        Ok(EditorJob {
            argv: argv.to_vec(),
            directory: directory.into(),
            child: None,
            cancelled: false,
            done: false,
            setup: EditorSetup::new(Instant::now()),
        })
    }
    fn restore_editor(&mut self) -> Result<()> {
        self.editor_active = false;
        self.resume_terminal()?;
        // An interrupted editor cannot run its own screen/cursor cleanup.
        // Resume NONBLOCK before waking Writer, then reset before any frame.
        if self.raw.is_some() && !self.writer.text("\x1b[?1049l\x1b[?25h\x1b[0m".into()) {
            return Err("cli_output");
        }
        Ok(())
    }
}
impl Drop for Io {
    fn drop(&mut self) {
        let _ = self.stop_reporting();
        self.writer.finish();
        if let Some(raw) = &mut self.raw {
            raw.restore();
        }
    }
}

pub use ansa_linux::terminal::EditorExit;
/// The frontend keeps polling this owner while an external editor controls the
/// terminal. No other reader or repaint may use `Io` during that handoff.
pub struct EditorJob {
    argv: Vec<String>,
    directory: PathBuf,
    child: Option<ansa_linux::terminal::ForegroundEditor>,
    cancelled: bool,
    done: bool,
    setup: EditorSetup,
}
enum EditorSetupPoll {
    Cancelled,
    Failed(&'static str),
    Waiting,
    Ready,
}
struct EditorSetup {
    drain: Drain,
    deadline: Instant,
}
impl EditorSetup {
    fn new(now: Instant) -> Self {
        Self {
            drain: Drain::default(),
            deadline: now + Duration::from_secs(1),
        }
    }
    fn poll(
        &mut self,
        cancelled: bool,
        failed: bool,
        idle: bool,
        written: u64,
        now: Instant,
    ) -> EditorSetupPoll {
        // Cancellation cannot wait for stdout: no process has been launched,
        // so the handoff can be abandoned immediately even under flow control.
        if cancelled {
            return EditorSetupPoll::Cancelled;
        }
        if failed {
            return EditorSetupPoll::Failed("editor_setup_output_failed");
        }
        if now >= self.deadline || self.drain.stalled(written, idle, now) {
            return EditorSetupPoll::Failed("editor_setup_output_stalled");
        }
        if idle {
            EditorSetupPoll::Ready
        } else {
            EditorSetupPoll::Waiting
        }
    }
}
impl EditorJob {
    pub fn cancel(&mut self) -> Result<()> {
        self.cancelled = true;
        if let Some(child) = &mut self.child {
            child.cancel().map_err(|_| "editor_cancel")?;
        }
        Ok(())
    }
    /// Emergency cleanup after the caller's cancellation budget. The caller
    /// must terminate the harness after this call, including when it returns
    /// an error. Returning to a model or tool dispatch is not permitted.
    pub fn abort_for_exit(&mut self, io: &mut Io) -> Result<EditorExit> {
        self.cancelled = true;
        self.done = true;
        let result = self.child.take().map_or_else(
            || {
                Ok(EditorExit {
                    code: 130,
                    cleanup_unknown: false,
                })
            },
            |mut child| {
                child
                    .abandon_for_exit()
                    .map_err(|_| "editor_cleanup_unknown")
            },
        );
        let restored = io.restore_editor();
        match (result, restored) {
            (Err(code), _) | (_, Err(code)) => Err(code),
            (Ok(exit), Ok(())) => Ok(exit),
        }
    }
    pub fn poll(&mut self, io: &mut Io) -> Result<Option<EditorExit>> {
        if self.done {
            return Ok(None);
        }
        if self.child.is_none() {
            match self.setup.poll(
                self.cancelled,
                io.writer.failed(),
                io.writer.idle(),
                io.writer.written(),
                Instant::now(),
            ) {
                EditorSetupPoll::Cancelled => {
                    self.done = true;
                    io.restore_editor()?;
                    return Ok(Some(EditorExit {
                        code: 130,
                        cleanup_unknown: false,
                    }));
                }
                EditorSetupPoll::Failed(code) => {
                    self.done = true;
                    io.restore_editor()?;
                    return Err(code);
                }
                EditorSetupPoll::Waiting => return Ok(None),
                EditorSetupPoll::Ready => {}
            }
            let prepared = (|| {
                io.raw
                    .as_mut()
                    .ok_or("editor_requires_terminal")?
                    .restore_checked()
                    .map_err(|_| "cli_termios")?;
                io._nonblocking.restore().map_err(|_| "cli_output")?;
                let mut command = std::process::Command::new(&self.argv[0]);
                command.args(&self.argv[1..]).current_dir(&self.directory);
                ansa_linux::terminal::ForegroundEditor::spawn(command, &io.input)
                    .map_err(|_| "editor_spawn")
            })();
            match prepared {
                Ok(child) => self.child = Some(child),
                Err(code) => {
                    self.done = true;
                    io.restore_editor()?;
                    return Err(code);
                }
            }
        }
        match self.child.as_mut().expect("started editor").poll() {
            Ok(Some(exit)) => {
                self.child = None;
                self.done = true;
                io.restore_editor()?;
                Ok(Some(exit))
            }
            Ok(None) => Ok(None),
            Err(_) => {
                // A failed wait cannot establish primary cleanup. Do not
                // drop into a blocking wait or return to dispatch: reclaim
                // what we can nonblockingly, then require harness shutdown.
                let _ = self.abort_for_exit(io);
                Err("editor_cleanup_unknown")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn drain_ignores_idle_encoding_time_and_requires_pending_write_progress() {
        let start = Instant::now();
        let mut drain = Drain::default();
        assert!(!drain.stalled(0, true, start));
        // Serialization or scheduling may take arbitrarily long before any
        // bytes are enqueued. That time does not consume the writer's budget.
        let enqueued = start + Duration::from_secs(2);
        assert!(!drain.stalled(0, false, enqueued));
        assert!(!drain.stalled(0, false, enqueued + Duration::from_millis(249)));
        assert!(drain.stalled(0, false, enqueued + Duration::from_millis(250)));
        assert!(!drain.stalled(0, true, enqueued + Duration::from_secs(1)));
        let enqueued = start + Duration::from_secs(4);
        assert!(!drain.stalled(0, false, enqueued));
        assert!(!drain.stalled(4096, false, enqueued + Duration::from_millis(200)));
        assert!(!drain.stalled(4096, false, enqueued + Duration::from_millis(449)));
        assert!(drain.stalled(4096, false, enqueued + Duration::from_millis(450)));
    }
    #[test]
    fn editor_setup_cancel_preempts_blocked_output_and_expired_deadlines() {
        let now = Instant::now();
        let mut setup = EditorSetup::new(now);
        assert!(matches!(
            setup.poll(false, false, false, 0, now),
            EditorSetupPoll::Waiting
        ));
        assert!(matches!(
            setup.poll(true, false, false, 0, now + Duration::from_secs(2)),
            EditorSetupPoll::Cancelled
        ));
        assert!(matches!(
            setup.poll(true, true, true, 0, now + Duration::from_secs(2)),
            EditorSetupPoll::Cancelled
        ));
    }
    #[test]
    fn editor_setup_bounds_idle_drain_and_slow_progress_before_launch() {
        let now = Instant::now();
        let mut setup = EditorSetup::new(now);
        assert!(matches!(
            setup.poll(false, false, false, 0, now),
            EditorSetupPoll::Waiting
        ));
        assert!(matches!(
            setup.poll(false, false, false, 1, now + Duration::from_millis(200)),
            EditorSetupPoll::Waiting
        ));
        assert!(matches!(
            setup.poll(false, false, false, 1, now + Duration::from_millis(450)),
            EditorSetupPoll::Failed("editor_setup_output_stalled")
        ));
        let mut setup = EditorSetup::new(now);
        for millis in [0, 200, 400, 600, 800] {
            assert!(matches!(
                setup.poll(
                    false,
                    false,
                    false,
                    millis,
                    now + Duration::from_millis(millis)
                ),
                EditorSetupPoll::Waiting
            ));
        }
        assert!(matches!(
            setup.poll(false, false, true, 1000, now + Duration::from_secs(1)),
            EditorSetupPoll::Failed("editor_setup_output_stalled")
        ));
    }
}
