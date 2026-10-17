//! Render widgets at a fixed size and dump the buffer, so the app crates can snapshot-test
//! their screens without a terminal.

use crate::width::grapheme_width;
use ratatui::buffer::{Buffer, Cell};
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier};
use ratatui::widgets::Widget;
use std::fmt::Write as _;

/// An off-screen terminal: a `Buffer` plus dump helpers.
#[derive(Clone, Debug)]
pub struct TestTerminal {
    buf: Buffer,
}

impl TestTerminal {
    pub fn new(width: u16, height: u16) -> Self {
        Self {
            buf: Buffer::empty(Rect::new(0, 0, width, height)),
        }
    }

    /// Render one widget over the whole screen.
    pub fn render<W: Widget>(width: u16, height: u16, widget: W) -> Self {
        let mut t = Self::new(width, height);
        let area = t.area();
        widget.render(area, &mut t.buf);
        t
    }

    /// Run arbitrary drawing code against the buffer.
    pub fn draw(&mut self, f: impl FnOnce(&mut Buffer, Rect)) -> &mut Self {
        let area = self.area();
        f(&mut self.buf, area);
        self
    }

    pub fn area(&self) -> Rect {
        self.buf.area
    }

    pub fn buffer(&self) -> &Buffer {
        &self.buf
    }

    pub fn buffer_mut(&mut self) -> &mut Buffer {
        &mut self.buf
    }

    pub fn cell(&self, x: u16, y: u16) -> Option<&Cell> {
        self.buf.cell((x, y))
    }

    /// Text of one row with trailing spaces trimmed.
    pub fn row(&self, y: u16) -> String {
        row_text(&self.buf, y)
    }

    /// Plain text dump, one line per row, trailing spaces trimmed. Wide characters appear
    /// once; the cell they cover is skipped.
    pub fn plain(&self) -> String {
        plain_text(&self.buf)
    }

    /// Dump with 24-bit SGR, one line per row, reset at every row end.
    pub fn ansi(&self) -> String {
        ansi_dump(&self.buf)
    }
}

fn cells_of(buf: &Buffer, y: u16) -> impl Iterator<Item = &Cell> {
    let mut skip = 0usize;
    (0..buf.area.width).filter_map(move |x| {
        if skip > 0 {
            skip -= 1;
            return None;
        }
        let c = &buf[(buf.area.x + x, buf.area.y + y)];
        skip = grapheme_width(c.symbol()).saturating_sub(1);
        Some(c)
    })
}

pub fn row_text(buf: &Buffer, y: u16) -> String {
    let mut s = String::new();
    for c in cells_of(buf, y) {
        s.push_str(c.symbol());
    }
    s.trim_end().to_string()
}

pub fn plain_text(buf: &Buffer) -> String {
    (0..buf.area.height)
        .map(|y| row_text(buf, y))
        .collect::<Vec<_>>()
        .join("\n")
}

fn sgr_color(out: &mut String, c: Color, fg: bool) {
    let base = if fg { 30 } else { 40 };
    let _ = match c {
        Color::Reset => write!(out, "\x1b[{}m", base + 9),
        Color::Black => write!(out, "\x1b[{}m", base),
        Color::Red => write!(out, "\x1b[{}m", base + 1),
        Color::Green => write!(out, "\x1b[{}m", base + 2),
        Color::Yellow => write!(out, "\x1b[{}m", base + 3),
        Color::Blue => write!(out, "\x1b[{}m", base + 4),
        Color::Magenta => write!(out, "\x1b[{}m", base + 5),
        Color::Cyan => write!(out, "\x1b[{}m", base + 6),
        Color::Gray => write!(out, "\x1b[{}m", base + 7),
        Color::DarkGray => write!(out, "\x1b[{}m", base + 60),
        Color::LightRed => write!(out, "\x1b[{}m", base + 61),
        Color::LightGreen => write!(out, "\x1b[{}m", base + 62),
        Color::LightYellow => write!(out, "\x1b[{}m", base + 63),
        Color::LightBlue => write!(out, "\x1b[{}m", base + 64),
        Color::LightMagenta => write!(out, "\x1b[{}m", base + 65),
        Color::LightCyan => write!(out, "\x1b[{}m", base + 66),
        Color::White => write!(out, "\x1b[{}m", base + 67),
        Color::Indexed(n) => write!(out, "\x1b[{};5;{}m", base + 8, n),
        Color::Rgb(r, g, b) => write!(out, "\x1b[{};2;{};{};{}m", base + 8, r, g, b),
    };
}

