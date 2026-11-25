//! Bounded terminal editor. Paste contents never become terminal actions.
use crate::provider::contracts::ImageInput;

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
    pub(super) clipboard_anchor: Option<Caret>,
    pub(super) images: Vec<DraftImage>,
    pub(super) images_before: usize,
    paste_end: Vec<u8>,
    paste_cr: bool,
    pub overflow: bool,
    complete_right: bool,
    alt_utf8: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(super) struct Caret {
    byte: usize,
    // Images at the same byte boundary are distinct editing positions.
    images: usize,
}

#[derive(Clone, Debug)]
pub(super) struct DraftImage {
    at: usize,
    data: ImageInput,
}

impl Editor {
    pub(super) fn caret(&self) -> Caret {
        Caret {
            byte: self.cursor,
            images: self.images_before,
        }
    }

    fn caret_at(&self, byte: usize, after_images: bool) -> Caret {
        let images = self
            .images
            .partition_point(|image| image.at < byte || after_images && image.at == byte);
        Caret { byte, images }
    }

    fn place(&mut self, caret: Caret) {
        self.cursor = caret.byte;
        self.images_before = caret.images;
    }

    fn move_to(&mut self, byte: usize, after_images: bool) {
        self.place(self.caret_at(byte, after_images));
    }

    pub(super) fn attach_image(&mut self, at: Caret, image: ImageInput) {
        if self.caret() >= at {
            self.images_before += 1;
        }
        self.images.insert(
            at.images,
            DraftImage {
                at: at.byte,
                data: image,
            },
        );
    }

    pub(super) fn attached_images(&self) -> impl ExactSizeIterator<Item = &ImageInput> {
        self.images.iter().map(|image| &image.data)
    }

    pub(super) fn clear_images(&mut self) {
        self.images.clear();
        self.images_before = 0;
        if let Some(anchor) = &mut self.clipboard_anchor {
            anchor.images = 0;
        }
    }

    pub(super) fn is_empty(&self) -> bool {
        self.bytes.is_empty() && self.images.is_empty()
    }

    pub fn set_completion(&mut self, active: bool) {
        self.complete_right = active;
    }
    pub fn replace(&mut self, begin: usize, end: usize, value: &[u8]) -> bool {
        let start = if begin == self.cursor {
            self.caret()
        } else {
            self.caret_at(begin, true)
        };
        let finish = if begin == end {
            start
        } else {
            self.caret_at(end, false)
        };
        // Completion replaces a text span. Explicit image removal is handled
        // by the editor's deletion actions.
        start.images == finish.images && self.replace_text(start, finish, value)
    }

