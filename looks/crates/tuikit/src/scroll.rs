//! Scrollback viewport math and a scrollbar thumb.

use crate::paint::put_str;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::widgets::Widget;

/// Viewport over `content` rows. With `sticky` set the view follows new content; scrolling up
/// clears it and reaching the bottom again sets it.
#[derive(Clone, Debug)]
pub struct ScrollState {
    offset: usize,
    viewport: usize,
    content: usize,
    sticky: bool,
}

impl Default for ScrollState {
    fn default() -> Self {
        Self {
            offset: 0,
            viewport: 0,
            content: 0,
            sticky: true,
        }
    }
}

impl ScrollState {
    pub fn new() -> Self {
        Self::default()
    }

    /// Top visible row.
    pub fn offset(&self) -> usize {
        self.offset
    }

    pub fn viewport(&self) -> usize {
        self.viewport
    }

    pub fn content_len(&self) -> usize {
        self.content
    }

    pub fn is_sticky(&self) -> bool {
        self.sticky
    }

    pub fn max_offset(&self) -> usize {
        self.content.saturating_sub(self.viewport)
    }

    pub fn at_bottom(&self) -> bool {
        self.offset >= self.max_offset()
    }

    /// Update sizes after layout. Keeps the bottom pinned when sticky, otherwise only clamps.
    pub fn set_extent(&mut self, content: usize, viewport: usize) {
        self.content = content;
        self.viewport = viewport;
        if self.sticky {
            self.offset = self.max_offset();
        } else {
            self.offset = self.offset.min(self.max_offset());
            if self.at_bottom() && content > 0 {
                self.sticky = true;
            }
        }
    }

    /// Rows `[start, end)` that are on screen.
    pub fn visible_range(&self) -> std::ops::Range<usize> {
        self.offset..(self.offset + self.viewport).min(self.content)
    }

    pub fn scroll_by(&mut self, delta: isize) {
        let max = self.max_offset();
        let next = if delta < 0 {
            self.offset.saturating_sub(delta.unsigned_abs())
        } else {
            self.offset.saturating_add(delta as usize).min(max)
        };
        self.offset = next;
        self.sticky = self.at_bottom();
    }

    pub fn page_up(&mut self) {
        self.scroll_by(-(self.viewport.max(1) as isize));
    }

    pub fn page_down(&mut self) {
        self.scroll_by(self.viewport.max(1) as isize);
    }

    pub fn half_page_up(&mut self) {
        self.scroll_by(-((self.viewport / 2).max(1) as isize));
    }

    pub fn half_page_down(&mut self) {
        self.scroll_by((self.viewport / 2).max(1) as isize);
    }

    pub fn to_top(&mut self) {
        self.offset = 0;
        self.sticky = self.at_bottom();
    }

    pub fn to_bottom(&mut self) {
        self.offset = self.max_offset();
        self.sticky = true;
    }

    /// Bring `row` into view with the least movement (for jumping to a message).
    pub fn reveal(&mut self, row: usize) {
        if row < self.offset {
            self.offset = row;
        } else if self.viewport > 0 && row >= self.offset + self.viewport {
            self.offset = row + 1 - self.viewport;
        }
        self.offset = self.offset.min(self.max_offset());
        self.sticky = self.at_bottom();
    }

    /// `(start, len)` of the thumb in half-cells for a track `track_rows` tall (2 units per
    /// row). `None` when everything fits.
    pub fn thumb_halves(&self, track_rows: usize) -> Option<(usize, usize)> {
        if self.content <= self.viewport || self.viewport == 0 || track_rows == 0 {
            return None;
        }
        let units = track_rows * 2;
        // OpenTUI's slider never learns the viewport height (it stays at 1), so the thumb is
        // `units / (range + 1)` half-cells: a sliver on anything that scrolls at all, 2.5 rows
        // on a 28 row track with 9 rows of overflow.
        let max = self.max_offset().max(1);
        let len = (units / (max + 1)).clamp(1, units);
        let start = ((self.offset as f64 / max as f64) * (units - len) as f64).round() as usize;
        Some((start.min(units - len), len))
    }
}

/// One-column scrollbar. Draws nothing when the content fits.
pub struct Scrollbar<'a> {
    pub state: &'a ScrollState,
    pub track: Color,
    pub thumb: Color,
}

