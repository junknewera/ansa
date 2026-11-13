//! Bounded output worker. Blocking stdout never owns runtime control.
use super::{Result, carbon::Frame};
use ansa_linux::terminal::{Descriptor, Nonblocking, RawMode, Signals};
use std::{
    collections::VecDeque,
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};
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
}
pub struct Writer {
    shared: Arc<Shared>,
    join: Option<JoinHandle<()>>,
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
        });
        let state = shared.clone();
        let join = thread::Builder::new()
            .name("ansa-output".into())
            .spawn(move || {
                let mut previous = 0;
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
                        Item::Text(v) => (0, v),
                        Item::Frame(sequence, v) => {
                            let result = v.paint(previous);
                            previous = v.lines.len();
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
                            Ok(n) => remaining = &remaining[n..],
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
                        break;
                    }
                    state.inflight.store(false, Ordering::Release);
                    if remaining.is_empty() && sequence > 0 {
                        state.displayed.store(sequence, Ordering::Release);
                    }
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
        if p.queue.len() >= 32 || p.bytes + text.len() > 32 * 1024 * 1024 + 1 {
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
    pub fn displayed(&self, sequence: u64) -> bool {
        self.shared.displayed.load(Ordering::Acquire) >= sequence
    }
    pub fn failed(&self) -> bool {
        self.shared.failed.load(Ordering::Acquire)
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
    /// Drain normally; permanently blocked output must still join within 300ms.
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
        Ok(Self {
            input,
            diagnostics,
            writer,
            raw,
            size,
            color,
            _nonblocking: nonblocking,
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
}
impl Drop for Io {
    fn drop(&mut self) {
        self.writer.finish();
        if let Some(raw) = &mut self.raw {
            raw.restore();
        }
    }
}
