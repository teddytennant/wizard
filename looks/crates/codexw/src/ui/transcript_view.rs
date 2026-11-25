// OWNER: app (the owned transcript)
//! The transcript the session draws itself when it owns the alternate screen: every committed
//! cell and the in-flight one, laid out at the screen width, scrollable, with a return-to-bottom
//! control in the row above the composer.
//!
//! Ported from Codex 0.174.0-alpha.6 (`openai/codex` 06f334aa), `codex-rs/tui/src/
//! transcript_view.rs` (positions anchored inside an entry, `scroll`, `jump_to_latest`),
//! `transcript_view/input.rs` (`JumpTarget`, `handle_scroll_key`, wheel accumulation) and
//! `transcript_view/follow_control.rs` (labels, hover, click), Apache-2.0. Modified: cells are
//! codexw's `HistoryCell`s laid out through the same wrapping the inline renderer uses; upstream's
//! selection, search, copy mode, disclosure, turn details and history paging are not here.

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
use ratatui::buffer::Buffer;
use ratatui::layout::{Position, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};

use super::BoxedCell;
use crate::style::palette;
use crate::term::history_insert::wrap_history_line;
use crate::wrap::line_width;

/// Codex `tui.mouse_scroll_speed`'s default: one row per wheel event.
pub const MOUSE_SCROLL_SPEED: f64 = 1.0;

/// Transcript jumps (Codex `transcript_view/input.rs` `JumpTarget`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JumpTarget {
    Beginning,
    Latest,
}

impl JumpTarget {
    /// Codex `JumpTarget::from_key`: Ctrl+Home and Ctrl+End, with Alt+< and Alt+> for terminals
    /// that do not send those.
    pub fn from_key(key: KeyEvent) -> Option<Self> {
        if key.kind == KeyEventKind::Release {
            return None;
        }
        match (key.code, key.modifiers) {
            (KeyCode::Home, KeyModifiers::CONTROL) => Some(Self::Beginning),
            (KeyCode::End, KeyModifiers::CONTROL) => Some(Self::Latest),
            // Zellij can omit the shifted alternate character from enhanced key events.
            (KeyCode::Char(','), m) if m == (KeyModifiers::ALT | KeyModifiers::SHIFT) => {
                Some(Self::Beginning)
            }
            (KeyCode::Char('.'), m) if m == (KeyModifiers::ALT | KeyModifiers::SHIFT) => {
                Some(Self::Latest)
            }
            // Terminals differ on whether shifted punctuation retains the Shift modifier.
            (KeyCode::Char('<'), m) if m.difference(KeyModifiers::SHIFT) == KeyModifiers::ALT => {
                Some(Self::Beginning)
            }
            (KeyCode::Char('>'), m) if m.difference(KeyModifiers::SHIFT) == KeyModifiers::ALT => {
                Some(Self::Latest)
            }
            _ => None,
        }
    }
}

/// A reading position inside an entry (Codex `Anchor`): rewrapping at another width keeps the
/// entry in view instead of turning the position into an unrelated screen row.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Anchor {
    /// Absolute index into the app's cells; `LIVE` for the in-flight cell.
    cell: usize,
    offset: usize,
}

const LIVE: usize = usize::MAX;

/// Codex `follow_control.rs` `FollowControl`.
#[derive(Default)]
struct FollowControl {
    area: Option<Rect>,
    pointer: Option<Position>,
    pressed: bool,
}

#[derive(Default)]
pub struct TranscriptView {
    // layout cache
    width: u16,
    raw: bool,
    from: usize,
    laid_out: usize,
    emitted: bool,
    rows: Vec<Line<'static>>,
    /// Row where each laid-out cell starts (its separator included), indexed from `from`.
    cell_start: Vec<usize>,
    // reading position: `None` follows the latest row
    anchor: Option<Anchor>,
    unseen_activity: bool,
    pending_mouse_scroll: f64,
    // from the last render
    area: Rect,
    total: usize,
    live_rows: usize,
    tail_visible: bool,
    follow: FollowControl,
}

