//! The prompt history panel (spec 4.9, 9.8.3). `Up` on an empty composer opens it: the newest
//! prompt is selected and live-filled into the composer, `Up` and `Down` walk it. The newest entry
//! sits at the bottom, nearest the box.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};

use crate::app::App;
use crate::ui::{put, st, Layout};

const MAX_ROWS: usize = 8;
const MAX_RESULTS: usize = 100;

#[derive(Clone, Debug, Default)]
pub struct Panel {
    /// What the composer held before the panel opened.
    pub saved: String,
    /// The prompts, newest last.
    pub items: Vec<String>,
    pub selected: usize,
    /// Keeps the bottom entry selected until the user moves.
    pub stick: bool,
}

/// The latest prompts, newest last.
fn entries(app: &App) -> Vec<String> {
    let all: Vec<String> = app
        .ed
        .history_entries()
        .filter(|e| !e.is_empty())
        .map(str::to_string)
        .collect();
    all[all.len().saturating_sub(MAX_RESULTS)..].to_vec()
}

/// Open the panel and fill the composer with the newest prompt.
pub fn open(app: &mut App) {
    let items = entries(app);
    let selected = items.len().saturating_sub(1);
    app.inp.hist = Some(Panel {
        saved: app.ed.text().to_string(),
        items,
        selected,
        stick: true,
    });
    fill(app);
    app.popup_closed = false;
}

/// Put the selected entry into the composer; a `! ` entry restores shell mode.
fn fill(app: &mut App) {
    let Some(p) = app.inp.hist.as_ref() else {
        return;
    };
    let Some(text) = p.items.get(p.selected).cloned() else {
        return;
    };
    app.shell_mode = false;
    match text.strip_prefix("! ") {
        Some(cmd) => {
            app.shell_mode = true;
            app.ed.set_text(cmd);
        }
        None => app.ed.set_text(&text),
    }
}

/// Close and put back what the composer held.
fn close_restoring(app: &mut App) {
    let Some(p) = app.inp.hist.take() else { return };
    app.shell_mode = false;
    app.ed.set_text(&p.saved);
}

fn move_by(app: &mut App, delta: isize) -> bool {
    let Some(p) = app.inp.hist.as_mut() else {
        return false;
    };
    let len = p.items.len();
    if len == 0 {
        return false;
    }
    let to = (p.selected as isize + delta).clamp(0, len as isize - 1) as usize;
    if to == p.selected {
        return false;
    }
    p.selected = to;
    p.stick = false;
    true
}

/// Keys while the panel is open: all of them are taken. Returns after handling.
pub fn key(app: &mut App, key: KeyEvent) {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    match key.code {
        KeyCode::Esc => close_restoring(app),
        KeyCode::Char('c') if ctrl => close_restoring(app),
        KeyCode::Enter | KeyCode::Tab => {
            let pick = app
                .inp
                .hist
                .as_ref()
                .and_then(|p| p.items.get(p.selected).cloned());
            match pick {
                Some(text) => {
                    app.inp.hist = None;
                    app.shell_mode = false;
                    match text.strip_prefix("! ") {
                        Some(cmd) => {
                            app.shell_mode = true;
                            app.ed.set_text(cmd);
                        }
                        None => app.ed.set_text(&text),
                    }
                }
                None => close_restoring(app),
            }
        }
        KeyCode::Up | KeyCode::Char('p' | 'k') if matches!(key.code, KeyCode::Up) || ctrl => {
            if move_by(app, -1) {
                fill(app);
            }
        }
        KeyCode::Down | KeyCode::Char('n' | 'j') if matches!(key.code, KeyCode::Down) || ctrl => {
            if move_by(app, 1) {
                fill(app);
            } else {
                // already on the newest: Down backs out
                close_restoring(app);
            }
        }
        KeyCode::PageUp | KeyCode::Char('u') if matches!(key.code, KeyCode::PageUp) || ctrl => {
            if move_by(app, -(MAX_ROWS as isize / 2)) {
                fill(app);
            }
        }
        KeyCode::PageDown | KeyCode::Char('d') if matches!(key.code, KeyCode::PageDown) || ctrl => {
            if move_by(app, MAX_ROWS as isize / 2) {
                fill(app);
            }
        }
        _ => {
            // an edit detaches: the panel closes, the text stays, the key applies
            app.inp.hist = None;
            crate::keys::edit_key(app, key);
        }
    }
}

