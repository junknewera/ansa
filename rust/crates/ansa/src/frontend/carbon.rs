//! Carbon layout in the main screen buffer. All untrusted bytes are sanitized.
use super::cells as display_cells;
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub role: String,
    pub text: String,
    pub action: Option<String>,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    pub lines: Vec<String>,
    pub cursor: usize,
    pub scroll_limit: usize,
    pub tool_actions: Vec<(usize, String)>,
    /// Chat row range and right column, all zero based and exclusive at end.
    pub chat_bounds: Option<(usize, usize, usize)>,
    pub inspector_content_rows: usize,
    viewport: Option<(usize, usize)>,
    cursor_row: usize,
    status_rows: Vec<usize>,
    panes: Vec<Option<PaneRow>>,
}
#[derive(Debug, Clone, PartialEq, Eq)]
struct PaneRow {
    column: usize,
    left: String,
    right: String,
}
impl PaneRow {
    fn line(&self) -> String {
        format!("{}{}", self.left, self.right)
    }
    fn matches(&self, line: &str) -> bool {
        line.strip_prefix(self.left.as_str()) == Some(self.right.as_str())
    }
}
impl Frame {
    fn pane(&self, row: usize) -> Option<&PaneRow> {
        self.panes
            .get(row)?
            .as_ref()
            .filter(|pane| self.lines.get(row).is_some_and(|line| pane.matches(line)))
    }
    fn paint_row(&self, row: usize, output: &mut String) {
        if let Some(pane) = self.pane(row) {
            output.push_str(&pane.left);
            // A terminal can compose multiple emoji scalars into one glyph.
            // The adjoining pane has a fixed column independent of that width.
            output.push('\r');
            if pane.column > 0 {
                output.push_str(&format!("\x1b[{}C", pane.column));
            }
            if pane.left.contains('\x1b') {
                output.push_str("\x1b[0m");
            }
            output.push_str(&pane.right);
        } else {
            output.push_str(&self.lines[row]);
        }
    }
    /// Status belongs to the live runtime; the selected text remains frozen.
    pub fn update_status_from(&mut self, live: &Self) {
        if self.viewport == live.viewport && self.lines.len() == live.lines.len() {
            for &row in &live.status_rows {
                if let (Some(target), Some(source)) = (self.lines.get_mut(row), live.lines.get(row))
                {
                    target.clone_from(source);
                }
            }
            self.status_rows.clone_from(&live.status_rows);
        }
    }
    pub fn paint(&self, previous: usize) -> String {
        if self.viewport.is_some() {
            let mut out = "\x1b[H\x1b[2J".to_owned();
            for i in 0..self.lines.len() {
                if i > 0 {
                    out.push_str("\r\n");
                }
                self.paint_row(i, &mut out);
            }
            let up = self.lines.len().saturating_sub(self.cursor_row + 1);
            if up > 0 {
                out.push_str(&format!("\x1b[{up}A"));
            }
            out.push('\r');
            if self.cursor > 0 {
                out.push_str(&format!("\x1b[{}C", self.cursor));
            }
            return out;
        }
        let rows = self.lines.len();
        let mut out = if previous == 0 {
            format!("{}\r\x1b[{}A", "\n".repeat(rows - 1), rows - 1)
        } else {
            format!("\r\x1b[{}A", previous.saturating_sub(2))
        };
        for (i, line) in self.lines.iter().enumerate() {
            out.push_str("\x1b[2K");
            out.push_str(line);
            if i + 1 < rows {
                out.push_str("\r\n");
            }
        }
        out.push_str(&format!("\x1b[1A\r\x1b[{}C", self.cursor));
        if previous > rows {
            out.push_str("\x1b7\x1b[1B");
            for _ in rows..previous {
                out.push_str("\r\n\x1b[2K");
            }
            out.push_str("\x1b8");
        }
        out
    }
    pub fn paint_changes(&self, previous: &Self) -> String {
        if self.viewport.is_none()
            || self.viewport != previous.viewport
            || self.lines.len() != previous.lines.len()
        {
            return self.paint(previous.lines.len());
        }
        let mut out = String::from("\r");
        if previous.cursor_row > 0 {
            out.push_str(&format!("\x1b[{}A", previous.cursor_row));
        }
        let mut row = 0;
        for (index, line) in self.lines.iter().enumerate() {
            let old_pane = previous.pane(index);
            let new_pane = self.pane(index);
            if previous.lines[index] == *line && old_pane == new_pane {
                continue;
            }
            let segment = match (old_pane, new_pane) {
                (Some(before), Some(after))
                    if before.column == after.column && before.left == after.left =>
                {
                    Some((before.right.as_str(), after.right.as_str(), after.column))
                }
                (None, None) => Some((previous.lines[index].as_str(), line.as_str(), 0)),
                _ => None,
            };
            let Some((old_segment, new_segment, base_column)) = segment else {
                if index > row {
                    out.push_str(&format!("\x1b[{}B", index - row));
                }
                let colored = line.contains('\x1b') || previous.lines[index].contains('\x1b');
                out.push('\r');
                if colored {
                    out.push_str("\x1b[0m");
                }
                out.push_str("\x1b[K");
                self.paint_row(index, &mut out);
                if colored {
                    out.push_str("\x1b[0m");
                }
                row = index;
                continue;
            };
            let before = paint_glyphs(old_segment);
            let after = paint_glyphs(new_segment);
            let equal = before
                .iter()
                .zip(&after)
                .take_while(|(before, after)| before == after)
                .count();
            if equal == before.len() && equal == after.len() {
                continue;
            }
            let column: usize = base_column
                + after[..equal]
                    .iter()
                    .map(|glyph| glyph.width)
                    .sum::<usize>();
            if index > row {
                out.push_str(&format!("\x1b[{}B", index - row));
            }
            out.push('\r');
            if column > 0 {
                out.push_str(&format!("\x1b[{column}C"));
            }
            let colored = new_segment.contains('\x1b') || old_segment.contains('\x1b');
            if colored {
                out.push_str("\x1b[0m");
            }
            // Clear only the changed suffix. In a split workspace the chat
            // occupies the common prefix and must stay on screen untouched.
            out.push_str("\x1b[K");
            let mut style = "";
            for glyph in &after[equal..] {
                if style != glyph.style.as_ref() {
                    out.push_str("\x1b[0m");
                    out.push_str(&glyph.style);
                }
                out.push_str(&glyph.text);
                style = &glyph.ending_style;
            }
            if colored {
                out.push_str("\x1b[0m");
            }
            row = index;
        }
        let target = self.cursor_row;
        if row > target {
            out.push_str(&format!("\x1b[{}A", row - target));
        }
        if row < target {
            out.push_str(&format!("\x1b[{}B", target - row));
        }
        out.push('\r');
        if self.cursor > 0 {
            out.push_str(&format!("\x1b[{}C", self.cursor));
        }
        out
    }
}
/// A printable base and its combining marks form one repaint boundary. Styles
/// are retained so a color change is visible even when the text is unchanged.
#[derive(PartialEq, Eq)]
struct PaintGlyph {
    text: String,
    width: usize,
    style: std::sync::Arc<str>,
    ending_style: std::sync::Arc<str>,
}
fn paint_glyphs(line: &str) -> Vec<PaintGlyph> {
    struct Scalar {
        visible_at: usize,
        raw_at: usize,
        raw_end: usize,
        style: std::sync::Arc<str>,
    }
    let mut visible = String::new();
    let mut scalars = Vec::new();
    let mut style: std::sync::Arc<str> = "".into();
    let mut at = 0;
    while at < line.len() {
        let remaining = &line[at..];
        if let Some(sgr) = remaining.strip_prefix("\x1b[")
            && let Some(end) = sgr.find('m')
            && sgr[..end]
                .bytes()
                .all(|byte| byte.is_ascii_digit() || matches!(byte, b';' | b':'))
        {
            let sequence = &remaining[..end + 3];
            style = if sequence == "\x1b[0m" || sequence == "\x1b[m" {
                "".into()
            } else {
                format!("{style}{sequence}").into()
            };
            at += sequence.len();
            continue;
        }
        let cp = remaining.chars().next().expect("char boundary");
        scalars.push(Scalar {
            visible_at: visible.len(),
            raw_at: at,
            raw_end: at + cp.len_utf8(),
            style: style.clone(),
        });
        visible.push(cp);
        at += cp.len_utf8();
    }
    let mut result = Vec::new();
    let mut scalar = 0;
    let mut end = 0;
    for cluster in display_cells::graphemes(&visible) {
        end += cluster.len();
        let first = &scalars[scalar];
        while scalar + 1 < scalars.len() && scalars[scalar + 1].visible_at < end {
            scalar += 1;
        }
        let last = &scalars[scalar];
        result.push(PaintGlyph {
            text: line[first.raw_at..last.raw_end].into(),
            width: display_cells::width(cluster),
            style: first.style.clone(),
            ending_style: last.style.clone(),
        });
        scalar += 1;
    }
    result
}
#[derive(Clone)]
struct Glyph {
    text: String,
    width: usize,
    newline: bool,
    source_end: usize,
}
fn glyphs(bytes: &[u8]) -> Vec<Glyph> {
    let mut safe = String::new();
    let mut offsets = Vec::new();
    let mut at = 0;
    while at < bytes.len() {
        let ch = bytes[at];
        if ch < 32 || ch == 127 {
            safe.push_str(match ch {
                b'\n' => "\n",
                b'\t' => " ",
                _ => "?",
            });
            at += 1;
            offsets.push((safe.len(), at));
            continue;
        }
        let n = match ch {
            0..=0x7f => 1,
            0xc2..=0xdf => 2,
            0xe0..=0xef => 3,
            0xf0..=0xf4 => 4,
            _ => 0,
        };
        let cp = if n > 0 && at + n <= bytes.len() {
            std::str::from_utf8(&bytes[at..at + n])
                .ok()
                .and_then(|s| s.chars().next())
        } else {
            None
        };
        if let Some(cp) = cp {
            safe.push(if matches!(cp as u32, 0x80..=0x9f) {
                '?'
            } else {
                cp
            });
            at += n;
        } else {
            safe.push('?');
            at += 1;
        }
        offsets.push((safe.len(), at));
    }
    let mut result = Vec::new();
    let mut end = 0;
    let mut offset = 0;
    for cluster in display_cells::graphemes(&safe) {
        end += cluster.len();
        while offsets[offset].0 < end {
            offset += 1;
        }
        let newline = cluster == "\n";
        result.push(Glyph {
            text: cluster.into(),
            width: if newline {
                0
            } else {
                display_cells::width(cluster)
            },
            newline,
            source_end: offsets[offset].1,
        });
    }
    result
}
fn compact_input_glyphs(bytes: &[u8]) -> Vec<Glyph> {
    let mut result = glyphs(bytes);
    for glyph in &mut result {
        if glyph.newline {
            glyph.text = "↵".into();
            glyph.width = 1;
            glyph.newline = false;
        }
    }
    result
}
#[derive(Clone)]
struct Styled {
    glyph: Glyph,
    bold: bool,
    code: bool,
    ink: &'static str,
}
#[allow(clippy::too_many_arguments)]
pub fn layout(
    history: &[Entry],
    input: &[u8],
    cursor: usize,
    status: &str,
    expanded: bool,
    scroll: usize,
    context: &str,
    size: (usize, usize),
    color: bool,
) -> Option<Frame> {
    layout_impl(
        history, input, cursor, status, expanded, scroll, context, size, color, false,
    )
}
#[allow(clippy::too_many_arguments)]
pub fn layout_full(
    history: &[Entry],
    input: &[u8],
    cursor: usize,
    status: &str,
    scroll: usize,
    context: &str,
    size: (usize, usize),
    color: bool,
) -> Option<Frame> {
    layout_impl(
        history, input, cursor, status, true, scroll, context, size, color, true,
    )
}
#[allow(clippy::too_many_arguments)]
fn layout_impl(
    history: &[Entry],
    input: &[u8],
    cursor: usize,
    status: &str,
    _expanded: bool,
    scroll: usize,
    context: &str,
    size: (usize, usize),
    color: bool,
    full: bool,
) -> Option<Frame> {
    let (cols, rows) = size;
    if cols < 5 || rows < 5 {
        return None;
    }
    let max_rows = if full { rows } else { (rows - 1).clamp(4, 30) };
    let cols = cols - 1;
    let mint = if color { "\x1b[38;2;133;220;177m" } else { "" };
    let reset = if color { "\x1b[0m" } else { "" };
    let mut wrapped = vec![];
    let mut tool_actions = Vec::new();
    let mut previous_tool = false;
    for entry in history {
        let tool = entry.role.starts_with("tool");
        if !wrapped.is_empty() && !(tool && previous_tool) {
            wrapped.push(String::new());
        }
        previous_tool = tool;
        if let Some(action) = &entry.action {
            tool_actions.push((wrapped.len(), action.clone()));
        }
        let safe: String = glyphs(entry.text.as_bytes())
            .iter()
            .map(|v| v.text.as_str())
            .collect();
        let assistant = entry.role == "assistant";
        let user = entry.role == "user";
        let mut fence = false;
        let mut first = true;
        for text in safe.split('\n') {
            if assistant && text.starts_with("```") {
                fence = !fence;
                continue;
            }
            let mut text = text;
            let mut heading = false;
            if assistant && !fence && text.starts_with('#') {
                let trimmed = text.trim_start_matches('#');
                if let Some(rest) = trimmed.strip_prefix(' ') {
                    text = rest;
                    heading = true;
                }
            }
            let paint = TranscriptPaint::new(entry, text);
            let mut chars = vec![];
            if user && first {
                for text in [if color { "›" } else { ">" }, " "] {
                    chars.push(Styled {
                        glyph: Glyph {
                            text: text.into(),
                            width: 1,
                            newline: false,
                            source_end: 0,
                        },
                        bold: true,
                        code: true,
                        ink: LAVENDER,
                    });
                }
            }
            let mut bold = false;
            let mut code = false;
            let mut at = 0;
            let mut visible = String::new();
            let mut styles = Vec::new();
            while at < text.len() {
                if assistant
                    && !fence
                    && text[at..].starts_with("**")
                    && (bold || text[at + 2..].contains("**"))
                {
                    bold = !bold;
                    at += 2;
                    continue;
                }
                if assistant
                    && !fence
                    && text.as_bytes()[at] == b'`'
                    && (code || text[at + 1..].contains('`'))
                {
                    code = !code;
                    at += 1;
                    continue;
                }
                let cp = text[at..].chars().next().expect("char boundary");
                styles.push((visible.len(), bold || heading || user, code));
                visible.push(cp);
                at += cp.len_utf8();
            }
            let mut scalar = 0;
            for glyph in glyphs(visible.as_bytes()) {
                let (offset, bold, code) = styles[scalar];
                while scalar < styles.len() && styles[scalar].0 < glyph.source_end {
                    scalar += 1;
                }
                let ink = paint.ink(offset, code || fence, heading);
                chars.push(Styled {
                    glyph,
                    bold,
                    code,
                    ink,
                });
            }
            let mut pos = 0;
            loop {
                let mut end = pos;
                let mut last_space = chars.len();
                let mut used = 0;
                while end < chars.len() && used + chars[end].glyph.width <= cols {
                    if chars[end].glyph.text == " " && end > pos {
                        last_space = end;
                    }
                    used += chars[end].glyph.width;
                    end += 1;
                }
                if end < chars.len() && last_space < end {
                    end = last_space;
                }
                if end == pos && end < chars.len() {
                    end += 1;
                }
                let mut line = if color && user {
                    USER_SURFACE.to_owned()
                } else {
                    String::new()
                };
                let mut was = (false, false, "");
                for (index, ch) in chars[pos..end].iter().enumerate() {
                    if color && (ch.bold, ch.code, ch.ink) != was {
                        line.push_str(reset);
                        if user {
                            line.push_str(USER_SURFACE);
                        }
                        if ch.bold {
                            line.push_str("\x1b[1m");
                        }
                        line.push_str(ch.ink);
                        was = (ch.bold, ch.code, ch.ink);
                    }
                    // Disclosure and detail guides retain the same cell width.
                    let glyph = if color && tool && entry.action.is_some() && pos == 0 && index == 0
                    {
                        match ch.glyph.text.as_str() {
                            ">" => "▸",
                            "v" => "▾",
                            _ => &ch.glyph.text,
                        }
                    } else if color
                        && entry.role == "tool_detail"
                        && pos == 0
                        && index == 0
                        && ch.glyph.text == " "
                    {
                        "│"
                    } else {
                        &ch.glyph.text
                    };
                    line.push_str(glyph);
                }
                if color {
                    if user {
                        line.push_str(&" ".repeat(cols.saturating_sub(cells(&line))));
                    }
                    line.push_str(reset);
                }
                wrapped.push(line);
                pos = end;
                if pos < chars.len() && chars[pos].glyph.text == " " {
                    pos += 1;
                }
                if pos >= chars.len() {
                    break;
                }
            }
            first = false;
        }
    }
    let available = max_rows - if full { 5 } else { 3 };
    let end = wrapped.len() - scroll.min(wrapped.len().saturating_sub(available));
    let begin = end.saturating_sub(available);
    let mut header = format!("{mint}{}ansa{reset}", if color { "\x1b[1m" } else { "" });
    let mut used = 4;
    for glyph in glyphs(context.as_bytes()) {
        if glyph.newline || used + glyph.width + 2 > cols {
            break;
        }
        if used == 4 {
            header.push_str("  ");
            used += 2;
        }
        header.push_str(&glyph.text);
        used += glyph.width;
    }
    let mut lines = vec![header];
    lines.extend_from_slice(&wrapped[begin..end]);
    let mut status_lines = status.splitn(3, '\n');
    let status = if full {
        status_lines.next().unwrap_or("")
    } else {
        status
    };
    if full {
        lines.resize(max_rows - 4, String::new());
        for _ in 0..2 {
            let mut line = String::new();
            let mut used = 0;
            for glyph in glyphs(status_lines.next().unwrap_or("").as_bytes()) {
                if glyph.newline || used + glyph.width > cols {
                    break;
                }
                line.push_str(&glyph.text);
                used += glyph.width;
            }
            lines.push(line);
        }
    }
    let cursor = cursor.min(input.len());
    let input_glyphs = compact_input_glyphs(input);
    let split = input_glyphs.partition_point(|glyph| glyph.source_end <= cursor);
    let (before, after) = input_glyphs.split_at(split);
    let mut before_width: usize = before.iter().map(|v| v.width).sum();
    let mut first_visible = 0;
    while first_visible < before.len() && before_width > cols - 3 {
        before_width -= before[first_visible].width;
        first_visible += 1;
    }
    let mut editor = format!(
        "{}{mint}> {}",
        if color { "\x1b[48;2;42;46;48m" } else { "" },
        if color { "\x1b[39m" } else { "" }
    );
    let mut used = 2;
    for glyph in &before[first_visible..] {
        editor.push_str(&glyph.text);
        used += glyph.width;
    }
    for glyph in after {
        if used + glyph.width > cols {
            break;
        }
        editor.push_str(&glyph.text);
        used += glyph.width;
    }
    if color {
        editor.push_str(&" ".repeat(cols.saturating_sub(used)));
        editor.push_str(reset);
    }
    lines.push(editor);
    let mut clipped = String::new();
    let mut used = 0;
    let text = format!("{status}  / команды  Tab фокус  ? помощь");
    for glyph in glyphs(text.as_bytes()) {
        if glyph.newline || used + glyph.width > cols {
            break;
        }
        clipped.push_str(&glyph.text);
        used += glyph.width;
    }
    lines.push(format!("{mint}{clipped}{reset}"));
    let cursor_row = lines.len() - 2;
    Some(Frame {
        lines,
        cursor: 2 + before_width,
        scroll_limit: wrapped.len().saturating_sub(available),
        tool_actions: tool_actions
            .into_iter()
            .filter_map(|(row, key)| {
                (row >= begin && row < end).then_some((row.saturating_sub(begin) + 1, key))
            })
            .collect(),
        chat_bounds: Some((1, available + 1, cols)),
        inspector_content_rows: 0,
        viewport: full.then_some(size),
        cursor_row,
        status_rows: if full {
            vec![max_rows - 4, max_rows - 3, max_rows - 1]
        } else {
            vec![cursor_row + 1]
        },
        panes: Vec::new(),
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Focus {
    #[default]
    Chat,
    Inspector,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum InspectorKind {
    #[default]
    Plain,
    Agents,
    Source,
    Diff,
    Markdown,
    Command,
}
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Inspector {
    pub title: String,
    pub tabs: Vec<String>,
    pub selected_tab: usize,
    pub lines: Vec<String>,
    /// Index in the complete lines collection, before viewport scrolling.
    pub selected: Option<usize>,
    pub offset: usize,
    pub footer: String,
    pub kind: InspectorKind,
    pub markdown: Option<std::sync::Arc<Vec<super::markdown::Row>>>,
}
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Choice {
    pub label: String,
    pub detail: String,
}
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Popup {
    pub title: String,
    pub subtitle: String,
    pub items: Vec<Choice>,
    pub selected: usize,
    pub footer: String,
}
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Workspace {
    pub breadcrumb: String,
    pub model: String,
    pub effort: String,
    pub activity: String,
    pub reading: bool,
    pub new_events: usize,
    pub focus: Focus,
    pub inspector: Option<Inspector>,
    pub popup: Option<Popup>,
}
const MINT: &str = "\x1b[38;2;133;220;177m";
const TEXT: &str = "\x1b[38;2;215;223;235m";
const MUTED: &str = "\x1b[38;2;145;157;177m";
const SKY: &str = "\x1b[38;2;125;183;245m";
const LAVENDER: &str = "\x1b[38;2;188;166;242m";
const AMBER: &str = "\x1b[38;2;232;189;117m";
const RED: &str = "\x1b[38;2;241;142;150m";
const RULE: &str = "\x1b[38;2;62;74;93m";
const USER_SURFACE: &str = "\x1b[48;2;28;40;57m";
const INPUT_SURFACE: &str = "\x1b[48;2;34;41;54m";
const RESET: &str = "\x1b[0m";
const SELECTED: &str = "\x1b[48;2;46;63;88m\x1b[38;2;232;240;255m";

fn tool_ink(role: &str) -> &'static str {
    match role {
        "tool_success" => MINT,
        "tool_error" => RED,
        "tool_warning" | "tool_running" => AMBER,
        _ => MUTED,
    }
}
struct TranscriptPaint {
    base: &'static str,
    header: &'static str,
    fields: Vec<usize>,
    group: bool,
    tool: bool,
}
impl TranscriptPaint {
    fn new(entry: &Entry, text: &str) -> Self {
        let tool = entry.role.starts_with("tool") && entry.role != "tool_detail";
        let group = entry
            .action
            .as_deref()
            .is_some_and(|key| key.starts_with("group:"));
        Self {
            base: if entry.role == "tool_detail" {
                if text.starts_with("    ") { SKY } else { MUTED }
            } else if tool {
                tool_ink(&entry.role)
            } else if entry.role == "reasoning" {
                MUTED
            } else {
                TEXT
            },
            header: if group { LAVENDER } else { SKY },
            fields: if tool {
                text.match_indices(" · ").map(|(at, _)| at).collect()
            } else {
                Vec::new()
            },
            group,
            tool,
        }
    }
    fn ink(&self, at: usize, code: bool, heading: bool) -> &'static str {
        if self.tool {
            if at < 2 || self.fields.is_empty() {
                return self.base;
            }
            let field = self.fields.partition_point(|offset| *offset <= at);
            return match (self.group, field) {
                (_, 0) => self.header,
                (true, 1) => MUTED,
                (true, 2) | (false, 1) => self.base,
                (true, _) => SKY,
                _ => MUTED,
            };
        }
        if heading {
            SKY
        } else if code {
            AMBER
        } else {
            self.base
        }
    }
}

fn styled(text: &str, style: &str, color: bool) -> String {
    if color {
        format!("{style}{text}{RESET}")
    } else {
        text.into()
    }
}
/// Rendered rows only contain our SGR sequences; source controls have already
/// been replaced by `glyphs`. This also keeps clipboard contents plain text.
fn plain(text: &str) -> String {
    let mut out = String::new();
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\x1b' && chars.peek() == Some(&'[') {
            chars.next();
            for ch in chars.by_ref() {
                if ('@'..='~').contains(&ch) {
                    break;
                }
            }
        } else {
            out.push(ch);
        }
    }
    out
}
fn cells(text: &str) -> usize {
    glyphs(plain(text).as_bytes()).iter().map(|g| g.width).sum()
}
fn clipped(text: &str, columns: usize) -> String {
    let mut result = String::new();
    let mut used = 0;
    for glyph in glyphs(text.as_bytes()) {
        if glyph.newline || used + glyph.width > columns {
            break;
        }
        result.push_str(&glyph.text);
        used += glyph.width;
    }
    result
}
fn padded(text: &str, columns: usize) -> String {
    format!("{text}{}", " ".repeat(columns.saturating_sub(cells(text))))
}
fn heading(left: &str, right: &str, columns: usize, color: bool) -> String {
    let right = clipped(right, columns / 2);
    let left = clipped(left, columns.saturating_sub(cells(&right) + 2));
    format!(
        "{}{}{}",
        if let Some(rest) = left.strip_prefix("ANSA") {
            format!(
                "{}{}",
                styled("ANSA", "\x1b[1m\x1b[38;2;133;220;177m", color),
                styled(rest, MUTED, color)
            )
        } else {
            styled(&left, SKY, color)
        },
        " ".repeat(columns.saturating_sub(cells(&left) + cells(&right))),
        styled(&right, MUTED, color)
    )
}
fn rule(columns: usize, color: bool) -> String {
    styled(&"─".repeat(columns), RULE, color)
}
fn editor_rows(
    input: &[u8],
    cursor: usize,
    columns: usize,
    maximum: usize,
    color: bool,
) -> (Vec<String>, usize, usize) {
    let cursor = cursor.min(input.len());
    let mut logical = vec![String::new()];
    let mut row = 0;
    let mut used = 2;
    let mut cursor_at = (0, 2);
    let mut consumed = 0;
    for glyph in glyphs(input) {
        let contains_cursor = consumed <= cursor && cursor < glyph.source_end;
        if contains_cursor {
            cursor_at = (row, used);
        }
        if glyph.newline {
            logical.push(String::new());
            row += 1;
            used = 2;
        } else {
            if used + glyph.width > columns && used > 2 {
                logical.push(String::new());
                row += 1;
                used = 2;
                if contains_cursor {
                    cursor_at = (row, used);
                }
            }
            logical[row].push_str(&glyph.text);
            used += glyph.width;
        }
        // Cursor byte offsets refer to the original input, including controls
        // represented as text and invalid bytes represented by one '?' each.
        consumed = glyph.source_end;
    }
    if cursor == input.len() {
        cursor_at = (row, used);
    }
    let visible = maximum.max(1).min(logical.len());
    let first = cursor_at.0.saturating_sub(visible - 1);
    let mut lines = Vec::with_capacity(visible);
    for (index, text) in logical.iter().enumerate().skip(first).take(visible) {
        let prefix = if index == 0 { "› " } else { "  " };
        lines.push(if color {
            format!(
                "{}{}",
                styled(prefix, &format!("{INPUT_SURFACE}{LAVENDER}\x1b[1m"), true),
                styled(
                    &padded(text, columns.saturating_sub(2)),
                    &format!("{INPUT_SURFACE}{TEXT}"),
                    true
                )
            )
        } else {
            format!("{prefix}{text}")
        });
    }
    (
        lines,
        cursor_at.0 - first,
        cursor_at.1.min(columns.saturating_sub(1)),
    )
}
fn inspector_rows(
    panel: &Inspector,
    width: usize,
    height: usize,
    color: bool,
    focused: bool,
) -> Vec<String> {
    let mut rows = vec![styled(
        &clipped(&panel.title, width),
        if focused { MINT } else { MUTED },
        color,
    )];
    if !panel.tabs.is_empty() && height > 2 {
        let mut used = 0;
        let mut tabs = String::new();
        for (i, tab) in panel.tabs.iter().enumerate() {
            let gap = if i != 0 { 2 } else { 0 };
            let safe = clipped(tab, width.saturating_sub(used + gap));
            if safe.is_empty() {
                break;
            }
            if gap > 0 {
                tabs.push_str("  ");
                used += 2;
            }
            tabs.push_str(&styled(
                &safe,
                if i == panel.selected_tab {
                    "\x1b[1;4m\x1b[38;2;125;183;245m"
                } else {
                    MUTED
                },
                color,
            ));
            used += cells(&safe);
        }
        rows.push(tabs);
    }
    if height > 3 {
        rows.push(rule(width, color));
    }
    let footer = usize::from(!panel.footer.is_empty());
    let available = height.saturating_sub(rows.len() + footer);
    if let Some(markdown) = &panel.markdown {
        for line in markdown.iter().skip(panel.offset).take(available) {
            let mut rendered = String::new();
            for span in &line.spans {
                let mut style = String::new();
                if span.style.heading > 0 || span.style.bold {
                    style.push_str("\x1b[1m");
                }
                if span.style.heading > 0 {
                    style.push_str(SKY);
                } else if span.style.code {
                    style.push_str(AMBER);
                }
                if span.style.italic {
                    style.push_str("\x1b[3m");
                }
                if span.style.link {
                    style.push_str(SKY);
                    style.push_str("\x1b[4m");
                }
                if span.style.strike {
                    style.push_str("\x1b[9m");
                }
                rendered.push_str(&styled(&span.text, &style, color));
            }
            rows.push(rendered);
        }
    } else {
        for (index, text) in panel
            .lines
            .iter()
            .enumerate()
            .skip(panel.offset)
            .take(available)
        {
            let safe = clipped(text, width);
            let selected = panel.selected == Some(index);
            let style = if selected {
                SELECTED
            } else if matches!(panel.kind, InspectorKind::Diff)
                && safe.starts_with('+')
                && !safe.starts_with("+++")
            {
                MINT
            } else if matches!(panel.kind, InspectorKind::Diff)
                && safe.starts_with('-')
                && !safe.starts_with("---")
            {
                "\x1b[38;2;235;153;142m"
            } else if matches!(panel.kind, InspectorKind::Markdown) && safe.starts_with('#') {
                MINT
            } else {
                ""
            };
            let rendered = if selected { padded(&safe, width) } else { safe };
            rows.push(if panel.kind == InspectorKind::Source && !selected {
                source_line(&rendered, color)
            } else {
                styled(&rendered, style, color)
            });
        }
    }
    rows.resize(height.saturating_sub(footer), String::new());
    if footer > 0 && height > 0 {
        rows.push(styled(&clipped(&panel.footer, width), MUTED, color));
    }
    rows.truncate(height);
    rows
}
/// Main Carbon screen. Inspectors are temporary and never reduce the chat to
/// an unreadable sliver: narrow terminals show the inspector in the body.
#[allow(clippy::too_many_arguments)]
pub fn layout_workspace(
    history: &[Entry],
    input: &[u8],
    cursor: usize,
    status: &str,
    scroll: usize,
    context: &str,
    size: (usize, usize),
    color: bool,
    ui: &Workspace,
) -> Option<Frame> {
    let (columns, height) = size;
    if columns < 12 || height < 8 {
        return layout_full(history, input, cursor, status, scroll, context, size, color);
    }
    let width = columns - 1;
    let (editor, editor_cursor_row, editor_cursor) =
        editor_rows(input, cursor, width, (height / 4).clamp(1, 4), color);
    let body_height = height.saturating_sub(7 + editor.len()).max(1);
    let mut rows = vec![heading(
        &format!("ANSA  {context}"),
        &ui.activity,
        width,
        color,
    )];
    let location = if ui.breadcrumb.is_empty() {
        "main"
    } else {
        &ui.breadcrumb
    };
    let mut breadcrumb = location.to_owned();
    if ui.reading {
        breadcrumb.push_str("  /  HISTORY · follow paused");
        if ui.new_events > 0 {
            breadcrumb.push_str(&format!(" · {} new", ui.new_events));
        }
    }
    rows.push(styled(
        &clipped(&breadcrumb, width),
        if ui.reading { MINT } else { MUTED },
        color,
    ));
    rows.push(rule(width, color));
    let side_by_side = ui.inspector.is_some() && width >= 100;
    let panel_width = if side_by_side {
        (width * 2 / 5).max(34)
    } else {
        width
    };
    let chat_width = if side_by_side {
        width - panel_width - 3
    } else {
        width
    };
    let chat = layout_impl(
        history,
        b"",
        0,
        "",
        true,
        scroll,
        "",
        (chat_width + 1, body_height + 5),
        color,
        true,
    )?;
    let chat_rows = &chat.lines[1..body_height + 1];
    let mut panes = vec![None; height];
    if let Some(panel) = &ui.inspector {
        let panel_rows = inspector_rows(
            panel,
            panel_width,
            body_height,
            color,
            ui.focus == Focus::Inspector,
        );
        if side_by_side {
            for (left, right) in chat_rows.iter().zip(panel_rows) {
                let pane = PaneRow {
                    column: chat_width,
                    left: padded(left, chat_width),
                    right: format!(" {} {}", styled("│", RULE, color), right),
                };
                let line = pane.line();
                panes[rows.len()] = Some(pane);
                rows.push(line);
            }
        } else {
            rows.extend(panel_rows);
        }
    } else {
        rows.extend_from_slice(chat_rows);
    }
    rows.push(rule(width, color));
    let cursor_row = rows.len() + editor_cursor_row;
    rows.extend(editor);
    rows.push(rule(width, color));
    let mut metadata = ui.model.clone();
    if !ui.effort.is_empty() {
        if !metadata.is_empty() {
            metadata.push_str(" · ");
        }
        metadata.push_str(&ui.effort);
    }
    let detail = status.lines().next().unwrap_or_default();
    if !detail.is_empty() {
        if !metadata.is_empty() {
            metadata.push_str(" · ");
        }
        metadata.push_str(detail);
    }
    rows.push(heading(
        &metadata,
        "/ commands · Tab focus · ? keys",
        width,
        color,
    ));
    // Diagnostics are separate from frozen transcript rows.
    let notice = status.lines().nth(1).unwrap_or_default();
    let notice = if notice.is_empty() {
        status.lines().nth(2).unwrap_or_default()
    } else {
        notice
    };
    rows.push(styled(&clipped(notice, width), MUTED, color));
    rows.resize(height, String::new());
    rows.truncate(height);
    if let Some(popup) = &ui.popup {
        paint_popup(&mut rows, popup, width, color);
    }
    Some(Frame {
        lines: rows,
        status_rows: vec![height - 2, height - 1],
        panes,
        cursor: editor_cursor,
        cursor_row: cursor_row.min(height - 1),
        scroll_limit: chat.scroll_limit,
        inspector_content_rows: ui.inspector.as_ref().map_or(0, |panel| {
            body_height.saturating_sub(
                1 + usize::from(!panel.tabs.is_empty() && body_height > 2)
                    + usize::from(body_height > 3)
                    + usize::from(!panel.footer.is_empty()),
            )
        }),
        viewport: Some(size),
        tool_actions: if (side_by_side || ui.inspector.is_none()) && ui.popup.is_none() {
            chat.tool_actions
                .into_iter()
                .map(|(row, key)| (row + 2, key))
                .collect()
        } else {
            Vec::new()
        },
        chat_bounds: ((side_by_side || ui.inspector.is_none()) && ui.popup.is_none()).then_some((
            3,
            3 + body_height,
            chat_width,
        )),
    })
}
fn paint_popup(rows: &mut [String], popup: &Popup, columns: usize, color: bool) {
    if rows.len() < 6 || columns < 12 {
        return;
    }
    let width = columns.saturating_sub(6).clamp(10, 58);
    let visible = popup.items.len().min(rows.len().saturating_sub(6));
    let first = popup.selected.saturating_sub(visible.saturating_sub(1));
    let left = (columns - width - 2) / 2;
    let mut overlay = vec![format!("┌{}┐", "─".repeat(width))];
    overlay.push(format!(
        "│{}│",
        padded(&styled(&clipped(&popup.title, width), MINT, color), width)
    ));
    overlay.push(format!(
        "│{}│",
        padded(&clipped(&popup.subtitle, width), width)
    ));
    for (index, item) in popup.items.iter().enumerate().skip(first).take(visible) {
        let prefix = if index == popup.selected {
            "› "
        } else {
            "  "
        };
        let label = format!("{prefix}{}", item.label);
        let detail = clipped(&item.detail, width / 3);
        let label = clipped(&label, width.saturating_sub(cells(&detail) + 1));
        let text = format!(
            "{}{}{}",
            label,
            " ".repeat(width.saturating_sub(cells(&label) + cells(&detail))),
            detail
        );
        overlay.push(format!(
            "│{}│",
            styled(
                &text,
                if index == popup.selected {
                    SELECTED
                } else {
                    ""
                },
                color
            )
        ));
    }
    overlay.push(format!(
        "│{}│",
        padded(&clipped(&popup.footer, width), width)
    ));
    overlay.push(format!("└{}┘", "─".repeat(width)));
    let top = rows.len().saturating_sub(overlay.len()) / 2;
    for (row, overlay) in rows.iter_mut().skip(top).zip(overlay) {
        let base = plain(row);
        let before = slice_cells(&base, 0, left);
        let after = slice_cells(&base, left + width + 2, columns);
        *row = format!("{}{overlay}{after}", padded(&before, left));
    }
}
fn slice_cells(text: &str, begin: usize, end: usize) -> String {
    let mut output = String::new();
    let mut column = 0;
    let mut include = false;
    for glyph in glyphs(text.as_bytes()) {
        if glyph.newline {
            break;
        }
        if glyph.width > 0 {
            include = column >= begin && column < end;
        }
        if include {
            output.push_str(&glyph.text);
        }
        column += glyph.width;
    }
    output
}
fn selection_bounds(text: &str, mut begin: usize, mut end: usize) -> (usize, usize) {
    let mut column = 0;
    for glyph in glyphs(text.as_bytes()) {
        if column < begin && begin < column + glyph.width {
            begin = column;
        }
        if column < end && end < column + glyph.width {
            end = column + glyph.width;
        }
        column += glyph.width;
    }
    (begin, end)
}
fn selected_segment(text: &str, first: usize, last: usize) -> String {
    let line = plain(text);
    let (first, last) = selection_bounds(&line, first, last);
    slice_cells(&line, first, last)
}
fn highlighted_segment(text: &str, first: usize, last: usize, color: bool) -> String {
    let line = plain(text);
    let (first, last) = selection_bounds(&line, first, last);
    let before = slice_cells(&line, 0, first);
    let selected = slice_cells(&line, first, last);
    let after = slice_cells(&line, last, usize::MAX);
    if selected.is_empty() {
        line
    } else {
        format!(
            "{before}{}{after}",
            styled(&selected, if color { SELECTED } else { "\x1b[7m" }, true)
        )
    }
}
impl Frame {
    pub fn plain_lines(&self) -> Vec<String> {
        self.lines.iter().map(|line| plain(line)).collect()
    }
    /// Inclusive endpoints in physical cells. Combining marks remain attached
    /// to their selected base character and wide characters are never split.
    pub fn selected_text(&self, anchor: (usize, usize), focus: (usize, usize)) -> String {
        if self.lines.is_empty() {
            return String::new();
        }
        let (begin, end) = if anchor <= focus {
            (anchor, focus)
        } else {
            (focus, anchor)
        };
        let mut text = String::new();
        for row in begin.0..=end.0.min(self.lines.len().saturating_sub(1)) {
            if row > begin.0 {
                text.push('\n');
            }
            let first = if row == begin.0 { begin.1 } else { 0 };
            let last = if row == end.0 {
                end.1.saturating_add(1)
            } else {
                usize::MAX
            };
            let selected = if let Some(pane) = self.pane(row) {
                let mut selected =
                    selected_segment(&pane.left, first.min(pane.column), last.min(pane.column));
                if last > pane.column {
                    selected.push_str(&selected_segment(
                        &pane.right,
                        first.saturating_sub(pane.column),
                        last.saturating_sub(pane.column),
                    ));
                }
                selected
            } else {
                selected_segment(&self.lines[row], first, last)
            };
            text.push_str(if last == usize::MAX {
                selected.trim_end()
            } else {
                &selected
            });
        }
        text
    }
    fn highlight(&self, anchor: (usize, usize), focus: (usize, usize), color: bool) -> Self {
        let mut frame = self.clone();
        if frame.lines.is_empty() {
            return frame;
        }
        let (begin, end) = if anchor <= focus {
            (anchor, focus)
        } else {
            (focus, anchor)
        };
        for row in begin.0..=end.0.min(frame.lines.len().saturating_sub(1)) {
            let first = if row == begin.0 { begin.1 } else { 0 };
            let last = if row == end.0 {
                end.1.saturating_add(1)
            } else {
                usize::MAX
            };
            if let Some(pane) = frame.pane(row).cloned() {
                let pane = PaneRow {
                    column: pane.column,
                    left: highlighted_segment(
                        &pane.left,
                        first.min(pane.column),
                        last.min(pane.column),
                        color,
                    ),
                    right: highlighted_segment(
                        &pane.right,
                        first.saturating_sub(pane.column),
                        last.saturating_sub(pane.column),
                        color,
                    ),
                };
                frame.lines[row] = pane.line();
                frame.panes[row] = Some(pane);
            } else {
                frame.lines[row] = highlighted_segment(&frame.lines[row], first, last, color);
            }
        }
        frame
    }
}
/// A drag owns the exact displayed frame. Incoming events cannot change the
/// bytes copied on release. The controller can keep using `frame` until release.
#[derive(Debug, Clone)]
pub struct Selection {
    frozen: Frame,
    anchor: (usize, usize),
    focus: (usize, usize),
    moved: bool,
}
impl Selection {
    pub fn new(frame: Frame, column: usize, row: usize) -> Self {
        Self {
            frozen: frame,
            anchor: (row, column),
            focus: (row, column),
            moved: false,
        }
    }
    pub fn drag(&mut self, column: usize, row: usize) {
        self.focus = (row.min(self.frozen.lines.len().saturating_sub(1)), column);
        self.moved |= self.focus != self.anchor;
    }
    pub fn frame(&self, color: bool) -> Frame {
        self.frozen.highlight(self.anchor, self.focus, color)
    }
    /// Resize clips the frozen display without changing selection endpoints or
    /// the source bytes used by `finish`. Reflow resumes after the drag ends.
    pub fn frame_at(&self, size: (usize, usize), color: bool) -> Frame {
        let mut frame = self.frozen.clone();
        let columns = size.0.saturating_sub(1);
        frame.lines.clear();
        frame.panes = vec![None; size.1];
        for (row, line) in self.frozen.lines.iter().enumerate().take(size.1) {
            if let Some(pane) = self.frozen.pane(row) {
                let left = clipped(&plain(&pane.left), columns.min(pane.column));
                if pane.column < columns {
                    let pane = PaneRow {
                        column: pane.column,
                        left,
                        right: clipped(&plain(&pane.right), columns - pane.column),
                    };
                    frame.lines.push(pane.line());
                    frame.panes[row] = Some(pane);
                } else {
                    frame.lines.push(left);
                }
            } else {
                frame.lines.push(clipped(&plain(line), columns));
            }
        }
        frame.lines.resize(size.1, String::new());
        frame.viewport = Some(size);
        frame.cursor_row = frame.cursor_row.min(size.1.saturating_sub(1));
        frame.cursor = frame.cursor.min(size.0.saturating_sub(1));
        frame.highlight(self.anchor, self.focus, color)
    }
    pub fn text(&self) -> Option<String> {
        self.moved
            .then(|| self.frozen.selected_text(self.anchor, self.focus))
            .filter(|text| !text.is_empty())
    }
    pub fn finish(&mut self, column: usize, row: usize) -> Option<String> {
        self.drag(column, row);
        self.text()
    }
}
fn source_line(text: &str, color: bool) -> String {
    if !color {
        return text.into();
    }
    let mut result = String::new();
    let mut at = 0;
    while at < text.len() {
        let remaining = &text[at..];
        let ch = remaining.chars().next().expect("char boundary");
        if remaining.starts_with("//") || ch == '#' {
            result.push_str(&styled(remaining, MUTED, true));
            break;
        }
        if matches!(ch, '\'' | '"') {
            let mut end = at + ch.len_utf8();
            let mut escaped = false;
            for cp in text[end..].chars() {
                end += cp.len_utf8();
                if !escaped && cp == ch {
                    break;
                }
                escaped = !escaped && cp == '\\';
            }
            result.push_str(&styled(&text[at..end], MINT, true));
            at = end;
        } else if ch.is_alphanumeric() || ch == '_' {
            let length = remaining
                .find(|cp: char| !cp.is_alphanumeric() && cp != '_')
                .unwrap_or(remaining.len());
            let token = &remaining[..length];
            if matches!(
                token,
                "pub"
                    | "fn"
                    | "async"
                    | "await"
                    | "let"
                    | "mut"
                    | "const"
                    | "impl"
                    | "self"
                    | "Self"
                    | "use"
                    | "mod"
                    | "match"
                    | "if"
                    | "else"
                    | "for"
                    | "while"
                    | "loop"
                    | "return"
                    | "break"
                    | "continue"
                    | "struct"
                    | "enum"
                    | "trait"
                    | "where"
                    | "move"
                    | "ref"
                    | "def"
                    | "class"
                    | "from"
                    | "import"
                    | "as"
                    | "with"
                    | "try"
                    | "except"
                    | "finally"
                    | "raise"
                    | "yield"
                    | "in"
                    | "is"
                    | "not"
                    | "and"
                    | "or"
                    | "None"
                    | "True"
                    | "False"
                    | "true"
                    | "false"
            ) {
                result.push_str(&styled(token, "\x1b[38;2;231;196;112m", true));
            } else {
                result.push_str(token);
            }
            at += length;
        } else {
            result.push(ch);
            at += ch.len_utf8();
        }
    }
    result
}
