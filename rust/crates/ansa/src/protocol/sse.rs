//! Bounded, incremental SSE framing for one-shot HTTP response bodies.
use std::collections::VecDeque;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Event {
    pub name: String,
    pub data: String,
}
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    pub body: usize,
    pub line: usize,
    pub event: usize,
    pub events: usize,
    pub arguments: usize,
    pub items: usize,
    pub calls: usize,
    pub json_depth: usize,
    pub json_nodes: usize,
}
impl Default for Limits {
    fn default() -> Self {
        Self {
            body: 16 * 1024 * 1024,
            line: 1024 * 1024,
            event: 1024 * 1024,
            events: 65536,
            arguments: 256 * 1024,
            items: 64,
            calls: 16,
            json_depth: 64,
            json_nodes: 131072,
        }
    }
}

/// Parser retains completed events until drained. Any parsing failure is terminal.
pub struct Parser {
    limits: Limits,
    queue: VecDeque<Event>,
    line: Vec<u8>,
    name: String,
    data: String,
    bytes: usize,
    event_bytes: usize,
    count: usize,
    start: bool,
    prefix: Vec<u8>,
    cr: bool,
    has_data: bool,
    closed: bool,
    poisoned: bool,
    continuation: u8,
    point: u32,
    minimum: u32,
}
impl Parser {
    pub fn new(limits: Limits) -> Self {
        Self {
            limits,
            queue: VecDeque::new(),
            line: Vec::new(),
            name: String::new(),
            data: String::new(),
            bytes: 0,
            event_bytes: 0,
            count: 0,
            start: true,
            prefix: Vec::new(),
            cr: false,
            has_data: false,
            closed: false,
            poisoned: false,
            continuation: 0,
            point: 0,
            minimum: 0,
        }
    }
    pub fn take(&mut self) -> Vec<Event> {
        self.queue.drain(..).collect()
    }
    pub fn feed(&mut self, input: &[u8]) -> Result<(), &'static str> {
        if self.closed || self.poisoned {
            return Err("sse_closed");
        }
        let result = self.feed_inner(input);
        if result.is_err() {
            self.poisoned = true;
        }
        result
    }
    fn feed_inner(&mut self, input: &[u8]) -> Result<(), &'static str> {
        if input.len() > self.limits.body.saturating_sub(self.bytes) {
            return Err("sse_body_limit");
        }
        self.bytes += input.len();
        for b in input.iter().copied() {
            self.utf8(b)?;
            if self.start {
                self.prefix.push(b);
                if b"\xef\xbb\xbf".starts_with(&self.prefix) {
                    if self.prefix.len() == 3 {
                        self.start = false;
                        self.prefix.clear();
                    }
                    continue;
                }
                self.start = false;
                let pending = std::mem::take(&mut self.prefix);
                for p in pending {
                    self.byte(p)?;
                }
            } else {
                self.byte(b)?;
            }
        }
        Ok(())
    }
    fn utf8(&mut self, b: u8) -> Result<(), &'static str> {
        if self.continuation != 0 {
            if b & 0xc0 != 0x80 {
                return Err("sse_utf8_continuation");
            }
            self.point = (self.point << 6) | u32::from(b & 0x3f);
            self.continuation -= 1;
            if self.continuation == 0
                && (self.point < self.minimum
                    || self.point > 0x10ffff
                    || (0xd800..=0xdfff).contains(&self.point))
            {
                return Err("sse_utf8_codepoint");
            }
        } else if b < 0x80 {
            return Ok(());
        } else if (0xc2..=0xdf).contains(&b) {
            self.continuation = 1;
            self.point = u32::from(b & 0x1f);
            self.minimum = 0x80;
        } else if (0xe0..=0xef).contains(&b) {
            self.continuation = 2;
            self.point = u32::from(b & 0x0f);
            self.minimum = 0x800;
        } else if (0xf0..=0xf4).contains(&b) {
            self.continuation = 3;
            self.point = u32::from(b & 0x07);
            self.minimum = 0x10000;
        } else {
            return Err("sse_utf8_lead");
        }
        Ok(())
    }
    fn byte(&mut self, b: u8) -> Result<(), &'static str> {
        if self.cr {
            self.cr = false;
            if b == b'\n' {
                return Ok(());
            }
        }
        self.event_bytes += 1;
        if self.event_bytes > self.limits.event {
            return Err("sse_event_limit");
        }
        if b == b'\r' || b == b'\n' {
            self.line()?;
            self.cr = b == b'\r';
        } else {
            if self.line.len() >= self.limits.line {
                return Err("sse_line_limit");
            }
            self.line.push(b);
        }
        Ok(())
    }
    fn line(&mut self) -> Result<(), &'static str> {
        if self.line.is_empty() {
            if self.has_data {
                self.count += 1;
                if self.count > self.limits.events {
                    return Err("sse_event_count");
                }
                self.data.pop();
                self.queue.push_back(Event {
                    name: if self.name.is_empty() {
                        "message".into()
                    } else {
                        std::mem::take(&mut self.name)
                    },
                    data: std::mem::take(&mut self.data),
                });
                self.has_data = false;
                self.event_bytes = 0;
            }
            self.name.clear();
            self.event_bytes = 0;
        } else if self.line[0] != b':' {
            let split = self
                .line
                .iter()
                .position(|b| *b == b':')
                .unwrap_or(self.line.len());
            let key = std::str::from_utf8(&self.line[..split]).map_err(|_| "sse_utf8")?;
            let mut value = if split == self.line.len() {
                &[][..]
            } else {
                &self.line[split + 1..]
            };
            if value.first() == Some(&b' ') {
                value = &value[1..];
            }
            if key == "data" {
                if value.len() + 1 > self.limits.event.saturating_sub(self.data.len()) {
                    return Err("sse_data_limit");
                }
                self.data
                    .push_str(std::str::from_utf8(value).map_err(|_| "sse_utf8")?);
                self.data.push('\n');
                self.has_data = true;
            } else if key == "event" {
                self.name = std::str::from_utf8(value)
                    .map_err(|_| "sse_utf8")?
                    .to_owned();
            }
        }
        self.line.clear();
        Ok(())
    }
    pub fn finish(&mut self) -> Result<Vec<Event>, &'static str> {
        if self.closed || self.poisoned {
            return Err("sse_closed");
        }
        self.closed = true;
        if self.continuation != 0 {
            self.poisoned = true;
            return Err("sse_utf8_eof");
        }
        if self.start && !self.prefix.is_empty() {
            for b in std::mem::take(&mut self.prefix) {
                self.byte(b)?;
            }
        }
        if !self.line.is_empty() || self.has_data || !self.name.is_empty() {
            self.poisoned = true;
            return Err("sse_unterminated_event");
        }
        Ok(self.take())
    }
}
