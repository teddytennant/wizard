// OWNER: shared (frame layout and paint order; keep additive)
//! One frame: paint `bg_base` on every cell, then the active screen, docked strips, popups,
//! toasts and modals, in the order of spec 1.9. Grok writes a background on every cell, so the
//! whole buffer starts as blank cells on `bg_base` and everything after only sets the cells it
//! owns; ratatui's diff then writes nothing for cells that did not change.

pub mod anim;
pub mod composer;
pub mod dialogs;
pub mod footer;
mod grok_day_theme;
mod grok_night_theme;
pub mod markdown;
pub mod minimal;
pub mod syntax;
mod tokyo_night_theme;
pub mod tools;
pub mod tools_body;
pub mod transcript;
pub mod transcript_nav;
pub mod transcript_viewer;
pub mod welcome;
pub mod worktree;

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use tuikit::paint::put_str;

use crate::app::{App, Screen};
use crate::theme::Theme;

/// Blank `r` with `bg`, clipped to the buffer.
pub fn fill(buf: &mut Buffer, r: Rect, bg: Color) {
    tuikit::paint::fill(buf, r, Style::new().bg(bg));
}

pub fn st(fg: Color) -> Style {
    Style::new().fg(fg)
}

pub fn bold(fg: Color) -> Style {
    Style::new().fg(fg).add_modifier(Modifier::BOLD)
}

pub fn dim(fg: Color) -> Style {
    Style::new().fg(fg).add_modifier(Modifier::DIM)
}

/// Draw `s` at (`x`,`y`), clipped to the whole buffer. Returns the x after the last cell.
pub fn put(buf: &mut Buffer, x: u16, y: u16, s: &str, style: Style) -> u16 {
    let clip = buf.area;
    put_str(buf, x, y, s, style, clip)
}

/// Draw `s` so that it ends at column `right` (exclusive).
pub fn put_right(buf: &mut Buffer, right: u16, y: u16, s: &str, style: Style) -> u16 {
    let w = tuikit::width::display_width(s) as u16;
    let x = right.saturating_sub(w);
    put(buf, x, y, s, style);
    x
}

/// Where everything sits in the agent screen (and, for the composer, on home too).
#[derive(Clone, Debug)]
pub struct Layout {
    pub w: u16,
    pub h: u16,
    pub compact: bool,
    /// Outer horizontal padding: 2 columns, 1 in compact mode.
    pub hpad: u16,
    pub header_y: u16,
    /// Scrollback rows, full width (the scrollbar column is the last one).
    pub view: Rect,
    /// Row below the viewport; hosts `▼`.
    pub host_y: u16,
    /// The todo pane's frame: `(first row, rows including both borders)`.
    pub todo: Option<(u16, u16)>,
    pub turn_y: Option<u16>,
    pub banner_y: Option<u16>,
    /// Dock rows (queue): `(first row, height)`.
    pub dock: Option<(u16, u16)>,
    /// Divider row of the `/find` bar (the bar is the row below), when it is open.
    pub find_y: Option<u16>,
    pub composer: Rect,
    /// Text rows inside the composer box.
    pub text_rows: u16,
    pub bar_y: u16,
}

pub const SHORT_ROWS: u16 = 16;
pub const COMPACT_ROWS: u16 = 20;
pub const MIN_VIEW_ROWS: u16 = 5;

