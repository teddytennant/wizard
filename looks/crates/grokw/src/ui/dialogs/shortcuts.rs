//! The keyboard shortcuts cheatsheet (spec 6.4, 8.8): groups that fold, a search, a filter that hides
//! the rows that do nothing in the current pane, and a detail page for a row.

use std::collections::HashSet;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Style;
use tuikit::paint::put_str;
use tuikit::width::{display_width, truncate};

use super::widgets::{self, Hint};
use super::{modal_frame, modal_rect, Modal};
use crate::app::{App, Focus};
use crate::ui::{bold, put, put_right, st, Layout};

/// One row: what it does, the chords, and when it works.
struct Row {
    label: &'static str,
    keys: &'static str,
    /// Works only with the scrollback focused (dimmed while the prompt has the keyboard).
    scrollback: bool,
    /// Man-page text for the detail view; empty when the label says it all.
    help: &'static str,
}

const fn r(label: &'static str, keys: &'static str) -> Row {
    Row {
        label,
        keys,
        scrollback: false,
        help: "",
    }
}

const fn sb(label: &'static str, keys: &'static str) -> Row {
    Row {
        label,
        keys,
        scrollback: true,
        help: "",
    }
}

const fn rh(label: &'static str, keys: &'static str, help: &'static str) -> Row {
    Row {
        label,
        keys,
        scrollback: false,
        help,
    }
}

/// The groups in Grok's order. `Dashboard` rows belong to a screen grokw does not have, so they
/// are always dimmed.
const GROUPS: &[(&str, bool, &[Row])] = &[
    (
        "Essentials",
        false,
        &[
            r("Send", "Enter"),
            r("Focus scrollback", "Tab"),
            r("Cancel turn", "Ctrl+c"),
            r("Cycle mode (Normal / Plan / Always-approve)", "Shift+Tab"),
            r("Quit", "Ctrl+q / Ctrl+d"),
            r("Command palette", "Ctrl+p / ?"),
            r("Keyboard shortcuts", "Ctrl+. / Ctrl+x"),
            r("Open the settings modal", "F2 / Ctrl+, / Super+,"),
        ],
    ),
    (
        "Input",
        false,
        &[
            r("Send now while running (cancels the current turn)", "Ctrl+Enter / Ctrl+i"),
            r("Voice dictation (Ctrl+Space / F8)", "Ctrl+Space / F8"),
            r("Toggle multiline", "Ctrl+m"),
            r("Stash / pop prompt draft", "Ctrl+s / Alt+s"),
            r("Shell mode (type ! on empty prompt)", "!"),
            rh(
                "Paste images (and text) from the clipboard",
                "Ctrl+v",
                "Pastes clipboard images into the prompt as chips, and plain text as typed.\nUse Ctrl+V for screenshots, browser \"Copy Image\", and file-manager image copies.\nYou can also drag an image file into the prompt.",
            ),
            rh(
                "Undo the last prompt edit",
                "Ctrl+z",
                "Undoes the last change in the prompt editor.\nCovers typing, deletes, line/word kills, and clearing a draft.",
            ),
            rh(
                "Redo the last undone prompt edit",
                "Ctrl+Shift+z / Alt+z",
                "Redoes the last undone change in the prompt editor.\nThe second chord is the fallback for terminals that cannot send the first one.",
            ),
            rh(
                "Prompt history",
                "↑",
                "Recalls previously sent prompts.\nPress Up on an empty prompt to browse earlier prompts, newest first; each move live-populates the composer so you can edit and resend.\nWith prompts queued, Up moves focus into the queue pane on the last row instead.\nRun /history to open a searchable history panel and filter by text.",
            ),
        ],
    ),
    (
        "Conversation Navigation",
        true,
        &[
            sb("Select next entry", "↓"),
            sb("Select previous entry", "↑"),
            sb("Next turn", "Shift+→"),
            sb("Previous turn", "Shift+←"),
            sb("Scroll up one line", "Ctrl+k"),
            sb("Scroll down one line", "Ctrl+j"),
            sb("Scroll up half page", "Ctrl+u"),
            sb("Scroll down half page", "Ctrl+d"),
            sb("Scroll up one page", "Page Up"),
            sb("Scroll down one page", "Page Down"),
            rh(
                "Search scrollback",
                "/find",
                "Searches the conversation scrollback for text and jumps between matches.\nIn the prompt input, run /find to search. In vim mode, you can also press / while the scrollback is focused.\nType a query, then use n and N (or the arrow keys) to step through matches. Press Enter to jump to a match and Esc to dismiss.",
            ),
        ],
    ),
    (
        "Conversation Actions",
        true,
        &[
            sb("Collapse selected entry", "←"),
            sb("Expand selected entry", "→"),
            sb("Toggle all thinking blocks", "Ctrl+e"),
            sb("Open in viewer", "Enter / Ctrl+f"),
        ],
    ),
    (
        "Panels",
        false,
        &[
            r("Toggle tasks pane", "Ctrl+g"),
            r("Toggle todo pane", "Ctrl+t"),
            r("Toggle prompt queue", "Ctrl+; / Ctrl+'"),
            r("Open extensions", "Ctrl+l"),
            r("Send running task to background", "Ctrl+b"),
        ],
    ),
    (
        "Session",
        false,
        &[
            r("Open sessions", "Ctrl+r"),
            r("Toggle always-approve", "Ctrl+o"),
            r("New session", "Ctrl+n"),
            r("Pick model", "Ctrl+m"),
        ],
    ),
    (
        "Dashboard",
        true,
        &[
            rh("Open the Agent Dashboard", "Ctrl+\\ / Ctrl+4", ""),
            r("Select next row", "↓ / j"),
            r("Select previous row", "↑ / k"),
            r("Pin / unpin agent", "Ctrl+t"),
            r("Rename agent", "Ctrl+r"),
            r("Stop / Delete agent", "Ctrl+x"),
            r("Cycle dispatch mode", "Shift+Tab"),
            r("Toggle row grouping", "Ctrl+g"),
            r("Reorder agent up", "Shift+↑"),
            r("Reorder agent down", "Shift+↓"),
            r("Show shortcuts overlay", "Ctrl+. / ?"),
            rh(
                "Close dashboard",
                "Esc",
                "Closes the dashboard and returns to where you were. Esc is a cascade: it first dismisses an open peek or clears an active filter, and only exits once nothing else is pending.",
            ),
            r("Toggle always-approve", "Ctrl+O"),
            r("Change working directory for new agents", "Ctrl+l"),
            r("Toggle worktree mode for new agents", "Ctrl+w"),
            r("Previous session", "Ctrl+["),
            r("Next session", "Ctrl+]"),
        ],
    ),
];

