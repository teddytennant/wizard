// OWNER: terminal (the `--minimal` screen, spec 1.12)
//! Minimal mode: a handful of live rows at the bottom of the normal screen and nothing else. The
//! transcript is flush left in the terminal's own colours; the last few rows of it stay in the
//! live area, and rows that scroll off the top of that window are written above it into the
//! terminal's scrollback and never touched again. Under the window: a dim `minimal · /help`
//! line, a bare `❯` prompt, and `model · context · ctrl+o transcript`.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};

use super::footer::fmt_count;
use super::transcript::{self, Geo};
use super::{anim, put, st};
use crate::app::App;

/// Rows of the live area the popup may take: a rule, up to eight rows and a footer.
const POPUP_ROWS: usize = 8;

/// Lay out the live area into `buf` (the viewport) and return the transcript rows that have
/// scrolled out of its window since the last frame, ready to print above it.
pub fn draw(buf: &mut Buffer, app: &mut App) -> Option<Buffer> {
    let (w, h) = (buf.area.width, buf.area.height);
    if w == 0 || h == 0 {
        return None;
    }
    let th = app.theme.clone();
    let geo = Geo {
        w,
        hpad: 0,
        cx: 0,
        cw: w as usize,
        text_w: w as usize,
        stamps: false,
        tight: true,
    };
    let doc = transcript::build(app, &geo);
    let tick = app.tick();

    // the prompt: `❯ ` and the text, wrapped under the glyph
    let text = app.ed.text().to_string();
    let room = (w as usize).saturating_sub(2).max(1);
    let mut prompt: Vec<String> = Vec::new();
    for line in text.split('\n') {
        let wrapped = tuikit::width::wrap(line, room);
        if wrapped.is_empty() {
            prompt.push(String::new());
        } else {
            prompt.extend(wrapped);
        }
    }
    let popup = super::composer::popup(app);
    let popup_rows = popup
        .as_ref()
        .map_or(0, |p| p.items.len().min(POPUP_ROWS) + 2);
    let hint = 1;
    // the status line gives way to the popup
    let status = usize::from(popup.is_none());
    let max_prompt = (h as usize)
        .saturating_sub(hint + status + popup_rows)
        .max(1);
    if prompt.len() > max_prompt {
        let cut = prompt.len() - max_prompt;
        prompt.drain(..cut);
    }
    let bottom = hint + prompt.len() + popup_rows + status;
    let window = (h as usize).saturating_sub(bottom);

    // the transcript, minus the gap after its last block
    let last_gap = doc
        .entries
        .len()
        .checked_sub(1)
        .map_or(0, |i| doc.total - doc.starts[i] - doc.entries[i].rows.len());
    let rows = doc.total - last_gap;
    // rows above the window go to the scrollback once, and are not drawn again
    let first = rows.saturating_sub(window).max(app.min_committed.min(rows));
    let mut commit = None;
    if first > app.min_committed {
        let n = first - app.min_committed;
        let mut b = Buffer::empty(Rect::new(0, 0, w, n as u16));
        for i in 0..n {
            if let Some((e, r)) = doc.locate(app.min_committed + i) {
                transcript::paint_row(&mut b, &doc.entries[e].rows[r], i as u16, tick, &th, 0);
            }
        }
        if th.bandless {
            super::resolve_dim(&mut b);
        }
        app.min_committed = first;
        commit = Some(b);
    }
    // what is left of the window, bottom-aligned against the prompt
    let shown = rows - first;
    let top = window.saturating_sub(shown) as u16;
    for i in 0..shown {
        if let Some((e, r)) = doc.locate(first + i) {
            transcript::paint_row(buf, &doc.entries[e].rows[r], top + i as u16, tick, &th, 0);
        }
    }
    app.view.doc = doc;

    let mut y = window as u16;
    // the hint line, or what the turn is doing
    if app.busy() {
        let spin = anim::spinner(tick).to_string();
        let started = app.turn.started.unwrap_or_default();
        let el = transcript::fmt_duration(app.now().saturating_sub(started));
        let x = put(buf, 0, y, &spin, st(th.accent_running));
        put(
            buf,
            x,
            y,
            &format!(" Working… {el}"),
            Style::new().add_modifier(Modifier::DIM),
        );
    } else {
        put(
            buf,
            0,
            y,
            "minimal · /help",
            Style::new().add_modifier(Modifier::DIM),
        );
    }
    y += 1;
    for (i, l) in prompt.iter().enumerate() {
        if i == 0 {
            put(buf, 0, y, "❯", st(th.accent_user));
        }
        put(buf, 2, y, l, Style::new());
        y += 1;
    }
    // the caret sits after the last prompt character
    let last = prompt.last().map_or(0, |l| tuikit::width::display_width(l));
    app.cursor = Some(((2 + last as u16).min(w.saturating_sub(1)), y - 1));
    if let Some(p) = popup {
        let dim = Style::new().add_modifier(Modifier::DIM);
        put(buf, 0, y, &"─".repeat(w as usize), dim);
        y += 1;
        let sel = app.popup_sel.min(p.items.len().saturating_sub(1));
        let n = p.items.len().min(POPUP_ROWS);
        // the window of rows follows the cursor
        let from = if sel >= n { sel + 1 - n } else { 0 };
        for (k, it) in p.items.iter().enumerate().skip(from).take(n) {
            let cur = k == sel;
            let lead = if cur { "❯ " } else { "  " };
            let label_w = 28usize.min((w as usize).saturating_sub(2));
            let mut label = it.label.clone();
            if let Some(t) = &it.tag {
                label.push_str(&format!(" {t}"));
            }
            let label = tuikit::width::truncate(&label, label_w);
            let main = if cur {
                Style::new()
                    .fg(th.fuzzy_accent)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::new()
            };
            let x = put(buf, 0, y, lead, main);
            let x = put(buf, x, y, &label, main);
            let pad = (2 + label_w).saturating_sub(x as usize);
            let x = x + pad as u16;
            let desc = tuikit::width::truncate(&it.desc, (w as usize).saturating_sub(x as usize));
            put(
                buf,
                x,
                y,
                &desc,
                if cur { st(th.path) } else { Style::new() },
            );
            y += 1;
        }
        put(
            buf,
            1,
            y,
            "↑/↓ navigate · enter confirm · esc cancel",
            Style::new(),
        );
    } else {
        // `Grok 4.7 (high) · 1.8K / 256K (1%) · ctrl+o transcript`
        let dim = Style::new().add_modifier(Modifier::DIM);
        let mut parts: Vec<String> = vec![app.model_label()];
        if let Some((used, total)) = app.context() {
            let pct = (used as f64 / total.max(1) as f64 * 100.0).round() as u64;
            parts.push(format!(
                "{} / {} ({pct}%)",
                fmt_count(used),
                fmt_count(total)
            ));
        }
        parts.push("ctrl+o transcript".into());
        let mut x = 0u16;
        for (i, p) in parts.iter().enumerate() {
            if i > 0 {
                x = put(buf, x, y, " · ", dim);
            }
            x = put(buf, x, y, p, Style::new());
        }
    }
    if th.bandless {
        super::resolve_dim(buf);
    }
    commit
}
