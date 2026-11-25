//! Bounded terminal editor. Paste contents never become terminal actions.
pub const MAX_COMPOSER_BYTES: usize = crate::protocol::json::Json::FRAME_BYTES;
const PASTE_END: &[u8] = b"\x1b[201~";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseKind {
    Press,
    Drag,
    Release,
    WheelUp,
    WheelDown,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MouseEvent {
    /// Zero-based terminal cell coordinates.
    pub column: usize,
    pub row: usize,
    pub kind: MouseKind,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    Submit,
    /// Kept for consumers of the typed input API.
    QueueSubmit,
    Overflow,
    /// Kept for consumers of the old typed input API; no key emits it.
    ToggleMode,
    /// Kept for consumers of the old typed input API.
    ToggleHistory,
    FocusNext,
    FocusPrevious,
    Escape,
    Mouse(MouseEvent),
    PageUp,
    PageDown,
    HistoryOlder,
    HistoryNewer,
    OpenFiles,
    OpenTerminals,
    ToggleTools,
    CopyHistory,
    Complete,
    Eof,
}
#[derive(Default, Debug)]
pub struct Editor {
    pub bytes: Vec<u8>,
    pub cursor: usize,
    escape: Vec<u8>,
    paste: bool,
    paste_start: usize,
    pub(super) pasted_ranges: Vec<std::ops::Range<usize>>,
    pub(super) clipboard_anchor: Option<usize>,
    paste_end: Vec<u8>,
    paste_cr: bool,
    pub overflow: bool,
    complete_right: bool,
    alt_utf8: usize,
}
impl Editor {
    pub fn set_completion(&mut self, active: bool) {
        self.complete_right = active;
    }
    pub fn replace(&mut self, begin: usize, end: usize, value: &[u8]) -> bool {
        if self.overflow
            || begin > end
            || end > self.bytes.len()
            || self.bytes.len() - (end - begin) + value.len() > MAX_COMPOSER_BYTES
        {
            return false;
        }
        let Ok(text) = std::str::from_utf8(&self.bytes) else {
            return false;
        };
        if !text.is_char_boundary(begin)
            || !text.is_char_boundary(end)
            || std::str::from_utf8(value).is_err()
        {
            return false;
        }
        self.adjust_pasted_ranges(begin, end, value.len());
        self.bytes.splice(begin..end, value.iter().copied());
        self.cursor = begin + value.len();
        true
    }
    pub fn clear(&mut self) {
        self.bytes.clear();
        self.cursor = 0;
        self.overflow = false;
        self.escape.clear();
        self.paste = false;
        self.paste_start = 0;
        self.pasted_ranges.clear();
        self.clipboard_anchor = None;
        self.paste_end.clear();
        self.paste_cr = false;
        self.complete_right = false;
        self.alt_utf8 = 0;
    }
    pub fn text(&self) -> super::Result<&str> {
        std::str::from_utf8(&self.bytes).map_err(|_| "json_string_invalid")
    }
    pub fn escape_pending(&self) -> bool {
        !self.escape.is_empty()
    }
    /// Call after a short idle interval, since ESC is also a sequence prefix.
    pub fn flush_escape(&mut self) -> Option<Action> {
        let escape = self.escape.as_slice() == b"\x1b";
        self.escape.clear();
        escape.then_some(Action::Escape)
    }
    pub fn pasting(&self) -> bool {
        self.paste
    }
    fn adjust_pasted_ranges(&mut self, begin: usize, end: usize, replacement: usize) {
        if let Some(anchor) = &mut self.clipboard_anchor
            && begin < *anchor
        {
            *anchor = begin + replacement + anchor.saturating_sub(end);
        }
        let first = self.pasted_ranges.partition_point(|r| r.end <= begin);
        let after = first + self.pasted_ranges[first..].partition_point(|r| r.start < end);
        self.pasted_ranges.drain(first..after);
        for range in &mut self.pasted_ranges[first..] {
            range.start = range.start - (end - begin) + replacement;
            range.end = range.end - (end - begin) + replacement;
        }
    }
    fn erase(&mut self, begin: usize, end: usize) {
        if begin != end {
            self.adjust_pasted_ranges(begin, end, 0);
            self.bytes.drain(begin..end);
        }
    }
    fn mark_paste(&mut self, start: usize) {
        let end = self.cursor;
        if !self.overflow
            && let Some(text) = self
                .bytes
                .get(start..end)
                .and_then(|bytes| std::str::from_utf8(bytes).ok())
            && (text.chars().nth(1_000).is_some()
                || text.bytes().filter(|&b| b == b'\n').nth(9).is_some())
        {
            let index = self.pasted_ranges.partition_point(|r| r.start < start);
            self.pasted_ranges.insert(index, start..end);
        }
    }
    pub(super) fn insert_clipboard_text(&mut self, start: usize, text: &str) -> bool {
        let clean: String = text
            .replace("\r\n", "\n")
            .replace('\r', "\n")
            .chars()
            .filter(|&ch| !ch.is_control() || matches!(ch, '\n' | '\t'))
            .collect();
        let cursor = self.cursor;
        if !self.replace(start, start, clean.as_bytes()) {
            return false;
        }
        self.mark_paste(start);
        self.cursor = if cursor < start {
            cursor
        } else {
            cursor + clean.len()
        };
        true
    }
    /// Only presentation changes. Moving inside a paste reveals it for editing.
    pub(super) fn composer_view(&self) -> (std::borrow::Cow<'_, [u8]>, usize) {
        if self.pasted_ranges.is_empty() {
            return (std::borrow::Cow::Borrowed(&self.bytes), self.cursor);
        }
        let mut display = Vec::new();
        let mut cursor = self.cursor;
        let mut after = 0;
        for range in &self.pasted_ranges {
            if range.start < self.cursor && self.cursor < range.end {
                continue;
            }
            let Some(text) = self
                .bytes
                .get(range.clone())
                .and_then(|bytes| std::str::from_utf8(bytes).ok())
            else {
                continue;
            };
            display.extend_from_slice(&self.bytes[after..range.start]);
            let marker = format!("[pasted text · {} chars]", text.chars().count());
            display.extend_from_slice(marker.as_bytes());
            if range.end <= self.cursor {
                cursor = cursor - range.len() + marker.len();
            }
            after = range.end;
        }
        display.extend_from_slice(&self.bytes[after..]);
        (std::borrow::Cow::Owned(display), cursor)
    }
    fn insert(&mut self, ch: u8) {
        if !self.overflow && self.bytes.len() < MAX_COMPOSER_BYTES {
            self.cursor = self.cursor.min(self.bytes.len());
            self.adjust_pasted_ranges(self.cursor, self.cursor, 1);
            self.bytes.insert(self.cursor, ch);
            self.cursor += 1;
        } else {
            self.overflow = true;
        }
    }
    fn previous(&self, cursor: usize) -> usize {
        let mut pos = cursor.saturating_sub(1);
        while pos > 0 && self.bytes[pos] & 0xc0 == 0x80 {
            pos -= 1;
        }
        pos
    }
    fn next(&self, cursor: usize) -> usize {
        let mut pos = (cursor + 1).min(self.bytes.len());
        while pos < self.bytes.len() && self.bytes[pos] & 0xc0 == 0x80 {
            pos += 1;
        }
        pos
    }
    fn line_start(&self) -> usize {
        self.bytes[..self.cursor]
            .iter()
            .rposition(|&b| b == b'\n')
            .map_or(0, |i| i + 1)
    }
    fn line_end(&self) -> usize {
        self.bytes[self.cursor..]
            .iter()
            .position(|&b| b == b'\n')
            .map_or(self.bytes.len(), |i| self.cursor + i)
    }
    fn backspace(&mut self) {
        if !self.overflow && self.cursor > 0 {
            let previous = self.previous(self.cursor);
            self.erase(previous, self.cursor);
            self.cursor = previous;
        }
    }
    fn delete(&mut self) {
        if !self.overflow && self.cursor < self.bytes.len() {
            self.erase(self.cursor, self.next(self.cursor));
        }
    }
    fn word_left(&self) -> usize {
        let mut at = self.cursor;
        while at > 0 && self.bytes[self.previous(at)].is_ascii_whitespace() {
            at = self.previous(at);
        }
        while at > 0 && !self.bytes[self.previous(at)].is_ascii_whitespace() {
            at = self.previous(at);
        }
        at
    }
    fn word_right(&self) -> usize {
        let mut at = self.cursor;
        while at < self.bytes.len() && !self.bytes[at].is_ascii_whitespace() {
            at = self.next(at);
        }
        while at < self.bytes.len() && self.bytes[at].is_ascii_whitespace() {
            at = self.next(at);
        }
        at
    }
    fn pasted_byte(&mut self, ch: u8) {
        if ch == b'\r' {
            self.insert(b'\n');
            self.paste_cr = true;
        } else {
            if ch != b'\n' || !self.paste_cr {
                // Control bytes cannot execute actions or escape from the UI.
                if ch >= 32 || matches!(ch, b'\n' | b'\t') {
                    self.insert(ch);
                }
            }
            self.paste_cr = false;
        }
    }
    fn pasted(&mut self, ch: u8) {
        self.paste_end.push(ch);
        if self.paste_end == PASTE_END {
            self.paste = false;
            self.paste_end.clear();
            self.paste_cr = false;
            self.mark_paste(self.paste_start);
            return;
        }
        while !PASTE_END.starts_with(&self.paste_end) {
            let ch = self.paste_end.remove(0);
            self.pasted_byte(ch);
        }
    }
    fn submit(&self) -> Option<Action> {
        Some(if self.overflow {
            Action::Overflow
        } else {
            Action::Submit
        })
    }
    fn sequence(&mut self) -> Option<Action> {
        let action = match self.escape.as_slice() {
            b"\x1b[A" | b"\x1bOA" => Some(Action::HistoryOlder),
            b"\x1b[B" | b"\x1bOB" => Some(Action::HistoryNewer),
            b"\x1b[D" | b"\x1bOD" => {
                self.cursor = self.previous(self.cursor);
                None
            }
            b"\x1b[C" | b"\x1bOC" => {
                if self.complete_right {
                    self.escape.clear();
                    return Some(Action::Complete);
                }
                self.cursor = self.next(self.cursor);
                None
            }
            b"\x1b[1;5D" => {
                self.cursor = self.word_left();
                None
            }
            b"\x1b[1;5C" => {
                self.cursor = self.word_right();
                None
            }
            b"\x1b[H" | b"\x1bOH" | b"\x1b[1~" | b"\x1b[7~" => {
                self.cursor = self.line_start();
                None
            }
            b"\x1b[F" | b"\x1bOF" | b"\x1b[4~" | b"\x1b[8~" => {
                self.cursor = self.line_end();
                None
            }
            b"\x1b[3~" => {
                self.delete();
                None
            }
            b"\x1b[5~" => Some(Action::PageUp),
            b"\x1b[6~" => Some(Action::PageDown),
            b"\x1b[Z" => Some(Action::FocusPrevious),
            b"\x1b\r" | b"\x1b\n" | b"\x1b[13;3u" | b"\x1b[13;2u" | b"\x1b[27;2;13~"
            | b"\x1b[27;3;13~" => {
                self.insert(b'\n');
                None
            }
            b"\x1b[200~" => {
                self.paste = true;
                self.paste_start = self.cursor;
                None
            }
            value if value.starts_with(b"\x1b[<") => parse_mouse(value).map(Action::Mouse),
            _ => None,
        };
        self.escape.clear();
        action
    }
    pub fn feed(&mut self, ch: u8, terminal: bool) -> Option<Action> {
        if terminal {
            if self.alt_utf8 > 0 {
                if (0x80..=0xbf).contains(&ch) {
                    self.alt_utf8 -= 1;
                    return None;
                }
                self.alt_utf8 = 0;
            }
            if self.paste {
                self.pasted(ch);
                return None;
            }
            // Alt is an ESC-prefixed key, never a standalone Escape followed
            // by typing. In particular it must not navigate the conversation.
            if self.escape == b"\x1b" && !matches!(ch, b'[' | b'O' | b'\r' | b'\n' | 27) {
                self.escape.clear();
                match ch {
                    b'b' => self.cursor = self.word_left(),
                    b'f' => self.cursor = self.word_right(),
                    8 | 127 if !self.overflow => {
                        let begin = self.word_left();
                        self.erase(begin, self.cursor);
                        self.cursor = begin;
                    }
                    0xc2..=0xdf => self.alt_utf8 = 1,
                    0xe0..=0xef => self.alt_utf8 = 2,
                    0xf0..=0xf4 => self.alt_utf8 = 3,
                    _ => {}
                }
                return None;
            }
            if !self.escape.is_empty() || ch == 27 {
                if ch == 27 && !self.escape.is_empty() {
                    self.escape.clear();
                    self.escape.push(ch);
                    return Some(Action::Escape);
                }
                self.escape.push(ch);
                if self.escape.len() > 48 {
                    self.escape.clear();
                    return None;
                }
                let complete = self.escape.len() > 2 && (0x40..=0x7e).contains(&ch)
                    || self.escape.len() == 2 && matches!(ch, b'\r' | b'\n');
                return if complete { self.sequence() } else { None };
            }
            match ch {
                6 => return Some(Action::OpenFiles),
                20 => return Some(Action::OpenTerminals),
                15 => return Some(Action::ToggleTools),
                25 => return Some(Action::CopyHistory),
                9 => return Some(Action::FocusNext),
                4 if self.bytes.is_empty() => return Some(Action::Eof),
                4 => self.delete(),
                1 => self.cursor = self.line_start(),
                5 => self.cursor = self.line_end(),
                11 if !self.overflow => {
                    self.erase(self.cursor, self.line_end());
                }
                21 if !self.overflow => {
                    let begin = self.line_start();
                    self.erase(begin, self.cursor);
                    self.cursor = begin;
                }
                23 if !self.overflow => {
                    let begin = self.word_left();
                    self.erase(begin, self.cursor);
                    self.cursor = begin;
                }
                127 | 8 => self.backspace(),
                b'\r' => return self.submit(),
                b'\n' => self.insert(b'\n'),
                0..=31 => {}
                _ => self.insert(ch),
            }
        } else if ch == b'\n' {
            return self.submit();
        } else {
            self.insert(ch);
        }
        None
    }
}
fn parse_mouse(bytes: &[u8]) -> Option<MouseEvent> {
    let end = *bytes.last()?;
    if !matches!(end, b'M' | b'm') {
        return None;
    }
    let text = std::str::from_utf8(bytes.get(3..bytes.len() - 1)?).ok()?;
    let mut fields = text.split(';');
    let button: usize = fields.next()?.parse().ok()?;
    let column = fields.next()?.parse::<usize>().ok()?.checked_sub(1)?;
    let row = fields.next()?.parse::<usize>().ok()?.checked_sub(1)?;
    if fields.next().is_some() || column > 65534 || row > 65534 {
        return None;
    }
    let kind = match button {
        value if value & 64 != 0 && value & 3 == 0 => MouseKind::WheelUp,
        value if value & 64 != 0 && value & 3 == 1 => MouseKind::WheelDown,
        value if value & 3 == 0 && end == b'm' => MouseKind::Release,
        value if value & 3 == 0 && value & 32 != 0 => MouseKind::Drag,
        value if value & 3 == 0 => MouseKind::Press,
        _ => return None,
    };
    Some(MouseEvent { column, row, kind })
}

#[cfg(test)]
mod paste_display_tests {
    use super::*;

    #[test]
    fn long_bracketed_paste_projects_in_place_without_replacing_the_text() {
        let mut editor = Editor::default();
        assert!(editor.replace(0, 0, b"leftright"));
        editor.cursor = 4;
        let text = "я".repeat(1001);
        for byte in format!("\x1b[200~{text}\x1b[201~").bytes() {
            assert!(editor.feed(byte, true).is_none());
        }
        let (display, cursor) = editor.composer_view();
        let prefix = "left[pasted text · 1001 chars]";
        assert_eq!(display.as_ref(), format!("{prefix}right").as_bytes());
        assert_eq!(cursor, prefix.len());
        assert_eq!(editor.text().unwrap(), format!("left{text}right"));
        assert_eq!(editor.feed(b'\r', true), Some(Action::Submit));
        assert!(editor.replace(0, 4, b">"));
        assert_eq!(
            editor.composer_view().0.as_ref(),
            format!(">{}right", &prefix[4..]).as_bytes()
        );
        assert!(editor.replace(1, 3, b"x"));
        assert_eq!(editor.composer_view().0.as_ref(), editor.bytes);
    }

    #[test]
    fn clipboard_text_uses_character_and_line_thresholds() {
        let mut editor = Editor::default();
        assert!(editor.insert_clipboard_text(editor.cursor, &"я".repeat(1000)));
        assert!(editor.pasted_ranges.is_empty());
        editor.clear();
        let ten = ["a"; 10].join("\r\n");
        assert!(editor.insert_clipboard_text(editor.cursor, &ten));
        assert!(editor.pasted_ranges.is_empty());
        editor.clear();
        let eleven = ["a"; 11].join("\r\n");
        assert!(editor.insert_clipboard_text(editor.cursor, &eleven));
        assert_eq!(
            editor.composer_view().0.as_ref(),
            "[pasted text · 21 chars]".as_bytes()
        );
        assert_eq!(editor.text().unwrap(), ["a"; 11].join("\n"));
    }
}