#[derive(Clone, Debug)]
pub struct Detail {
    pub title: String,
    pub keys: String,
    pub body: String,
    pub dimmed: bool,
    pub scroll: u16,
}

#[derive(Clone, Debug)]
pub struct Shortcuts {
    pub query: String,
    pub searching: bool,
    /// Groups that are open.
    pub open: HashSet<usize>,
    /// Rows showing their help under them.
    pub inline: HashSet<(usize, usize)>,
    pub sel: usize,
    pub top: usize,
    /// `f`: hide the rows that do nothing here.
    pub hide_dim: bool,
    pub detail: Option<Detail>,
    /// The scrollback has the keyboard when it opened; decides which rows are dimmed.
    pub scrollback: bool,
}

impl Shortcuts {
    pub fn new(app: &App) -> Shortcuts {
        Shortcuts {
            query: String::new(),
            searching: false,
            open: HashSet::from([0]),
            inline: HashSet::new(),
            sel: 1,
            top: 0,
            hide_dim: false,
            detail: None,
            scrollback: app.focus == Focus::Scrollback,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Vis {
    Header(usize),
    Entry(usize, usize),
    /// A line of the help text under an expanded row.
    Help(usize, usize, usize),
}

fn dimmed(s: &Shortcuts, g: usize, row: &Row) -> bool {
    GROUPS[g].0 == "Dashboard" || (row.scrollback && !s.scrollback)
}

fn matches(q: &str, row: &Row) -> bool {
    let q = q.to_lowercase();
    row.label.to_lowercase().contains(&q) || row.keys.to_lowercase().contains(&q)
}

fn rows(s: &Shortcuts) -> Vec<Vis> {
    let mut v = Vec::new();
    for (g, (_, _, entries)) in GROUPS.iter().enumerate() {
        let hits: Vec<usize> = entries
            .iter()
            .enumerate()
            .filter(|(_, e)| s.query.is_empty() || matches(&s.query, e))
            .filter(|(_, e)| !(s.hide_dim && dimmed(s, g, e)))
            .map(|(i, _)| i)
            .collect();
        if hits.is_empty() && (!s.query.is_empty() || s.hide_dim) {
            continue;
        }
        v.push(Vis::Header(g));
        if s.open.contains(&g) || !s.query.is_empty() {
            for i in hits {
                v.push(Vis::Entry(g, i));
                if s.inline.contains(&(g, i)) {
                    let help = help_of(&GROUPS[g].2[i]);
                    for (k, _) in help.split('\n').enumerate() {
                        v.push(Vis::Help(g, i, k));
                    }
                }
            }
        }
    }
    v
}

fn help_of(e: &Row) -> &str {
    if e.help.is_empty() {
        e.label
    } else {
        e.help
    }
}

fn selectable(v: &Vis) -> bool {
    !matches!(v, Vis::Help(..))
}

fn footer(s: &Shortcuts) -> Vec<Hint<'static>> {
    if s.detail.is_some() {
        return vec![("Esc", "back"), ("↑/↓", "scroll"), ("Ctrl+./X", "close")];
    }
    vec![
        ("↑/↓", "nav"),
        ("f", if s.hide_dim { "show all" } else { "filter" }),
        ("e/Space/→", "expand"),
        ("←", "collapse"),
        ("Enter", "details"),
        ("/", "search"),
        ("Esc", "close"),
    ]
}

pub fn open(app: &mut App) {
    app.enter_session();
    app.modal = Some(Modal::Shortcuts(Shortcuts::new(app)));
    app.dirty = true;
}

pub fn draw(buf: &mut Buffer, app: &App, lay: &Layout, s: &Shortcuts) {
    let th = &app.theme;
    let r = modal_rect(lay.w, lay.h, 0.7, 80, 44, 4);
    let inner = modal_frame(buf, app, r, "Keyboard Shortcuts");
    if inner.height < 8 {
        return;
    }
    let hints = footer(s);
    let f = widgets::foot_rows(r, &hints);
    let b = widgets::body(r, f, false);
    widgets::draw_hints(buf, app, r, &hints);
    if let Some(d) = &s.detail {
        draw_detail(buf, app, r, &b, d);
        return;
    }
    widgets::draw_search(buf, app, r, b.search_y, &s.query, s.searching);
    widgets::draw_divider(buf, app, r, b.div_y, 0);
    let vis = rows(s);
    let h = b.list_h as usize;
    let bar = vis.len() > h;
    let x0 = r.x + 3;
    let x1 = r.right() - 3 - bar as u16;
    for (i, v) in vis.iter().skip(s.top).take(h).enumerate() {
        let y = b.list_y + i as u16;
        let selected = s.top + i == s.sel;
        let bg = if selected { th.bg_visual } else { th.bg_base };
        if selected {
            widgets::band(buf, app, x0, x1, y);
        }
        let bg_style = |fg| Style::new().fg(fg).bg(bg);
        match *v {
            Vis::Header(g) => {
                let (name, _, entries) = GROUPS[g];
                let open = s.open.contains(&g) || !s.query.is_empty();
                let mut ls = bg_style(th.text_primary);
                if selected {
                    ls = ls.add_modifier(widgets::bolded(true));
                }
                let n = if s.query.is_empty() && !s.hide_dim {
                    entries.len()
                } else {
                    vis.iter()
                        .filter(|e| matches!(e, Vis::Entry(gg, _) if *gg == g))
                        .count()
                };
                put(
                    buf,
                    x0,
                    y,
                    if open { "◆ " } else { "› " },
                    bg_style(th.gray_dim),
                );
                let text = if open {
                    name.to_string()
                } else {
                    format!("{name} ({n})")
                };
                put(buf, x0 + 2, y, &text, ls);
            }
            Vis::Entry(g, i) => {
                let e = &GROUPS[g].2[i];
                let dim = dimmed(s, g, e);
                let mut ls = bg_style(if dim {
                    th.recede(th.text_primary, 0.5)
                } else {
                    th.text_primary
                });
                if selected {
                    ls = ls.add_modifier(widgets::bolded(true));
                }
                let kx = x1 - 1;
                let kw = display_width(e.keys);
                // the label gives way to the keys, ending in `…`
                let room = (kx as usize).saturating_sub(x0 as usize + 4 + kw + 2);
                put(buf, x0 + 2, y, "◆ ", bg_style(th.gray_dim));
                put(buf, x0 + 4, y, &truncate(e.label, room), ls);
                put_right(
                    buf,
                    kx,
                    y,
                    e.keys,
                    bg_style(if dim { th.gray_dim } else { th.gray }),
                );
            }
            Vis::Help(g, i, k) => {
                let line = help_of(&GROUPS[g].2[i]).split('\n').nth(k).unwrap_or("");
                put_str(
                    buf,
                    x0 + 6,
                    y,
                    line,
                    Style::new()
                        .fg(th.gray)
                        .bg(bg)
                        .add_modifier(ratatui::style::Modifier::ITALIC),
                    Rect::new(x0, y, x1 - x0, 1),
                );
            }
        }
    }
    if bar {
        widgets::draw_scrollbar(buf, app, r, b.list_y, b.list_h, vis.len(), s.top);
    }
}

fn draw_detail(buf: &mut Buffer, app: &App, r: Rect, b: &widgets::Body, d: &Detail) {
    let th = &app.theme;
    let x = r.x + 3;
    let w = (r.width as usize).saturating_sub(6);
    let mut lines: Vec<(String, Style)> = vec![(d.title.clone(), bold(th.text_primary))];
    if !d.keys.is_empty() {
        lines.push((d.keys.clone(), st(th.gray_bright)));
    }
    if !d.body.is_empty() && d.body != d.title {
        lines.push((String::new(), st(th.text_primary)));
        for (i, para) in d.body.split('\n').enumerate() {
            if i > 0 {
                lines.push((String::new(), st(th.text_primary)));
            }
            for l in wrap(para, w) {
                lines.push((l, st(th.text_primary)));
            }
        }
    }
    if d.dimmed {
        lines.push((String::new(), st(th.text_primary)));
        lines.push(("(not active in current context)".into(), st(th.gray_dim)));
    }
    let h = (b.foot_y.saturating_sub(1)).saturating_sub(b.search_y) as usize;
    let skip = (d.scroll as usize).min(lines.len().saturating_sub(h));
    for (i, (l, s)) in lines.iter().skip(skip).take(h).enumerate() {
        put(buf, x, b.search_y + i as u16, l, *s);
    }
}

/// Greedy word wrap at `w` cells.
pub fn wrap(s: &str, w: usize) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    for word in s.split(' ') {
        if cur.is_empty() {
            cur = word.to_string();
        } else if display_width(&cur) + 1 + display_width(word) <= w {
            cur.push(' ');
            cur.push_str(word);
        } else {
            out.push(std::mem::take(&mut cur));
            cur = word.to_string();
        }
    }
    out.push(cur);
    out
}

fn list_h(app: &App, s: &Shortcuts) -> usize {
    let lay = Layout::compute(app);
    let r = modal_rect(lay.w, lay.h, 0.7, 80, 44, 4);
    let f = widgets::foot_rows(r, &footer(s));
    widgets::body(r, f, false).list_h as usize
}

pub fn key(app: &mut App, key: KeyEvent) {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let h = {
        let Some(Modal::Shortcuts(s)) = app.modal.as_ref() else {
            return;
        };
        list_h(app, s)
    };
    let Some(Modal::Shortcuts(s)) = app.modal.as_mut() else {
        return;
    };
    // Ctrl+. and Ctrl+x close from anywhere
    if ctrl && matches!(key.code, KeyCode::Char('.' | 'x')) {
        app.modal = None;
        return;
    }
    if let Some(d) = s.detail.as_mut() {
        match key.code {
            KeyCode::Esc | KeyCode::Backspace | KeyCode::Left | KeyCode::Char('h') => {
                s.detail = None;
            }
            KeyCode::Down | KeyCode::Char('j') => d.scroll = d.scroll.saturating_add(1),
            KeyCode::Up | KeyCode::Char('k') => d.scroll = d.scroll.saturating_sub(1),
            KeyCode::PageDown => d.scroll = d.scroll.saturating_add(10),
            KeyCode::PageUp => d.scroll = d.scroll.saturating_sub(10),
            _ => {}
        }
        return;
    }
    if s.searching {
        match key.code {
            KeyCode::Esc => {
                s.searching = false;
                s.query.clear();
            }
            KeyCode::Enter => s.searching = false,
            KeyCode::Backspace => {
                s.query.pop();
            }
            KeyCode::Char(c) if !ctrl => s.query.push(c),
            _ => {}
        }
        s.sel = rows(s).iter().position(selectable).unwrap_or(0);
        s.top = 0;
        return;
    }
    let v = rows(s);
    let move_sel = |s: &mut Shortcuts, delta: isize| {
        let mut i = s.sel as isize;
        loop {
            i += delta;
            if i < 0 || i as usize >= v.len() {
                return;
            }
            if selectable(&v[i as usize]) {
                s.sel = i as usize;
                return;
            }
        }
    };
    match key.code {
        KeyCode::Esc => {
            if !s.query.is_empty() {
                s.query.clear();
            } else {
                app.modal = None;
                return;
            }
        }
        KeyCode::Char('/' | 'i') => s.searching = true,
        KeyCode::Down | KeyCode::Char('j') => move_sel(s, 1),
        KeyCode::Up | KeyCode::Char('k') => move_sel(s, -1),
        KeyCode::PageDown => s.sel = (s.sel + 10).min(v.len().saturating_sub(1)),
        KeyCode::PageUp => s.sel = s.sel.saturating_sub(10),
        KeyCode::Char('f') => s.hide_dim = !s.hide_dim,
        KeyCode::Char('e' | ' ') | KeyCode::Right => match v.get(s.sel) {
            Some(Vis::Header(g)) => {
                s.open.insert(*g);
            }
            Some(Vis::Entry(g, i)) => {
                s.inline.insert((*g, *i));
            }
            _ => {}
        },
        KeyCode::Char('E') | KeyCode::Left => match v.get(s.sel) {
            Some(Vis::Header(g)) => {
                s.open.remove(g);
            }
            // collapsing a row that is already closed folds its group
            Some(Vis::Entry(g, i)) if !s.inline.remove(&(*g, *i)) => {
                s.open.remove(g);
                if let Some(p) = v.iter().position(|x| *x == Vis::Header(*g)) {
                    s.sel = p;
                }
            }
            _ => {}
        },
        KeyCode::Enter => match v.get(s.sel) {
            Some(Vis::Header(g)) if !s.open.remove(g) => {
                s.open.insert(*g);
            }
            Some(Vis::Entry(g, i)) => {
                let e = &GROUPS[*g].2[*i];
                let dim = dimmed(s, *g, e);
                s.detail = Some(Detail {
                    title: e.label.to_string(),
                    keys: e.keys.to_string(),
                    body: help_of(e).to_string(),
                    dimmed: dim,
                    scroll: 0,
                });
                s.query.clear();
                s.searching = false;
            }
            _ => {}
        },
        _ => {}
    }
    // keep the selection on a row that is there, and in view
    let v = rows(s);
    if v.is_empty() {
        s.sel = 0;
    } else {
        s.sel = s.sel.min(v.len() - 1);
        if !selectable(&v[s.sel]) {
            s.sel = s.sel.saturating_sub(1);
        }
    }
    if s.sel < s.top {
        s.top = s.sel;
    } else if s.sel >= s.top + h {
        s.top = s.sel + 1 - h.max(1);
    }
    s.top = s.top.min(v.len().saturating_sub(h));
}
