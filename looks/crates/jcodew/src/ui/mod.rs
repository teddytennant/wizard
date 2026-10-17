//! One frame. The transcript, then the queued prompts, the status line, the notice line, the
//! composer and the session line. While everything fits, the lower rows sit right under the
//! transcript and the rest of the screen is blank; once it does not, they hold the bottom and
//! the transcript scrolls above them.

pub mod header;
pub mod input;
pub mod line;
pub mod markdown;
pub mod overlays;
pub mod prepare;
pub mod tools;
pub mod widgets;

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::Span;
use unicode_width::UnicodeWidthStr;

use crate::app::{App, Overlay, ViewMetrics};
use crate::theme;
use line::spans_width;
use prepare::{Prepared, Row};

/// Write spans starting at `(x, y)`, at most `max` cells. Returns the cell after the last one.
pub fn put(buf: &mut Buffer, x: u16, y: u16, spans: &[Span<'_>], max: u16) -> u16 {
    let end = x.saturating_add(max).min(buf.area.right());
    let mut cx = x;
    if y >= buf.area.bottom() {
        return cx;
    }
    for s in spans {
        if cx >= end {
            break;
        }
        let (nx, _) = buf.set_span(cx, y, s, end - cx);
        cx = nx;
    }
    cx
}

pub fn put_right(buf: &mut Buffer, area: Rect, spans: &[Span<'_>]) {
    let w = spans_width(spans) as u16;
    let x = area.x + area.width.saturating_sub(w);
    put(buf, x, area.y, spans, area.width);
}

pub fn clear(buf: &mut Buffer, area: Rect) {
    for y in area.y..area.bottom().min(buf.area.bottom()) {
        for x in area.x..area.right().min(buf.area.right()) {
            buf[(x, y)].reset();
        }
    }
}

/// Where the scrollbar thumb sits: `(offset, height)` in a track of `track` rows.
pub fn thumb(total: usize, visible: usize, scroll: usize, track: usize) -> (usize, usize) {
    if total <= visible || track == 0 {
        return (0, 0);
    }
    let height = (visible * track).div_ceil(total).clamp(1, track);
    let max_scroll = total - visible;
    let offset = scroll.min(max_scroll) * (track - height) / max_scroll;
    (offset, height)
}

fn draw_scrollbar(buf: &mut Buffer, x: u16, area: Rect, total: usize, scroll: usize) {
    let track = area.height as usize;
    let (offset, height) = thumb(total, track, scroll, track);
    let st = Style::default().fg(theme::scroll_thumb());
    for i in 0..height {
        let glyph = if height == 1 {
            "•"
        } else if i == 0 {
            "╷"
        } else if i + 1 == height {
            "╵"
        } else {
            "│"
        };
        put(
            buf,
            x,
            area.y + (offset + i) as u16,
            &[Span::styled(glyph, st)],
            1,
        );
    }
}

/// The last prompt above the viewport, dimmed, on one or two rows at the top.
fn draw_prompt_preview(buf: &mut Buffer, area: Rect, prep: &Prepared, scroll: usize) -> u16 {
    let Some((_, num, text)) = prep.prompts.iter().rev().find(|(row, _, _)| *row < scroll) else {
        return 0;
    };
    let dim = Modifier::DIM;
    let bg = theme::user_bg();
    let num_s = num.to_string();
    let prefix_len = num_s.width() + 2;
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let content_width = (area.width as usize).saturating_sub(prefix_len + 2).max(4);
    let head = vec![
        Span::styled(
            num_s,
            Style::default().fg(theme::dim()).bg(bg).add_modifier(dim),
        ),
        Span::styled(
            "› ",
            Style::default().fg(theme::user()).bg(bg).add_modifier(dim),
        ),
    ];
    let body = Style::default()
        .fg(theme::user_text())
        .bg(bg)
        .add_modifier(dim);
    let chars: Vec<char> = flat.chars().collect();
    if chars.len() <= content_width {
        let mut spans = head;
        spans.push(Span::styled(flat, body));
        clear(buf, Rect::new(area.x, area.y, area.width, 1));
        put(buf, area.x, area.y, &spans, area.width);
        return 1;
    }
    let first: String = chars[..content_width].iter().collect();
    let last: String = chars[chars.len() - content_width..].iter().collect();
    let mut l1 = head;
    l1.push(Span::styled(format!("{} ...", first.trim_end()), body));
    let l2 = vec![
        Span::styled(
            " ".repeat(prefix_len),
            Style::default().bg(bg).add_modifier(dim),
        ),
        Span::styled(format!("... {}", last.trim_start()), body),
    ];
    for (i, l) in [l1, l2].iter().enumerate() {
        let y = area.y + i as u16;
        if y >= area.bottom() {
            break;
        }
        clear(buf, Rect::new(area.x, y, area.width, 1));
        put(buf, area.x, y, l, area.width);
    }
    2
}

fn draw_rows(buf: &mut Buffer, area: Rect, rows: &[Row], scroll: usize) {
    for (i, row) in rows
        .iter()
        .skip(scroll)
        .take(area.height as usize)
        .enumerate()
    {
        let y = area.y + i as u16;
        put(buf, area.x, y, &row.spans, area.width);
        if row.prompt.is_some() && area.width > 0 {
            put(
                buf,
                area.x + area.width - 1,
                y,
                &[Span::styled("│", Style::default().fg(theme::user()))],
                1,
            );
        }
    }
}

pub fn draw(buf: &mut Buffer, app: &mut App) {
    let (w, h) = app.size;
    let screen = Rect::new(0, 0, w, h).intersection(buf.area);
    clear(buf, screen);
    app.cursor = None;
    if w < 4 || h < 4 {
        return;
    }
    match app.overlay.clone() {
        Some(Overlay::Help { scroll }) => {
            let max = overlays::draw_help(buf, screen, app, scroll);
            if let Some(Overlay::Help { scroll }) = app.overlay.as_mut() {
                *scroll = (*scroll).min(max);
            }
            return;
        }
        Some(Overlay::Sessions { sel, loading }) => {
            overlays::draw_sessions(buf, screen, app, sel, loading);
            return;
        }
        None => {}
    }

    let queued_h = app.queue.len().min(3) as u16;
    let notif_h = u16::from(app.notice.is_some() || app.hotkey_note.is_some());
    let input_h = input::input_height(app, w);
    let fixed = 1 + queued_h + notif_h + input_h + 1;
    let wide = (w - 1) as usize;
    let band = widgets::todo_band(app, wide, h.saturating_sub(fixed) as usize);
    let band_h = band.len() as u16;
    let mut prep = prepare::prepare(app, wide, h);
    let packed = band.len() + prep.rows.len() + fixed as usize <= h as usize;
    let msg_h = if packed {
        (band.len() + prep.rows.len()).max(1) as u16
    } else {
        h.saturating_sub(fixed).max(1)
    };
    let view_h = msg_h.saturating_sub(band_h);
    let scrollbar = prep.rows.len() > view_h as usize;
    if scrollbar {
        prep = prepare::prepare(app, wide - 1, h);
    }
    let total = prep.rows.len();
    let max_scroll = total.saturating_sub(view_h as usize);
    let scroll = match app.scroll {
        Some(s) if s < max_scroll => s,
        _ => {
            app.scroll = None;
            max_scroll
        }
    };
    app.view = ViewMetrics {
        total,
        height: view_h as usize,
        prompts: prep.prompts.iter().map(|p| p.0).collect(),
    };

    let text_w = if scrollbar { w - 2 } else { w - 1 };
    for (i, row) in band.iter().enumerate() {
        put(buf, 1, i as u16, row, text_w);
    }
    let messages = Rect::new(1, band_h, text_w, view_h);
    draw_rows(buf, messages, &prep.rows, scroll);
    if scroll > 0 {
        draw_prompt_preview(buf, messages, &prep, scroll);
    }
    if scrollbar {
        draw_scrollbar(
            buf,
            w - 1,
            Rect::new(0, 0, w, msg_h),
            total + band.len(),
            scroll,
        );
    }

    let mut y = msg_h;
    let mut take = |n: u16| {
        let r = Rect::new(0, y.min(h), w, n.min(h.saturating_sub(y)));
        y += n;
        r
    };
    let queued = take(queued_h);
    let status = take(1);
    let notification = take(notif_h);
    let composer = take(input_h);
    let session = take(1);
    input::draw_queued(buf, queued, app);
    if status.height > 0 {
        input::draw_status(buf, status, app);
    }
    if notification.height > 0 {
        input::draw_notification(buf, notification, app);
    }
    if composer.height > 0 {
        app.cursor = input::draw_input(buf, composer, app);
    }
    if session.height > 0 {
        input::draw_session_line(buf, session, app);
    }
    if let Some(filter) = crate::commands::model_filter(app.ed.text()) {
        input::draw_model_picker(buf, composer, app, filter);
    } else if let Some(list) = input::suggestions_active(app) {
        input::draw_suggestions(buf, composer, screen, app, &list);
    }
}

#[cfg(test)]
mod tests {
    use super::thumb;

    #[test]
    fn thumb_follows_the_scroll_position() {
        // 37 visible rows of 46: a 30-row thumb, at the bottom when scrolled to the end
        assert_eq!(thumb(46, 37, 9, 37), (7, 30));
        assert_eq!(thumb(46, 37, 0, 37), (0, 30));
        assert_eq!(thumb(10, 37, 0, 37), (0, 0));
    }
}