impl TranscriptView {
    pub fn new() -> Self {
        Self {
            tail_visible: true,
            ..Default::default()
        }
    }

    /// Drop the layout; the next render lays everything out again. The reading position stays.
    pub fn invalidate(&mut self) {
        self.rows.clear();
        self.cell_start.clear();
        self.laid_out = self.from;
        self.emitted = false;
        self.width = 0;
    }

    /// Codex `is_following`.
    pub fn is_following(&self) -> bool {
        self.anchor.is_none()
    }

    /// Codex `can_return_to_latest`.
    pub fn can_return_to_latest(&self) -> bool {
        !self.is_following()
    }

    pub fn has_unseen_activity(&self) -> bool {
        self.unseen_activity
    }

    /// Codex `jump_to_latest`.
    pub fn jump_to_latest(&mut self) {
        self.anchor = None;
        self.unseen_activity = false;
    }

    /// Codex `jump_to_beginning`, without upstream's wait for older pages (all cells are here).
    pub fn jump_to_beginning(&mut self) {
        if self.total <= self.area.height as usize {
            return;
        }
        self.anchor = Some(self.anchor_for(0));
    }

    /// Bring the layout up to date with `cells[from..]` at `width`.
    pub fn sync(&mut self, cells: &[BoxedCell], from: usize, width: u16, raw: bool) {
        if width != self.width || raw != self.raw || from != self.from {
            self.from = from;
            self.invalidate();
            self.width = width;
            self.raw = raw;
        }
        if cells.len() < self.laid_out {
            self.invalidate();
            self.width = width;
        }
        let w = width.max(1) as usize;
        for cell in cells.iter().skip(self.laid_out) {
            self.cell_start.push(self.rows.len());
            let lines = if raw {
                cell.raw_lines()
            } else {
                cell.display_lines(width)
            };
            if lines.is_empty() {
                continue;
            }
            if !cell.is_stream_continuation() {
                if self.emitted {
                    self.rows.push(Line::default());
                } else {
                    self.emitted = true;
                }
            }
            for line in lines {
                let pieces = if raw {
                    vec![line]
                } else {
                    wrap_history_line(&line, w)
                };
                for p in pieces {
                    self.rows.extend(hard_wrap(p, w));
                }
            }
        }
        self.laid_out = cells.len().max(self.from);
    }

    fn anchor_for(&self, row: usize) -> Anchor {
        if row >= self.rows.len() {
            return Anchor {
                cell: LIVE,
                offset: row - self.rows.len(),
            };
        }
        let i = self
            .cell_start
            .partition_point(|&s| s <= row)
            .saturating_sub(1);
        Anchor {
            cell: self.from + i,
            offset: row - self.cell_start.get(i).copied().unwrap_or(0),
        }
    }

    fn top_for(&self, a: Anchor) -> usize {
        if a.cell == LIVE {
            return self.rows.len() + a.offset;
        }
        if a.cell < self.from {
            return 0;
        }
        let i = a.cell - self.from;
        let Some(&start) = self.cell_start.get(i) else {
            return self.rows.len();
        };
        let end = self
            .cell_start
            .get(i + 1)
            .copied()
            .unwrap_or(self.rows.len());
        (start + a.offset).min(end.saturating_sub(1).max(start))
    }

    fn max_top(&self) -> usize {
        self.total.saturating_sub(self.area.height as usize)
    }

    fn current_top(&self) -> usize {
        match self.anchor {
            None => self.max_top(),
            Some(a) => self.top_for(a).min(self.max_top()),
        }
    }

    /// Codex `scroll`: move the reading position by `rows`; reaching the bottom going down
    /// follows the latest row again.
    pub fn scroll(&mut self, rows: isize) {
        if self.area.is_empty() || rows == 0 {
            return;
        }
        let top = self.current_top();
        let max = self.max_top();
        if rows < 0 && top == 0 {
            return;
        }
        let next = (top as isize + rows).clamp(0, max as isize) as usize;
        if rows > 0 && next >= max {
            self.jump_to_latest();
        } else {
            self.anchor = Some(self.anchor_for(next));
        }
    }

