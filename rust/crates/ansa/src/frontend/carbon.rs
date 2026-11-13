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
}
impl Frame {
    pub fn paint(&self, previous: usize) -> String {
        let rows = self.lines.len();
        let mut out = if previous == 0 {
            format!("{}\r\x1b[{}A", "\n".repeat(rows - 1), rows - 1)
        } else {
            format!("\r\x1b[{}A", previous - 2)
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
    let (cols, rows) = size;
    if cols < 5 || rows < 5 {
        return None;
    }
    let max_rows = (rows - 1).clamp(4, 30);
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
            let mut code = user;
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
    let available = max_rows - 3;
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
    let cursor = cursor.min(input.len());
    let before = glyphs(&input[..cursor], &width);
    let after = glyphs(&input[cursor..], &width);
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
    let text = format!(
        "{status}  Tab: режим  Ctrl+F: история{}",
        if expanded { "  PgUp/PgDn" } else { "" }
    );
    for glyph in glyphs(text.as_bytes(), &width) {
        if glyph.newline || used + glyph.width > cols {
            break;
        }
        clipped.push_str(&glyph.text);
        used += glyph.width;
    }
    lines.push(format!("{mint}{clipped}{reset}"));
    Some(Frame {
        lines,
        cursor: 2 + before_width,
        scroll_limit: wrapped.len().saturating_sub(available),
    })
}
