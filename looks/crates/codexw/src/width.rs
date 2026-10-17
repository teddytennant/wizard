// OWNER: history-cells
//! Terminal display-width helpers (spec B.0.4) and the guard for fixed prefix columns.

use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// Terminal cell width of `text`, counting halfwidth sound marks as one cell like Codex.
pub fn display_width(text: &str) -> usize {
    UnicodeWidthStr::width(text)
        + text
            .chars()
            .filter(|ch| matches!(ch, '\u{FF9E}' | '\u{FF9F}'))
            .count()
}

pub fn char_width(ch: char) -> usize {
    if matches!(ch, '\u{FF9E}' | '\u{FF9F}') {
        1
    } else {
        UnicodeWidthChar::width(ch).unwrap_or(0)
    }
}

/// Columns left after reserving `reserved`, or `None` when nothing usable is left; `None` means
/// "draw the prefix only".
pub fn usable_content_width(total: usize, reserved: usize) -> Option<usize> {
    total.checked_sub(reserved).filter(|n| *n > 0)
}