    /// Codex `handle_scroll_key`: page keys (one row of overlap), the jumps, and Esc back to
    /// the latest row while reading. True when the key was the transcript's.
    pub fn handle_scroll_key(&mut self, key: KeyEvent) -> bool {
        if key.kind == KeyEventKind::Release {
            return false;
        }
        let jump = JumpTarget::from_key(key);
        match (key.code, key.modifiers) {
            (KeyCode::PageUp | KeyCode::PageDown, KeyModifiers::NONE) => {
                let height = self.area.height.max(1) as isize;
                let page = (height - 1).max(1);
                self.scroll(if key.code == KeyCode::PageUp {
                    -page
                } else {
                    page
                });
            }
            (KeyCode::Esc, KeyModifiers::NONE) if self.can_return_to_latest() => {
                self.jump_to_latest();
            }
            _ if jump == Some(JumpTarget::Beginning) => self.jump_to_beginning(),
            _ if jump == Some(JumpTarget::Latest) => self.jump_to_latest(),
            _ => return false,
        }
        true
    }

    /// Wheel and pointer input. True when the view changed and needs a frame.
    pub fn handle_mouse(&mut self, event: MouseEvent) -> bool {
        if self.handle_follow_control_mouse(event) {
            return true;
        }
        let point = Position::new(event.column, event.row);
        if !self.area.contains(point) {
            return false;
        }
        match event.kind {
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                // Codex `handle_mouse_with_mode`: fractional speeds accumulate, and a change of
                // direction drops the remainder.
                let direction = if event.kind == MouseEventKind::ScrollUp {
                    -1.0
                } else {
                    1.0
                };
                if self.pending_mouse_scroll.signum() != direction {
                    self.pending_mouse_scroll = 0.0;
                }
                self.pending_mouse_scroll += direction * MOUSE_SCROLL_SPEED;
                let rounded = self.pending_mouse_scroll.round();
                if rounded != 0.0 && (self.pending_mouse_scroll - rounded).abs() < 1e-9 {
                    self.pending_mouse_scroll = rounded;
                }
                let rows = self.pending_mouse_scroll as isize;
                self.pending_mouse_scroll = self.pending_mouse_scroll.fract();
                if rows != 0 {
                    self.scroll(rows);
                }
                true
            }
            _ => false,
        }
    }

    /// Paint the transcript into `area`: from the top while it fits, its last rows while
    /// following, the anchored rows while reading. `live` is the in-flight cell.
    pub fn render(&mut self, live: &[Line<'static>], area: Rect, buf: &mut Buffer) {
        let w = area.width.max(1) as usize;
        let mut tail: Vec<Line<'static>> = Vec::new();
        if !live.is_empty() {
            if !self.rows.is_empty() {
                tail.push(Line::default());
            }
            for l in live {
                tail.extend(hard_wrap(l.clone(), w));
            }
        }
        let total = self.rows.len() + tail.len();
        if !self.is_following() && total > self.total && self.total > 0 {
            self.unseen_activity = true;
        }
        self.area = area;
        self.total = total;
        self.live_rows = tail.len();
        let top = self.current_top();
        if self.anchor.is_some() && top >= self.max_top() {
            // The reading position caught up with the bottom: follow again.
            self.jump_to_latest();
        }
        let h = area.height as usize;
        self.tail_visible = top + h >= total;
        for i in 0..h.min(total.saturating_sub(top)) {
            let row = top + i;
            let line = if row < self.rows.len() {
                &self.rows[row]
            } else {
                &tail[row - self.rows.len()]
            };
            let y = area.y + i as u16;
            if line.style.bg.is_some() {
                tuikit::paint::fill(buf, Rect::new(area.x, y, area.width, 1), line.style);
            }
            tuikit::paint::put_line(buf, area.x, y, line, area);
        }
    }

    /// Codex `follow_control.rs` `follow_control_label`.
    pub fn follow_control_label(&self, width: u16) -> Option<&'static str> {
        if self.area.is_empty() || self.tail_visible {
            return None;
        }
        let labels = if self.unseen_activity {
            [
                " New activity · ↓ Back to bottom · esc ",
                " New activity · ↓ Bottom ",
                " New · ↓ Bottom ",
                " ↓ Bottom ",
                " ↓ ",
            ]
        } else {
            [
                " ↓ Back to bottom · esc ",
                " ↓ Back to bottom ",
                " ↓ Bottom · esc ",
                " ↓ Bottom ",
                " ↓ ",
            ]
        };
        labels
            .into_iter()
            .find(|label| crate::wrap::width_of(label) <= usize::from(width))
    }

    /// Codex `render_follow_control`: the label centred in the gap row above the composer,
    /// reversed and bold under the pointer.
    pub fn render_follow_control(&mut self, area: Option<Rect>, buf: &mut Buffer) {
        let Some(area) = area.filter(|a| !a.is_empty()) else {
            self.follow = FollowControl::default();
            return;
        };
        let Some(label) = self.follow_control_label(area.width) else {
            self.follow = FollowControl::default();
            return;
        };
        let width = crate::wrap::width_of(label) as u16;
        let target = Rect::new(area.x + (area.width - width) / 2, area.y, width, 1);
        let hovered = self.follow.pointer.is_some_and(|p| target.contains(p));
        self.follow.area = Some(target);
        let p = palette();
        let accent = p.accent().fg;
        let mut style: Style = p.user_message_style();
        if let Some(fg) = accent {
            style = style.fg(fg);
        }
        let style = if hovered {
            style.reversed().bold()
        } else {
            style
        };
        tuikit::paint::put_line(buf, target.x, target.y, &Line::styled(label, style), target);
    }

    /// Codex `handle_follow_control_mouse`: press and release on the control returns to the
    /// latest row; moving onto or off it changes the hover.
    fn handle_follow_control_mouse(&mut self, event: MouseEvent) -> bool {
        let Some(area) = self.follow.area else {
            return false;
        };
        let point = Position::new(event.column, event.row);
        let was_hovered = self.follow.pointer.is_some_and(|p| area.contains(p));
        let inside = area.contains(point);
        self.follow.pointer = Some(point);
        match event.kind {
            MouseEventKind::Down(MouseButton::Left) if inside => {
                self.follow.pressed = true;
                true
            }
            MouseEventKind::Up(MouseButton::Left) if self.follow.pressed => {
                self.follow.pressed = false;
                if inside {
                    self.jump_to_latest();
                }
                true
            }
            MouseEventKind::Drag(MouseButton::Left) if self.follow.pressed => true,
            MouseEventKind::Moved if inside != was_hovered => true,
            _ => false,
        }
    }

    /// Rows laid out (committed cells only), for tests.
    pub fn committed_rows(&self) -> usize {
        self.rows.len()
    }
}

