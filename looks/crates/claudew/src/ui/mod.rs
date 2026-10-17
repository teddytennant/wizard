//! Drawing. `draw` lays out one frame: the transcript on top, and under it either the prompt
//! box with its footer or a dialog. Ctrl+O swaps the whole screen for the detailed transcript.

pub mod dialogs;
pub mod diff;
pub mod markdown;
pub mod prompt;
pub mod transcript;

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};

use crate::app::{App, Dialog};
use crate::text::{blit, put, Row};

pub fn draw(buf: &mut Buffer, app: &mut App) {
    buf.reset();
    app.cursor = None;
    let area = buf.area;
    if area.width < 4 || area.height < 2 {
        return;
    }
    if app.detail {
        draw_detail(buf, app);
        let links = app.links.borrow().clone();
        app.link_spots = hyperlinks(buf, &links);
        return;
    }
    let w = area.width as usize;
    let mut rows = transcript::rows(app, w, false);
    let top = match app.dialog.clone() {
        Some(Dialog::Permission { req, sel }) if crate::app::is_plan(&req) => {
            dialogs::plan_review(buf, app, &req, sel)
        }
        Some(Dialog::Permission { req, sel }) => {
            rows.push(Row::new());
            rows.extend(dialogs::permission(app, &req, sel, w));
            area.height
        }
        Some(d) => dialogs::draw(buf, app, &d),
        None => prompt::draw(buf, app),
    };
    // the status panel sits under one blank row and the transcript's last
    let skip = u16::from(matches!(app.dialog, Some(Dialog::Status)) && top > 1);
    draw_rows(buf, app, &rows, Rect::new(0, skip, area.width, top - skip));
    if app.scroll > 0 && app.dialog.is_none() && top > 0 {
        let pill = " Jump to bottom (ctrl+End) ↓ ";
        let pw = crate::text::width(pill) as u16;
        let x = area.width.saturating_sub(pw).div_ceil(2);
        let st = Style::new().bg(app.theme.user_bg).fg(app.theme.user_fg);
        // the pill covers what is under it, underline and all
        for i in 0..pw.min(area.width.saturating_sub(x)) {
            buf[(x + i, top - 1)].reset();
        }
        put(buf, x, top - 1, pill, st);
    }
    let links = app.links.borrow().clone();
    app.link_spots = hyperlinks(buf, &links);
}

/// Where each link's text appears whole on one row: (column, row, text, url, colour). The
/// frame loop writes these again inside OSC 8 once the frame is on screen, so the terminal can
/// open them; the cells themselves stay plain, since a cell holding an escape sequence would be
/// measured as wide and the cells after it skipped.
fn hyperlinks(
    buf: &Buffer,
    links: &[(String, String)],
) -> Vec<(u16, u16, String, String, ratatui::style::Color)> {
    let area = buf.area;
    let mut out = Vec::new();
    for (text, url) in links {
        // `prefix\0label`: only the label is the link, and only where the prefix is drawn
        // right before it (a tool row's `Update(` before its file)
        let (prefix, label) = text.split_once('\0').unwrap_or(("", text.as_str()));
        if !prefix.is_empty() {
            let seg = |t: &str| -> Vec<String> {
                unicode_segmentation::UnicodeSegmentation::graphemes(t, true)
                    .map(str::to_string)
                    .collect()
            };
            let (pre, lab) = (seg(prefix), seg(label));
            let at = |x: u16, y: u16, g: &[String]| {
                x as usize + g.len() <= area.right() as usize
                    && g.iter()
                        .enumerate()
                        .all(|(i, c)| buf[(x + i as u16, y)].symbol() == c)
            };
            for y in area.top()..area.bottom() {
                for x in area.left()..area.right() {
                    if !at(x, y, &pre) {
                        continue;
                    }
                    // the label may run on to the next row when the path wraps
                    let lx = x + pre.len() as u16;
                    let fit = lab.len().min((area.right() - lx) as usize);
                    if fit == 0 || !at(lx, y, &lab[..fit]) {
                        continue;
                    }
                    out.push((lx, y, lab[..fit].concat(), url.clone(), buf[(lx, y)].fg));
                    if fit < lab.len() && y + 1 < area.bottom() {
                        let rest = &lab[fit..];
                        let n = rest.len().min(area.width as usize);
                        if let Some(x2) =
                            (area.left()..area.right()).find(|&x2| at(x2, y + 1, &rest[..n]))
                        {
                            out.push((
                                x2,
                                y + 1,
                                rest[..n].concat(),
                                url.clone(),
                                buf[(x2, y + 1)].fg,
                            ));
                        }
                    }
                }
            }
            continue;
        }
        let want: Vec<&str> =
            unicode_segmentation::UnicodeSegmentation::graphemes(text.as_str(), true).collect();
        if want.is_empty() {
            continue;
        }
        for y in area.top()..area.bottom() {
            let mut x = area.left();
            while x + want.len() as u16 <= area.right() {
                // a label counts only where it is drawn as a link; a bare URL anywhere
                let styled = text.contains("://") || buf[(x, y)].fg == markdown::LINK;
                let hit = styled
                    && want
                        .iter()
                        .enumerate()
                        .all(|(i, g)| buf[(x + i as u16, y)].symbol() == *g);
                if hit {
                    out.push((x, y, text.clone(), url.clone(), buf[(x, y)].fg));
                    x += want.len() as u16;
                } else {
                    x += 1;
                }
            }
        }
    }
    out
}

/// Draw the tail of `rows` into `area`, `app.scroll` rows up from the end.
fn draw_rows(buf: &mut Buffer, app: &mut App, rows: &[Row], area: Rect) {
    let view = area.height as usize;
    let max_scroll = rows.len().saturating_sub(view);
    app.scroll = app.scroll.min(max_scroll);
    app.last_view = (rows.len(), view);
    let start = max_scroll - app.scroll;
    // a prompt whose row above has scrolled off is not drawn at all, as the reference leaves
    // it out; whatever else is cut at the top shows its tail
    let user_bg = app.theme.user_bg;
    let is_user = |r: &Row| r.0.first().is_some_and(|(_, st)| st.bg == Some(user_bg));
    let mut hide = start > 0;
    for (i, row) in rows.iter().skip(start).take(view).enumerate() {
        if hide && is_user(row) {
            continue;
        }
        hide = false;
        blit(buf, area.x, area.y + i as u16, row);
    }
}

fn draw_detail(buf: &mut Buffer, app: &mut App) {
    let area = buf.area;
    let (w, h) = (area.width, area.height);
    let rows = transcript::rows(app, w as usize, true);
    draw_rows(buf, app, &rows, Rect::new(0, 0, w, h.saturating_sub(3)));
    let th = app.theme;
    put(
        buf,
        0,
        h - 2,
        &"─".repeat(w as usize),
        Style::new().add_modifier(Modifier::DIM),
    );
    if app.shortcuts {
        let table = prompt::shortcut_table(app, w as usize, false, th.dim());
        let top = (h - 2).saturating_sub(table.len() as u16);
        for (i, row) in table.iter().enumerate() {
            blit(buf, 0, top + i as u16, row);
        }
    }
    let mut hint = format!(
        "Showing detailed transcript · ctrl+o to toggle · ↑↓ scroll · v to open in {} · ? for shortcuts",
        crate::app::editor_name()
    );
    if crate::text::width(&hint) + 12 > w as usize {
        hint = "Showing detailed transcript · ctrl+o to toggle · ? for shortcuts".to_string();
    }
    put(buf, 2, h - 1, &hint, th.dim());
    put(buf, w.saturating_sub(8), h - 1, "verbose", th.dim());
}
