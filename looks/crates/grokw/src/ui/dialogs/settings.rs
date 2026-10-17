//! The settings modal (spec 6.5). The rows, labels, descriptions, search words and choice lists
//! are Grok Build's own (`settings_defs.rs` is generated from its catalog). A row is either
//! wired to something grokw does (compact mode, timestamps, vim scrollback keys, thinking blocks,
//! multiline, the theme) or only remembered, and a remembered one says so under its description.
//! Rows for what wizard owns (permissions, models, data sharing) are not listed.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use tuikit::paint::put_str;
use tuikit::width::{display_width, truncate};

use super::settings_defs::{DEFS, HINT_CHILDREN};
use super::widgets::{self, Hint};
use super::{modal_frame, modal_rect, Modal};
use crate::app::App;
use crate::ui::{bold, put, put_right, st, Layout};

// ---- the catalog's types --------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cat {
    Appearance,
    Mouse,
    Editor,
    Advanced,
}

impl Cat {
    fn title(self) -> &'static str {
        match self {
            Cat::Appearance => "Appearance",
            Cat::Mouse => "Mouse",
            Cat::Editor => "Editor & Input",
            Cat::Advanced => "Advanced",
        }
    }
}

pub struct Choice {
    pub canon: &'static str,
    pub name: &'static str,
    pub desc: &'static str,
}

pub enum Kind {
    Bool(bool),
    Enum {
        default: &'static str,
        choices: &'static [Choice],
        preview: bool,
    },
    Int {
        default: i64,
        min: i64,
        max: i64,
    },
    Group(&'static [Def]),
}

pub struct Def {
    pub key: &'static str,
    pub cat: Cat,
    pub label: &'static str,
    pub desc: &'static str,
    pub keywords: &'static [&'static str],
    pub kind: Kind,
    pub restart: bool,
}

// ---- values ---------------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Val {
    B(bool),
    S(String),
    I(i64),
}

/// What the modal remembers for rows grokw does not act on; saved beside the other state.
#[derive(Clone, Debug, Default)]
pub struct Prefs {
    vals: HashMap<String, Val>,
    path: Option<PathBuf>,
}

impl Prefs {
    pub fn load(state_dir: Option<&std::path::Path>) -> Prefs {
        let mut p = Prefs::default();
        if let Some(d) = state_dir {
            let path = d.join("settings.json");
            if let Ok(s) = std::fs::read_to_string(&path) {
                if let Ok(m) = serde_json::from_str::<HashMap<String, serde_json::Value>>(&s) {
                    for (k, v) in m {
                        let val = match v {
                            serde_json::Value::Bool(b) => Val::B(b),
                            serde_json::Value::String(s) => Val::S(s),
                            serde_json::Value::Number(n) => Val::I(n.as_i64().unwrap_or(0)),
                            _ => continue,
                        };
                        p.vals.insert(k, val);
                    }
                }
            }
            p.path = Some(path);
        }
        p
    }

    fn save(&self) {
        let Some(path) = &self.path else { return };
        let m: HashMap<&str, serde_json::Value> = self
            .vals
            .iter()
            .map(|(k, v)| {
                (
                    k.as_str(),
                    match v {
                        Val::B(b) => serde_json::Value::Bool(*b),
                        Val::S(s) => serde_json::Value::String(s.clone()),
                        Val::I(i) => serde_json::Value::from(*i),
                    },
                )
            })
            .collect();
        if let Ok(s) = serde_json::to_string_pretty(&m) {
            let _ = std::fs::write(path, s);
        }
    }
}

fn default_of(d: &Def) -> Val {
    match &d.kind {
        Kind::Bool(b) => Val::B(*b),
        Kind::Enum { default, .. } => Val::S((*default).to_string()),
        Kind::Int { default, .. } => Val::I(*default),
        Kind::Group(_) => Val::B(true),
    }
}

/// Rows grokw acts on: what the row reads and writes is app state, not a remembered value.
fn wired(key: &str) -> bool {
    matches!(
        key,
        "compact_mode"
            | "show_timestamps"
            | "vim_mode"
            | "show_thinking_blocks"
            | "multiline_mode"
            | "theme"
    )
}

impl App {
    /// A remembered integer setting, for tests.
    pub fn prefs_get_int(&self, key: &str) -> Option<i64> {
        match self.prefs.vals.get(key) {
            Some(Val::I(i)) => Some(*i),
            _ => None,
        }
    }
}