impl Layout {
    pub fn compute(app: &App) -> Layout {
        let (w, h) = app.size;
        let compact = app.compact_mode || h <= COMPACT_ROWS;
        // the welcome screen only narrows its side margin; its rows keep the normal padding
        let tight = compact && app.screen == Screen::Session;
        let vpad: u16 = if tight { 0 } else { 1 };
        let hpad: u16 = if compact { 1 } else { 2 };
        let gap: u16 = if tight { 0 } else { 1 };
        let short = h <= SHORT_ROWS;
        let prompt_gap: u16 = if tight || short { 0 } else { 1 };
        let header_y = vpad;
        let bar_y = h.saturating_sub(1 + vpad);
        let comp_w = w.saturating_sub(hpad * 2);
        // the box grows with its text up to half the screen, borders included
        let max_rows = (h / 2).saturating_sub(2).max(1);
        let text_rows = composer::text_rows(app, comp_w, max_rows);
        let bottom = bar_y.saturating_sub(gap + 1);
        // `/jump` takes the composer's slot with its list
        let jump_h = app
            .view
            .nav
            .jump
            .as_ref()
            .filter(|_| app.screen == Screen::Session)
            .map(|j| transcript_nav::overlay_height(j.turns.len(), h));
        let box_h = jump_h.unwrap_or(text_rows + 2);
        let top = (bottom + 1).saturating_sub(box_h);
        let composer = Rect::new(hpad, top, comp_w, box_h);

        // strips above the composer, each preceded by a blank gap row
        let session = app.screen == Screen::Session;
        let mut turn = session && app.busy();
        let mut banner = session && footer::banner_text(app).is_some();
        let dock_rows = if session { footer::dock_rows(app) } else { 0 };
        let strip_gap = if tight { 0 } else { 1 };
        let mut view_top = header_y + 1 + if tight { 0 } else { 1 };
        // the todo pane sits directly under the header and pushes the scrollback down
        let todo = (session && app.todo.open && !app.tr.todos.is_empty()).then(|| {
            let n = footer::todo_visible(app).len();
            let rows = n.min(((h as f32 * 0.15) as usize).clamp(1, 10)) as u16 + 2;
            (header_y + 1, rows)
        });
        if let Some((y, rows)) = todo {
            view_top = y + rows;
        }
        let rows_above = |turn: bool, banner: bool, dock: u16| -> u16 {
            let mut n = 0;
            if turn {
                n += 1 + strip_gap;
            }
            if banner {
                n += 1 + strip_gap;
            }
            if dock > 0 {
                n += dock + strip_gap;
            }
            n
        };
        let mut dock = dock_rows;
        // drop strips until the viewport keeps its minimum
        loop {
            let n = rows_above(turn, banner, dock);
            let end = top.saturating_sub(prompt_gap + n);
            if end.saturating_sub(view_top) >= MIN_VIEW_ROWS || (!turn && !banner && dock == 0) {
                break;
            }
            if banner {
                banner = false;
            } else if dock > 0 {
                dock = 0;
            } else {
                turn = false;
            }
        }
        // stack from the composer upward: prompt gap, dock, banner, turn row; each strip has a
        // blank row above it, and that row hosts `▼` for the strip below the viewport
        let mut y = top.saturating_sub(prompt_gap);
        let dock_out = (dock > 0).then(|| {
            y = y.saturating_sub(dock);
            let r = (y, dock);
            y = y.saturating_sub(strip_gap);
            r
        });
        let banner_y = banner.then(|| {
            y = y.saturating_sub(1);
            let r = y;
            y = y.saturating_sub(strip_gap);
            r
        });
        let turn_y = turn.then(|| {
            y = y.saturating_sub(1);
            let r = y;
            y = y.saturating_sub(strip_gap);
            r
        });
        let view_bottom = y.max(view_top);
        let mut view = Rect::new(0, view_top, w, view_bottom.saturating_sub(view_top).max(1));
        // the find bar takes the two rows under the viewport; the arrow that would sit on the
        // first of them is covered by its divider
        let find_y = (session && app.view.nav.find.is_some()).then(|| {
            let take = 2.min(view.height.saturating_sub(1));
            view.height -= take;
            view.bottom()
        });
        let host_y = view.bottom();
        Layout {
            w,
            h,
            compact,
            hpad,
            header_y,
            view,
            host_y,
            todo,
            turn_y,
            banner_y,
            dock: dock_out,
            find_y,
            composer,
            text_rows,
            bar_y,
        }
    }
}

