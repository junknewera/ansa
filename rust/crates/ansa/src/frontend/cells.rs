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
