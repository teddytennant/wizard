// OWNER: renderer
//! The inline terminal: a bottom viewport inside the terminal's own screen, with finalized
//! history written into scrollback above it.
//!
//! Ratatui's `Viewport::Inline` and `insert_before` are not used. Codex owns the viewport rect,
//! the double buffer and the scrollback writes, so this module follows that design
//! (`custom_terminal.rs` / `tui.rs` in Codex 0.147.0, spec Part A.4, A.16) and reproduces its
//! byte stream. The diff and draw loop is derived from ratatui's own `Terminal` (MIT).

use std::io::{self, Write};

use crossterm::cursor::{MoveTo, SetCursorStyle};
use crossterm::queue;
use crossterm::style::{
    Attribute as CAttribute, Colors, Print, SetAttribute, SetBackgroundColor, SetColors,
    SetForegroundColor,
};
use crossterm::terminal::{Clear, ClearType};
use ratatui::buffer::{Buffer, Cell};
use ratatui::layout::{Position, Rect, Size};
use ratatui::prelude::IntoCrossterm;
use ratatui::style::{Color, Modifier};
use ratatui::text::Line;
use ratatui::widgets::Widget;
use unicode_width::UnicodeWidthStr;

use super::history_insert;

/// `CSI ? 2026 h`: begin synchronized update.
pub const SYNC_BEGIN: &str = "\x1b[?2026h";
/// `CSI ? 2026 l`: end synchronized update.
pub const SYNC_END: &str = "\x1b[?2026l";
/// Reset scroll region, attributes, home, clear screen, purge scrollback, home. One write.
pub const CLEAR_ALL: &str = "\x1b[r\x1b[0m\x1b[H\x1b[2J\x1b[3J\x1b[H";

pub struct Frame<'a> {
    cursor_position: Option<Position>,
    cursor_style: SetCursorStyle,
    area: Rect,
    buffer: &'a mut Buffer,
}

impl Frame<'_> {
    pub fn area(&self) -> Rect {
        self.area
    }
    pub fn buffer_mut(&mut self) -> &mut Buffer {
        self.buffer
    }
    pub fn render_widget<W: Widget>(&mut self, widget: W, area: Rect) {
        widget.render(area, self.buffer);
    }
    /// Show the cursor at `position` after the frame. Without this call the cursor is hidden.
    pub fn set_cursor_position(&mut self, position: impl Into<Position>) {
        self.cursor_position = Some(position.into());
    }
    pub fn set_cursor_style(&mut self, style: SetCursorStyle) {
        self.cursor_style = style;
    }
}

/// How history lines meet the terminal's right edge.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WrapPolicy {
    /// Wrapped at word boundaries before they are written (rich cells).
    PreWrap,
    /// Written as they are; the terminal breaks the rows, so a selection copies the source
    /// text (raw output mode).
    Terminal,
}

pub struct InlineTerminal<W: Write> {
    out: W,
    buffers: [Buffer; 2],
    current: usize,
    pub viewport_area: Rect,
    pub last_known_screen_size: Size,
    /// Position of the last cell written by `flush`, or set explicitly. History insertion
    /// restores the cursor here.
    pub last_known_cursor_pos: Position,
    hidden_cursor: bool,
    alt_saved: Option<Rect>,
    alt_active: bool,
    pending_history: Vec<(Line<'static>, WrapPolicy)>,
}

impl<W: Write> InlineTerminal<W> {
    /// `cursor` is the probed cursor position (0-based); the viewport starts there as an
    /// empty rect, so the first frame draws directly under the shell prompt.
    pub fn new(out: W, screen: Size, cursor: Position) -> Self {
        Self {
            out,
            buffers: [Buffer::empty(Rect::ZERO), Buffer::empty(Rect::ZERO)],
            current: 0,
            viewport_area: Rect::new(0, cursor.y, 0, 0),
            last_known_screen_size: screen,
            last_known_cursor_pos: cursor,
            hidden_cursor: false,
            alt_saved: None,
            alt_active: false,
            pending_history: Vec::new(),
        }
    }