impl Widget for Scrollbar<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        if area.is_empty() {
            return;
        }
        let rows = area.height as usize;
        let Some((start, len)) = self.state.thumb_halves(rows) else {
            return;
        };
        for r in 0..rows {
            let top = (r * 2) >= start && (r * 2) < start + len;
            let bottom = (r * 2 + 1) >= start && (r * 2 + 1) < start + len;
            let (sym, style) = match (top, bottom) {
                (true, true) => ("█", Style::new().fg(self.thumb).bg(self.track)),
                (true, false) => ("▀", Style::new().fg(self.thumb).bg(self.track)),
                (false, true) => ("▄", Style::new().fg(self.thumb).bg(self.track)),
                (false, false) => (" ", Style::new().bg(self.track)),
            };
            put_str(buf, area.left(), area.top() + r as u16, sym, style, area);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn st(content: usize, viewport: usize) -> ScrollState {
        let mut s = ScrollState::new();
        s.set_extent(content, viewport);
        s
    }

    #[test]
    fn sticky_follows_growth_until_the_user_scrolls_up() {
        let mut s = st(100, 10);
        assert_eq!(s.offset(), 90);
        s.set_extent(120, 10);
        assert_eq!(s.offset(), 110);
        s.scroll_by(-5);
        assert!(!s.is_sticky());
        s.set_extent(130, 10);
        assert_eq!(s.offset(), 105); // did not follow
        s.scroll_by(1000);
        assert!(s.is_sticky());
        s.set_extent(140, 10);
        assert_eq!(s.offset(), 130);
    }

    #[test]
    fn page_and_half_page() {
        let mut s = st(100, 10);
        s.page_up();
        assert_eq!(s.offset(), 80);
        s.half_page_up();
        assert_eq!(s.offset(), 75);
        s.half_page_down();
        s.page_down();
        assert_eq!(s.offset(), 90);
        s.to_top();
        assert_eq!(s.offset(), 0);
        assert!(!s.is_sticky());
        s.to_bottom();
        assert!(s.is_sticky());
    }

    #[test]
    fn content_shorter_than_viewport_pins_to_zero() {
        let mut s = st(3, 10);
        assert_eq!(s.offset(), 0);
        s.scroll_by(-4);
        s.scroll_by(4);
        assert_eq!(s.offset(), 0);
        assert!(s.thumb_halves(10).is_none());
        assert_eq!(s.visible_range(), 0..3);
    }

    #[test]
    fn shrinking_content_clamps_offset() {
        let mut s = st(100, 10);
        s.scroll_by(-50);
        s.set_extent(20, 10);
        assert!(s.offset() <= 10);
    }

    #[test]
    fn reveal_moves_minimally() {
        let mut s = st(100, 10);
        s.to_top();
        s.reveal(5);
        assert_eq!(s.offset(), 0);
        s.reveal(15);
        assert_eq!(s.offset(), 6);
        s.reveal(2);
        assert_eq!(s.offset(), 2);
    }

    #[test]
    fn thumb_follows_opentuis_slider_not_the_visible_fraction() {
        // opencode 1.18.34 at 120x36: a 28 row track and 9 rows of overflow draw 5 half-cells,
        // rows 25 to 27 at the bottom and rows 0 to 2 at the top
        let mut s = st(37, 28);
        assert_eq!(s.thumb_halves(28), Some((51, 5)));
        s.to_top();
        assert_eq!(s.thumb_halves(28), Some((0, 5)));
        // at 80x24 the track is 16 rows and the thumb a single half-cell, row 15
        let s = st(60, 16);
        assert_eq!(s.thumb_halves(16), Some((31, 1)));
    }

    #[test]
    fn scrollbar_draws_thumb_and_survives_empty() {
        let mut s = st(100, 10);
        s.to_top();
        let area = Rect::new(0, 0, 1, 10);
        let mut b = Buffer::empty(area);
        Scrollbar {
            state: &s,
            track: Color::Black,
            thumb: Color::White,
        }
        .render(area, &mut b);
        // the thumb is one half-cell at the top of a 10 row track
        assert_eq!(b[(0, 0)].symbol(), "▀");
        assert_eq!(b[(0, 5)].symbol(), " ");
        let z = Rect::new(0, 0, 0, 0);
        let mut zb = Buffer::empty(z);
        Scrollbar {
            state: &s,
            track: Color::Black,
            thumb: Color::White,
        }
        .render(z, &mut zb);
    }
}
