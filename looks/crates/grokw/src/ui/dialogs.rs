// OWNER: dialogs (palette, cheatsheet, settings, context/usage, doctor, tutorial and the rest are stubs)
//! Modal windows and the full-frame pickers. `modal_frame` is the shared chrome of spec 6.2
//! (square border, title on the top edge, `[✗]` button, no dimming behind); the resume picker is
//! built on it for the agent screen and as the full-frame list on home (spec 2.18, 6.8).

use agent_core::SessionInfo;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use tuikit::paint::put_str;
use tuikit::width::{display_width, truncate};

use super::{bold, dim, put, put_right, st, Layout};
use crate::app::{App, Screen};

pub mod docs;
pub mod doctor;
pub mod page;
pub mod settings;
mod settings_defs;
pub mod shortcuts;
pub mod tutorial;
pub mod usage;
pub mod widgets;

#[derive(Clone, Debug)]
pub enum Modal {
    Resume(ResumePicker),
    Palette(PaletteState),
    Shortcuts(shortcuts::Shortcuts),
    Settings(settings::Settings),
    Docs(docs::Docs),
    Tutorial(tutorial::Tutorial),
    /// Context, usage limit and session info tabs.
    Usage(usage::UsageModal),
    /// A window for a surface that is not built yet: title and the lines it prints.
    Stub {
        title: String,
        lines: Vec<String>,
    },
}