pub fn value(app: &App, d: &Def) -> Val {
    match d.key {
        "compact_mode" => Val::B(app.compact_mode),
        "show_timestamps" => Val::B(app.timestamps),
        "vim_mode" => Val::B(app.vim_mode),
        "show_thinking_blocks" => Val::B(app.show_thinking),
        "multiline_mode" => Val::B(app.multiline),
        "theme" => Val::S(app.theme.kind.canonical().to_string()),
        k => app
            .prefs
            .vals
            .get(k)
            .cloned()
            .unwrap_or_else(|| default_of(d)),
    }
}

fn show(d: &Def, v: &Val) -> String {
    match (v, &d.kind) {
        (Val::B(b), _) => if *b { "on" } else { "off" }.to_string(),
        (Val::I(i), _) => i.to_string(),
        (Val::S(s), Kind::Enum { choices, .. }) => choices
            .iter()
            .find(|c| c.canon == s)
            .map_or(s.clone(), |c| c.name.to_string()),
        (Val::S(s), _) => s.clone(),
    }
}

fn set_value(app: &mut App, d: &Def, v: Val) {
    match (d.key, &v) {
        ("compact_mode", Val::B(b)) => app.compact_mode = *b,
        ("show_timestamps", Val::B(b)) => app.timestamps = *b,
        ("vim_mode", Val::B(b)) => app.vim_mode = *b,
        ("show_thinking_blocks", Val::B(b)) => app.show_thinking = *b,
        ("multiline_mode", Val::B(b)) => app.multiline = *b,
        ("theme", Val::S(s)) => crate::commands::run(app, "theme", s),
        (k, _) => {
            app.prefs.vals.insert(k.to_string(), v.clone());
            app.prefs.save();
        }
    }
    if d.key != "theme" {
        let t = format!("✓ {}: {}", d.label, show(d, &v));
        app.toast(t);
    }
}

// ---- state ----------------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub enum Pane {
    Browse,
    /// An enum chooser: the focused choice and what to put back on Esc.
    Choose {
        key: &'static str,
        sel: usize,
        orig: String,
    },
    /// A number stepper.
    Step {
        key: &'static str,
        val: i64,
    },
    /// The `Show contextual hints` sheet.
    Sheet {
        sel: usize,
    },
}

#[derive(Clone, Debug)]
pub struct Settings {
    pub query: String,
    pub searching: bool,
    pub sel: usize,
    pub top: usize,
    pub open: HashSet<&'static str>,
    pub pane: Pane,
    pub confirm_reset: Option<&'static str>,
}

fn find(key: &str) -> Option<&'static Def> {
    DEFS.iter()
        .chain(HINT_CHILDREN.iter())
        .find(|d| d.key == key)
}

pub fn open(app: &mut App) {
    app.enter_session();
    app.modal = Some(Modal::Settings(Settings {
        query: String::new(),
        searching: false,
        sel: 0,
        top: 0,
        open: HashSet::new(),
        pane: Pane::Browse,
        confirm_reset: None,
    }));
    app.dirty = true;
}

/// The rows that match the filter, grouped by category in Grok's order and in catalog order
/// within a category.
fn visible(s: &Settings) -> Vec<&'static Def> {
    let q = s.query.to_lowercase();
    let mut v: Vec<&'static Def> = DEFS
        .iter()
        .filter(|d| {
            q.is_empty()
                || d.label.to_lowercase().contains(&q)
                || d.keywords.iter().any(|k| k.contains(&q))
        })
        .collect();
    v.sort_by_key(|d| d.cat as u8);
    v
}

#[derive(Clone, Copy, Debug)]
enum Item {
    Header(Cat),
    Blank,
    Row(usize),
    Desc(usize, usize),
}

fn desc_lines(d: &Def, width: usize) -> Vec<String> {
    let mut text = d.desc.to_string();
    if d.restart {
        text.push_str(" · restart");
    }
    if !wired(d.key) && !matches!(d.kind, Kind::Group(_)) {
        text.push_str(" · remembered only; grokw does not act on it");
    }
    let mut v = super::shortcuts::wrap(&text, width);
    v.truncate(8);
    v
}

