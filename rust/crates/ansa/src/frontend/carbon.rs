//! Carbon layout in the main screen buffer. All untrusted bytes are sanitized.
use ansa_linux::terminal::Width;
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub role: String,
    pub text: String,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    pub lines: Vec<String>,
    pub cursor: usize,
    pub scroll_limit: usize,
    viewport: Option<(usize, usize)>,
    cursor_row: usize,
}
impl Frame {
    pub fn paint(&self, previous: usize) -> String {
        if self.viewport.is_some() {
            let mut out = "\x1b[H\x1b[2J".to_owned();
            for (i, line) in self.lines.iter().enumerate() {
                if i > 0 {
                    out.push_str("\r\n");
                }
                out.push_str(line);
            }
            let up = self.lines.len().saturating_sub(self.cursor_row + 1);
            if up > 0 {
                out.push_str(&format!("\x1b[{up}A"));
            }
            out.push_str(&format!("\r\x1b[{}C", self.cursor));
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
            if previous.lines[index] == *line {
                continue;
            }
            if index > row {
                out.push_str(&format!("\x1b[{}B", index - row));
            }
            out.push_str("\r\x1b[2K");
            out.push_str(line);
            row = index;
        }
        let target = self.cursor_row;
        if row > target {
            out.push_str(&format!("\x1b[{}A", row - target));
        }
        if row < target {
            out.push_str(&format!("\x1b[{}B", target - row));
        }
        out.push_str(&format!("\r\x1b[{}C", self.cursor));
        out
    }
}
#[derive(Clone)]
struct Glyph {
    text: String,
    width: usize,
    newline: bool,
}
fn fallback(cp: char) -> usize {
    match cp as u32 {
        0x0300..=0x036f
        | 0x1ab0..=0x1aff
        | 0x1dc0..=0x1dff
        | 0x20d0..=0x20ff
        | 0xfe00..=0xfe0f
        | 0xfe20..=0xfe2f
        | 0xe0100..=0xe01ef
        | 0x200d => 0,
        0x1100..=0x115f
        | 0x2329
        | 0x232a
        | 0x2e80..=0x303e
        | 0x3040..=0xa4cf
        | 0xac00..=0xd7a3
        | 0xf900..=0xfaff
        | 0xfe10..=0xfe19
        | 0xfe30..=0xfe6f
        | 0xff00..=0xff60
        | 0xffe0..=0xffe6
        | 0x1f300..=0x1faff
        | 0x20000..=0x3fffd => 2,
        _ => 1,
    }
}
fn glyphs(bytes: &[u8], width: &Width) -> Vec<Glyph> {
    let mut result = vec![];
    let mut at = 0;
    while at < bytes.len() {
        let ch = bytes[at];
        if ch < 32 || ch == 127 {
            result.push(Glyph {
                text: match ch {
                    b'\n' => "\n",
                    b'\t' => " ",
                    _ => "?",
                }
                .into(),
                width: usize::from(ch != b'\n'),
                newline: ch == b'\n',
            });
            at += 1;
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
            result.push(if matches!(cp as u32, 0x80..=0x9f) {
                Glyph {
                    text: "?".into(),
                    width: 1,
                    newline: false,
                }
            } else {
                Glyph {
                    text: cp.to_string(),
                    width: width.width(cp).unwrap_or_else(|| fallback(cp)),
                    newline: false,
                }
            });
            at += n;
        } else {
            result.push(Glyph {
                text: "?".into(),
                width: 1,
                newline: false,
            });
            at += 1;
        }
    }
    result
}
fn compact_input_glyphs(bytes: &[u8], width: &Width) -> Vec<Glyph> {
    let mut result = glyphs(bytes, width);
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
    let width = Width::default();
    let mint = if color { "\x1b[38;2;133;220;177m" } else { "" };
    let reset = if color { "\x1b[0m" } else { "" };
    let mut wrapped = vec![];
    for entry in history {
        if !wrapped.is_empty() {
            wrapped.push(String::new());
        }
        let safe: String = glyphs(entry.text.as_bytes(), &width)
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
            let mut chars = vec![];
            if user && first {
                for text in [">", " "] {
                    chars.push(Styled {
                        glyph: Glyph {
                            text: text.into(),
                            width: 1,
                            newline: false,
                        },
                        bold: true,
                        code: true,
                    });
                }
            }
            let mut bold = false;
            let mut code = user || entry.role == "tool" && first;
            let mut at = 0;
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
                for glyph in glyphs(&text.as_bytes()[at..at + cp.len_utf8()], &width) {
                    chars.push(Styled {
                        glyph,
                        bold: bold || heading || user,
                        code,
                    });
                }
                at += cp.len_utf8();
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
                let mut line = String::new();
                let mut was = (false, false);
                for ch in &chars[pos..end] {
                    if color && (ch.bold, ch.code) != was {
                        line.push_str(reset);
                        if ch.bold {
                            line.push_str("\x1b[1m");
                        }
                        if ch.code {
                            line.push_str(mint);
                        }
                        was = (ch.bold, ch.code);
                    }
                    line.push_str(&ch.glyph.text);
                }
                if color {
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
    let end = wrapped.len() - scroll.min(wrapped.len().saturating_sub(1));
    let begin = end.saturating_sub(available);
    let mut header = format!("{mint}{}ansa{reset}", if color { "\x1b[1m" } else { "" });
    let mut used = 4;
    for glyph in glyphs(context.as_bytes(), &width) {
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
            for glyph in glyphs(status_lines.next().unwrap_or("").as_bytes(), &width) {
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
    let before = compact_input_glyphs(&input[..cursor], &width);
    let after = compact_input_glyphs(&input[cursor..], &width);
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
    for glyph in glyphs(text.as_bytes(), &width) {
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
        viewport: full.then_some(size),
        cursor_row,
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
const MUTED: &str = "\x1b[38;2;151;158;155m";
const RULE: &str = "\x1b[38;2;80;93;87m";
const RESET: &str = "\x1b[0m";
const SELECTED: &str = "\x1b[48;2;28;68;44m\x1b[38;2;219;255;230m";

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
    glyphs(plain(text).as_bytes(), &Width::default())
        .iter()
        .map(|g| g.width)
        .sum()
}
fn clipped(text: &str, columns: usize) -> String {
    let mut result = String::new();
    let mut used = 0;
    for glyph in glyphs(text.as_bytes(), &Width::default()) {
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
        styled(&left, MINT, color),
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
    let width = Width::default();
    let cursor = cursor.min(input.len());
    let mut logical = vec![String::new()];
    let mut row = 0;
    let mut used = 2;
    let mut cursor_at = (0, 2);
    let mut consumed = 0;
    for glyph in glyphs(input, &width) {
        if consumed == cursor {
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
                if consumed == cursor {
                    cursor_at = (row, used);
                }
            }
            logical[row].push_str(&glyph.text);
            used += glyph.width;
        }
        // Valid UTF-8 is copied as-is; invalid bytes each render as one '?'.
        // Original byte counts must stay separate from sanitized text lengths.
        let ch = input.get(consumed).copied().unwrap_or_default();
        consumed += match ch {
            0xc2..=0xdf
                if input
                    .get(consumed..consumed + 2)
                    .is_some_and(|v| std::str::from_utf8(v).is_ok()) =>
            {
                2
            }
            0xe0..=0xef
                if input
                    .get(consumed..consumed + 3)
                    .is_some_and(|v| std::str::from_utf8(v).is_ok()) =>
            {
                3
            }
            0xf0..=0xf4
                if input
                    .get(consumed..consumed + 4)
                    .is_some_and(|v| std::str::from_utf8(v).is_ok()) =>
            {
                4
            }
            _ => 1,
        };
    }
    if cursor == input.len() {
        cursor_at = (row, used);
    }
    let visible = maximum.max(1).min(logical.len());
    let first = cursor_at.0.saturating_sub(visible - 1);
    let mut lines = Vec::with_capacity(visible);
    for (index, text) in logical.iter().enumerate().skip(first).take(visible) {
        let prefix = if index == 0 { "› " } else { "  " };
        lines.push(format!("{}{text}", styled(prefix, MINT, color)));
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
                if i == panel.selected_tab { MINT } else { MUTED },
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
                rows.push(format!(
                    "{} {} {}",
                    padded(left, chat_width),
                    styled("│", RULE, color),
                    right
                ));
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
        cursor: editor_cursor,
        cursor_row: cursor_row.min(height - 1),
        scroll_limit: chat.scroll_limit,
        viewport: Some(size),
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
    for glyph in glyphs(text.as_bytes(), &Width::default()) {
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
    for glyph in glyphs(text.as_bytes(), &Width::default()) {
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
            let line = plain(&self.lines[row]);
            let (first, last) = selection_bounds(&line, first, last);
            let selected = slice_cells(&line, first, last);
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
            let line = plain(&frame.lines[row]);
            let (first, last) = selection_bounds(&line, first, last);
            let before = slice_cells(&line, 0, first);
            let selected = slice_cells(&line, first, last);
            let after = slice_cells(&line, last, usize::MAX);
            frame.lines[row] = format!(
                "{before}{}{after}",
                styled(&selected, if color { SELECTED } else { "\x1b[7m" }, true)
            );
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
        frame.lines = frame
            .plain_lines()
            .iter()
            .map(|line| clipped(line, size.0.saturating_sub(1)))
            .take(size.1)
            .collect();
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
