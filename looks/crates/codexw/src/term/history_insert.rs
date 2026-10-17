// OWNER: renderer
//! Write finalized history rows into the terminal's scrollback above the inline viewport.
//!
//! Scroll-region trick, Codex `insert_history.rs` (spec A.4.4): push the viewport down with
//! reverse index when it is not yet at the bottom, then set a region covering the rows above
//! it and let `\r\n` at the region's bottom push rows into scrollback.

use std::io::{self, Write};

use crossterm::cursor::{MoveDown, MoveTo, MoveToColumn, RestorePosition, SavePosition};
use crossterm::queue;
use crossterm::style::{
    Attribute as CAttribute, Color as CColor, Colors, Print, SetAttribute, SetBackgroundColor,
    SetColors, SetForegroundColor,
};
use crossterm::terminal::{Clear, ClearType};
use ratatui::layout::Rect;
use ratatui::prelude::IntoCrossterm;
use ratatui::style::{Color, Modifier};
use ratatui::text::{Line, Span};

use super::inline::{InlineTerminal, WrapPolicy, modifier_diff};

use crate::wrap::{
    WrapOpts, adaptive_wrap_line, line_contains_url_like, line_has_mixed_url_and_non_url_tokens,
    line_width,
};

/// The leading whitespace of a line as a styled prefix, used as the continuation indent.
fn leading_whitespace_prefix(line: &Line<'_>) -> Line<'static> {
    let mut spans = Vec::new();
    for span in &line.spans {
        let end = span
            .content
            .char_indices()
            .find_map(|(i, ch)| (!ch.is_whitespace()).then_some(i))
            .unwrap_or(span.content.len());
        if end > 0 {
            spans.push(Span::styled(span.content[..end].to_string(), span.style));
        }
        if end < span.content.len() {
            break;
        }
    }
    Line::from(spans).style(line.style)
}

/// Wrap one history line to `width`. A URL-only line stays whole so the terminal's own wrap keeps
/// it clickable; otherwise words wrap and continuation rows keep the leading indent.
pub fn wrap_history_line(line: &Line<'static>, width: usize) -> Vec<Line<'static>> {
    if line_contains_url_like(line) && !line_has_mixed_url_and_non_url_tokens(line) {
        return vec![line.clone()];
    }
    let opts = WrapOpts::new(width).subsequent_indent(leading_whitespace_prefix(line));
    adaptive_wrap_line(line, &opts)
}

pub fn insert_history_lines<W: Write>(
    term: &mut InlineTerminal<W>,
    lines: Vec<Line<'static>>,
    policy: WrapPolicy,
) -> io::Result<()> {
    let screen = term.last_known_screen_size;
    let mut area = term.viewport_area;
    let mut update_area = false;
    let last_cursor = term.last_known_cursor_pos;
    let wrap_width = area.width.max(1) as usize;

    let mut wrapped: Vec<Line<'static>> = Vec::new();
    let mut rows = 0usize;
    for line in &lines {
        let pieces = match policy {
            WrapPolicy::PreWrap => wrap_history_line(line, wrap_width),
            WrapPolicy::Terminal => vec![line.clone()],
        };
        for l in pieces {
            rows += line_width(&l).max(1).div_ceil(wrap_width);
            wrapped.push(l);
        }
    }
    let rows = rows as u16;

    let w = term.writer();
    let cursor_top = if area.bottom() < screen.height {
        // Not at the bottom yet: push the viewport down to make room, never past the bottom.
        let scroll = rows.min(screen.height - area.bottom());
        write!(w, "\x1b[{};{}r", area.top() + 1, screen.height)?;
        queue!(w, MoveTo(0, area.top()))?;
        for _ in 0..scroll {
            queue!(w, Print("\x1bM"))?;
        }
        write!(w, "\x1b[r")?;
        let top = area.top().saturating_sub(1);
        area.y += scroll;
        update_area = true;
        top
    } else {
        area.top().saturating_sub(1)
    };

    // Rows above the viewport become the scroll region; the cursor starts on its last row.
    write!(w, "\x1b[1;{}r", area.top())?;
    queue!(w, MoveTo(0, cursor_top))?;
    for line in &wrapped {
        queue!(w, Print("\r\n"))?;
        write_history_line(w, line, wrap_width)?;
    }
    write!(w, "\x1b[r")?;
    queue!(w, MoveTo(last_cursor.x, last_cursor.y))?;

    if update_area {
        term.set_viewport_area(Rect { ..area });
    }
    Ok(())
}

fn write_history_line<W: Write>(w: &mut W, line: &Line<'_>, wrap_width: usize) -> io::Result<()> {
    let physical = line_width(line).max(1).div_ceil(wrap_width) as u16;
    if physical > 1 {
        queue!(w, SavePosition)?;
        for _ in 1..physical {
            queue!(
                w,
                MoveDown(1),
                MoveToColumn(0),
                Clear(ClearType::UntilNewLine)
            )?;
        }
        queue!(w, RestorePosition)?;
    }
    queue!(
        w,
        SetColors(Colors::new(
            line.style
                .fg
                .map(IntoCrossterm::into_crossterm)
                .unwrap_or(CColor::Reset),
            line.style
                .bg
                .map(IntoCrossterm::into_crossterm)
                .unwrap_or(CColor::Reset),
        ))
    )?;
    // With the line's bg active, erase-to-end paints the rest of the row in that colour.
    queue!(w, Clear(ClearType::UntilNewLine))?;
    write_spans(w, line)
}

fn write_spans<W: Write>(w: &mut W, line: &Line<'_>) -> io::Result<()> {
    let mut fg = Color::Reset;
    let mut bg = Color::Reset;
    let mut last = Modifier::empty();
    let mut link: Option<String> = None;
    for span in &line.spans {
        let style = span.style.patch(line.style);
        let mut modifier = Modifier::empty();
        modifier.insert(style.add_modifier);
        modifier.remove(style.sub_modifier);
        if modifier != last {
            modifier_diff(w, last, modifier)?;
            last = modifier;
        }
        let nfg = style.fg.unwrap_or(Color::Reset);
        let nbg = style.bg.unwrap_or(Color::Reset);
        if nfg != fg || nbg != bg {
            queue!(
                w,
                SetColors(Colors::new(nfg.into_crossterm(), nbg.into_crossterm()))
            )?;
            fg = nfg;
            bg = nbg;
        }
        let url = crate::hyperlink::url_of(style.underline_color);
        if url != link {
            write!(w, "{}", crate::hyperlink::osc8(url.as_deref()))?;
            link = url;
        }
        queue!(w, Print(span.content.as_ref()))?;
    }
    if link.is_some() {
        write!(w, "{}", crate::hyperlink::osc8(None))?;
    }
    queue!(
        w,
        SetForegroundColor(CColor::Reset),
        SetBackgroundColor(CColor::Reset),
        SetAttribute(CAttribute::Reset),
    )
}
