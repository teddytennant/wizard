//! `/context`, `/usage` and `/session-info`: one window with three tabs (spec 6.9). What the
//! window shows is what wizard reports: tokens in context, the window size, session token totals
//! and cost. The category breakdown of the context, the account allowance and the API timing are
//! not in the ACP stream, so those places say so in the same words and colors Grok uses for an
//! empty state.

use agent_core::transcript::{Part, Role};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Style;
use tuikit::paint::put_str;

use super::widgets::{self, Hint};
use super::{modal_frame, Modal};
use crate::app::App;
use crate::ui::{bold, put, st, Layout};

pub const TABS: [&str; 3] = ["Context usage", "Usage limit", "Session info"];

#[derive(Clone, Debug, Default)]
pub struct UsageModal {
    pub tab: usize,
    pub scroll: usize,
}

pub fn open(app: &mut App, tab: usize) {
    app.enter_session();
    app.modal = Some(Modal::Usage(UsageModal { tab, scroll: 0 }));
    app.dirty = true;
}

/// The window: 65% wide up to 100, as tall as the screen less two rows a side, at most 30,
/// centered.
pub fn rect(w: u16, h: u16) -> Rect {
    let cap = w.saturating_sub(4).min(100);
    let mw = (((w as f32) * 0.65) as u16).min(cap).max(44).min(w);
    let mh = h.saturating_sub(4).clamp(1, 30);
    Rect::new((w - mw) / 2, (h - mh) / 2, mw, mh)
}

fn hints(tab: usize) -> Vec<Hint<'static>> {
    let mut v = vec![
        ("Tab", "switch"),
        ("↑/↓", "scroll"),
        ("c", "copy session ID"),
    ];
    if tab == 2 {
        v.push(("y", "copy all"));
    }
    v.push(("Esc", "close"));
    v
}

/// `1.5k`, `28.1k`, `256k`: tokens as the window prints them.
pub fn fmt_tokens(n: u64) -> String {
    if n < 1000 {
        n.to_string()
    } else if n < 99_500 {
        format!("{:.1}k", n as f64 / 1000.0)
    } else if n < 1_000_000 {
        format!("{}k", (n as f64 / 1000.0).round() as u64)
    } else {
        format!("{:.1}m", n as f64 / 1e6)
    }
}

fn fmt_pct(p: f64) -> String {
    if p < 10.0 {
        format!("{p:.1}%")
    } else {
        format!("{p:.0}%")
    }
}

fn thousands(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

type Line = Vec<(String, Style)>;

fn line(s: &str, style: Style) -> Line {
    vec![(s.to_string(), style)]
}

/// The model id without its provider: `xai/grok-4.7` is `grok-4.7`.
fn model_id(app: &App) -> String {
    let m = &app.config.model;
    m.rsplit('/').next().unwrap_or(m).to_string()
}

fn cwd_full(app: &App) -> String {
    if app.config.cwd.is_empty() {
        app.opts.cwd.display().to_string()
    } else {
        app.config.cwd.clone()
    }
}

/// The model's display name without the effort: `Grok 4.7`.
fn model_name(app: &App) -> String {
    app.config
        .models
        .iter()
        .find(|m| m.id == app.config.model)
        .map(|m| m.name.clone())
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| model_id(app))
}

fn turns_and_calls(app: &App) -> (usize, usize) {
    let turns = app
        .tr
        .messages
        .iter()
        .filter(|m| m.role == Role::User)
        .count();
    let calls = app
        .tr
        .messages
        .iter()
        .flat_map(|m| m.parts.iter())
        .filter(|p| matches!(p, Part::Tool(_)))
        .count();
    (turns, calls)
}