/// Paint one frame of `app` into `buf`.
pub fn draw(buf: &mut Buffer, app: &mut App) {
    let area = buf.area;
    app.size = (area.width, area.height);
    let bg = app.theme.bg_base;
    fill(buf, area, bg);
    app.cursor = None;
    app.anim_in_view = false;
    let lay = Layout::compute(app);
    match app.screen {
        Screen::Home => welcome::draw(buf, app, &lay),
        Screen::Session => {
            footer::draw_header(buf, app, &lay);
            transcript::draw(buf, app, &lay);
            footer::draw_strips(buf, app, &lay);
            footer::draw_todo(buf, app, &lay);
            transcript_nav::draw_find(buf, app, &lay);
        }
    }
    let hide_chrome = app.screen == Screen::Home && app.home.picker.is_some();
    if !hide_chrome {
        if app.screen == Screen::Session && app.view.nav.jump.is_some() {
            transcript_nav::draw_jump(buf, app, &lay);
        } else {
            composer::draw(buf, app, &lay);
        }
        footer::draw_bar(buf, app, &lay);
    }
    if app.screen == Screen::Session && app.view.nav.viewer.is_some() {
        transcript_viewer::draw(buf, app, &lay);
    }
    // a toast belongs to the screen behind a window, which covers it
    footer::draw_toast(buf, app, &lay);
    if app.screen == Screen::Home {
        worktree::draw(buf, app);
    }
    dialogs::draw(buf, app, &lay);
    if dialogs::is_open(app) {
        app.cursor = None;
    }
    if app.theme.bandless {
        resolve_dim(buf);
    }
}

/// Turn `theme::DIM_FG` into the terminal default colour plus SGR 2, and a background of that
/// value into the default background.
pub(crate) fn resolve_dim(buf: &mut Buffer) {
    for cell in buf.content.iter_mut() {
        match cell.fg {
            Color::Rgb(1, 2, 255) => {
                cell.fg = Color::Reset;
                cell.modifier.insert(Modifier::DIM);
            }
            Color::Rgb(1, 2, n) => {
                cell.fg = Color::Indexed(n);
                cell.modifier.insert(Modifier::DIM);
            }
            _ => {}
        }
        if cell.bg == crate::theme::DIM_FG {
            cell.bg = Color::Reset;
        }
    }
}

/// Shorthand for the theme inside draw code.
pub fn theme(app: &App) -> &Theme {
    &app.theme
}

/// Scrollbar thumb as ratatui's `Scrollbar` sizes it: `total` rows of content, `vis` of them on
/// screen from row `top`, a track of `track` cells. Returns `(first cell, length)`; `None` when
/// everything fits.
pub fn thumb(total: usize, vis: usize, top: usize, track: usize) -> Option<(usize, usize)> {
    if total <= vis || track == 0 || vis == 0 {
        return None;
    }
    let scrollable = (total - 1) as f64;
    let t = track as f64;
    let start = (top as f64 * t / scrollable).round().clamp(0.0, t - 1.0) as usize;
    let end = ((top + vis) as f64 * t / scrollable).round().clamp(0.0, t) as usize;
    Some((start, end.saturating_sub(start).max(1)))
}

/// The transcript's scrollbar thumb the way `tui-scrollbar` lays it out: the track is eight
/// sub-cells per row, the thumb is `viewport / content` of it and slides in proportion to the
/// offset, and every row the thumb touches, even partly, is drawn as a full block. Returns the
/// first row and the row count; `None` when everything fits.
pub fn thumb_rows(content: usize, vis: usize, top: usize, track: usize) -> Option<(usize, usize)> {
    if content <= vis || track == 0 || vis == 0 {
        return None;
    }
    const SUB: usize = 8;
    let track_len = track * SUB;
    let thumb_len = ((vis * track_len + content / 2) / content).clamp(SUB, track_len);
    let max_offset = content - vis;
    let max_start = track_len - thumb_len;
    let start = (top.min(max_offset) * max_start + max_offset / 2) / max_offset;
    let end = start + thumb_len;
    let first = start / SUB;
    let last = end.div_ceil(SUB);
    Some((first, (last - first).max(1)))
}