impl Modal {
    /// Whether the shortcuts bar goes blank while this window is open.
    pub fn hides_bar(&self) -> bool {
        match self {
            // the tutorial keeps the bar
            Modal::Tutorial(_) => false,
            Modal::Resume(_)
            | Modal::Palette(_)
            | Modal::Shortcuts(_)
            | Modal::Settings(_)
            | Modal::Docs(_)
            | Modal::Usage(_)
            | Modal::Stub { .. } => true,
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct ResumePicker {
    pub rows: Vec<SessionInfo>,
    pub sel: usize,
    pub query: String,
    pub searching: bool,
    pub loading: bool,
}

impl ResumePicker {
    pub fn set_sessions(&mut self, s: &[SessionInfo]) {
        self.rows = s.to_vec();
        self.loading = false;
        self.sel = self.sel.min(self.rows.len().saturating_sub(1));
    }

    pub fn visible(&self) -> Vec<&SessionInfo> {
        let q = self.query.to_lowercase();
        self.rows
            .iter()
            .filter(|s| {
                q.is_empty()
                    || s.title.to_lowercase().contains(&q)
                    || s.id.to_lowercase().contains(&q)
            })
            .collect()
    }

    pub fn selected(&self) -> Option<&SessionInfo> {
        self.visible().get(self.sel).copied()
    }
}

/// `just now`, `3m ago`, `2h ago`, `5d ago`, `1mo ago`.
pub fn ago(now: i64, then: i64) -> String {
    if then <= 0 {
        return String::new();
    }
    let d = (now - then).max(0);
    match d {
        0..=59 => "just now".to_string(),
        60..=3599 => format!("{}m ago", d / 60),
        3600..=86399 => format!("{}h ago", d / 3600),
        86400..=2591999 => format!("{}d ago", d / 86400),
        _ => format!("{}mo ago", d / 2592000),
    }
}

/// Last two path components joined by `-`: the picker's group name.
pub fn group_name(cwd: &str) -> String {
    let parts: Vec<&str> = cwd.trim_end_matches('/').rsplit('/').take(2).collect();
    parts.into_iter().rev().collect::<Vec<_>>().join("-")
}

// ---- chrome ---------------------------------------------------------------------------------

/// Size and place a window the way `ModalWindow` does: `pct` of the width capped at `max_w`,
/// at least `min_w`, with `v_margin` rows above and below.
pub fn modal_rect(w: u16, h: u16, pct: f32, max_w: u16, min_w: u16, v_margin: u16) -> Rect {
    let cap = (w.saturating_sub(4)).min(max_w);
    let mw = (((w as f32) * pct) as u16).min(cap).max(min_w).min(w);
    let mh = h.saturating_sub(2 * v_margin).max(1);
    Rect::new((w - mw) / 2, v_margin.min(h.saturating_sub(1)), mw, mh)
}

/// Draw the window frame and return the inner rect.
pub fn modal_frame(buf: &mut Buffer, app: &App, r: Rect, title: &str) -> Rect {
    let th = &app.theme;
    if r.width < 20 || r.height < 6 {
        return Rect::default();
    }
    super::fill(buf, r, th.bg_base);
    let bs = st(th.gray_dim);
    let right = r.x + r.width - 1;
    let bottom = r.y + r.height - 1;
    // top edge: `┌─ Title ───── [✗] ─┐`
    let mut top = String::from("┌");
    top.push_str(&"─".repeat(r.width as usize - 2));
    top.push('┐');
    put(buf, r.x, r.y, &top, bs);
    if !title.is_empty() {
        put(
            buf,
            r.x + 2,
            r.y,
            &format!(" {title} "),
            bold(th.text_primary),
        );
    }
    put(buf, right - 6, r.y, " [✗] ─┐", bs);
    for y in r.y + 1..bottom {
        put(buf, r.x, y, "│", bs);
        put(buf, right, y, "│", bs);
    }
    let mut bot = String::from("└");
    bot.push_str(&"─".repeat(r.width as usize - 2));
    bot.push('┘');
    put(buf, r.x, bottom, &bot, bs);
    Rect::new(r.x + 1, r.y + 1, r.width - 2, r.height - 2)
}

// ---- drawing --------------------------------------------------------------------------------

pub fn draw(buf: &mut Buffer, app: &mut App, lay: &Layout) {
    let Some(m) = app.modal.clone() else { return };
    match m {
        Modal::Resume(p) => draw_resume_modal(buf, app, lay, &p),
        Modal::Palette(p) => draw_palette(buf, app, lay, &p),
        Modal::Shortcuts(s) => shortcuts::draw(buf, app, lay, &s),
        Modal::Usage(m) => usage::draw(buf, app, lay, &m),
        Modal::Settings(s) => settings::draw(buf, app, lay, &s),
        Modal::Docs(d) => docs::draw(buf, app, lay, &d),
        Modal::Tutorial(t) => tutorial::draw(buf, app, lay, &t),
        Modal::Stub { title, lines } => {
            let r = modal_rect(lay.w, lay.h, 0.5, 80, 44, 4);
            let inner = modal_frame(buf, app, r, &title);
            let th = app.theme.clone();
            for (i, l) in lines.iter().enumerate() {
                let y = inner.y + 2 + i as u16;
                if y >= inner.bottom().saturating_sub(2) {
                    break;
                }
                put_str(buf, inner.x + 2, y, l, st(th.gray), inner);
            }
            let hint = "Esc close";
            let hx = inner.x + (inner.width.saturating_sub(display_width(hint) as u16)) / 2;
            let (k, rest) = hint.split_once(' ').unwrap_or((hint, ""));
            let nx = put(buf, hx, inner.bottom() - 2, k, bold(th.accent_user));
            put(
                buf,
                nx,
                inner.bottom() - 2,
                &format!(" {rest}"),
                st(th.gray),
            );
        }
    }
}

fn footer_hints(buf: &mut Buffer, app: &App, inner: Rect, y: u16, hints: &[(&str, &str)]) {
    let th = &app.theme;
    let total: usize = hints
        .iter()
        .map(|(k, l)| display_width(k) + 1 + display_width(l))
        .sum::<usize>()
        + hints.len().saturating_sub(1) * 5;
    let mut x = inner.x + (inner.width as usize).saturating_sub(total) as u16 / 2;
    for (i, (k, l)) in hints.iter().enumerate() {
        if i > 0 {
            x = put(buf, x, y, "  |  ", st(th.gray_dim));
        }
        x = put(buf, x, y, k, bold(th.accent_user));
        x = put(buf, x, y, &format!(" {l}"), st(th.gray));
    }
}

fn draw_resume_modal(buf: &mut Buffer, app: &App, lay: &Layout, p: &ResumePicker) {
    let th = &app.theme;
    let r = modal_rect(lay.w, lay.h, 0.65, 100, 44, 4);
    let inner = modal_frame(buf, app, r, "Resume session");
    if inner.height < 6 {
        return;
    }
    let search_y = inner.y + 1;
    // the search row starts three cells in, like the palette's
    let x = r.x + 3;
    if p.searching {
        let nx = put(buf, x, search_y, " search: ", st(th.gray));
        let nx2 = put(buf, nx, search_y, &p.query, st(th.text_primary));
        put(
            buf,
            nx2,
            search_y,
            " ",
            Style::new().fg(th.bg_base).bg(th.text_primary),
        );
    } else {
        put(buf, x, search_y, " / to search", st(th.gray_dim));
    }
    let fx = r.right().saturating_sub(6);
    put(buf, fx.saturating_sub(5), search_y, "Grok", st(th.gray_dim));
    put(buf, fx, search_y, "f", bold(th.gray_dim));
    put(
        buf,
        r.x,
        search_y + 1,
        &format!("│{}│", "─".repeat(r.width as usize - 2)),
        st(th.gray_dim),
    );
    let list_top = search_y + 2;
    let list_bot = inner.bottom().saturating_sub(2);
    if p.loading {
        let t = format!("{} Loading…", super::anim::dots(app.tick()));
        let y = list_top + (list_bot - list_top) / 2;
        let x = inner.x + (inner.width.saturating_sub(display_width(&t) as u16)) / 2;
        put(buf, x, y, &t, st(th.gray));
    } else {
        let rows = p.visible();
        let (bx0, bx1) = (r.x + 3, r.right().saturating_sub(4));
        draw_session_rows(buf, app, &rows, p.sel, bx0, bx1, list_top, list_bot);
        // a thumb in the last inner column when the list scrolls
        let n = rows.len() + 1;
        let vis = list_bot.saturating_sub(list_top) as usize;
        let first = p.sel.saturating_sub(vis.saturating_sub(2));
        if let Some((pos, size)) = super::thumb(n, vis, first, vis) {
            for i in pos..(pos + size).min(vis) {
                put(
                    buf,
                    r.right() - 2,
                    list_top + i as u16,
                    "█",
                    st(th.gray_dim).bg(th.gray_dim),
                );
            }
        }
    }
    footer_hints(
        buf,
        app,
        inner,
        inner.bottom() - 1,
        &[
            ("↑↓", "nav"),
            ("e", "expand"),
            ("/", "search"),
            ("f", "filter"),
            ("d", "delete"),
            ("i", "search"),
        ],
    );
}

/// Group header and session rows. The selected row's band spans `[bx0, bx1)`; the marker sits
/// two cells in from the band's left edge, the title four, and the age is right-aligned in
/// eight columns one cell before the band's right edge. Rows run from `y0` down to `y1`.
#[allow(clippy::too_many_arguments)]
fn draw_session_rows(
    buf: &mut Buffer,
    app: &App,
    rows: &[&SessionInfo],
    sel: usize,
    bx0: u16,
    bx1: u16,
    y0: u16,
    y1: u16,
) {
    let th = &app.theme;
    if rows.is_empty() || bx1 <= bx0 + 12 {
        return;
    }
    let now = app.unix_now();
    // one group named after the directory, as every listed session shares it
    let group = group_name(&rows[0].cwd);
    let gy = y0;
    let name = format!(" {group} ");
    let nx = put(buf, bx0, gy, &name, bold(th.gray));
    for c in nx..bx1 {
        put(buf, c, gy, "─", st(th.gray_dim));
    }
    let visible = (y1.saturating_sub(gy + 1)) as usize;
    let first = sel.saturating_sub(visible.saturating_sub(1));
    let band_w = bx1 - bx0;
    for (i, s) in rows.iter().enumerate().skip(first).take(visible) {
        let y = gy + 1 + (i - first) as u16;
        let selected = i == sel;
        let bg = if selected { th.bg_visual } else { th.bg_base };
        if selected {
            super::fill(buf, Rect::new(bx0, y, band_w, 1), bg);
        }
        let clip = Rect::new(bx0, y, band_w, 1);
        put_str(
            buf,
            bx0 + 2,
            y,
            "› ",
            Style::new().fg(th.gray_dim).bg(bg),
            clip,
        );
        let label = ago(now, s.updated);
        let label_w = 8usize;
        let room = (band_w as usize).saturating_sub(4 + 1 + label_w + 1);
        let title = truncate(first_title(&s.title), room);
        let mut st_ = Style::new().fg(th.text_primary).bg(bg);
        if selected {
            st_ = st_.add_modifier(Modifier::BOLD);
        }
        put_str(buf, bx0 + 4, y, &title, st_, clip);
        // right-aligned in eight columns, one cell before the band's edge
        let lx = bx1 - 1 - label_w as u16;
        put_str(
            buf,
            lx + (label_w - display_width(&label).min(label_w)) as u16,
            y,
            &label,
            Style::new().fg(th.gray).bg(bg),
            clip,
        );
    }
}

fn first_title(t: &str) -> &str {
    t.lines().next().unwrap_or("")
}

/// The home screen's full-frame session list (spec 2.18).
pub fn draw_home_picker(buf: &mut Buffer, app: &mut App, lay: &Layout) {
    let th = app.theme.clone();
    let Some(p) = app.home.picker.clone() else {
        return;
    };
    let (w, h) = (lay.w, lay.h);
    if w < 20 || h < 10 {
        return;
    }
    let bs = st(th.gray_dim);
    let right = w - 1;
    let bot = h - 2;
    put(buf, 0, 2, &format!("┌{}┐", "─".repeat(w as usize - 2)), bs);
    for y in 3..bot {
        put(buf, 0, y, "│", bs);
        put(buf, right, y, "│", bs);
    }
    put(
        buf,
        0,
        bot,
        &format!("└{}┘", "─".repeat(w as usize - 2)),
        bs,
    );
    let _ = right;
    put(buf, 3, 3, "Resume session", bold(th.text_primary));
    put(buf, w - 4, 3, "[✗]", st(th.gray));
    put(buf, 0, 4, &format!("├{}┤", "─".repeat(w as usize - 2)), bs);
    if p.searching {
        let nx = put(buf, 1, 5, " search: ", st(th.gray));
        let nx2 = put(buf, nx, 5, &p.query, st(th.text_primary));
        put(
            buf,
            nx2,
            5,
            " ",
            Style::new().fg(th.bg_base).bg(th.text_primary),
        );
    } else {
        put(buf, 1, 5, " / to search", bs);
    }
    put(buf, w - 9, 5, "Grok", bs);
    put(buf, w - 4, 5, "f", bold(th.gray_dim));
    put(buf, 0, 6, &format!("│{}│", "─".repeat(w as usize - 2)), bs);
    if p.loading {
        let t = format!("{} Loading…", super::anim::dots(app.tick()));
        put(
            buf,
            (w.saturating_sub(display_width(&t) as u16)) / 2,
            12.min(bot - 1),
            &t,
            st(th.gray),
        );
    } else {
        let rows = p.visible();
        draw_session_rows(buf, app, &rows, p.sel, 1, w - 1, 7, bot.saturating_sub(1));
    }
    // footer hints, clipped at the border with no ellipsis
    let y = h - 3;
    let clip = Rect::new(2, y, w.saturating_sub(5), 1);
    let hints = [
        ("Esc", "back"),
        ("Enter", "select"),
        ("ctrl+w", "worktree"),
        ("↑↓", "navigate"),
        ("f", "filter"),
        ("d", "delete"),
        ("e/Shift+e", "expand"),
        ("y", "copy"),
    ];
    let mut x = 2u16;
    for (i, (k, l)) in hints.iter().enumerate() {
        if i > 0 {
            x = put_str(buf, x, y, "  │  ", dim(th.gray), clip);
        }
        x = put_str(buf, x, y, k, bold(th.accent_user), clip);
        x = put_str(buf, x, y, &format!(":{l}"), st(th.gray), clip);
    }
}

// ---- keys -----------------------------------------------------------------------------------

/// Keys for the open resume picker (home or modal). Returns true when it took the key.
pub fn picker_key(app: &mut App, key: KeyEvent) -> bool {
    let in_modal = matches!(app.modal, Some(Modal::Resume(_)));
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let mut picked: Option<String> = None;
    let mut close = false;
    {
        let p = if in_modal {
            match app.modal.as_mut() {
                Some(Modal::Resume(p)) => p,
                _ => return false,
            }
        } else {
            match app.home.picker.as_mut() {
                Some(p) => p,
                None => return false,
            }
        };
        let n = p.visible().len();
        match key.code {
            KeyCode::Esc => {
                if p.searching {
                    p.searching = false;
                } else if !p.query.is_empty() {
                    p.query.clear();
                } else {
                    close = true;
                }
            }
            KeyCode::Enter => picked = p.selected().map(|s| s.id.clone()),
            KeyCode::Down => p.sel = (p.sel + 1).min(n.saturating_sub(1)),
            KeyCode::Up => p.sel = p.sel.saturating_sub(1),
            KeyCode::Char('c') | KeyCode::Char('d') if ctrl && !in_modal => {
                app.quit = true;
                return true;
            }
            KeyCode::Backspace if p.searching => {
                p.query.pop();
                p.sel = 0;
            }
            KeyCode::Char(c) if p.searching && !ctrl => {
                p.query.push(c);
                p.sel = 0;
            }
            KeyCode::Char('/') => p.searching = true,
            KeyCode::Char('j') => p.sel = (p.sel + 1).min(n.saturating_sub(1)),
            KeyCode::Char('k') => p.sel = p.sel.saturating_sub(1),
            _ => {}
        }
    }
    if close {
        app.modal = None;
        app.home.picker = None;
    }
    if let Some(id) = picked {
        app.modal = None;
        app.home.picker = None;
        app.resume_session(id);
    }
    app.dirty = true;
    true
}

pub fn is_open(app: &App) -> bool {
    app.modal.is_some() || (app.screen == Screen::Home && app.home.picker.is_some())
}

// ---- command palette ------------------------------------------------------------------------

/// One palette entry: its section, label and the right-hand text, which also says what it runs.
const PALETTE: &[(&str, &[(&str, &str)])] = &[
    (
        "Session",
        &[
            ("New Session", "Ctrl+N"),
            ("New Session in Worktree", "Ctrl+P → worktree"),
            ("Agent Dashboard", "/dashboard"),
            ("Back to Home", "/home"),
            ("Delete This Session", "/delete"),
            ("Resume Session", "/resume"),
            ("Rename Session", "/rename "),
            ("Session Info", "/session-info"),
            ("Send Feedback", "/feedback"),
        ],
    ),
    (
        "Context",
        &[
            ("Compact History", "/compact"),
            ("Context Usage", "/context"),
            ("View Plan", "/view-plan"),
            ("Memory", "/memory"),
        ],
    ),
    (
        "Model & Input",
        &[
            ("Switch Model", "/model"),
            ("Always Approve Mode", "/always-approve"),
            ("Multiline Input", "/multiline"),
            ("Edit Prompt in External Editor", "/edit-prompt"),
        ],
    ),
    (
        "Tools",
        &[
            ("Hooks", "/hooks"),
            ("Plugins", "/plugins"),
            ("Marketplace", "/marketplace"),
            ("Skills", "/skills"),
            ("Workflows", "/workflows"),
            ("MCP Servers", "/mcps"),
            ("Manage Agents", "/config-agents"),
        ],
    ),
    (
        "Other",
        &[
            ("Switch Theme", "/theme"),
            ("Settings", "F2"),
            ("Keyboard Shortcuts", "Ctrl+."),
            ("How-to Guides", "/docs"),
            ("Tutorial", "/tutorial"),
            ("Quit", "Ctrl+Q"),
        ],
    ),
];

#[derive(Clone, Debug, Default)]
pub struct PaletteState {
    pub query: String,
    /// Index among the visible entries; nothing is highlighted on first paint.
    pub sel: Option<usize>,
    pub top: usize,
}

#[derive(Clone, Debug)]
enum PRow {
    Header(&'static str),
    Entry(usize, &'static str, &'static str),
    Blank,
}

impl PaletteState {
    /// Rows after the search filter: a header stays only if an entry under it matches.
    fn rows(&self) -> Vec<PRow> {
        let q = self.query.to_lowercase();
        let mut rows = Vec::new();
        let mut n = 0;
        for (section, entries) in PALETTE {
            let hits: Vec<_> = entries
                .iter()
                .filter(|(l, r)| {
                    q.is_empty() || l.to_lowercase().contains(&q) || r.to_lowercase().contains(&q)
                })
                .collect();
            if hits.is_empty() {
                continue;
            }
            if !rows.is_empty() {
                rows.push(PRow::Blank);
            }
            rows.push(PRow::Header(section));
            for (l, r) in hits {
                rows.push(PRow::Entry(n, l, r));
                n += 1;
            }
        }
        rows
    }

    fn count(&self) -> usize {
        self.rows()
            .iter()
            .filter(|r| matches!(r, PRow::Entry(..)))
            .count()
    }

    fn selected(&self) -> Option<(&'static str, &'static str)> {
        let i = self.sel?;
        self.rows().into_iter().find_map(|r| match r {
            PRow::Entry(n, l, rt) if n == i => Some((l, rt)),
            _ => None,
        })
    }

    fn scroll_to_selection(&mut self, visible: usize) {
        let Some(i) = self.sel else { return };
        let rows = self.rows();
        let Some(at) = rows
            .iter()
            .position(|r| matches!(r, PRow::Entry(n, ..) if *n == i))
        else {
            return;
        };
        if at < self.top {
            // keep the section header in view when moving up to its first entry
            self.top = if at > 0 && matches!(rows[at - 1], PRow::Header(_)) {
                at - 1
            } else {
                at
            };
        } else if at >= self.top + visible {
            self.top = at + 1 - visible;
        }
    }
}

pub fn open_palette(app: &mut App) {
    app.enter_session();
    app.modal = Some(Modal::Palette(PaletteState::default()));
    app.dirty = true;
}

fn draw_palette(buf: &mut Buffer, app: &App, lay: &Layout, p: &PaletteState) {
    let th = &app.theme;
    let r = modal_rect(lay.w, lay.h, 0.5, 80, 44, 4);
    let inner = modal_frame(buf, app, r, "Commands");
    if inner.height < 8 {
        return;
    }
    // inner.y is the pad row; search, divider, list, pad, hints
    let search_y = inner.y + 1;
    let nx = put(buf, r.x + 3, search_y, " search: ", st(th.gray));
    let q = put(buf, nx, search_y, &p.query, st(th.text_primary));
    put(
        buf,
        q,
        search_y,
        " ",
        Style::new().fg(th.bg_base).bg(th.text_primary),
    );
    put(
        buf,
        r.x,
        search_y + 1,
        &format!("│{}│", "─".repeat(r.width as usize - 2)),
        st(th.gray_dim),
    );
    let list_y = search_y + 2;
    let list_h = inner.bottom().saturating_sub(2).saturating_sub(list_y) as usize;
    let rows = p.rows();
    let scrolled = rows.len() > list_h;
    let x0 = r.x + 3;
    // content shifts one cell left when a scrollbar takes the last column
    let x1 = r.right() - 3 - scrolled as u16;
    for (i, row) in rows.iter().skip(p.top).take(list_h).enumerate() {
        let y = list_y + i as u16;
        match row {
            PRow::Blank => {}
            PRow::Header(name) => {
                let nx = put(buf, x0, y, &format!(" {name} "), bold(th.gray));
                for c in nx..x1 {
                    put(buf, c, y, "─", st(th.gray_dim));
                }
            }
            PRow::Entry(n, label, right) => {
                let selected = p.sel == Some(*n);
                let bg = if selected { th.bg_visual } else { th.bg_base };
                if selected {
                    super::fill(buf, Rect::new(x0, y, x1 - x0, 1), bg);
                }
                put(buf, x0, y, "◆ ", Style::new().fg(th.gray_dim).bg(bg));
                let mut ls = Style::new().fg(th.text_primary).bg(bg);
                if selected {
                    ls = ls.add_modifier(Modifier::BOLD);
                }
                // when label and right text do not both fit, the right text is cut to half the
                // room and the label takes what is left; both end in `…`
                let avail = (x1 - 1).saturating_sub(x0 + 2) as usize;
                let (lw, rw) = (display_width(label), display_width(right));
                let (label, right) = if lw + rw + 2 > avail {
                    let r_cap = (avail.saturating_sub(2)) / 2;
                    let r_w = rw.min(r_cap);
                    (
                        truncate(label, avail.saturating_sub(2 + r_w)),
                        truncate(right, r_w),
                    )
                } else {
                    ((*label).to_string(), (*right).to_string())
                };
                put(buf, x0 + 2, y, &label, ls);
                put_right(buf, x1 - 1, y, &right, Style::new().fg(th.gray).bg(bg));
            }
        }
    }
    if let Some((pos, size)) = super::thumb(rows.len(), list_h, p.top, list_h) {
        for i in pos..(pos + size).min(list_h) {
            put(
                buf,
                r.right() - 2,
                list_y + i as u16,
                "█",
                st(th.gray_dim).bg(th.gray_dim),
            );
        }
    }
    footer_hints(
        buf,
        app,
        inner,
        inner.bottom() - 1,
        &[("↑/↓", "nav"), ("Enter", "select"), ("Esc", "close")],
    );
}

/// Keys for the open palette.
pub fn palette_key(app: &mut App, key: KeyEvent) {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let list_h = {
        let lay = Layout::compute(app);
        let r = modal_rect(lay.w, lay.h, 0.5, 80, 44, 4);
        r.height.saturating_sub(2).saturating_sub(2 + 2 + 1 + 1) as usize
    };
    let Some(Modal::Palette(p)) = app.modal.as_mut() else {
        return;
    };
    let n = p.count();
    match key.code {
        KeyCode::Esc => {
            if p.query.is_empty() {
                app.modal = None;
            } else {
                p.query.clear();
                p.sel = None;
                p.top = 0;
            }
        }
        KeyCode::Char('p') if ctrl => app.modal = None,
        KeyCode::Down => {
            p.sel = Some(match p.sel {
                None => 0,
                Some(i) => (i + 1).min(n.saturating_sub(1)),
            });
            p.scroll_to_selection(list_h);
        }
        KeyCode::Up => {
            p.sel = match p.sel {
                Some(i) if i > 0 => Some(i - 1),
                _ => None,
            };
            p.scroll_to_selection(list_h);
            if p.sel.is_none() {
                p.top = 0;
            }
        }
        KeyCode::Backspace => {
            p.query.pop();
            p.sel = None;
            p.top = 0;
        }
        KeyCode::Char(c) if !ctrl => {
            p.query.push(c);
            p.sel = None;
            p.top = 0;
        }
        KeyCode::Enter => {
            let pick = p.selected().or_else(|| {
                // with a filter and nothing highlighted, Enter takes the first match
                (!p.query.is_empty()).then(|| {
                    p.rows().into_iter().find_map(|r| match r {
                        PRow::Entry(_, l, rt) => Some((l, rt)),
                        _ => None,
                    })
                })?
            });
            if let Some((_, right)) = pick {
                app.modal = None;
                run_palette_entry(app, right);
            }
        }
        _ => {}
    }
    app.dirty = true;
}

fn run_palette_entry(app: &mut App, right: &str) {
    match right {
        "Ctrl+N" => app.new_session(),
        "Ctrl+P → worktree" => app.toast("Worktrees are not built in grokw yet."),
        "F2" => crate::commands::run(app, "settings", ""),
        "Ctrl+." => shortcuts::open(app),
        "Ctrl+Q" => app.quit_now(),
        "/edit-prompt" => {
            // the palette keeps the draft
            app.enter_session();
            let draft = crate::ui::composer::input::expand_all(app, app.ed.text());
            crate::ui::composer::input::request_editor(app, draft);
        }
        "/rename " => {
            app.ed.set_text("/rename ");
            app.enter_session();
        }
        cmd if cmd.starts_with('/') => crate::commands::run(app, &cmd[1..], ""),
        _ => {}
    }
}