/// Break a row that is still wider than `width` (a URL kept whole, raw text) at the columns, as
/// the terminal would; continuation rows keep the line's style.
pub fn hard_wrap(line: Line<'static>, width: usize) -> Vec<Line<'static>> {
    if width == 0 || line_width(&line) <= width {
        return vec![line];
    }
    let style = line.style;
    let mut out = Vec::new();
    let mut cur: Vec<Span<'static>> = Vec::new();
    let mut cur_w = 0usize;
    for span in line.spans {
        let mut piece = String::new();
        for ch in span.content.chars() {
            let cw = crate::width::char_width(ch);
            if cur_w + cw > width && cur_w > 0 {
                if !piece.is_empty() {
                    cur.push(Span::styled(std::mem::take(&mut piece), span.style));
                }
                out.push(Line::from(std::mem::take(&mut cur)).style(style));
                cur_w = 0;
            }
            piece.push(ch);
            cur_w += cw;
        }
        if !piece.is_empty() {
            cur.push(Span::styled(piece, span.style));
        }
    }
    if !cur.is_empty() {
        out.push(Line::from(cur).style(style));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::HistoryCell;

    #[derive(Debug)]
    struct Text(Vec<&'static str>);

    impl HistoryCell for Text {
        fn display_lines(&self, _width: u16) -> Vec<Line<'static>> {
            self.0.iter().map(|s| Line::from(*s)).collect()
        }
    }

    fn cells(n: usize) -> Vec<BoxedCell> {
        (0..n)
            .map(|_| Box::new(Text(vec!["a", "b", "c"])) as BoxedCell)
            .collect()
    }

    fn view(cells: &[BoxedCell], h: u16) -> (TranscriptView, Buffer) {
        let mut v = TranscriptView::new();
        v.sync(cells, 0, 20, false);
        let area = Rect::new(0, 0, 20, h);
        let mut buf = Buffer::empty(area);
        v.render(&[], area, &mut buf);
        (v, buf)
    }

    #[test]
    fn cells_are_separated_by_one_blank_row() {
        let c = cells(3);
        let (v, _) = view(&c, 5);
        // 3 rows each, 2 separators
        assert_eq!(v.committed_rows(), 11);
    }

    #[test]
    fn scrolling_up_reads_and_down_to_the_bottom_follows_again() {
        let c = cells(10);
        let (mut v, mut buf) = view(&c, 5);
        assert!(v.is_following());
        assert!(v.follow_control_label(80).is_none());
        v.scroll(-3);
        assert!(!v.is_following());
        v.render(&[], Rect::new(0, 0, 20, 5), &mut buf);
        assert_eq!(v.follow_control_label(80), Some(" ↓ Back to bottom · esc "));
        assert_eq!(v.follow_control_label(12), Some(" ↓ Bottom "));
        v.scroll(3);
        assert!(v.is_following());
    }

    #[test]
    fn reading_keeps_its_place_and_notices_new_activity() {
        let mut c = cells(10);
        let (mut v, mut buf) = view(&c, 5);
        v.scroll(-10);
        let top = v.current_top();
        c.push(Box::new(Text(vec!["new"])));
        v.sync(&c, 0, 20, false);
        v.render(&[], Rect::new(0, 0, 20, 5), &mut buf);
        assert_eq!(v.current_top(), top);
        assert!(v.has_unseen_activity());
        assert_eq!(
            v.follow_control_label(80),
            Some(" New activity · ↓ Back to bottom · esc ")
        );
    }

    #[test]
    fn a_resize_keeps_the_entry_in_view() {
        let c = cells(10);
        let (mut v, mut buf) = view(&c, 5);
        v.scroll(-20);
        let a = v.anchor.unwrap();
        v.sync(&c, 0, 10, false);
        let mut small = Buffer::empty(Rect::new(0, 0, 10, 5));
        v.render(&[], Rect::new(0, 0, 10, 5), &mut small);
        assert_eq!(v.anchor_for(v.current_top()).cell, a.cell);
        let _ = &mut buf;
    }

    #[test]
    fn keys_page_jump_and_esc() {
        let c = cells(10);
        let (mut v, _) = view(&c, 5);
        assert!(!v.handle_scroll_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)));
        assert!(v.handle_scroll_key(KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE)));
        assert_eq!(v.current_top(), v.max_top() - 4);
        assert!(v.handle_scroll_key(KeyEvent::new(KeyCode::Home, KeyModifiers::CONTROL)));
        assert_eq!(v.current_top(), 0);
        assert!(v.handle_scroll_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)));
        assert!(v.is_following());
        assert_eq!(
            JumpTarget::from_key(KeyEvent::new(KeyCode::Char('>'), KeyModifiers::ALT)),
            Some(JumpTarget::Latest)
        );
    }

    #[test]
    fn wide_rows_break_at_the_edge() {
        let rows = hard_wrap(Line::from("https://example.com/a/very/long/path"), 10);
        assert_eq!(rows.len(), 4);
        assert!(rows.iter().all(|r| line_width(r) <= 10));
    }
}