fn context_lines(app: &App, width: u16) -> Vec<Line> {
    let th = &app.theme;
    let mut v: Vec<Line> = vec![line("Context", bold(th.text_primary)), vec![]];
    let used = app.tr.usage.context_tokens;
    let Some((_, total)) = app.context() else {
        v.push(line(
            "The context window size is not reported by wizard for this model.",
            st(th.gray),
        ));
        return v;
    };
    let pct = used as f64 / total.max(1) as f64 * 100.0;
    v.push(line(
        &format!(
            "{} / {} tokens ({pct:.2}%)",
            fmt_tokens(used),
            fmt_tokens(total)
        ),
        st(th.text_secondary),
    ));
    v.push(line(&model_id(app), st(th.gray_bright)));
    v.push(vec![]);
    // 100 cells: 20 by 5 when there is room, else 10 by 10
    let cols = if width >= 50 { 20 } else { 10 };
    let cells_used = ((used as f64 / total.max(1) as f64 * 100.0).round() as usize).min(100);
    for row in 0..100 / cols {
        let mut l: Line = Vec::new();
        for c in 0..cols {
            let i = row * cols + c;
            if i < cells_used {
                l.push(("◆ ".into(), st(th.gray_bright)));
            } else {
                l.push(("◇ ".into(), st(th.gray_dim)));
            }
        }
        v.push(l);
    }
    v.push(vec![]);
    let free = total.saturating_sub(used);
    let w = fmt_tokens(used).len().max(fmt_tokens(free).len());
    let free_pct = 100.0 - pct.min(100.0);
    for (glyph, gc, label, n, p) in [
        ("◆", th.gray_bright, "In use", used, pct),
        ("◇", th.gray_dim, "Free", free, free_pct),
    ] {
        v.push(vec![
            (format!("{glyph} "), st(gc)),
            (format!("{label:<10}"), st(th.text_secondary)),
            (format!("{:>w$} tokens", fmt_tokens(n)), st(th.gray)),
            (
                format!("   {:>6}", format!("({})", fmt_pct(p))),
                st(th.gray),
            ),
        ]);
    }
    v.push(vec![]);
    v.push(line(
        "Breakdown by category is not reported by wizard.",
        st(th.gray_dim),
    ));
    v.push(vec![]);
    let (turns, calls) = turns_and_calls(app);
    v.push(line(
        &format!("Turns: {turns} · Tool calls: {calls}"),
        st(th.gray),
    ));
    v
}

fn usage_lines(app: &App) -> Vec<Line> {
    let th = &app.theme;
    let u = &app.tr.usage;
    let mut v: Vec<Line> = vec![line("Usage limit", bold(th.text_primary)), vec![]];
    v.push(line("No billing data available.", st(th.gray)));
    v.push(line(
        "Wizard does not report account limits over ACP.",
        st(th.gray_dim),
    ));
    v.push(vec![]);
    let (turns, _) = turns_and_calls(app);
    if turns == 0 {
        v.push(line(
            "Session usage: no model calls yet in this session.",
            bold(th.text_primary),
        ));
        return v;
    }
    v.push(line(
        "Session usage (since start or last resume):",
        bold(th.text_primary),
    ));
    let cached = if u.cached_tokens > 0 {
        format!(" ({} cached)", thousands(u.cached_tokens))
    } else {
        String::new()
    };
    let row = |l: &str, val: String| -> Line { vec![(format!("  {l:<16}{val}"), st(th.gray))] };
    v.push(row(
        "Input tokens:",
        format!("{}{cached}", thousands(u.input_tokens)),
    ));
    v.push(row("Output tokens:", thousands(u.output_tokens)));
    v.push(row(
        "Total tokens:",
        thousands(u.input_tokens + u.output_tokens),
    ));
    if let Some(c) = u.cost_usd {
        v.push(row("Cost:", format!("${c:.4}")));
    }
    v
}

