//! Terminal cell measurements for sanitized text. Callers handle line breaks
//! and escape sequences before measuring or clipping display content.
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

pub fn graphemes(text: &str) -> impl Iterator<Item = &str> {
    UnicodeSegmentation::graphemes(text, true)
}

/// Use the same extended clusters for wrapping, padding, cursor and selection.
pub fn width(text: &str) -> usize {
    graphemes(text).map(UnicodeWidthStr::width).sum()
}

pub(super) fn wrap_lines(lines: &[String], max_width: usize) -> Vec<String> {
    let mut output = Vec::new();
    for line in lines {
        let mut row = String::new();
        let mut used = 0;
        for glyph in graphemes(line) {
            let cells = width(glyph);
            if used + cells > max_width && !row.is_empty() {
                output.push(std::mem::take(&mut row));
                used = 0;
            }
            row.push_str(glyph);
            used += cells;
        }
        output.push(row);
    }
    output
}