    fn replace_text(&mut self, start: Caret, finish: Caret, value: &[u8]) -> bool {
        let (begin, end) = (start.byte, finish.byte);
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
        self.adjust_edit(start, finish, value.len());
        self.bytes.splice(begin..end, value.iter().copied());
        self.place(Caret {
            byte: begin + value.len(),
            ..start
        });
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
        self.clear_images();
        self.paste_end.clear();
        self.paste_cr = false;
        self.complete_right = false;
        self.alt_utf8 = 0;
    }
    pub(super) fn clear_text(&mut self) {
        let mut images = std::mem::take(&mut self.images);
        self.clear();
        for image in &mut images {
            image.at = 0;
        }
        self.images_before = images.len();
        self.images = images;
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
    fn adjust_edit(&mut self, start: Caret, finish: Caret, replacement: usize) {
        if let Some(anchor) = &mut self.clipboard_anchor
            && start < *anchor
        {
            anchor.byte = start.byte + replacement + anchor.byte.saturating_sub(finish.byte);
            anchor.images = start.images + anchor.images.saturating_sub(finish.images);
        }
        self.images.drain(start.images..finish.images);
        for image in &mut self.images[start.images..] {
            image.at = start.byte + replacement + image.at.saturating_sub(finish.byte);
        }
        if start.byte != finish.byte || replacement != 0 {
            self.adjust_pasted_ranges(start.byte, finish.byte, replacement);
        }
    }

    fn adjust_pasted_ranges(&mut self, begin: usize, end: usize, replacement: usize) {
        let first = self.pasted_ranges.partition_point(|r| r.end <= begin);
        let after = first + self.pasted_ranges[first..].partition_point(|r| r.start < end);
        self.pasted_ranges.drain(first..after);
        for range in &mut self.pasted_ranges[first..] {
            range.start = range.start - (end - begin) + replacement;
            range.end = range.end - (end - begin) + replacement;
        }
    }
    fn erase(&mut self, start: Caret, finish: Caret) {
        if start != finish {
            self.adjust_edit(start, finish, 0);
            self.bytes.drain(start.byte..finish.byte);
            self.place(start);
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
    pub(super) fn insert_clipboard_text(&mut self, start: Caret, text: &str) -> bool {
        let clean: String = text
            .replace("\r\n", "\n")
            .replace('\r', "\n")
            .chars()
            .filter(|&ch| !ch.is_control() || matches!(ch, '\n' | '\t'))
            .collect();
        let mut caret = self.caret();
        if !self.replace_text(start, start, clean.as_bytes()) {
            return false;
        }
        self.mark_paste(start.byte);
        if caret >= start {
            caret.byte += clean.len();
        }
        self.place(caret);
        true
    }
    /// Only presentation changes. Moving inside a paste reveals it for editing.
    pub(super) fn composer_view(&self) -> (std::borrow::Cow<'_, [u8]>, usize) {
        if self.pasted_ranges.is_empty() && self.images.is_empty() {
            return (std::borrow::Cow::Borrowed(&self.bytes), self.cursor);
        }
        let mut previews = Vec::new();
        for range in &self.pasted_ranges {
            let first_inside = self.images.partition_point(|image| image.at <= range.start);
            if range.start < self.cursor && self.cursor < range.end
                || self
                    .images
                    .get(first_inside)
                    .is_some_and(|image| image.at < range.end)
            {
                continue;
            }
            let Some(text) = self
                .bytes
                .get(range.clone())
                .and_then(|bytes| std::str::from_utf8(bytes).ok())
            else {
                continue;
            };
            let marker = format!("[pasted text · {} chars]", text.chars().count());
            previews.push((range.start, range.end, marker, range.end <= self.cursor));
        }
        for (index, image) in self.images.iter().enumerate() {
            let mut marker = format!("[Image #{}]", index + 1);
            if self.bytes.get(image.at) == Some(&b'@')
                && std::str::from_utf8(&self.bytes[..image.at])
                    .is_ok_and(|prefix| prefix.is_empty() || prefix.ends_with(char::is_whitespace))
            {
                marker.push(' ');
            }
            previews.push((image.at, image.at, marker, index < self.images_before));
        }
        previews.sort_by_key(|(start, end, _, _)| (*start, *end));
        let mut display = Vec::new();
        let mut cursor = self.cursor;
        let mut after = 0;
        for (start, end, marker, before_cursor) in previews {
            display.extend_from_slice(&self.bytes[after..start]);
            display.extend_from_slice(marker.as_bytes());
            if before_cursor {
                cursor = cursor - (end - start) + marker.len();
            }
            after = end;
        }
        display.extend_from_slice(&self.bytes[after..]);
        (std::borrow::Cow::Owned(display), cursor)
    }
    fn insert(&mut self, ch: u8) {
        if !self.overflow && self.bytes.len() < MAX_COMPOSER_BYTES {
            self.cursor = self.cursor.min(self.bytes.len());
            let at = self.caret();
            self.adjust_edit(at, at, 1);
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
        if !self.overflow {
            self.erase(self.previous_caret(), self.caret());
        }
    }
    fn delete(&mut self) {
        if !self.overflow {
            self.erase(self.caret(), self.next_caret());
        }
    }
    fn previous_caret(&self) -> Caret {
        let at = self.caret();
        if at.images > 0 && self.images[at.images - 1].at == at.byte {
            Caret {
                images: at.images - 1,
                ..at
            }
        } else if at.byte > 0 {
            self.caret_at(self.previous(at.byte), true)
        } else {
            at
        }
    }

    fn next_caret(&self) -> Caret {
        let at = self.caret();
        if self
            .images
            .get(at.images)
            .is_some_and(|image| image.at == at.byte)
        {
            Caret {
                images: at.images + 1,
                ..at
            }
        } else if at.byte < self.bytes.len() {
            self.caret_at(self.next(at.byte), false)
        } else {
            at
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
                self.place(self.previous_caret());
                None
            }
            b"\x1b[C" | b"\x1bOC" => {
                if self.complete_right {
                    self.escape.clear();
                    return Some(Action::Complete);
                }
                self.place(self.next_caret());
                None
            }
            b"\x1b[1;5D" => {
                self.move_to(self.word_left(), false);
                None
            }
            b"\x1b[1;5C" => {
                self.move_to(self.word_right(), true);
                None
            }
            b"\x1b[H" | b"\x1bOH" | b"\x1b[1~" | b"\x1b[7~" => {
                self.move_to(self.line_start(), false);
                None
            }
            b"\x1b[F" | b"\x1bOF" | b"\x1b[4~" | b"\x1b[8~" => {
                self.move_to(self.line_end(), true);
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
                    b'b' => self.move_to(self.word_left(), false),
                    b'f' => self.move_to(self.word_right(), true),
                    8 | 127 if !self.overflow => {
                        self.erase(self.caret_at(self.word_left(), false), self.caret());
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
                4 if self.is_empty() => return Some(Action::Eof),
                4 => self.delete(),
                1 => self.move_to(self.line_start(), false),
                5 => self.move_to(self.line_end(), true),
                11 if !self.overflow => {
                    self.erase(self.caret(), self.caret_at(self.line_end(), true));
                }
                21 if !self.overflow => {
                    self.erase(self.caret_at(self.line_start(), false), self.caret());
                }
                23 if !self.overflow => {
                    self.erase(self.caret_at(self.word_left(), false), self.caret());
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
        assert!(editor.insert_clipboard_text(editor.caret(), &"я".repeat(1000)));
        assert!(editor.pasted_ranges.is_empty());
        editor.clear();
        let ten = ["a"; 10].join("\r\n");
        assert!(editor.insert_clipboard_text(editor.caret(), &ten));
        assert!(editor.pasted_ranges.is_empty());
        editor.clear();
        let eleven = ["a"; 11].join("\r\n");
        assert!(editor.insert_clipboard_text(editor.caret(), &eleven));
        assert_eq!(
            editor.composer_view().0.as_ref(),
            "[pasted text · 21 chars]".as_bytes()
        );
        assert_eq!(editor.text().unwrap(), ["a"; 11].join("\n"));
    }
}