    pub fn writer(&mut self) -> &mut W {
        &mut self.out
    }
    pub fn alt_active(&self) -> bool {
        self.alt_active
    }
    pub fn pending_history_len(&self) -> usize {
        self.pending_history.len()
    }

    /// Queue finalized lines for the scrollback; they are written at the start of the next draw.
    pub fn queue_history(&mut self, lines: Vec<Line<'static>>) {
        self.queue_history_with(lines, WrapPolicy::PreWrap);
    }

    pub fn queue_history_with(&mut self, lines: Vec<Line<'static>>, policy: WrapPolicy) {
        self.pending_history
            .extend(lines.into_iter().map(|l| (l, policy)));
    }

    /// Drop queued history, used when a resize makes the wrap width stale.
    pub fn clear_pending_history(&mut self) {
        self.pending_history.clear();
    }

    pub fn set_viewport_area(&mut self, area: Rect) {
        self.buffers[0].resize(area);
        self.buffers[1].resize(area);
        self.viewport_area = area;
    }

    /// Force the next draw to repaint the whole viewport.
    pub fn invalidate_viewport(&mut self) {
        self.buffers[1 - self.current].reset();
    }

    /// Clear from `pos` to the end of the screen and force a full repaint.
    pub fn clear_after_position(&mut self, pos: Position) -> io::Result<()> {
        queue!(
            self.out,
            MoveTo(pos.x, pos.y),
            Clear(ClearType::FromCursorDown)
        )?;
        self.buffers[1 - self.current].reset();
        Ok(())
    }

    /// Clear the viewport (used on exit so the composer disappears and the cursor rests at the
    /// viewport's top-left).
    pub fn clear(&mut self) -> io::Result<()> {
        if self.viewport_area.is_empty() {
            return Ok(());
        }
        self.clear_after_position(self.viewport_area.as_position())?;
        self.out.flush()
    }

    /// Wipe the visible screen and the terminal's scrollback; the viewport goes back to row 0.
    pub fn clear_scrollback_and_screen(&mut self) -> io::Result<()> {
        self.out.write_all(CLEAR_ALL.as_bytes())?;
        self.out.flush()?;
        self.last_known_cursor_pos = Position::new(0, 0);
        self.buffers[1 - self.current].reset();
        self.viewport_area.y = 0;
        Ok(())
    }

    /// Wipe the visible screen only (alt-screen replay).
    pub fn clear_visible_screen(&mut self) -> io::Result<()> {
        queue!(self.out, MoveTo(0, 0), Clear(ClearType::All), MoveTo(0, 0))?;
        self.out.flush()?;
        self.last_known_cursor_pos = Position::new(0, 0);
        self.buffers[1 - self.current].reset();
        Ok(())
    }

    pub fn hide_cursor(&mut self) -> io::Result<()> {
        queue!(self.out, crossterm::cursor::Hide)?;
        self.hidden_cursor = true;
        Ok(())
    }
    pub fn show_cursor(&mut self) -> io::Result<()> {
        queue!(self.out, crossterm::cursor::Show)?;
        self.hidden_cursor = false;
        Ok(())
    }

    // ---- frame path -------------------------------------------------------------------------

    /// Draw one chat frame: reposition the viewport for `height` rows, write queued history above
    /// it, diff and flush the viewport, place the cursor. All inside one synchronized update.
    pub fn draw(
        &mut self,
        height: u16,
        screen: Size,
        render: impl FnOnce(&mut Frame),
    ) -> io::Result<()> {
        self.out.write_all(SYNC_BEGIN.as_bytes())?;
        let r = (|| {
            let full = self.update_inline_viewport(height, screen)?;
            self.flush_pending_history()?;
            if full {
                self.invalidate_viewport();
            }
            self.draw_with_size(screen, render)
        })();
        self.out.write_all(SYNC_END.as_bytes())?;
        self.out.flush()?;
        r
    }