pub fn draw(buf: &mut Buffer, app: &App, lay: &Layout) {
    let Some(p) = app.inp.hist.as_ref() else {
        return;
    };
    let th = &app.theme;
    let count = p.items.len();
    let item_rows = count.clamp(1, MAX_ROWS) as u16;
    let bottom = lay.composer.y.saturating_sub(1);
    let panel_h = item_rows + 2;
    let top = bottom.saturating_sub(panel_h - 1);
    let x0 = lay.hpad;
    let w = lay.w.saturating_sub(2 * lay.hpad);
    if top >= bottom || w <= 4 {
        return;
    }
    crate::ui::fill(buf, Rect::new(x0, top, w, panel_h), th.bg_light);
    let rule = "─".repeat(w as usize);
    put(buf, x0, top, &rule, st(th.bg_light).bg(th.bg_base));
    put(buf, x0, bottom, &rule, st(th.bg_light).bg(th.bg_base));
    let n = count.to_string();
    put(
        buf,
        x0 + w - n.len() as u16 - 1,
        top,
        &n,
        st(th.gray).bg(th.bg_base),
    );
    put(buf, x0 + 1, top, " history ", st(th.gray).bg(th.bg_base));
    let ix = lay.composer.x + 2;
    let iw = lay.composer.width.saturating_sub(2);
    if count == 0 {
        put(
            buf,
            ix,
            top + 1,
            "  no matching history",
            st(th.gray).bg(th.bg_light),
        );
        return;
    }
    let vis = item_rows as usize;
    let scroll = if p.selected >= vis {
        p.selected + 1 - vis
    } else {
        0
    };
    let bar = count > vis;
    let text_w = if bar { iw.saturating_sub(2) } else { iw };
    let fill_w = if bar { iw.saturating_sub(1) } else { iw };
    for (vi, ri) in (scroll..count.min(scroll + vis)).enumerate() {
        let y = top + 1 + vi as u16;
        let sel = ri == p.selected;
        let bg = if sel { th.bg_visual } else { th.bg_light };
        let bold = if sel {
            Modifier::BOLD
        } else {
            Modifier::empty()
        };
        crate::ui::fill(buf, Rect::new(ix, y, fill_w, 1), bg);
        if sel {
            put(
                buf,
                ix,
                y,
                "❯ ",
                Style::new().fg(th.text_primary).bg(bg).add_modifier(bold),
            );
        }
        let normal = Style::new().fg(th.text_primary).bg(bg).add_modifier(bold);
        let max_col = ix + text_w;
        let mut col = ix + 2;
        for ch in p.items[ri]
            .chars()
            .map(|c| if c == '\n' || c == '\r' { ' ' } else { c })
        {
            let cw = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0) as u16;
            if col + cw > max_col {
                if col > ix + 2 {
                    put(buf, col - 1, y, "…", normal);
                }
                break;
            }
            let mut b = [0u8; 4];
            put(buf, col, y, ch.encode_utf8(&mut b), normal);
            col += cw;
        }
    }
    if bar {
        let sx = ix + iw - 1;
        let (pos, size) = crate::ui::thumb(count, vis, scroll, vis).unwrap_or((0, vis));
        for i in 0..vis {
            let y = top + 1 + i as u16;
            if i >= pos && i < pos + size {
                put(buf, sx, y, "█", st(th.gray_dim).bg(th.bg_dark));
            } else {
                put(buf, sx, y, " ", Style::new().bg(th.bg_dark));
            }
        }
    }
}
