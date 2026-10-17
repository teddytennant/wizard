// OWNER: transcript
//! A laid-out transcript row and the small helpers every block layout shares.
//!
//! Rows are relative to the transcript column: x = 0 is the user bar, text starts at x = 2,
//! and the row is exactly `Cx::width` cells wide when it carries a background.

use std::time::Instant;

use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use tuikit::theme::Theme;
use tuikit::width::{display_width, spans_width, truncate_line};

use crate::palette::{Glyphs, Palette};

/// Everything a block needs to lay itself out.
pub struct Cx<'a> {
    pub p: &'a Palette,
    pub theme: &'a Theme,
    pub g: &'a Glyphs,
    /// Column width `C`.
    pub width: usize,
    /// `ctrl+o` detail: every tool body and thought is open.
    pub detail: bool,
    pub now: Instant,
    /// Spinner frame index, from the wall clock at 10 fps.
    pub spin: usize,
}

impl Cx<'_> {
    pub fn spinner(&self) -> &'static str {
        self.g.spinner[self.spin % self.g.spinner.len()]
    }
}

#[derive(Clone, Debug, Default)]
pub struct Row {
    pub spans: Vec<Span<'static>>,
    /// Painted across the whole column behind the spans.
    pub bg: Option<Color>,
    /// The summary row of a foldable block; a click on it toggles the block.
    pub head: bool,
}

impl Row {
    pub fn new(spans: Vec<Span<'static>>) -> Row {
        Row {
            spans,
            bg: None,
            head: false,
        }
    }

    pub fn blank() -> Row {
        Row::default()
    }

    pub fn bg(mut self, c: Color) -> Row {
        self.bg = Some(c);
        self
    }

    pub fn head(mut self) -> Row {
        self.head = true;
        self
    }

    pub fn width(&self) -> usize {
        spans_width(&self.spans)
    }

    pub fn text(&self) -> String {
        self.spans.iter().map(|s| s.content.as_ref()).collect()
    }
}

pub fn sp(text: impl Into<String>, style: Style) -> Span<'static> {
    Span::styled(text.into(), style)
}

pub fn spaces(n: usize) -> Span<'static> {
    Span::raw(" ".repeat(n))
}

/// Spans of `left`, spaces, then `right` flush with the column's right edge. `left` is cut
/// with an ellipsis when both do not fit, so the row never wraps (checklist 21). At least one
/// space separates them.
pub fn two_col(
    left: Vec<Span<'static>>,
    right: Vec<Span<'static>>,
    width: usize,
    gap_style: Style,
) -> Vec<Span<'static>> {
    let rw = spans_width(&right);
    let avail = width.saturating_sub(rw + usize::from(rw > 0));
    let left = if spans_width(&left) > avail {
        truncate_line(&Line::from(left), avail).spans
    } else {
        left
    };
    let lw = spans_width(&left);
    let mut out = left;
    out.push(Span::styled(
        " ".repeat(width.saturating_sub(lw + rw)),
        gap_style,
    ));
    out.extend(right);
    out
}

/// `s` cut to `max` cells with an ellipsis.
pub fn cut(s: &str, max: usize) -> String {
    tuikit::width::truncate(s, max)
}

/// Compact duration: `0.4s`, `2.1s`, `14s`, `1m 05s`.
pub fn fmt_dur(d: std::time::Duration) -> String {
    let s = d.as_secs_f64();
    if s < 10.0 {
        format!("{s:.1}s")
    } else if s < 60.0 {
        format!("{:.0}s", s)
    } else {
        format!("{}m {:02}s", d.as_secs() / 60, d.as_secs() % 60)
    }
}

/// Like [`fmt_dur`] but keeps a decimal under a minute, for the turn footer.
pub fn fmt_dur_long(d: std::time::Duration) -> String {
    let s = d.as_secs_f64();
    if s < 60.0 {
        format!("{s:.1}s")
    } else {
        fmt_dur(d)
    }
}

/// `1234` -> `1.2k`, `12_345` -> `12k`.
pub fn fmt_tokens(n: u64) -> String {
    match n {
        0..=999 => n.to_string(),
        1_000..=9_999 => format!("{:.1}k", n as f64 / 1000.0),
        10_000..=999_999 => format!("{}k", n / 1000),
        _ => format!("{:.1}M", n as f64 / 1_000_000.0),
    }
}

pub fn text_width(s: &str) -> usize {
    display_width(s)
}

/// Wrap plain text into rows of at most `width` cells, hard newlines kept.
pub fn wrap_plain(s: &str, width: usize) -> Vec<String> {
    tuikit::width::wrap(s, width.max(1))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn two_col_pads_and_truncates() {
        let st = Style::default();
        let r = two_col(vec![sp("abc", st)], vec![sp("xy", st)], 10, st);
        assert_eq!(
            r.iter().map(|s| s.content.as_ref()).collect::<String>(),
            "abc     xy"
        );
        let r = two_col(vec![sp("abcdefghij", st)], vec![sp("xy", st)], 8, st);
        let t: String = r.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(display_width(&t), 8);
        assert!(t.ends_with(" xy") && t.contains('…'), "{t:?}");
    }

    #[test]
    fn durations() {
        use std::time::Duration as D;
        assert_eq!(fmt_dur(D::from_millis(400)), "0.4s");
        assert_eq!(fmt_dur(D::from_secs(14)), "14s");
        assert_eq!(fmt_dur(D::from_secs(65)), "1m 05s");
        assert_eq!(fmt_dur_long(D::from_millis(12_400)), "12.4s");
        assert_eq!(fmt_tokens(3100), "3.1k");
        assert_eq!(fmt_tokens(36_000), "36k");
    }
}