fn items(s: &Settings, vis: &[&'static Def], width: usize) -> Vec<Item> {
    let mut v = Vec::new();
    let mut last: Option<Cat> = None;
    for (i, d) in vis.iter().enumerate() {
        if last != Some(d.cat) {
            if last.is_some() {
                v.push(Item::Blank);
            }
            v.push(Item::Header(d.cat));
            last = Some(d.cat);
        }
        v.push(Item::Row(i));
        if s.open.contains(d.key) {
            for k in 0..desc_lines(d, width).len() {
                v.push(Item::Desc(i, k));
            }
        }
    }
    v
}

fn footer(s: &Settings) -> Vec<Hint<'static>> {
    if s.confirm_reset.is_some() {
        return vec![
            ("y", "reset"),
            ("n", "cancel"),
            ("Esc", "cancel"),
            ("F2", "cancel"),
        ];
    }
    match &s.pane {
        Pane::Choose { key, .. } => {
            let preview = matches!(
                find(key).map(|d| &d.kind),
                Some(Kind::Enum { preview: true, .. })
            );
            vec![
                ("↑/↓", if preview { "try" } else { "nav" }),
                ("Enter", "select"),
                ("double-click", "select"),
                ("Esc", if preview { "revert" } else { "cancel" }),
                ("d", "reset"),
            ]
        }
        Pane::Step { .. } => vec![
            ("↑/↓", "+/-a"),
            ("←/→", "+/-b"),
            ("Enter", "commit"),
            ("Esc", "cancel"),
            ("d", "reset"),
        ],
        Pane::Sheet { .. } => vec![
            ("↑/↓/j/k", "nav"),
            ("Space/Enter", "toggle"),
            ("Esc", "back"),
        ],
        Pane::Browse if s.searching => vec![
            ("type", "to filter"),
            ("↑/↓", "nav"),
            ("Backspace", "edit"),
            ("Enter", "commit"),
            ("Esc", "clear"),
        ],
        Pane::Browse => vec![
            ("↑/↓/j/k", "nav"),
            ("g/G", "top/btm"),
            ("Space", "toggle"),
            ("Enter", "toggle"),
            ("→", "expand"),
            ("/", "search"),
            ("d", "reset"),
            ("F2/Esc", "close"),
        ],
    }
}

fn tip(width: u16) -> &'static str {
    const LONG: &str =
        "Tip · Ask Wizard: \"change theme to grokday\" or \"what does compact mode do?\"";
    const SHORT: &str = "Tip · Ask Wizard to change a setting";
    if display_width(LONG) + 6 <= width as usize {
        LONG
    } else {
        SHORT
    }
}

fn win(lay: &Layout) -> Rect {
    modal_rect(lay.w, lay.h, 0.7, 110, 44, 3)
}

// ---- drawing --------------------------------------------------------------------------------

pub fn draw(buf: &mut Buffer, app: &App, lay: &Layout, s: &Settings) {
    let r = win(lay);
    let title = match &s.pane {
        Pane::Browse => "Settings".to_string(),
        Pane::Choose { key, .. } | Pane::Step { key, .. } => {
            format!("Settings › {}", find(key).map_or("", |d| d.label))
        }
        Pane::Sheet { .. } => "Settings › Show contextual hints".to_string(),
    };
    let inner = modal_frame(buf, app, r, &title);
    if inner.height < 8 {
        return;
    }
    // a sub-pane's title is underlined
    if !matches!(s.pane, Pane::Browse) {
        for x in r.x + 3..(r.x + 3 + display_width(&title) as u16).min(r.right() - 7) {
            if let Some(c) = buf.cell_mut((x, r.y)) {
                c.modifier |= Modifier::UNDERLINED;
            }
        }
    }
    let hints = footer(s);
    let f = widgets::foot_rows(r, &hints);
    let browse = matches!(s.pane, Pane::Browse);
    let b = widgets::body(r, f, browse);
    widgets::draw_hints(buf, app, r, &hints);
    match &s.pane {
        Pane::Browse => draw_browse(buf, app, r, &b, s),
        Pane::Choose { key, sel, .. } => draw_choose(buf, app, r, &b, key, *sel),
        Pane::Step { key, val } => draw_step(buf, app, r, &b, key, *val),
        Pane::Sheet { sel } => draw_sheet(buf, app, r, &b, *sel),
    }
}