    /// Reposition for the resize-reflow path (Codex `update_inline_viewport_for_resize_reflow`).
    /// Returns true when the viewport rect changed and everything must be repainted.
    fn update_inline_viewport(&mut self, height: u16, screen: Size) -> io::Result<bool> {
        let shrank = screen.height < self.last_known_screen_size.height;
        let grew = screen.height > self.last_known_screen_size.height;
        let was_bottom = self.viewport_area.bottom() == self.last_known_screen_size.height;
        let prev = self.viewport_area;

        let mut area = self.viewport_area;
        area.height = height.min(screen.height);
        area.width = screen.width;
        let mut full = false;

        if area.bottom() > screen.height {
            let by = area.bottom() - screen.height;
            if !shrank {
                self.scroll_region_up(0..area.top(), by)?;
            }
            area.y = screen.height - area.height;
        } else if grew && was_bottom {
            area.y = screen.height - area.height;
        }

        if area != self.viewport_area {
            let clear_pos = Position::new(0, prev.y.min(area.y));
            self.set_viewport_area(area);
            self.clear_after_position(clear_pos)?;
            full = true;
        }
        Ok(full)
    }

    /// `CSI first;last r`, `CSI n S`, `CSI r`: push the rows of `region` up by `amount`, the top
    /// ones into scrollback when the region starts at row 0.
    fn scroll_region_up(&mut self, region: std::ops::Range<u16>, amount: u16) -> io::Result<()> {
        if region.is_empty() || amount == 0 {
            return Ok(());
        }
        write!(
            self.out,
            "\x1b[{};{}r\x1b[{}S\x1b[r",
            region.start + 1,
            region.end,
            amount
        )
    }

    fn flush_pending_history(&mut self) -> io::Result<()> {
        if self.pending_history.is_empty() || self.alt_active {
            return Ok(());
        }
        let lines = std::mem::take(&mut self.pending_history);
        // One insertion per run of the same policy keeps the order and the scroll maths.
        let mut run: Vec<Line<'static>> = Vec::new();
        let mut policy = WrapPolicy::PreWrap;
        for (line, p) in lines {
            if !run.is_empty() && p != policy {
                history_insert::insert_history_lines(self, std::mem::take(&mut run), policy)?;
            }
            policy = p;
            run.push(line);
        }
        if !run.is_empty() {
            history_insert::insert_history_lines(self, run, policy)?;
        }
        Ok(())
    }

    /// The part of `draw` after the viewport is positioned: render, flush, cursor, swap.
    pub fn draw_with_size(
        &mut self,
        screen: Size,
        render: impl FnOnce(&mut Frame),
    ) -> io::Result<()> {
        if screen != self.last_known_screen_size {
            self.last_known_screen_size = screen;
        }
        let area = self.viewport_area;
        let cur = self.current;
        let mut frame = Frame {
            cursor_position: None,
            cursor_style: SetCursorStyle::DefaultUserShape,
            area,
            buffer: &mut self.buffers[cur],
        };
        render(&mut frame);
        let cursor_position = frame.cursor_position;
        let cursor_style = frame.cursor_style;

        self.flush()?;

        match cursor_position {
            None => self.hide_cursor()?,
            Some(p) => {
                queue!(self.out, cursor_style)?;
                self.show_cursor()?;
                queue!(self.out, MoveTo(p.x, p.y))?;
                self.last_known_cursor_pos = p;
            }
        }
        self.buffers[1 - self.current].reset();
        self.current = 1 - self.current;
        self.out.flush()
    }

    fn flush(&mut self) -> io::Result<()> {
        let updates = diff_buffers(&self.buffers[1 - self.current], &self.buffers[self.current]);
        if let Some(DrawCommand::Put { x, y, .. }) = updates
            .iter()
            .rfind(|c| matches!(c, DrawCommand::Put { .. }))
        {
            self.last_known_cursor_pos = Position::new(*x, *y);
        }
        draw(&mut self.out, updates.into_iter())
    }

    // ---- alternate screen -------------------------------------------------------------------

    /// `?1049h`, `?1007h`, viewport becomes the whole screen.
    pub fn enter_alt_screen(&mut self, screen: Size) -> io::Result<()> {
        if self.alt_active {
            return Ok(());
        }
        self.out.write_all(b"\x1b[?1049h\x1b[?1007h")?;
        self.alt_saved = Some(self.viewport_area);
        self.last_known_screen_size = screen;
        self.set_viewport_area(Rect::new(0, 0, screen.width, screen.height));
        self.clear()?;
        self.alt_active = true;
        Ok(())
    }

