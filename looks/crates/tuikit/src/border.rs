//! Border sets and frames ported from opencode's `ui/border.ts` and the prompt box in
//! `component/prompt/index.tsx`.

use crate::paint::{fill, pad, put_str};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::symbols::border::Set;
use ratatui::widgets::Widget;

const BLANK: &str = " ";

/// opencode's `SplitBorder`: only the left and right edges draw, as a heavy bar.
pub const SPLIT: Set<'static> = Set {
    top_left: BLANK,
    top_right: BLANK,
    bottom_left: BLANK,
    bottom_right: BLANK,
    vertical_left: "┃",
    vertical_right: "┃",
    horizontal_top: BLANK,
    horizontal_bottom: BLANK,
};

/// The prompt box: split border with `╹` closing the bar and `▀` as the lower edge, which
/// together draw the half-height cap under the input.
pub const PROMPT: Set<'static> = Set {
    bottom_left: "╹",
    horizontal_bottom: "▀",
    ..SPLIT
};

/// Background with a heavy bar down the left and/or right edge (toasts, permission and
/// question panels). Content goes in [`SplitBox::inner`].
#[derive(Clone, Copy, Debug)]
pub struct SplitBox {
    pub left: Option<Color>,
    pub right: Option<Color>,
    pub bg: Color,
}

impl SplitBox {
    pub fn left(color: Color, bg: Color) -> Self {
        Self {
            left: Some(color),
            right: None,
            bg,
        }
    }

    pub fn both(color: Color, bg: Color) -> Self {
        Self {
            left: Some(color),
            right: Some(color),
            bg,
        }
    }

    /// Area inside the bars.
    pub fn inner(&self, area: Rect) -> Rect {
        pad(
            area,
            self.left.is_some() as u16,
            0,
            self.right.is_some() as u16,
            0,
        )
    }
}

impl Widget for SplitBox {
    fn render(self, area: Rect, buf: &mut Buffer) {
        if area.is_empty() {
            return;
        }
        fill(buf, area, Style::new().bg(self.bg));
        for y in area.top()..area.bottom() {
            if let Some(c) = self.left {
                put_str(
                    buf,
                    area.left(),
                    y,
                    SPLIT.vertical_left,
                    Style::new().fg(c),
                    area,
                );
            }
            if let Some(c) = self.right {
                put_str(
                    buf,
                    area.right() - 1,
                    y,
                    SPLIT.vertical_right,
                    Style::new().fg(c),
                    area,
                );
            }
        }
    }
}

/// The input frame: `┃` on the left in `border`, `bg` panel behind the text, one blank row at
/// the top, and a final row of `╹▀▀▀` that reads as the bottom edge of the panel.
#[derive(Clone, Copy, Debug)]
pub struct PromptFrame {
    pub border: Color,
    pub bg: Color,
}

impl PromptFrame {
    pub const PAD_LEFT: u16 = 2;
    pub const PAD_RIGHT: u16 = 2;

    /// Rows the frame adds around `content_rows`: top padding plus the cap row.
    pub const CHROME_ROWS: u16 = 2;

    pub fn height_for(content_rows: u16) -> u16 {
        content_rows.saturating_add(Self::CHROME_ROWS)
    }

    /// Where content goes: inside the bar, padded, above the cap row.
    pub fn inner(area: Rect) -> Rect {
        let body = pad(area, 1, 1, 0, 1);
        pad(body, Self::PAD_LEFT, 0, Self::PAD_RIGHT, 0)
    }
}

impl Widget for PromptFrame {
    fn render(self, area: Rect, buf: &mut Buffer) {
        if area.is_empty() {
            return;
        }
        let body_h = area.height.saturating_sub(1);
        let body = Rect {
            height: body_h,
            ..area
        };
        let panel = pad(body, 1, 0, 0, 0);
        fill(buf, panel, Style::new().bg(self.bg));
        for y in body.top()..body.bottom() {
            put_str(
                buf,
                area.left(),
                y,
                PROMPT.vertical_left,
                Style::new().fg(self.border),
                area,
            );
        }
        let cap_y = area.bottom() - 1;
        put_str(
            buf,
            area.left(),
            cap_y,
            PROMPT.bottom_left,
            Style::new().fg(self.border),
            area,
        );
        let line = PROMPT
            .horizontal_bottom
            .repeat(area.width.saturating_sub(1) as usize);
        put_str(
            buf,
            area.left() + 1,
            cap_y,
            &line,
            Style::new().fg(self.bg),
            area,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(buf: &Buffer) -> Vec<String> {
        (0..buf.area.height)
            .map(|y| (0..buf.area.width).map(|x| buf[(x, y)].symbol()).collect())
            .collect()
    }

    #[test]
    fn prompt_frame_draws_bar_panel_and_cap() {
        let area = Rect::new(0, 0, 8, 4);
        let mut b = Buffer::empty(area);
        PromptFrame {
            border: Color::Blue,
            bg: Color::Rgb(30, 30, 30),
        }
        .render(area, &mut b);
        assert_eq!(
            text(&b),
            vec!["┃       ", "┃       ", "┃       ", "╹▀▀▀▀▀▀▀"]
        );
        assert_eq!(b[(0, 0)].fg, Color::Blue);
        assert_eq!(b[(3, 1)].bg, Color::Rgb(30, 30, 30));
        assert_eq!(b[(3, 3)].fg, Color::Rgb(30, 30, 30));
        assert_eq!(b[(0, 3)].fg, Color::Blue);
    }

    #[test]
    fn prompt_inner_matches_opencode_padding() {
        let inner = PromptFrame::inner(Rect::new(0, 0, 40, 6));
        // bar(1) + padding(2) on the left, padding(2) on the right, 1 row above, cap below
        assert_eq!(inner, Rect::new(3, 1, 35, 4));
        assert_eq!(PromptFrame::height_for(4), 6);
    }

    #[test]
    fn smoke_zero_and_tiny_areas_do_not_panic() {
        for (w, h) in [(0, 0), (1, 1), (2, 1), (1, 5)] {
            let area = Rect::new(0, 0, w, h);
            let mut b = Buffer::empty(area);
            PromptFrame {
                border: Color::Red,
                bg: Color::Black,
            }
            .render(area, &mut b);
            SplitBox::both(Color::Red, Color::Black).render(area, &mut b);
            let _ = PromptFrame::inner(area);
        }
    }

    #[test]
    fn split_box_bars_and_inner() {
        let area = Rect::new(2, 1, 6, 2);
        let mut b = Buffer::empty(Rect::new(0, 0, 10, 4));
        let sb = SplitBox::both(Color::Green, Color::Black);
        sb.render(area, &mut b);
        assert_eq!(b[(2, 1)].symbol(), "┃");
        assert_eq!(b[(7, 2)].symbol(), "┃");
        assert_eq!(b[(4, 1)].bg, Color::Black);
        assert_eq!(sb.inner(area), Rect::new(3, 1, 4, 2));
    }
}
