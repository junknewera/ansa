//! Markdown is parsed off the UI thread; only owned, terminal-safe text reaches
//! the layout. Styles are semantic, so plain terminals use the same row mapping.
use super::cells;
use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd};
use std::sync::Arc;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Style {
    pub heading: u8,
    pub bold: bool,
    pub italic: bool,
    pub code: bool,
    pub link: bool,
    pub strike: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Span {
    pub text: String,
    pub style: Style,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Row {
    pub spans: Vec<Span>,
    /// Zero-based line in the Markdown source.
    pub source_line: usize,
    pub source_end_line: usize,
    /// Logical block and character offset, stable across viewport widths.
    pub anchor: (usize, usize),
}
impl Row {
    pub fn plain(&self) -> String {
        self.spans.iter().map(|span| span.text.as_str()).collect()
    }
    pub fn locate(rows: &[Self], anchor: (usize, usize)) -> usize {
        rows.partition_point(|row| row.anchor <= anchor)
            .saturating_sub(1)
    }
    pub fn source_row(rows: &[Self], source: usize) -> usize {
        rows.iter()
            .position(|row| !row.spans.is_empty() && row.source_end_line >= source)
            .unwrap_or(rows.len().saturating_sub(1))
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Piece {
    span: Span,
    source_line: usize,
}
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Line {
    pieces: Vec<Piece>,
    prefix: String,
    continuation: String,
    literal: bool,
    source_line: usize,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Document {
    lines: Arc<Vec<Line>>,
}
impl Document {
    pub fn parse(source: &str) -> Self {
        let mut builder = Builder::default();
        let starts: Vec<usize> = std::iter::once(0)
            .chain(source.match_indices('\n').map(|(offset, _)| offset + 1))
            .collect();
        let options =
            Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TABLES | Options::ENABLE_TASKLISTS;
        for (event, range) in Parser::new_ext(source, options).into_offset_iter() {
            let line = starts
                .partition_point(|start| *start <= range.start)
                .saturating_sub(1);
            builder.event(event, line);
        }
        builder.flush();
        while builder
            .lines
            .last()
            .is_some_and(|line| line.pieces.is_empty() && !line.literal)
        {
            builder.lines.pop();
        }
        Self {
            lines: Arc::new(builder.lines),
        }
    }

    /// Unwrapped readable text, without terminal escape sequences.
    pub fn plain(&self) -> String {
        let mut output = String::new();
        for line in self.lines.iter() {
            output.push_str(&line.prefix);
            for piece in &line.pieces {
                output.push_str(&piece.span.text);
            }
            output.push('\n');
        }
        output
    }

    /// Wrap logical blocks by terminal cells. Prefixes are clipped on narrow
    /// terminals, leaving room for content; continuation keeps the list indent.
    pub fn rows(&self, columns: usize) -> Vec<Row> {
        let mut rows = Vec::new();
        self.fill_rows(columns, &mut rows, &mut || false);
        rows
    }

    pub(crate) fn rows_cancelled(
        &self,
        columns: usize,
        stop: &crate::auth::Cancel,
    ) -> Option<Vec<Row>> {
        let mut rows = Vec::new();
        self.fill_rows(columns, &mut rows, &mut || stop.cancelled())
            .then_some(rows)
    }

    fn fill_rows(
        &self,
        columns: usize,
        rows: &mut Vec<Row>,
        cancelled: &mut impl FnMut() -> bool,
    ) -> bool {
        let columns = columns.max(1);
        for (block, line) in self.lines.iter().enumerate() {
            if cancelled() || !wrap(line, block, columns, rows, cancelled) {
                return false;
            }
        }
        true
    }
}

#[derive(Default)]
struct List {
    next: Option<u64>,
    indent: usize,
}
#[derive(Default)]
struct Builder {
    lines: Vec<Line>,
    current: Line,
    lists: Vec<List>,
    marker: Option<String>,
    quote: usize,
    heading: u8,
    strong: usize,
    emphasis: usize,
    strike: usize,
    link: usize,
    code: bool,
    table_head: bool,
    table_cell: usize,
}
impl Builder {
    fn style(&self) -> Style {
        Style {
            heading: self.heading,
            bold: self.strong > 0 || self.table_head,
            italic: self.emphasis > 0,
            code: self.code,
            link: self.link > 0,
            strike: self.strike > 0,
        }
    }

    fn begin(&mut self, line: usize) {
        if !self.current.pieces.is_empty() {
            return;
        }
        self.current.source_line = line;
        self.current.literal = self.code;
        let mut prefix = "> ".repeat(self.quote.min(8));
        // Prefix growth is bounded even for malicious nesting. Source content
        // stays intact; extra nesting simply shares the deepest visible indent.
        let indent = self
            .lists
            .iter()
            .map(|list| list.indent)
            .sum::<usize>()
            .min(40);
        let marker = self.marker.take();
        let first_indent = indent.saturating_sub(marker.as_ref().map_or(0, String::len));
        prefix.push_str(&" ".repeat(first_indent));
        if let Some(marker) = marker {
            prefix.push_str(&marker);
        }
        self.current.prefix = prefix;
        self.current.continuation =
            format!("{}{}", "> ".repeat(self.quote.min(8)), " ".repeat(indent));
        if self.code {
            self.current.prefix.push_str("  ");
            self.current.continuation.push_str("  ");
        }
    }

    fn append(&mut self, text: &str, line: usize, style: Style) {
        if text.is_empty() {
            return;
        }
        self.begin(line);
        let text = super::workspace::display_text(text);
        if let Some(previous) = self.current.pieces.last_mut()
            && previous.span.style == style
            && previous.source_line == line
        {
            previous.span.text.push_str(&text);
        } else {
            self.current.pieces.push(Piece {
                span: Span { text, style },
                source_line: line,
            });
        }
    }

    fn text(&mut self, text: &str, source_line: usize, style: Style) {
        let mut line = source_line;
        for (index, part) in text.split('\n').enumerate() {
            if index > 0 {
                if self.code {
                    if self.current.pieces.is_empty() {
                        self.begin(line);
                        self.lines.push(std::mem::take(&mut self.current));
                    } else {
                        self.flush();
                    }
                } else {
                    self.append(" ", line, style);
                }
                line += 1;
            }
            self.append(part, line, style);
        }
    }

    fn flush(&mut self) {
        if !self.current.pieces.is_empty() {
            self.lines.push(std::mem::take(&mut self.current));
        }
    }

    fn blank(&mut self, source_line: usize) {
        self.flush();
        if self
            .lines
            .last()
            .is_some_and(|line| !line.pieces.is_empty())
        {
            self.lines.push(Line {
                source_line,
                ..Line::default()
            });
        }
    }

    fn event(&mut self, event: Event<'_>, line: usize) {
        match event {
            Event::Start(tag) => match tag {
                Tag::Paragraph => {
                    if self.lists.is_empty() {
                        self.blank(line);
                    } else {
                        self.flush();
                    }
                }
                Tag::Heading { level, .. } => {
                    self.blank(line);
                    self.heading = level as u8;
                }
                Tag::CodeBlock(kind) => {
                    self.blank(line);
                    self.code = true;
                    if let pulldown_cmark::CodeBlockKind::Fenced(language) = kind
                        && !language.is_empty()
                    {
                        self.append(
                            language.as_ref(),
                            line,
                            Style {
                                code: true,
                                bold: true,
                                ..Style::default()
                            },
                        );
                        self.flush();
                    }
                }
                Tag::List(next) => {
                    self.flush();
                    self.lists.push(List { next, indent: 2 });
                }
                Tag::Item => {
                    self.flush();
                    if let Some(list) = self.lists.last_mut() {
                        let marker = if let Some(next) = &mut list.next {
                            let marker = format!("{next}. ");
                            *next = next.saturating_add(1);
                            marker
                        } else {
                            "- ".to_string()
                        };
                        list.indent = marker.len();
                        self.marker = Some(marker);
                    }
                }
                Tag::BlockQuote(_) => {
                    self.blank(line);
                    self.quote += 1;
                }
                Tag::Strong => self.strong += 1,
                Tag::Emphasis => self.emphasis += 1,
                Tag::Strikethrough => self.strike += 1,
                Tag::Link { .. } => self.link += 1,
                Tag::Image { .. } => self.append("[", line, self.style()),
                Tag::HtmlBlock => self.blank(line),
                Tag::Table(_) => self.blank(line),
                Tag::TableHead => {
                    self.flush();
                    self.table_head = true;
                    self.table_cell = 0;
                }
                Tag::TableRow => {
                    self.flush();
                    self.table_cell = 0;
                }
                Tag::TableCell => {
                    if self.table_cell > 0 {
                        self.append(" | ", line, self.style());
                    }
                    self.table_cell += 1;
                }
                _ => {}
            },
            Event::End(tag) => match tag {
                TagEnd::Paragraph => self.flush(),
                TagEnd::Heading(_) => {
                    self.flush();
                    self.heading = 0;
                }
                TagEnd::CodeBlock => {
                    self.flush();
                    self.code = false;
                }
                TagEnd::List(_) => {
                    self.flush();
                    self.lists.pop();
                }
                TagEnd::Item => {
                    self.flush();
                    self.marker = None;
                }
                TagEnd::BlockQuote(_) => {
                    self.flush();
                    self.quote = self.quote.saturating_sub(1);
                }
                TagEnd::Strong => self.strong = self.strong.saturating_sub(1),
                TagEnd::Emphasis => self.emphasis = self.emphasis.saturating_sub(1),
                TagEnd::Strikethrough => self.strike = self.strike.saturating_sub(1),
                TagEnd::Link => self.link = self.link.saturating_sub(1),
                TagEnd::Image => self.append("]", line, self.style()),
                TagEnd::HtmlBlock | TagEnd::Table | TagEnd::TableRow => self.flush(),
                TagEnd::TableHead => {
                    self.flush();
                    self.table_head = false;
                }
                _ => {}
            },
            Event::Text(text) => self.text(&text, line, self.style()),
            Event::Code(text) | Event::InlineMath(text) => {
                self.append(
                    &text,
                    line,
                    Style {
                        code: true,
                        ..self.style()
                    },
                );
            }
            Event::Html(text) | Event::InlineHtml(text) => {
                // HTML remains inert visible text; sanitize after parsing too,
                // because entities can decode to terminal or bidi controls.
                self.text(&text, line, self.style());
            }
            Event::DisplayMath(text) => self.text(
                &text,
                line,
                Style {
                    code: true,
                    ..self.style()
                },
            ),
            Event::SoftBreak => self.append(" ", line, self.style()),
            Event::HardBreak => self.flush(),
            Event::Rule => {
                self.blank(line);
                self.append("---", line, self.style());
                self.flush();
            }
            Event::TaskListMarker(checked) => {
                self.append(if checked { "[x] " } else { "[ ] " }, line, self.style())
            }
            Event::FootnoteReference(label) => {
                self.append(&format!("[{label}]"), line, self.style())
            }
        }
    }
}

#[derive(Clone, Copy)]
struct Unit<'a> {
    text: &'a str,
    cells: usize,
    style: Style,
    source_line: usize,
    source_end_line: usize,
    offset: usize,
}
fn prefix(text: &str, limit: usize) -> String {
    let mut output = String::new();
    let mut used = 0;
    for grapheme in cells::graphemes(text) {
        let count = cells::width(grapheme);
        if used + count > limit {
            break;
        }
        output.push_str(grapheme);
        used += count;
    }
    output
}
fn push_span(spans: &mut Vec<Span>, text: &str, style: Style) {
    if let Some(last) = spans.last_mut()
        && last.style == style
    {
        last.text.push_str(text);
    } else {
        spans.push(Span {
            text: text.to_string(),
            style,
        });
    }
}
fn wrap(
    line: &Line,
    block: usize,
    columns: usize,
    output: &mut Vec<Row>,
    cancelled: &mut impl FnMut() -> bool,
) -> bool {
    let mut text = String::new();
    let mut pieces = Vec::new();
    for piece in &line.pieces {
        for (index, ch) in piece.span.text.chars().enumerate() {
            if index % 1024 == 0 && cancelled() {
                return false;
            }
            text.push(ch);
        }
        pieces.push((text.len(), piece.span.style, piece.source_line));
    }
    // Segment the entire line so a style change between a base character and
    // its variation selector/combining mark cannot split one terminal glyph.
    let mut units = Vec::new();
    let mut logical_cells = 0;
    let mut byte = 0;
    let mut piece = 0;
    let mut offset = 0;
    for (index, grapheme) in cells::graphemes(&text).enumerate() {
        if index % 1024 == 0 && cancelled() {
            return false;
        }
        while pieces[piece].0 <= byte {
            piece += 1;
        }
        let (_, style, source_line) = pieces[piece];
        byte += grapheme.len();
        while pieces[piece].0 < byte {
            piece += 1;
        }
        let source_end_line = pieces[piece].2;
        if grapheme == "\t" {
            let count = if line.literal {
                4 - logical_cells % 4
            } else {
                1
            };
            for subcolumn in 0..count {
                units.push(Unit {
                    text: " ",
                    cells: 1,
                    style,
                    source_line,
                    source_end_line,
                    offset: offset + subcolumn,
                });
            }
            logical_cells += count;
            offset += count;
        } else {
            let count = cells::width(grapheme);
            units.push(Unit {
                text: grapheme,
                cells: count,
                style,
                source_line,
                source_end_line,
                offset,
            });
            logical_cells += count;
            offset += grapheme.chars().count();
        }
    }
    let mut start = 0;
    let mut first = true;
    loop {
        if cancelled() {
            return false;
        }
        if !line.literal {
            while start < units.len() && units[start].text == " " && !units[start].style.code {
                start += 1;
            }
            if start >= units.len() && !first {
                break;
            }
        }
        let leading = prefix(
            if first {
                &line.prefix
            } else {
                &line.continuation
            },
            columns - 1,
        );
        let capacity = columns - cells::width(&leading);
        let mut used = 0;
        let mut end = start;
        let mut boundary = None;
        while end < units.len() && used + units[end].cells <= capacity {
            if units[end].text == " " && !units[end].style.code {
                boundary = Some(end);
            }
            used += units[end].cells;
            end += 1;
        }
        let mut next = end;
        if end < units.len()
            && units[end].text != " "
            && !line.literal
            && let Some(space) = boundary
            && space > start
        {
            end = space;
            next = space + 1;
        }
        if !line.literal {
            while end > start && units[end - 1].text == " " && !units[end - 1].style.code {
                end -= 1;
            }
        }
        let source_line = units
            .get(start)
            .map_or(line.source_line, |unit| unit.source_line);
        let mut spans = Vec::new();
        if !leading.is_empty() {
            spans.push(Span {
                text: leading,
                style: Style::default(),
            });
        }
        if next == start && start < units.len() {
            // A two-cell glyph cannot fit a one-cell terminal. Keep progress and
            // a visible placeholder rather than writing into the adjacent pane.
            push_span(&mut spans, "?", units[start].style);
            next += 1;
        } else {
            for unit in &units[start..end] {
                push_span(&mut spans, unit.text, unit.style);
            }
        }
        let source_end_line = units
            .get(if end > start { end - 1 } else { start })
            .map_or(source_line, |unit| unit.source_end_line);
        output.push(Row {
            spans,
            source_line,
            source_end_line,
            anchor: (block, units.get(start).map_or(0, |unit| unit.offset)),
        });
        if next >= units.len() {
            break;
        }
        first = false;
        start = next;
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cancelled_wrapping_discards_partial_rows() {
        let document = Document::parse(&"word ".repeat(2048));
        let stop = crate::auth::Cancel::default();
        stop.cancel();
        assert!(document.rows_cancelled(20, &stop).is_none());

        let mut checks = 0;
        let mut rows = Vec::new();
        assert!(!document.fill_rows(20, &mut rows, &mut || {
            checks += 1;
            checks == 32
        }));
        assert!(!rows.is_empty());
        assert_eq!(checks, 32);
    }
}