fn sgr_mods(out: &mut String, m: Modifier) {
    for (flag, code) in [
        (Modifier::BOLD, 1),
        (Modifier::DIM, 2),
        (Modifier::ITALIC, 3),
        (Modifier::UNDERLINED, 4),
        (Modifier::SLOW_BLINK, 5),
        (Modifier::REVERSED, 7),
        (Modifier::HIDDEN, 8),
        (Modifier::CROSSED_OUT, 9),
    ] {
        if m.contains(flag) {
            let _ = write!(out, "\x1b[{code}m");
        }
    }
}

pub fn ansi_dump(buf: &Buffer) -> String {
    let mut out = String::new();
    for y in 0..buf.area.height {
        let mut fg = Color::Reset;
        let mut bg = Color::Reset;
        let mut mods = Modifier::empty();
        // Find the last cell that is not a plain blank on the default background so rows do
        // not carry a run of trailing escape codes.
        let cells: Vec<&Cell> = cells_of(buf, y).collect();
        let last = cells
            .iter()
            .rposition(|c| {
                c.symbol() != " " || c.bg != Color::Reset || c.modifier.contains(Modifier::REVERSED)
            })
            .map_or(0, |i| i + 1);
        for c in &cells[..last] {
            if c.modifier != mods {
                out.push_str("\x1b[0m");
                fg = Color::Reset;
                bg = Color::Reset;
                mods = c.modifier;
                sgr_mods(&mut out, mods);
            }
            if c.fg != fg {
                sgr_color(&mut out, c.fg, true);
                fg = c.fg;
            }
            if c.bg != bg {
                sgr_color(&mut out, c.bg, false);
                bg = c.bg;
            }
            out.push_str(c.symbol());
        }
        out.push_str("\x1b[0m\n");
    }
    out
}

/// Strip the SGR codes back out of an ANSI dump (for asserting dump == plain).
#[cfg(test)]
fn strip_sgr(s: &str) -> String {
    crate::ansi::strip(s)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::paint::put_str;
    use ratatui::style::Style;

    #[test]
    fn plain_trims_and_handles_wide_chars() {
        let mut t = TestTerminal::new(10, 2);
        t.draw(|b, a| {
            put_str(b, 0, 0, "日本 ok   ", Style::default(), a);
            put_str(b, 0, 1, "e\u{301}x", Style::default(), a);
        });
        assert_eq!(t.plain(), "日本 ok\ne\u{301}x");
        assert_eq!(t.row(0), "日本 ok");
    }

    #[test]
    fn ansi_dump_round_trips_text_and_emits_truecolor() {
        let mut t = TestTerminal::new(8, 2);
        t.draw(|b, a| {
            put_str(
                b,
                1,
                0,
                "hi",
                Style::new()
                    .fg(Color::Rgb(1, 2, 3))
                    .bg(Color::Rgb(4, 5, 6))
                    .add_modifier(Modifier::BOLD),
                a,
            );
        });
        let a = t.ansi();
        assert!(a.contains("\x1b[1m"));
        assert!(a.contains("\x1b[38;2;1;2;3m"));
        assert!(a.contains("\x1b[48;2;4;5;6m"));
        assert_eq!(strip_sgr(&a), " hi\n\n");
        assert_eq!(a.lines().count(), 2);
    }

    #[test]
    fn indexed_and_named_colours() {
        let mut s = String::new();
        sgr_color(&mut s, Color::Indexed(200), true);
        sgr_color(&mut s, Color::LightRed, false);
        sgr_color(&mut s, Color::Reset, true);
        assert_eq!(s, "\x1b[38;5;200m\x1b[101m\x1b[39m");
    }

    #[test]
    fn zero_size_terminal_dumps_empty() {
        let t = TestTerminal::new(0, 0);
        assert_eq!(t.plain(), "");
        assert_eq!(t.ansi(), "");
    }

    #[test]
    fn render_runs_a_widget_over_the_full_area() {
        struct W;
        impl Widget for W {
            fn render(self, area: Rect, buf: &mut Buffer) {
                put_str(buf, area.x, area.y, "x", Style::default(), area);
            }
        }
        let t = TestTerminal::render(3, 1, W);
        assert_eq!(t.plain(), "x");
    }
}
