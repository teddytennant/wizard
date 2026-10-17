// OWNER: transcript (the fullscreen block viewer, spec 3.13)
//! The viewer `Enter` opens on a message, thinking block, command, edit, read, search or fetch:
//! a rounded popup over the dimmed screen holding the block laid out open at the popup's width.
//! Geometry is from `render.rs` in the real pager: width `max(60, 0.95 W)`, height
//! `max(12, 0.92 h)` of the rows above the shortcuts bar, centred, `[x]` on the first inner row,
//! content inset two columns.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};

use super::transcript::{build_open, EntryKey, Geo, Row};
use super::transcript_nav::cells;
use super::{bold, put, st, Layout};
use crate::app::App;
use crate::theme::{blend, Theme};

#[derive(Debug)]
pub struct Viewer {
    pub key: EntryKey,
    /// First visible content row.
    pub top: usize,
    /// Rows of the open block at the width last drawn, and the visible height then.
    pub rows: Vec<Row>,
    pub visible: usize,
    /// Where the `[x]` button was drawn, for a click.
    pub close: Option<Rect>,
}

/// Where the popup goes for a screen of `w` x `h` with the shortcuts bar at `bar_y`.
pub fn popup_rect(w: u16, bar_y: u16) -> Rect {
    let above = bar_y; // rows 0..bar_y are what the popup floats over
    let pw = (((w as f32) * 0.95) as u16).max(60).min(w);
    let ph = (((above as f32) * 0.92) as u16)
        .max(12)
        .min(above.saturating_sub(2));
    let x = w.saturating_sub(pw) / 2;
    let y = above.saturating_sub(ph) / 2;
    Rect::new(x, y, pw, ph)
}

/// Rows of the popup's content area (inside the border, under the `[x]` row, above the bottom
/// border).
fn content_rect(p: Rect) -> Rect {
    Rect::new(
        p.x + 3,
        p.y + 2,
        p.width.saturating_sub(6),
        p.height.saturating_sub(3),
    )
}

impl Viewer {
    pub fn new(key: EntryKey) -> Viewer {
        Viewer {
            key,
            top: 0,
            rows: Vec::new(),
            visible: 1,
            close: None,
        }
    }

    pub fn max_top(&self) -> usize {
        self.rows.len().saturating_sub(self.visible.max(1))
    }

    pub fn scroll(&mut self, by: isize) {
        let to = (self.top as isize + by).clamp(0, self.max_top() as isize);
        self.top = to as usize;
    }
}

/// Dim everything above the bar halfway toward the background, as the real pager does behind a
/// popup: foreground blended, modifiers cleared, backgrounds kept.
fn dim_area(buf: &mut Buffer, area: Rect, th: &Theme) {
    for y in area.y..(area.y + area.height).min(buf.area.height) {
        for x in area.x..(area.x + area.width).min(buf.area.width) {
            let c = &mut buf[(x, y)];
            c.modifier = Modifier::empty();
            match (th.bg_base, c.fg) {
                (Color::Rgb(..), Color::Rgb(..)) => c.fg = blend(th.bg_base, c.fg, 0.5),
                _ => c.modifier.insert(Modifier::DIM),
            }
        }
    }
}

pub fn draw(buf: &mut Buffer, app: &mut App, lay: &Layout) {
    let th = app.theme.clone();
    let Some(key) = app.view.nav.viewer.as_ref().map(|v| v.key) else {
        return;
    };
    let p = popup_rect(lay.w, lay.bar_y);
    if p.height < 5 || p.width < 12 {
        return;
    }
    let cr = content_rect(p);
    let geo = Geo {
        w: cr.width,
        hpad: 0,
        cx: 0,
        cw: cr.width as usize,
        text_w: cr.width as usize,
        stamps: false,
        tight: false,
    };
    let rows = build_open(app, key, &geo)
        .map(|e| e.rows)
        .unwrap_or_default();
    let Some(v) = app.view.nav.viewer.as_mut() else {
        return;
    };
    v.rows = rows;
    v.visible = cr.height as usize;
    v.top = v.top.min(v.max_top());

    dim_area(buf, Rect::new(0, 0, lay.w, lay.bar_y), &th);
    let base = Style::new().fg(th.text_primary).bg(th.bg_base);
    buf.set_style(p, base);
    for y in p.y..p.y + p.height {
        for x in p.x..p.x + p.width {
            buf[(x, y)].set_symbol(" ");
        }
    }
    let b = st(th.gray_dim).bg(th.bg_base);
    put(buf, p.x, p.y, "╭", b);
    put(buf, p.x + p.width - 1, p.y, "╮", b);
    put(buf, p.x, p.y + p.height - 1, "╰", b);
    put(buf, p.x + p.width - 1, p.y + p.height - 1, "╯", b);
    for x in p.x + 1..p.x + p.width - 1 {
        put(buf, x, p.y, "─", b);
        put(buf, x, p.y + p.height - 1, "─", b);
    }
    for y in p.y + 1..p.y + p.height - 1 {
        put(buf, p.x, y, "│", b);
        put(buf, p.x + p.width - 1, y, "│", b);
    }
    // `[x]` four cells in from the right edge of the inner area
    let close = Rect::new(p.x + p.width - 1 - 4, p.y + 1, 3, 1);
    put(buf, close.x, close.y, "[x]", st(th.gray_dim).bg(th.bg_base));
    v.close = Some(close);

    for (i, row) in v
        .rows
        .iter()
        .skip(v.top)
        .take(cr.height as usize)
        .enumerate()
    {
        let y = cr.y + i as u16;
        for &(x0, x1, c) in &row.fills {
            super::fill(buf, Rect::new(cr.x + x0, y, x1.saturating_sub(x0), 1), c);
        }
        // the bullet of the first row is the list's chrome, not the viewer's
        let skip = if v.top + i == 0 { 2 } else { 0 };
        for s in &row.segs {
            if s.x < skip {
                continue;
            }
            let style = match s.anim {
                super::transcript::Anim::None => s.style,
                super::transcript::Anim::Wave(a) => s.style.fg(a),
            };
            put(buf, cr.x + s.x - skip, y, &s.text, style);
        }
    }
    // a thin scroll mark when there is more than fits
    if v.rows.len() > v.visible {
        let track = cr.height as usize;
        if let Some((pos, size)) = super::thumb(v.rows.len(), v.visible, v.top, track) {
            for i in 0..track {
                let on = i >= pos && i < pos + size;
                put(
                    buf,
                    p.x + p.width - 2,
                    cr.y + i as u16,
                    if on { "█" } else { " " },
                    st(if on { th.scrollbar_fg } else { th.scrollbar_bg }).bg(th.bg_base),
                );
            }
        }
    }
    let _ = bold;
}

/// The row text of the open block, for the quote.
pub fn text_of(rows: &[Row]) -> String {
    rows.iter()
        .map(|r| {
            let cs = cells(r);
            let mut s = String::new();
            let mut at = 0u16;
            for (x, ch, copy) in cs {
                if !copy {
                    continue;
                }
                while at < x {
                    s.push(' ');
                    at += 1;
                }
                s.push(ch);
                at = x + 1;
            }
            s.trim_end().to_string()
        })
        .collect::<Vec<_>>()
        .join("\n")
}
