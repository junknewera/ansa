//! Carbon layout in the main screen buffer. All untrusted bytes are sanitized.
use super::cells as display_cells;

pub(super) const NARROW_WORKSPACE_COLUMNS: usize = 80;

/// Immutable, decoded output belonging to the tool row that was displayed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutputView {
    pub title: String,
    pub text: String,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub role: String,
    pub text: String,
    pub action: Option<String>,
    /// Byte range built by the tool projection, never inferred from command text.
    pub status: Option<std::ops::Range<usize>>,
    pub edit: Option<std::sync::Arc<super::diff::EditView>>,
    pub output: Option<std::sync::Arc<OutputView>>,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    pub lines: Vec<String>,
    pub cursor: usize,
    pub scroll_limit: usize,
    pub tool_actions: Vec<(usize, String)>,
    /// Chat row range and right column, all zero based and exclusive at end.
    pub chat_bounds: Option<(usize, usize, usize)>,
    chat_rows: ChatRows,
    suggestion_bounds: Option<(usize, usize)>,
    pub(super) timeline_range: Option<(usize, usize)>,
    pub(super) timeline_total: usize,
    pub inspector_content_rows: usize,
    viewport: Option<(usize, usize)>,
    cursor_row: usize,
    status_rows: Vec<usize>,
    panes: Vec<Option<PaneRow>>,
    themed: bool,
    selection_style: Option<String>,
    edit_views: std::collections::BTreeMap<String, std::sync::Arc<super::diff::EditView>>,
    output_views: std::collections::BTreeMap<String, std::sync::Arc<OutputView>>,
    inspector_bounds: Option<(usize, usize, usize, usize)>,
    terminal_rows: std::collections::BTreeSet<usize>,
    pub(super) image: Option<super::graphics::Placement>,
}
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct ChatRows {
    wrapped: std::sync::Arc<[ChatRow]>,
    first: usize,
}
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct ChatRow {
    text: String,
    join: Option<&'static str>,
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
    pub(super) fn needs_full_paint(&self, previous: &Self) -> bool {
        self.viewport.is_none()
            || self.viewport != previous.viewport
            || self.lines.len() != previous.lines.len()
    }

    pub fn output_view(&self, key: &str) -> Option<std::sync::Arc<OutputView>> {
        self.output_views.get(key).cloned()
    }
    /// Content rectangle (column, row, width, height), excluding panel chrome.
    pub fn inspector_bounds(&self) -> Option<(usize, usize, usize, usize)> {
        self.inspector_bounds
    }
    pub fn edit_view(&self, key: &str) -> Option<std::sync::Arc<super::diff::EditView>> {
        self.edit_views.get(key).cloned()
    }
    pub fn apply_theme(&mut self, theme: &super::theme::Theme) {
        if self.themed {
            return;
        }
        self.themed = true;
        let mut replacements = theme.replacements();
        let base = format!("\x1b[49m{}", theme.foreground_sgr());
        replacements.push((RESET.into(), format!("{RESET}{base}")));
        let columns = self.viewport.map_or_else(
            || self.lines.iter().map(|line| cells(line)).max().unwrap_or(0),
            |size| size.0.saturating_sub(1),
        );
        let translate = |text: &str| {
            let mut output = String::new();
            let mut at = 0;
            while at < text.len() {
                if let Some((from, to)) = replacements
                    .iter()
                    .filter(|(from, _)| !from.is_empty() && text[at..].starts_with(from.as_str()))
                    .max_by_key(|(from, _)| from.len())
                {
                    output.push_str(to);
                    at += from.len();
                } else {
                    let ch = text[at..].chars().next().expect("char boundary");
                    output.push(ch);
                    at += ch.len_utf8();
                }
            }
            output
        };
        self.selection_style = Some(translate(SELECTED));
        let paint = |text: &str, width: usize, terminal: bool| {
            format!(
                "{base}{}{}{RESET}",
                if terminal {
                    text.into()
                } else {
                    translate(text)
                },
                " ".repeat(width.saturating_sub(cells(text)))
            )
        };
        for row in 0..self.lines.len() {
            if let Some(pane) = self.pane(row).cloned() {
                let pane = PaneRow {
                    column: pane.column,
                    left: paint(&pane.left, pane.column, false),
                    right: paint(
                        &pane.right,
                        columns.saturating_sub(pane.column),
                        self.terminal_rows.contains(&row),
                    ),
                };
                self.lines[row] = pane.line();
                self.panes[row] = Some(pane);
            } else {
                self.lines[row] =
                    paint(&self.lines[row], columns, self.terminal_rows.contains(&row));
            }
        }
    }
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
        if self.needs_full_paint(previous) {
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
    let mut edits = std::collections::BTreeMap::new();
    let mut outputs = std::collections::BTreeMap::new();
    let mut previous_tool = false;
    for entry in history {
        let tool = entry.role.starts_with("tool");
        let separate_tool = tool
            && previous_tool
            && entry.action.is_some()
            && !entry.role.starts_with("tool_child_")
            && !entry.role.starts_with("tool_diff_");
        if !wrapped.is_empty() && (!(tool && previous_tool) || separate_tool) {
            wrapped.push(ChatRow::default());
        }
        previous_tool = tool;
        if let Some(action) = &entry.action {
            tool_actions.push((wrapped.len(), action.clone()));
            if let Some(edit) = &entry.edit {
                edits.insert(action.clone(), edit.clone());
            }
            if let Some(output) = &entry.output {
                outputs.insert(action.clone(), output.clone());
            }
        }
        let safe: String = glyphs(entry.text.as_bytes())
            .iter()
            .map(|v| v.text.as_str())
            .collect();
        if let Some(kind) = match entry.role.as_str() {
            "tool_diff_add" => Some(super::diff::Kind::Add),
            "tool_diff_remove" => Some(super::diff::Kind::Remove),
            "tool_diff_hunk" => Some(super::diff::Kind::Hunk),
            "tool_diff_context" => Some(super::diff::Kind::Context),
            "tool_diff_notice" => Some(super::diff::Kind::Notice),
            _ => None,
        } {
            for text in safe.lines() {
                wrapped.push(ChatRow {
                    text: diff_line(&clipped(text, cols), kind, cols, color),
                    join: None,
                });
            }
            continue;
        }
        let assistant = entry.role == "assistant";
        let queued = entry.role == "queued_user";
        let user = entry.role == "user" || queued;
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
                for text in [
                    if queued {
                        "◷"
                    } else if color {
                        "›"
                    } else {
                        ">"
                    },
                    " ",
                ] {
                    chars.push(Styled {
                        glyph: Glyph {
                            text: text.into(),
                            width: 1,
                            newline: false,
                            source_end: 0,
                        },
                        bold: true,
                        code: true,
                        ink: if queued { MUTED } else { LAVENDER },
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
                styles.push((visible.len(), bold || heading || (user && !queued), code));
                visible.push(cp);
                at += cp.len_utf8();
            }
            let mut scalar = 0;
            for glyph in glyphs(visible.as_bytes()) {
                let (offset, bold, code) = styles[scalar];
                while scalar < styles.len() && styles[scalar].0 < glyph.source_end {
                    scalar += 1;
                }
                let ink = if queued {
                    MUTED
                } else {
                    paint.ink(offset, code || fence, heading)
                };
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
                let readable = if (assistant || user) && !fence {
                    cols.min(96)
                } else {
                    cols
                };
                while end < chars.len() && used + chars[end].glyph.width <= readable {
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
                let mut line = String::new();
                let mut was = (false, false, "");
                for (index, ch) in chars[pos..end].iter().enumerate() {
                    if color && (ch.bold, ch.code, ch.ink) != was {
                        line.push_str(reset);
                        if user && !queued {
                            line.push_str(USER_MESSAGE);
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
                    line.push_str(reset);
                }
                pos = end;
                let space = pos < chars.len() && chars[pos].glyph.text == " ";
                if space {
                    pos += 1;
                }
                wrapped.push(ChatRow {
                    text: line,
                    join: (pos < chars.len()).then_some(if space { " " } else { "" }),
                });
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
    lines.extend(wrapped[begin..end].iter().map(|line| line.text.clone()));
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
    let mut editor = format!("{mint}> {}", if color { "\x1b[39m" } else { "" });
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
    let text = format!("{status}  / commands  Tab focus  ? help");
    for glyph in glyphs(text.as_bytes()) {
        if glyph.newline || used + glyph.width > cols {
            break;
        }
        clipped.push_str(&glyph.text);
        used += glyph.width;
    }
    lines.push(format!("{mint}{clipped}{reset}"));
    let cursor_row = lines.len() - 2;
    let tool_actions: Vec<_> = tool_actions
        .into_iter()
        .filter_map(|(row, key)| {
            (row >= begin && row < end).then_some((row.saturating_sub(begin) + 1, key))
        })
        .collect();
    let edit_views = tool_actions
        .iter()
        .filter_map(|(_, key)| edits.remove(key).map(|edit| (key.clone(), edit)))
        .collect();
    let output_views = tool_actions
        .iter()
        .filter_map(|(_, key)| outputs.remove(key).map(|output| (key.clone(), output)))
        .collect();
    Some(Frame {
        lines,
        cursor: 2 + before_width,
        scroll_limit: wrapped.len().saturating_sub(available),
        tool_actions,
        edit_views,
        output_views,
        inspector_bounds: None,
        terminal_rows: Default::default(),
        chat_bounds: Some((1, available + 1, cols)),
        chat_rows: ChatRows {
            wrapped: wrapped.into(),
            first: begin,
        },
        timeline_range: None,
        timeline_total: 0,
        suggestion_bounds: None,
        inspector_content_rows: 0,
        viewport: full.then_some(size),
        cursor_row,
        status_rows: if full {
            vec![max_rows - 4, max_rows - 3, max_rows - 1]
        } else {
            vec![cursor_row + 1]
        },
        panes: Vec::new(),
        themed: false,
        selection_style: None,
        image: None,
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
    Plan,
    Source,
    Diff,
    Markdown,
    Command,
    Image,
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
    pub agents: Vec<AgentLine>,
    pub markdown: Option<std::sync::Arc<Vec<super::markdown::Row>>>,
    pub diff: Option<std::sync::Arc<Vec<super::diff::Row>>>,
    pub diff_column: usize,
    /// Grid already parsed by the terminal emulator; only bounded SGR is retained.
    pub rendered: Option<std::sync::Arc<Vec<String>>>,
    /// Zero-based column/row relative to the parsed grid.
    pub cursor: Option<(usize, usize)>,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentLine {
    pub title: String,
    pub state: String,
    pub detail: String,
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
    pub exit_confirmation: bool,
    pub reading: bool,
    pub new_events: usize,
    pub focus: Focus,
    pub inspector: Option<Inspector>,
    pub popup: Option<Popup>,
    pub suggestions: Option<Popup>,
    pub animation_frame: u8,
    pub animate: bool,
    pub draft_notice: bool,
    pub interrupt_hint: &'static str,
    pub result_selected: Option<String>,
    pub wide_diff: bool,
}
const MINT: &str = "\x1b[38;2;133;220;177m";
const TEXT: &str = "\x1b[38;2;215;223;235m";
const MUTED: &str = "\x1b[38;2;145;157;177m";
const SKY: &str = "\x1b[38;2;125;183;245m";
const LAVENDER: &str = "\x1b[38;2;188;166;242m";
const AMBER: &str = "\x1b[38;2;232;189;117m";
const CODE: &str = "\x1b[38;2;229;193;137m";
const RED: &str = "\x1b[38;2;241;142;150m";
const RULE: &str = "\x1b[38;2;62;74;93m";
const DIFF_ADD: &str = "\x1b[48;2;24;58;39m";
const DIFF_REMOVE: &str = "\x1b[48;2;68;31;36m";
const USER_MESSAGE: &str = "\x1b[48;2;29;35;39m";
const RESET: &str = "\x1b[0m";
const EXIT_HINT: &str = "Press Ctrl+C again to exit";
const SELECTED: &str = "\x1b[48;2;46;63;88m\x1b[38;2;232;240;255m";

fn tool_ink(role: &str) -> &'static str {
    match role
        .strip_prefix("tool_child_")
        .or_else(|| role.strip_prefix("tool_"))
        .unwrap_or(role)
    {
        "success" => MINT,
        "error" => RED,
        "warning" | "running" => AMBER,
        "output" => SKY,
        _ => MUTED,
    }
}
fn diff_line(text: &str, kind: super::diff::Kind, width: usize, color: bool) -> String {
    use super::diff::Kind;
    let style = match kind {
        Kind::Add => format!("{DIFF_ADD}{MINT}"),
        Kind::Remove => format!("{DIFF_REMOVE}{RED}"),
        Kind::Hunk => SKY.into(),
        Kind::Notice => MUTED.into(),
        Kind::Context => TEXT.into(),
    };
    styled(
        &if matches!(kind, Kind::Add | Kind::Remove) {
            padded(text, width)
        } else {
            text.into()
        },
        &style,
        color,
    )
}
struct TranscriptPaint {
    base: &'static str,
    header: &'static str,
    status: Option<std::ops::Range<usize>>,
    group: bool,
    tool: bool,
    shimmer: Option<usize>,
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
            } else if entry.role.starts_with("activity:")
                || matches!(
                    entry.role.as_str(),
                    "reasoning" | "activity" | "queued_user"
                )
            {
                MUTED
            } else {
                TEXT
            },
            header: if group { LAVENDER } else { SKY },
            status: entry.status.clone(),
            group,
            tool,
            shimmer: entry
                .role
                .strip_prefix("activity:")
                .and_then(|step| step.parse().ok()),
        }
    }
    fn ink(&self, at: usize, code: bool, heading: bool) -> &'static str {
        if let Some(step) = self.shimmer {
            return if (at + 32 - step % 32) % 32 < 5 {
                TEXT
            } else {
                MUTED
            };
        }
        if self.tool {
            if at < 2 || self.status.as_ref().is_none_or(|range| range.contains(&at)) {
                return self.base;
            }
            return if self.status.as_ref().is_some_and(|range| at >= range.end) {
                if self.group { SKY } else { MUTED }
            } else {
                self.header
            };
        }
        if heading {
            SKY
        } else if code {
            CODE
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
fn safe_grid_line(text: &str, width: usize, color: bool) -> String {
    let mut safe = String::new();
    let mut at = 0;
    while at < text.len() {
        let remaining = &text[at..];
        if let Some(sgr) = remaining.strip_prefix("\x1b[")
            && let Some(end) = sgr.find('m').filter(|end| *end <= 96)
            && sgr[..end]
                .bytes()
                .all(|byte| byte.is_ascii_digit() || matches!(byte, b';' | b':'))
        {
            if color {
                safe.push_str(&remaining[..end + 3]);
            }
            at += end + 3;
        } else {
            let ch = remaining.chars().next().expect("char boundary");
            if ch == '\n' {
                break;
            }
            safe.push(if ch.is_control() { '?' } else { ch });
            at += ch.len_utf8();
        }
    }
    clipped_painted(&safe, width)
}
fn heading(left: &str, right: &str, columns: usize, color: bool) -> String {
    let right = clipped(right, columns / 2);
    let left = clipped(left, columns.saturating_sub(cells(&right) + 2));
    format!(
        "{}{}{}",
        if let Some(rest) = left.strip_prefix("ANSA") {
            format!(
                "{}{}",
                styled("ANSA", "\x1b[1m\x1b[38;2;188;166;242m", color),
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
                styled(prefix, &format!("{LAVENDER}\x1b[1m"), true),
                styled(&padded(text, columns.saturating_sub(2)), TEXT, true)
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
    if panel.kind == InspectorKind::Agents {
        let stride = if available >= 4 { 2 } else { 1 };
        let count = available / stride;
        let first = panel
            .selected
            .unwrap_or_default()
            .saturating_sub(count.saturating_sub(1));
        for (index, agent) in panel.agents.iter().enumerate().skip(first).take(count) {
            let label = heading(&agent.title, &agent.state, width, color);
            rows.push(if panel.selected == Some(index) && focused {
                styled(&padded(&plain(&label), width), SELECTED, color)
            } else {
                label
            });
            if stride == 2 {
                rows.push(styled(&clipped(&agent.detail, width), MUTED, color));
            }
        }
    } else if let Some(rendered) = &panel.rendered {
        rows.extend(
            rendered
                .iter()
                .skip(panel.offset)
                .take(available)
                .map(|line| safe_grid_line(line, width, color)),
        );
    } else if let Some(diff) = &panel.diff {
        for row in diff.iter().skip(panel.offset).take(available) {
            let text = if matches!(
                row.kind,
                super::diff::Kind::Hunk | super::diff::Kind::Notice
            ) {
                clipped(&row.display(), width)
            } else {
                let gutter = clipped(&row.gutter(), width);
                let room = width.saturating_sub(cells(&gutter));
                format!(
                    "{gutter}{}",
                    clipped(
                        &slice_cells(
                            &super::workspace::display_text(&row.code()),
                            panel.diff_column,
                            panel.diff_column.saturating_add(room),
                        ),
                        room,
                    )
                )
            };
            rows.push(diff_line(&text, row.kind, width, color));
        }
    } else if let Some(markdown) = &panel.markdown {
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
                    style.push_str(CODE);
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
        let wrapped;
        let lines = if panel.kind == InspectorKind::Plan {
            wrapped = super::cells::wrap_lines(&panel.lines, width);
            &wrapped
        } else {
            &panel.lines
        };
        for (index, text) in lines.iter().enumerate().skip(panel.offset).take(available) {
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
                RED
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
/// an unreadable sliver: narrow terminals stack chat above the inspector.
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
        let status = if ui.exit_confirmation {
            EXIT_HINT
        } else {
            status
        };
        return layout_full(history, input, cursor, status, scroll, context, size, color);
    }
    let width = columns - 1;
    let (editor, editor_cursor_row, editor_cursor) =
        editor_rows(input, cursor, width, (height / 4).clamp(1, 4), color);
    let body_height = height.saturating_sub(7 + editor.len()).max(1);
    let mut rows = vec![heading("ANSA", "", width, color)];
    let mut breadcrumb = ui.breadcrumb.clone();
    if ui.reading {
        if !breadcrumb.is_empty() {
            breadcrumb.push_str(" · ");
        }
        breadcrumb.push_str("History");
        if ui.new_events > 0 {
            breadcrumb.push_str(&format!(" ↓ {}", ui.new_events));
        }
    }
    rows.push(styled(
        &clipped(&breadcrumb, width),
        if ui.reading { MINT } else { MUTED },
        color,
    ));
    rows.push(rule(width, color));
    let picture = ui
        .inspector
        .as_ref()
        .is_some_and(|p| p.kind == InspectorKind::Image);
    let plan = ui
        .inspector
        .as_ref()
        .is_some_and(|panel| panel.kind == InspectorKind::Plan);
    let side_by_side = ui.inspector.is_some()
        && !plan
        && !ui.wide_diff
        && (width >= 100 || picture && width >= 63);
    let panel_width = if side_by_side {
        if picture {
            width / 2
        } else {
            (width * 2 / 5).max(34)
        }
    } else {
        width
    };
    let chat_width = if side_by_side {
        width - panel_width - 3
    } else {
        width
    };
    let stacked = ui.inspector.is_some() && !side_by_side;
    let chat_height = if ui.wide_diff {
        0
    } else if plan {
        1
    } else if stacked {
        (body_height / 3).max(1)
    } else {
        body_height
    };
    let panel_height = if stacked {
        body_height.saturating_sub(chat_height + 1)
    } else {
        body_height
    };
    let animated;
    let history: &[Entry] = if history.iter().any(|entry| entry.role == "activity") {
        animated = history
            .iter()
            .cloned()
            .map(|mut entry| {
                if entry.role == "activity" {
                    let spinner = if ui.animate {
                        ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧"]
                            [usize::from(ui.animation_frame) % 8]
                    } else {
                        "◷"
                    };
                    entry.text = format!("{spinner} {}", entry.text);
                    if ui.animate {
                        entry.role = format!("activity:{}", ui.animation_frame % 32);
                    }
                }
                entry
            })
            .collect::<Vec<_>>();
        &animated
    } else {
        history
    };
    let chat = layout_impl(
        history,
        b"",
        0,
        "",
        true,
        scroll,
        "",
        (chat_width + 1, chat_height + 5),
        color,
        true,
    )?;
    let mut chat_rows = chat.lines[1..chat_height + 1].to_vec();
    if let Some(selected) = &ui.result_selected
        && let Some((row, _)) = chat.tool_actions.iter().find(|(_, key)| key == selected)
        && let Some(line) = row.checked_sub(1).and_then(|row| chat_rows.get_mut(row))
    {
        *line = if color {
            styled(&padded(&plain(line), chat_width), SELECTED, true)
        } else {
            clipped(&format!("› {}", plain(line)), chat_width)
        };
    }
    let mut inspector_bounds = None;
    let mut panes = vec![None; height];
    if let Some(panel) = &ui.inspector {
        let panel_rows = inspector_rows(
            panel,
            panel_width,
            panel_height,
            color,
            ui.focus == Focus::Inspector,
        );
        let chrome = 1
            + usize::from(!panel.tabs.is_empty() && panel_height > 2)
            + usize::from(panel_height > 3);
        inspector_bounds = Some((
            if side_by_side { chat_width + 3 } else { 0 },
            3 + if stacked { chat_height + 1 } else { 0 } + chrome,
            panel_width,
            panel_height.saturating_sub(chrome + usize::from(!panel.footer.is_empty())),
        ));
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
            rows.extend_from_slice(&chat_rows);
            rows.push(rule(width, color));
            rows.extend(panel_rows);
        }
    } else {
        rows.extend_from_slice(&chat_rows);
    }
    rows.push(rule(width, color));
    let mut cursor_row = rows.len() + editor_cursor_row;
    let mut cursor_column = editor_cursor;
    rows.extend(editor);
    rows.push(rule(width, color));
    let detail = status.lines().next().unwrap_or_default();
    let project = std::path::Path::new(context)
        .file_name()
        .and_then(std::ffi::OsStr::to_str)
        .unwrap_or(context);
    let metadata = [detail, ui.model.as_str(), ui.effort.as_str(), project]
        .into_iter()
        .filter(|value| !value.is_empty())
        .collect::<Vec<_>>()
        .join(" · ");
    let narrow = columns <= NARROW_WORKSPACE_COLUMNS;
    let terminal = ui.focus == Focus::Inspector
        && ui.popup.is_none()
        && ui
            .inspector
            .as_ref()
            .is_some_and(|panel| panel.kind == InspectorKind::Command);
    let navigation = if ui.result_selected.is_some() {
        if narrow {
            "↑/↓ · Enter · Esc"
        } else {
            "↑/↓ select · Enter open · Esc input"
        }
    } else if ui.popup.is_some() {
        ""
    } else if ui.suggestions.is_some() {
        "Tab complete"
    } else if terminal {
        "Click chat"
    } else if ui.focus == Focus::Inspector || ui.wide_diff {
        "Tab → chat"
    } else if let Some(panel) = &ui.inspector {
        match panel.kind {
            InspectorKind::Diff => "Tab → diff",
            InspectorKind::Agents => "Tab → agents",
            InspectorKind::Image => "Tab → chart",
            _ => "Tab → panel",
        }
    } else {
        "Tab mode"
    };
    let results =
        !chat.tool_actions.is_empty() && ui.result_selected.is_none() && ui.focus == Focus::Chat;
    let interrupt = if terminal {
        "Ctrl+C to program"
    } else {
        ui.interrupt_hint
    };
    let hints = [
        interrupt,
        navigation,
        if results {
            if narrow { "Ctrl+G" } else { "Ctrl+G results" }
        } else {
            ""
        },
        if ui.result_selected.is_some() || ui.popup.is_some() {
            ""
        } else {
            "? help"
        },
    ];
    let mut hint = String::new();
    for next in hints.into_iter().filter(|hint| !hint.is_empty()) {
        let gap = if hint.is_empty() { "" } else { " · " };
        if cells(&hint) + cells(gap) + cells(next) <= width / 2 {
            hint.push_str(gap);
            hint.push_str(next);
        }
    }
    rows.push(heading(&metadata, &hint, width, color));
    // Diagnostics are separate from frozen transcript rows.
    let notice = status.lines().nth(1).unwrap_or_default();
    let notice = if notice.is_empty() {
        status.lines().nth(2).unwrap_or_default()
    } else {
        notice
    };
    let notice = if notice.is_empty() && ui.draft_notice {
        "Draft · send after the task finishes"
    } else {
        notice
    };
    rows.push(styled(&clipped(notice, width), MUTED, color));
    rows.resize(height, String::new());
    rows.truncate(height);
    if ui.exit_confirmation {
        rows[height - 1] = styled(&clipped(EXIT_HINT, width), MUTED, color);
    }
    if let Some(popup) = &ui.popup {
        paint_popup(&mut rows, popup, width, color);
    }
    let suggestion_bounds = ui
        .suggestions
        .as_ref()
        .filter(|_| ui.popup.is_none())
        .and_then(|popup| {
            paint_suggestions(
                &mut rows,
                &mut panes,
                popup,
                chat_width,
                3,
                3 + chat_height,
                color,
            )
        });
    if ui.focus == Focus::Inspector
        && ui.popup.is_none()
        && let (Some((column, row, width, height)), Some((x, y))) = (
            inspector_bounds,
            ui.inspector.as_ref().and_then(|panel| panel.cursor),
        )
        && width > 0
        && height > 0
    {
        cursor_column = column + x.min(width - 1);
        cursor_row = row + y.min(height - 1);
    }
    let terminal_rows = inspector_bounds
        .filter(|_| {
            ui.inspector
                .as_ref()
                .is_some_and(|panel| panel.rendered.is_some())
                && ui.popup.is_none()
        })
        .map(|(_, row, _, height)| (row..row + height).collect())
        .unwrap_or_default();
    Some(Frame {
        lines: rows,
        status_rows: vec![height - 2, height - 1],
        image: None,
        panes,
        themed: false,
        selection_style: None,
        cursor: cursor_column,
        cursor_row: cursor_row.min(height - 1),
        scroll_limit: chat.scroll_limit,
        inspector_content_rows: ui.inspector.as_ref().map_or(0, |panel| {
            panel_height.saturating_sub(
                1 + usize::from(!panel.tabs.is_empty() && panel_height > 2)
                    + usize::from(panel_height > 3)
                    + usize::from(!panel.footer.is_empty()),
            )
        }),
        viewport: Some(size),
        inspector_bounds,
        terminal_rows,
        output_views: if ui.popup.is_none() {
            chat.output_views
        } else {
            Default::default()
        },
        edit_views: if ui.popup.is_none() {
            chat.edit_views
        } else {
            Default::default()
        },
        tool_actions: if ui.popup.is_none() {
            chat.tool_actions
                .into_iter()
                .map(|(row, key)| (row + 2, key))
                .filter(|(row, _)| {
                    suggestion_bounds.is_none_or(|(start, end)| *row < start || *row >= end)
                })
                .collect()
        } else {
            Vec::new()
        },
        chat_bounds: ui
            .popup
            .is_none()
            .then_some((3, 3 + chat_height, chat_width)),
        chat_rows: chat.chat_rows,
        timeline_range: None,
        timeline_total: 0,
        suggestion_bounds,
    })
}
fn paint_suggestions(
    rows: &mut [String],
    panes: &mut [Option<PaneRow>],
    popup: &Popup,
    width: usize,
    top: usize,
    bottom: usize,
    color: bool,
) -> Option<(usize, usize)> {
    let available = bottom.saturating_sub(top);
    if available == 0 || popup.items.is_empty() {
        return None;
    }
    let count = popup
        .items
        .len()
        .min(available.saturating_sub(1).max(1))
        .min(8);
    let first = popup.selected.saturating_sub(count.saturating_sub(1));
    let mut menu = popup
        .items
        .iter()
        .enumerate()
        .skip(first)
        .take(count)
        .map(|(index, item)| {
            let text = clipped(&format!("{}  {}", item.label, item.detail), width);
            styled(
                &padded(&text, width),
                if index == popup.selected {
                    SELECTED
                } else {
                    TEXT
                },
                color,
            )
        })
        .collect::<Vec<_>>();
    if menu.len() < available {
        menu.push(styled(&clipped(&popup.footer, width), MUTED, color));
    }
    let start = bottom.saturating_sub(menu.len());
    for (row, line) in (start..bottom).zip(menu) {
        if let Some(pane) = panes.get_mut(row).and_then(Option::as_mut) {
            pane.left = padded(&line, pane.column);
            rows[row] = pane.line();
        } else {
            rows[row] = line;
        }
    }
    Some((start, bottom))
}
fn paint_popup(rows: &mut [String], popup: &Popup, columns: usize, color: bool) {
    if rows.len() < 6 || columns < 12 {
        return;
    }
    let label = popup
        .items
        .iter()
        .map(|item| cells(&item.label) + 2)
        .max()
        .unwrap_or(0);
    let detail = popup
        .items
        .iter()
        .map(|item| cells(&item.detail))
        .max()
        .unwrap_or(0);
    let natural = [
        cells(&popup.title),
        cells(&popup.subtitle),
        cells(&popup.footer),
        label + usize::from(detail > 0) + detail,
    ]
    .into_iter()
    .max()
    .unwrap_or(10);
    let compact = matches!(popup.title.as_str(), "/model" | "/effort");
    let width = if compact {
        natural.max(10).min(columns.saturating_sub(2)).min(90)
    } else {
        columns.saturating_sub(6).clamp(10, 90)
    };
    let label_width = popup
        .items
        .iter()
        .map(|item| cells(&item.label) + 2)
        .max()
        .unwrap_or(0)
        .min(if !compact {
            width / 2
        } else if detail == 0 {
            width
        } else {
            width.saturating_sub((width / 3).max(1))
        });
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
        let text = if item.detail.is_empty() {
            padded(&clipped(&label, width), width)
        } else {
            padded(
                &format!(
                    "{} {}",
                    padded(&clipped(&label, label_width), label_width),
                    clipped(&item.detail, width.saturating_sub(label_width + 1))
                ),
                width,
            )
        };
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
fn highlighted_segment(
    text: &str,
    first: usize,
    last: usize,
    color: bool,
    selection_style: &str,
) -> String {
    let line = plain(text);
    let (first, last) = selection_bounds(&line, first, last);
    if first == last || first >= cells(text) {
        return text.into();
    }
    let mut output = String::new();
    let mut column = 0;
    let mut painted = String::new();
    for glyph in paint_glyphs(text) {
        let selected = column >= first && column < last;
        let style = if selected {
            format!(
                "{}{}",
                glyph.style,
                if color { selection_style } else { "\x1b[7m" }
            )
        } else {
            glyph.style.to_string()
        };
        if painted != style {
            output.push_str(RESET);
            output.push_str(&style);
        }
        output.push_str(&if selected {
            plain(&glyph.text)
        } else {
            glyph.text
        });
        painted = if selected {
            style
        } else {
            glyph.ending_style.to_string()
        };
        column += glyph.width;
    }
    output.push_str(RESET);
    output
}
fn clipped_painted(text: &str, columns: usize) -> String {
    if cells(text) <= columns {
        return text.into();
    }
    if !text.contains('\x1b') {
        return clipped(text, columns);
    }
    let mut output = String::new();
    let mut column = 0;
    let mut style = String::new();
    for glyph in paint_glyphs(text) {
        if column + glyph.width > columns {
            break;
        }
        if style != glyph.style.as_ref() {
            output.push_str(RESET);
            output.push_str(&glyph.style);
        }
        output.push_str(&glyph.text);
        style = glyph.ending_style.to_string();
        column += glyph.width;
    }
    output.push_str(RESET);
    output
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
    fn highlight(
        &self,
        anchor: (usize, usize),
        focus: (usize, usize),
        color: bool,
        chat_only: bool,
    ) -> Self {
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
                        frame.selection_style.as_deref().unwrap_or(SELECTED),
                    ),
                    right: if chat_only {
                        pane.right
                    } else {
                        highlighted_segment(
                            &pane.right,
                            first.saturating_sub(pane.column),
                            last.saturating_sub(pane.column),
                            color,
                            frame.selection_style.as_deref().unwrap_or(SELECTED),
                        )
                    },
                };
                frame.lines[row] = pane.line();
                frame.panes[row] = Some(pane);
            } else {
                frame.lines[row] = highlighted_segment(
                    &frame.lines[row],
                    first,
                    last,
                    color,
                    frame.selection_style.as_deref().unwrap_or(SELECTED),
                );
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
    chat: Option<ChatSelection>,
    pointer: (usize, usize),
    pub(super) scroll_pending: bool,
    pub(super) pending_range: Option<(usize, usize)>,
}

type ChatPoint = ((usize, usize), usize, usize);

#[derive(Debug, Clone)]
struct ChatSelection {
    anchor: ChatPoint,
    focus: ChatPoint,
    tail: usize,
    pages: std::collections::BTreeMap<(usize, usize), std::sync::Arc<[ChatRow]>>,
}

impl ChatSelection {
    fn bounds(frame: &Frame) -> Option<(usize, usize, usize)> {
        let (top, mut bottom, width) = frame.chat_bounds?;
        if let Some((first, _)) = frame.suggestion_bounds {
            bottom = bottom.min(first);
        }
        (top < bottom && width > 0).then_some((top, bottom, width))
    }
    fn point(frame: &Frame, column: usize, row: usize) -> Option<ChatPoint> {
        let page = frame.timeline_range?;
        let (top, bottom, width) = Self::bounds(frame)?;
        let total = frame.chat_rows.wrapped.len();
        if total == 0 {
            return None;
        }
        let line = frame.chat_rows.first + row.clamp(top, bottom.saturating_sub(1)) - top;
        let column = if line >= total {
            width - 1
        } else {
            column.min(width - 1)
        };
        Some((page, line.min(total - 1), column))
    }

    fn endpoints(&self) -> (ChatPoint, ChatPoint) {
        (self.anchor.min(self.focus), self.anchor.max(self.focus))
    }

    fn highlighted(&self, frame: Frame, color: bool) -> Frame {
        let Some((page, (top, bottom, width))) = frame.timeline_range.zip(Self::bounds(&frame))
        else {
            return frame;
        };
        let (start, end) = self.endpoints();
        let first = frame.chat_rows.first;
        let last = (first + bottom - top).min(frame.chat_rows.wrapped.len());
        if page < start.0 || page > end.0 || last <= first {
            return frame;
        }
        let from = if page == start.0 {
            (start.1, start.2)
        } else {
            (0, 0)
        };
        let to = if page == end.0 {
            (end.1, end.2)
        } else {
            (usize::MAX, width - 1)
        };
        if to.0 < first || from.0 >= last {
            return frame;
        }
        let from = if from.0 < first {
            (top, 0)
        } else {
            (top + from.0 - first, from.1)
        };
        let to = if to.0 >= last {
            (top + last - first - 1, width - 1)
        } else {
            (top + to.0 - first, to.1)
        };
        frame.highlight(from, to, color, true)
    }

    fn text(&self) -> String {
        let (start, end) = self.endpoints();
        let mut text = String::new();
        let mut join = "";
        for (page, rows) in self.pages.range(start.0..=end.0) {
            let first = if *page == start.0 { start.1 } else { 0 };
            let last = if *page == end.0 {
                end.1 + 1
            } else {
                rows.len()
            };
            for (index, row) in rows.iter().enumerate().take(last).skip(first) {
                let from = if (*page, index) == (start.0, start.1) {
                    start.2
                } else {
                    0
                };
                let to = if (*page, index) == (end.0, end.1) {
                    end.2 + 1
                } else {
                    usize::MAX
                };
                text.push_str(join);
                let selected = selected_segment(&row.text, from, to);
                text.push_str(&selected);
                if text.len() > super::copy::LIMIT {
                    return text;
                }
                join = if index + 1 == rows.len() {
                    "\n\n"
                } else {
                    row.join.unwrap_or("\n")
                };
            }
        }
        text
    }
}

impl Selection {
    pub fn new(frame: Frame, column: usize, row: usize) -> Self {
        Self {
            frozen: frame,
            anchor: (row, column),
            focus: (row, column),
            moved: false,
            chat: None,
            pointer: (column, row),
            scroll_pending: false,
            pending_range: None,
        }
    }
    pub(super) fn chat(frame: Frame, column: usize, row: usize) -> Option<Self> {
        let (top, bottom, _) = ChatSelection::bounds(&frame)?;
        if row < top
            || row >= bottom
            || frame.chat_rows.first + row - top >= frame.chat_rows.wrapped.len()
        {
            return None;
        }
        let point = ChatSelection::point(&frame, column, row)?;
        let rows = frame.chat_rows.wrapped.clone();
        let tail = frame.timeline_total;
        let mut result = Self::new(frame, column, row);
        result.chat = Some(ChatSelection {
            anchor: point,
            focus: point,
            tail,
            pages: [(point.0, rows)].into(),
        });
        Some(result)
    }
    pub(super) fn is_chat(&self) -> bool {
        self.chat.is_some()
    }
    pub(super) fn edge(&self) -> Option<bool> {
        if !self.moved || !self.is_chat() {
            return None;
        }
        let (top, bottom, _) = self.frozen.chat_bounds?;
        if self.pointer.1 <= top {
            Some(true)
        } else if self.pointer.1 + 1 >= bottom {
            Some(false)
        } else {
            None
        }
    }
    pub(super) fn neighbor(&self, older: bool) -> Option<(usize, usize)> {
        let (begin, end) = self.frozen.timeline_range?;
        self.chat.as_ref()?.pages.keys().copied().find(
            |&(a, b)| {
                if older { b == begin } else { a == end }
            },
        )
    }
    pub(super) fn tail(&self) -> Option<usize> {
        self.chat.as_ref().map(|chat| chat.tail)
    }
    pub(super) fn can_add_page(&self) -> bool {
        self.chat.as_ref().is_none_or(|chat| chat.pages.len() < 32)
    }
    pub(super) fn commit_scroll(&mut self, mut shown: Self) {
        shown.moved |= self.moved;
        shown.drag(self.pointer.0, self.pointer.1);
        *self = shown;
    }
    pub(super) fn update(
        &mut self,
        mut frame: Frame,
        scroll: usize,
        theme: Option<&super::theme::Theme>,
    ) {
        if !self.scroll_pending
            || self.pending_range.is_some() && self.pending_range == frame.timeline_range
            || self.frozen.viewport != frame.viewport
        {
            return;
        }
        let Some(chat) = &mut self.chat else {
            return;
        };
        let Some((range, _, _)) = ChatSelection::point(&frame, self.pointer.0, self.pointer.1)
        else {
            return;
        };
        if let Some(rows) = chat.pages.get(&range) {
            let (top, bottom, width) = frame.chat_bounds.expect("chat point has bounds");
            frame.scroll_limit = rows.len().saturating_sub(bottom - top);
            let end = rows.len() - scroll.min(frame.scroll_limit);
            let first = end.saturating_sub(bottom - top);
            for index in top..bottom {
                let text = rows
                    .get(first + index - top)
                    .map_or("", |row| row.text.as_str());
                if let Some(pane) = &mut frame.panes[index] {
                    pane.left = padded(text, width);
                    frame.lines[index] = pane.line();
                } else {
                    frame.lines[index] = text.into();
                }
            }
            frame.chat_rows = ChatRows {
                wrapped: rows.clone(),
                first,
            };
        } else {
            if chat.pages.len() >= 32
                || chat
                    .pages
                    .keys()
                    .any(|&(first, end)| first < range.1 && range.0 < end)
            {
                return;
            }
            chat.pages.insert(range, frame.chat_rows.wrapped.clone());
        }
        self.scroll_pending = false;
        self.pending_range = None;
        if let Some(theme) = theme {
            frame.apply_theme(theme);
        }
        self.frozen = frame;
        self.drag(self.pointer.0, self.pointer.1);
    }
    pub(super) fn can_scroll(&self, size: (usize, usize)) -> bool {
        !self.scroll_pending && self.frozen.viewport == Some(size)
    }
    pub(super) fn timeline_range(&self) -> Option<(usize, usize)> {
        self.frozen.timeline_range
    }
    pub fn drag(&mut self, column: usize, row: usize) {
        self.pointer = (column, row);
        if let Some(chat) = &mut self.chat
            && let Some(point) = ChatSelection::point(&self.frozen, column, row)
        {
            self.moved |= point != chat.anchor;
            chat.focus = point;
            return;
        }
        self.focus = (row.min(self.frozen.lines.len().saturating_sub(1)), column);
        self.moved |= self.focus != self.anchor;
    }
    pub fn frame(&self, color: bool) -> Frame {
        self.frozen.highlight(self.anchor, self.focus, color, false)
    }
    /// Resize clips the frozen display without changing selection endpoints or
    /// the source bytes used by `finish`. Reflow resumes after the drag ends.
    pub fn frame_at(&self, size: (usize, usize), color: bool) -> Frame {
        let mut frame = self.frozen.clone();
        frame.image = None;
        let columns = size.0.saturating_sub(1);
        frame.lines.clear();
        frame.panes = vec![None; size.1];
        for (row, line) in self.frozen.lines.iter().enumerate().take(size.1) {
            if let Some(pane) = self.frozen.pane(row) {
                let left = clipped_painted(&pane.left, columns.min(pane.column));
                if pane.column < columns {
                    let pane = PaneRow {
                        column: pane.column,
                        left,
                        right: clipped_painted(&pane.right, columns - pane.column),
                    };
                    frame.lines.push(pane.line());
                    frame.panes[row] = Some(pane);
                } else {
                    frame.lines.push(left);
                }
            } else {
                frame.lines.push(clipped_painted(line, columns));
            }
        }
        frame.lines.resize(size.1, String::new());
        frame.viewport = Some(size);
        frame.cursor_row = frame.cursor_row.min(size.1.saturating_sub(1));
        frame.cursor = frame.cursor.min(size.0.saturating_sub(1));
        if let Some(chat) = &self.chat {
            chat.highlighted(frame, color)
        } else {
            frame.highlight(self.anchor, self.focus, color, false)
        }
    }
    pub fn text(&self) -> Option<String> {
        self.moved
            .then(|| {
                self.chat.as_ref().map_or_else(
                    || self.frozen.selected_text(self.anchor, self.focus),
                    ChatSelection::text,
                )
            })
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
                result.push_str(&styled(token, CODE, true));
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