fn draw_browse(buf: &mut Buffer, app: &App, r: Rect, b: &widgets::Body, s: &Settings) {
    let th = &app.theme;
    widgets::draw_search(buf, app, r, b.search_y, &s.query, s.searching);
    widgets::draw_divider(buf, app, r, b.div_y, 2);
    if let Some(t) = b.tip_y {
        let text = tip(r.width);
        let x = r.x + (r.width.saturating_sub(display_width(text) as u16)) / 2;
        put(
            buf,
            x,
            t,
            text,
            st(th.gray_dim).add_modifier(Modifier::ITALIC),
        );
    }
    let vis = visible(s);
    let x0 = r.x + 3;
    let x1 = r.right() - 3;
    let its = items(s, &vis, (x1 - x0) as usize - 4);
    let h = b.list_h as usize;
    let dim_rest = s.confirm_reset.is_some();
    for (i, it) in its.iter().skip(s.top).take(h).enumerate() {
        let y = b.list_y + i as u16;
        // a header never loses its row to the gap above it: on the last row the gap gives way
        let it = match (*it, its.get(s.top + i + 1)) {
            (Item::Blank, Some(Item::Header(c))) if i + 1 == h => Item::Header(*c),
            (it, _) => it,
        };
        match it {
            Item::Blank => {}
            Item::Header(c) => {
                let nx = put(buf, x0, y, &format!(" {} ", c.title()), bold(th.gray));
                for x in nx..x1 {
                    put(buf, x, y, "─", st(th.gray_dim));
                }
            }
            Item::Row(vi) => {
                let d = vis[vi];
                let selected = vi == s.sel;
                let bg = if selected { th.bg_visual } else { th.bg_base };
                if selected {
                    widgets::band(buf, app, x0, x1, y);
                }
                if selected && s.confirm_reset == Some(d.key) {
                    let q = format!(
                        "Reset '{}' to default ({})?",
                        d.label,
                        show(d, &default_of(d))
                    );
                    put_str(
                        buf,
                        x0 + 2,
                        y,
                        &q,
                        Style::new()
                            .fg(th.text_secondary)
                            .bg(th.bg_visual)
                            .add_modifier(Modifier::BOLD),
                        Rect::new(x0, y, x1 - x0, 1),
                    );
                    continue;
                }
                let fade = |c| {
                    if dim_rest {
                        th.recede(c, 0.5)
                    } else {
                        c
                    }
                };
                let mut ls = Style::new().fg(fade(th.text_primary)).bg(bg);
                if selected {
                    ls = ls.add_modifier(Modifier::BOLD);
                }
                put(buf, x0, y, "▸ ", ls);
                put_str(buf, x0 + 2, y, d.label, ls, Rect::new(x0, y, x1 - x0, 1));
                let v = value(app, d);
                let text = match &d.kind {
                    Kind::Group(_) => String::new(),
                    _ => show(d, &v),
                };
                let off = matches!(v, Val::B(false));
                let vc = if off { th.gray } else { th.text_secondary };
                let edge = r.right() - 7;
                put_right(buf, edge, y, &text, Style::new().fg(fade(vc)).bg(bg));
                if matches!(d.kind, Kind::Enum { .. } | Kind::Group(_)) {
                    put(
                        buf,
                        r.right() - 5,
                        y,
                        "›",
                        Style::new().fg(fade(th.gray)).bg(bg),
                    );
                }
            }
            Item::Desc(vi, k) => {
                let lines = desc_lines(vis[vi], (x1 - x0) as usize - 4);
                let t = lines.get(k).map_or("", String::as_str);
                put_str(
                    buf,
                    x0 + 4,
                    y,
                    t,
                    Style::new().fg(th.gray).add_modifier(Modifier::ITALIC),
                    Rect::new(x0, y, x1 - x0, 1),
                );
            }
        }
    }
    if vis.is_empty() {
        put(buf, x0, b.list_y, "  No matching settings", st(th.gray_dim));
    }
}