    /// `?1007l`, `?1049l`, the inline viewport comes back.
    pub fn leave_alt_screen(&mut self) -> io::Result<()> {
        if !self.alt_active {
            return Ok(());
        }
        self.out.write_all(b"\x1b[?1007l\x1b[?1049l")?;
        if let Some(saved) = self.alt_saved.take() {
            self.set_viewport_area(saved);
        }
        self.invalidate_viewport();
        self.alt_active = false;
        self.out.flush()
    }

    /// One overlay frame on the alternate screen (full-screen viewport, no history flush).
    pub fn draw_alt(&mut self, screen: Size, render: impl FnOnce(&mut Frame)) -> io::Result<()> {
        self.out.write_all(SYNC_BEGIN.as_bytes())?;
        let r = (|| {
            let full = Rect::new(0, 0, screen.width, screen.height);
            if self.viewport_area != full {
                self.set_viewport_area(full);
                self.clear_after_position(Position::new(0, 0))?;
            }
            self.draw_with_size(screen, render)
        })();
        self.out.write_all(SYNC_END.as_bytes())?;
        self.out.flush()?;
        r
    }
}

impl<W: Write> Drop for InlineTerminal<W> {
    fn drop(&mut self) {
        let _ = queue!(self.out, SetCursorStyle::DefaultUserShape);
        if self.hidden_cursor {
            let _ = queue!(self.out, crossterm::cursor::Show);
        }
        let _ = self.out.flush();
    }
}

// ---- diff and draw ------------------------------------------------------------------------

#[derive(Debug)]
enum DrawCommand {
    Put { x: u16, y: u16, cell: Cell },
    ClearToEnd { x: u16, y: u16, bg: Color },
}

fn cell_width(c: &Cell) -> usize {
    UnicodeWidthStr::width(c.symbol()).max(1)
}

fn diff_buffers(a: &Buffer, b: &Buffer) -> Vec<DrawCommand> {
    let next = &b.content;
    let w = a.area.width as usize;
    let mut updates = vec![];
    let mut last_nonblank = vec![0u16; a.area.height as usize];
    for y in 0..a.area.height {
        let row = &next[y as usize * w..(y as usize + 1) * w];
        let bg = row.last().map(|c| c.bg).unwrap_or(Color::Reset);
        // The rightmost column that still matters: a glyph, a bg that differs from the row's
        // trailing bg, or a modifier. The rest of the row is one clear-to-end.
        let mut last = 0usize;
        let mut col = 0usize;
        while col < row.len() {
            let cell = &row[col];
            let cw = cell_width(cell);
            if cell.symbol() != " " || cell.bg != bg || cell.modifier != Modifier::empty() {
                last = col + cw.saturating_sub(1);
            }
            col += cw;
        }
        if last + 1 < row.len() {
            updates.push(DrawCommand::ClearToEnd {
                x: a.area.x + last as u16 + 1,
                y: a.area.y + y,
                bg,
            });
        }
        last_nonblank[y as usize] = last as u16;
    }
    let mut cells = a.diff(b);
    cells.sort_unstable_by_key(|(x, y, _)| (*y, *x));
    cells.dedup_by_key(|(x, y, _)| (*y, *x));
    for (x, y, cell) in cells {
        let row = (y - a.area.y) as usize;
        if x - a.area.x <= last_nonblank[row] {
            updates.push(DrawCommand::Put {
                x,
                y,
                cell: cell.clone(),
            });
        }
    }
    updates
}

fn draw(w: &mut impl Write, commands: impl Iterator<Item = DrawCommand>) -> io::Result<()> {
    let mut fg = Color::Reset;
    let mut bg = Color::Reset;
    let mut modifier = Modifier::empty();
    let mut link: Option<String> = None;
    let mut last_pos: Option<Position> = None;
    for command in commands {
        let (x, y) = match &command {
            DrawCommand::Put { x, y, .. } | DrawCommand::ClearToEnd { x, y, .. } => (*x, *y),
        };
        if !matches!(last_pos, Some(p) if x == p.x + 1 && y == p.y) {
            queue!(w, MoveTo(x, y))?;
        }
        last_pos = Some(Position::new(x, y));
        // A link never spans a jump or a clear: close it before the cursor moves on.
        let url = match &command {
            DrawCommand::Put { cell, .. } => crate::hyperlink::url_of(Some(cell.underline_color)),
            DrawCommand::ClearToEnd { .. } => None,
        };
        if url != link {
            write!(w, "{}", crate::hyperlink::osc8(url.as_deref()))?;
            link = url;
        }
        match &command {
            DrawCommand::Put { cell, .. } => {
                if cell.modifier != modifier {
                    modifier_diff(w, modifier, cell.modifier)?;
                    modifier = cell.modifier;
                }
                if cell.fg != fg || cell.bg != bg {
                    queue!(
                        w,
                        SetColors(Colors::new(
                            cell.fg.into_crossterm(),
                            cell.bg.into_crossterm()
                        ))
                    )?;
                    fg = cell.fg;
                    bg = cell.bg;
                }
                queue!(w, Print(cell.symbol()))?;
            }
            DrawCommand::ClearToEnd { bg: clear_bg, .. } => {
                queue!(w, SetAttribute(CAttribute::Reset))?;
                modifier = Modifier::empty();
                queue!(w, SetBackgroundColor((*clear_bg).into_crossterm()))?;
                bg = *clear_bg;
                queue!(w, Clear(ClearType::UntilNewLine))?;
            }
        }
    }
    if link.is_some() {
        write!(w, "{}", crate::hyperlink::osc8(None))?;
    }
    queue!(
        w,
        SetForegroundColor(crossterm::style::Color::Reset),
        SetBackgroundColor(crossterm::style::Color::Reset),
        SetAttribute(CAttribute::Reset),
    )
}

/// SGR transitions between two modifier sets, in Codex's order.
pub fn modifier_diff(w: &mut impl Write, from: Modifier, to: Modifier) -> io::Result<()> {
    let removed = from - to;
    if removed.contains(Modifier::REVERSED) {
        queue!(w, SetAttribute(CAttribute::NoReverse))?;
    }
    if removed.contains(Modifier::BOLD) {
        queue!(w, SetAttribute(CAttribute::NormalIntensity))?;
        if to.contains(Modifier::DIM) {
            queue!(w, SetAttribute(CAttribute::Dim))?;
        }
    }
    if removed.contains(Modifier::ITALIC) {
        queue!(w, SetAttribute(CAttribute::NoItalic))?;
    }
    if removed.contains(Modifier::UNDERLINED) {
        queue!(w, SetAttribute(CAttribute::NoUnderline))?;
    }
    if removed.contains(Modifier::DIM) {
        queue!(w, SetAttribute(CAttribute::NormalIntensity))?;
    }
    if removed.contains(Modifier::CROSSED_OUT) {
        queue!(w, SetAttribute(CAttribute::NotCrossedOut))?;
    }
    if removed.intersects(Modifier::SLOW_BLINK | Modifier::RAPID_BLINK) {
        queue!(w, SetAttribute(CAttribute::NoBlink))?;
    }
    let added = to - from;
    if added.contains(Modifier::REVERSED) {
        queue!(w, SetAttribute(CAttribute::Reverse))?;
    }
    if added.contains(Modifier::BOLD) {
        queue!(w, SetAttribute(CAttribute::Bold))?;
    }
    if added.contains(Modifier::ITALIC) {
        queue!(w, SetAttribute(CAttribute::Italic))?;
    }
    if added.contains(Modifier::UNDERLINED) {
        queue!(w, SetAttribute(CAttribute::Underlined))?;
    }
    if added.contains(Modifier::DIM) {
        queue!(w, SetAttribute(CAttribute::Dim))?;
    }
    if added.contains(Modifier::CROSSED_OUT) {
        queue!(w, SetAttribute(CAttribute::CrossedOut))?;
    }
    if added.contains(Modifier::SLOW_BLINK) {
        queue!(w, SetAttribute(CAttribute::SlowBlink))?;
    }
    if added.contains(Modifier::RAPID_BLINK) {
        queue!(w, SetAttribute(CAttribute::RapidBlink))?;
    }
    Ok(())
}