/// Label and value pairs of the Session info tab, for drawing and for `y`.
type Fields = Vec<(&'static str, String)>;

fn info_fields(app: &App) -> (Fields, Fields) {
    let mut long = Vec::new();
    if let Some(t) = app.titles.get(&app.tr.session_id) {
        long.push(("Title:", t.clone()));
    }
    long.push((
        "Shell version:",
        format!("{} [alpha]", app.config.version()),
    ));
    long.push(("Session ID:", app.tr.session_id.clone()));
    long.push(("Working directory:", cwd_full(app)));
    let (turns, _) = turns_and_calls(app);
    let mut short = vec![("Model:", model_name(app))];
    if !app.config.mode.is_empty() {
        short.push(("Mode:", app.config.mode.clone()));
    }
    short.push(("API Backend:", app.config.backend.clone()));
    short.push(("Turn:", turns.to_string()));
    if let Some((used, total)) = app.context() {
        let pct = (used as f64 / total.max(1) as f64 * 100.0).round() as u64;
        short.push(("Context:", format!("{used} / {total} tokens ({pct}%)")));
    }
    (long, short)
}

fn info_lines(app: &App) -> Vec<Line> {
    let th = &app.theme;
    let mut v: Vec<Line> = vec![vec![
        ("Session info".into(), bold(th.text_primary)),
        ("   click or drag to copy".into(), st(th.gray_dim)),
    ]];
    v.push(vec![]);
    let (long, short) = info_fields(app);
    for (l, val) in long {
        v.push(line(l, st(th.gray)));
        v.push(line(&val, st(th.text_primary)));
        v.push(vec![]);
    }
    for (l, val) in short {
        v.push(vec![
            (format!("{l} "), st(th.gray)),
            (val, st(th.text_primary)),
        ]);
    }
    v
}

fn lines_for(app: &App, tab: usize, width: u16) -> Vec<Line> {
    match tab {
        0 => context_lines(app, width),
        1 => usage_lines(app),
        _ => info_lines(app),
    }
}

pub fn draw(buf: &mut Buffer, app: &App, lay: &Layout, m: &UsageModal) {
    let th = &app.theme;
    let r = rect(lay.w, lay.h);
    let inner = modal_frame(buf, app, r, "");
    if inner.height < 6 {
        return;
    }
    // tab bar from three cells in, two spaces between labels
    let mut x = r.x + 3;
    for (i, t) in TABS.iter().enumerate() {
        let style = if i == m.tab {
            bold(th.text_secondary)
        } else {
            st(th.gray)
        };
        x = put(buf, x, r.y + 1, t, style);
        x += 2;
    }
    widgets::draw_divider(buf, app, r, r.y + 2, 0);
    let hs = hints(m.tab);
    let f = widgets::foot_rows(r, &hs);
    // the footer is a block of three rows here, hints at the bottom
    let block_top = (r.bottom() - 1).saturating_sub(f.max(3));
    widgets::draw_hints(buf, app, r, &hs);
    let top = r.y + 3;
    let h = block_top.saturating_sub(top) as usize;
    let cw = r.width.saturating_sub(6);
    let lines = lines_for(app, m.tab, cw);
    let scroll = m.scroll.min(lines.len().saturating_sub(h));
    let clip = Rect::new(r.x + 3, top, cw, h as u16);
    for (i, l) in lines.iter().skip(scroll).take(h).enumerate() {
        let mut x = r.x + 3;
        for (t, s) in l {
            x = put_str(buf, x, top + i as u16, t, *s, clip);
        }
    }
}

pub fn key(app: &mut App, key: KeyEvent) {
    let lay = Layout::compute(app);
    let r = rect(lay.w, lay.h);
    let Some(Modal::Usage(m)) = app.modal.clone() else {
        return;
    };
    let mut m = m;
    let hs = hints(m.tab);
    let f = widgets::foot_rows(r, &hs);
    let h = ((r.bottom() - 1).saturating_sub(f.max(3))).saturating_sub(r.y + 3) as usize;
    let total = lines_for(app, m.tab, r.width.saturating_sub(6)).len();
    let max_scroll = total.saturating_sub(h);
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let tab = m.tab;
    let switch = |m: &mut UsageModal, to: usize| {
        m.tab = to;
        m.scroll = 0;
    };
    match key.code {
        KeyCode::Esc => {
            app.modal = None;
            return;
        }
        KeyCode::Tab | KeyCode::Right | KeyCode::Char('l') => switch(&mut m, (tab + 1) % 3),
        KeyCode::BackTab | KeyCode::Left | KeyCode::Char('h') => switch(&mut m, (tab + 2) % 3),
        KeyCode::Char('1') => switch(&mut m, 0),
        KeyCode::Char('2') => switch(&mut m, 1),
        KeyCode::Char('3') => switch(&mut m, 2),
        KeyCode::Down | KeyCode::Char('j') => m.scroll = (m.scroll + 1).min(max_scroll),
        KeyCode::Up | KeyCode::Char('k') => m.scroll = m.scroll.saturating_sub(1),
        KeyCode::PageDown => m.scroll = (m.scroll + 10).min(max_scroll),
        KeyCode::PageUp => m.scroll = m.scroll.saturating_sub(10),
        KeyCode::Char('c') if !ctrl => {
            crate::app::copy_to_clipboard(&app.tr.session_id);
            app.toast_for("Copied!", 30);
        }
        KeyCode::Char('y') if !ctrl && m.tab == 2 => {
            let (long, short) = info_fields(app);
            let all: Vec<String> = long
                .into_iter()
                .chain(short)
                .map(|(l, v)| format!("{l} {v}"))
                .collect();
            crate::app::copy_to_clipboard(&all.join("\n"));
            app.toast_for("Copied!", 30);
        }
        _ => {}
    }
    app.modal = Some(Modal::Usage(m));
}