fn draw_choose(buf: &mut Buffer, app: &App, r: Rect, b: &widgets::Body, key: &str, sel: usize) {
    let th = &app.theme;
    let Some(d) = find(key) else { return };
    let Kind::Enum { choices, .. } = &d.kind else {
        return;
    };
    let committed = match value(app, d) {
        Val::S(s) => s,
        _ => String::new(),
    };
    let x0 = r.x + 3;
    let x1 = r.right() - 3;
    let top = b.search_y;
    put(buf, x0, top, d.label, bold(th.text_primary));
    let mut y = top + 1;
    for l in super::shortcuts::wrap(d.desc, (x1 - x0) as usize) {
        put(buf, x0, y, &l, st(th.gray_dim));
        y += 1;
    }
    y += 1;
    let room = b.foot_y.saturating_sub(2).saturating_sub(y) as usize;
    let first = sel.saturating_sub(room.saturating_sub(1));
    for (i, c) in choices.iter().enumerate().skip(first).take(room) {
        let focused = i == sel;
        let bg = if focused { th.bg_visual } else { th.bg_base };
        if focused {
            widgets::band(buf, app, x0, x1, y);
        }
        let on = c.canon == committed;
        let glyph = if on { "●" } else { "○" };
        let gc = if on { th.text_secondary } else { th.gray };
        let mut x = put(
            buf,
            x0,
            y,
            &format!(" {glyph}  "),
            Style::new().fg(gc).bg(bg),
        );
        let mut ns = Style::new().fg(th.text_primary).bg(bg);
        if focused {
            ns = ns.add_modifier(Modifier::BOLD);
        }
        x = put(buf, x, y, c.name, ns);
        if !c.desc.is_empty() {
            let room = (x1 as usize).saturating_sub(x as usize + 3);
            put(
                buf,
                x,
                y,
                &format!(" · {}", truncate(c.desc, room)),
                Style::new().fg(th.gray).bg(bg),
            );
        }
        y += 1;
    }
    if choices.len() > first + room {
        put(
            buf,
            x0,
            y,
            &format!("… {} more", choices.len() - first - room),
            st(th.gray_dim),
        );
    }
}

fn draw_step(buf: &mut Buffer, app: &App, r: Rect, b: &widgets::Body, key: &str, val: i64) {
    let th = &app.theme;
    let Some(d) = find(key) else { return };
    let x0 = r.x + 3;
    put(buf, x0, b.search_y, d.label, bold(th.text_primary));
    let mut y = b.search_y + 1;
    for l in super::shortcuts::wrap(d.desc, (r.width as usize).saturating_sub(6)) {
        put(buf, x0, y, &l, st(th.gray_dim));
        y += 1;
    }
    y += 1;
    let nx = put(buf, x0, y, " ‹  ", st(th.gray));
    let nx = put(buf, nx, y, &val.to_string(), bold(th.text_secondary));
    put(buf, nx, y, "  ›", st(th.gray));
}

fn draw_sheet(buf: &mut Buffer, app: &App, r: Rect, b: &widgets::Body, sel: usize) {
    let th = &app.theme;
    let x0 = r.x + 3;
    let x1 = r.right() - 3;
    for (i, d) in HINT_CHILDREN.iter().enumerate() {
        let y = b.search_y + i as u16;
        let focused = i == sel;
        let bg = if focused { th.bg_visual } else { th.bg_base };
        if focused {
            widgets::band(buf, app, x0, x1, y);
        }
        let on = matches!(value(app, d), Val::B(true));
        put(
            buf,
            x0,
            y,
            " ● ",
            Style::new()
                .fg(if on { th.text_secondary } else { th.gray })
                .bg(bg),
        );
        let mut ls = Style::new().fg(th.text_primary).bg(bg);
        if focused {
            ls = ls.add_modifier(Modifier::BOLD);
        }
        put(buf, x0 + 3, y, d.label, ls);
        put_right(
            buf,
            x1 - 1,
            y,
            if on { "on" } else { "off" },
            Style::new()
                .fg(if on { th.text_secondary } else { th.gray })
                .bg(bg),
        );
    }
}

// ---- keys -----------------------------------------------------------------------------------

fn list_h(app: &App, s: &Settings) -> usize {
    let lay = Layout::compute(app);
    let r = win(&lay);
    let f = widgets::foot_rows(r, &footer(s));
    widgets::body(r, f, true).list_h as usize
}

fn step_sizes(min: i64, max: i64) -> (i64, i64) {
    match max - min {
        0..=20 => (1, 1),
        21..=100 => (1, 5),
        _ => (5, 10),
    }
}

fn reset(app: &mut App, d: &'static Def) {
    let def = default_of(d);
    if value(app, d) == def {
        let t = format!("{}: already at default", d.label);
        app.toast(t);
    } else {
        set_value(app, d, def);
    }
}

pub fn key(app: &mut App, key: KeyEvent) {
    let h = match app.modal.as_ref() {
        Some(Modal::Settings(s)) => list_h(app, s),
        _ => return,
    };
    let Some(Modal::Settings(s)) = app.modal.clone() else {
        return;
    };
    let mut s = s;
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let mut close = false;
    if key.code == KeyCode::F(2) {
        // F2 closes from anywhere
        app.modal = None;
        return;
    }
    // a reset waiting for an answer
    if let Some(k) = s.confirm_reset {
        match key.code {
            KeyCode::Char('y') => {
                if let Some(d) = find(k) {
                    reset(app, d);
                }
                s.confirm_reset = None;
            }
            KeyCode::Char('n') | KeyCode::Esc => s.confirm_reset = None,
            _ => {}
        }
        app.modal = Some(Modal::Settings(s));
        return;
    }
    match s.pane.clone() {
        Pane::Choose { key: k, sel, orig } => {
            let Some(d) = find(k) else { return };
            let Kind::Enum {
                choices, preview, ..
            } = &d.kind
            else {
                return;
            };
            let n = choices.len();
            let mut sel = sel;
            let try_it = |app: &mut App, i: usize| {
                if *preview && d.key == "theme" {
                    if let Some(kind) = crate::theme::Kind::parse(choices[i].canon) {
                        if kind != crate::theme::Kind::Terminal || crate::theme::terminal_gate() {
                            app.theme = crate::theme::Theme::of(kind);
                        }
                    }
                }
            };
            match key.code {
                KeyCode::Up | KeyCode::Char('k') => {
                    sel = sel.saturating_sub(1);
                    try_it(app, sel);
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    sel = (sel + 1).min(n - 1);
                    try_it(app, sel);
                }
                KeyCode::Enter => {
                    // back to what was there, then the one commit that does the work
                    if d.key == "theme" {
                        if let Some(kind) = crate::theme::Kind::parse(&orig) {
                            app.theme = crate::theme::Theme::of(kind);
                        }
                    }
                    set_value(app, d, Val::S(choices[sel].canon.to_string()));
                    s.pane = Pane::Browse;
                    app.modal = Some(Modal::Settings(s));
                    return;
                }
                KeyCode::Esc => {
                    if d.key == "theme" {
                        if let Some(kind) = crate::theme::Kind::parse(&orig) {
                            app.theme = crate::theme::Theme::of(kind);
                        }
                    }
                    s.pane = Pane::Browse;
                    app.modal = Some(Modal::Settings(s));
                    return;
                }
                KeyCode::Char('d') if !ctrl => {
                    s.confirm_reset = Some(d.key);
                }
                _ => {}
            }
            if matches!(s.pane, Pane::Choose { .. }) {
                s.pane = Pane::Choose { key: k, sel, orig };
            }
        }
        Pane::Step { key: k, val } => {
            let Some(d) = find(k) else { return };
            let Kind::Int { min, max, .. } = &d.kind else {
                return;
            };
            let (a, bst) = step_sizes(*min, *max);
            let mut val = val;
            match key.code {
                KeyCode::Up => val = (val + a).min(*max),
                KeyCode::Down => val = (val - a).max(*min),
                KeyCode::Right => val = (val + bst).min(*max),
                KeyCode::Left => val = (val - bst).max(*min),
                KeyCode::Enter => {
                    set_value(app, d, Val::I(val));
                    s.pane = Pane::Browse;
                    app.modal = Some(Modal::Settings(s));
                    return;
                }
                KeyCode::Esc => {
                    s.pane = Pane::Browse;
                    app.modal = Some(Modal::Settings(s));
                    return;
                }
                KeyCode::Char('d') if !ctrl => s.confirm_reset = Some(d.key),
                _ => {}
            }
            if matches!(s.pane, Pane::Step { .. }) {
                s.pane = Pane::Step { key: k, val };
            }
        }
        Pane::Sheet { sel } => {
            let n = HINT_CHILDREN.len();
            let mut sel = sel;
            match key.code {
                KeyCode::Esc => {
                    s.pane = Pane::Browse;
                    app.modal = Some(Modal::Settings(s));
                    return;
                }
                KeyCode::Up | KeyCode::Char('k') => sel = sel.saturating_sub(1),
                KeyCode::Down | KeyCode::Char('j') => sel = (sel + 1).min(n - 1),
                KeyCode::Char(' ') | KeyCode::Enter => {
                    let d = &HINT_CHILDREN[sel];
                    let on = matches!(value(app, d), Val::B(true));
                    set_value(app, d, Val::B(!on));
                }
                _ => {}
            }
            s.pane = Pane::Sheet { sel };
        }
        Pane::Browse => {
            let vis = visible(&s);
            let n = vis.len();
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
                    KeyCode::Up => s.sel = s.sel.saturating_sub(1),
                    KeyCode::Down => s.sel = (s.sel + 1).min(visible(&s).len().saturating_sub(1)),
                    KeyCode::Char(c) if !ctrl => s.query.push(c),
                    _ => {}
                }
                if !matches!(key.code, KeyCode::Up | KeyCode::Down | KeyCode::Enter) {
                    s.sel = 0;
                    s.top = 0;
                }
            } else {
                let sel_def = vis.get(s.sel).copied();
                match key.code {
                    KeyCode::Esc => {
                        if !s.query.is_empty() {
                            s.query.clear();
                            s.sel = 0;
                        } else {
                            close = true;
                        }
                    }
                    KeyCode::Down | KeyCode::Char('j') => {
                        s.sel = (s.sel + 1).min(n.saturating_sub(1))
                    }
                    KeyCode::Up | KeyCode::Char('k') => s.sel = s.sel.saturating_sub(1),
                    KeyCode::Char('g') => s.sel = 0,
                    KeyCode::Char('G') => s.sel = n.saturating_sub(1),
                    KeyCode::Char('/') => s.searching = true,
                    KeyCode::Right | KeyCode::Char('l') => {
                        if let Some(d) = sel_def {
                            s.open.insert(d.key);
                        }
                    }
                    KeyCode::Left | KeyCode::Char('h') => {
                        if let Some(d) = sel_def {
                            s.open.remove(d.key);
                        }
                    }
                    KeyCode::Char('d') => {
                        if let Some(d) = sel_def {
                            if !matches!(d.kind, Kind::Group(_)) {
                                s.confirm_reset = Some(d.key);
                            }
                        }
                    }
                    KeyCode::Char(' ') | KeyCode::Enter => {
                        if let Some(d) = sel_def {
                            match &d.kind {
                                Kind::Bool(_) => {
                                    let on = matches!(value(app, d), Val::B(true));
                                    set_value(app, d, Val::B(!on));
                                }
                                Kind::Enum { choices, .. }
                                    if key.code == KeyCode::Enter
                                        || key.code == KeyCode::Char(' ') =>
                                {
                                    let cur = match value(app, d) {
                                        Val::S(c) => c,
                                        _ => String::new(),
                                    };
                                    let at =
                                        choices.iter().position(|c| c.canon == cur).unwrap_or(0);
                                    s.pane = Pane::Choose {
                                        key: d.key,
                                        sel: at,
                                        orig: cur,
                                    };
                                }
                                Kind::Int { .. } => {
                                    let v = match value(app, d) {
                                        Val::I(i) => i,
                                        _ => 0,
                                    };
                                    s.pane = Pane::Step { key: d.key, val: v };
                                }
                                Kind::Group(_) => s.pane = Pane::Sheet { sel: 0 },
                                _ => {}
                            }
                        }
                    }
                    _ => {}
                }
            }
            // keep the selected row inside the list
            let vis = visible(&s);
            s.sel = s.sel.min(vis.len().saturating_sub(1));
            let lay = Layout::compute(app);
            let r = win(&lay);
            let width = r.width.saturating_sub(6) as usize - 4;
            let its = items(&s, &vis, width);
            if let Some(at) = its
                .iter()
                .position(|i| matches!(i, Item::Row(v) if *v == s.sel))
            {
                // the header above the first row of a group counts as part of it
                let want_top = if at > 0 && matches!(its[at - 1], Item::Header(_)) {
                    at - 1
                } else {
                    at
                };
                if want_top < s.top {
                    s.top = want_top;
                } else if at >= s.top + h {
                    s.top = at + 1 - h;
                }
                s.top = s.top.min(its.len().saturating_sub(h));
            }
        }
    }
    if close {
        app.modal = None;
    } else {
        app.modal = Some(Modal::Settings(s));
    }
}
