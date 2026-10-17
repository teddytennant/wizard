// OWNER: visual
//! The right rail: session, context, cost, changes, todos and running agents, on `surface`.

use agent_core::{Todo, TodoStatus};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Style;
use tuikit::paint::{fill, put_str};
use tuikit::width::{display_width, truncate, truncate_left, wrap};

use super::status::{ctx_percent, gauge, gauge_style};
use crate::palette::{Depth, Glyphs, Palette};
use crate::ui::row::fmt_tokens;

pub const WIDTH: u16 = 36;

#[derive(Clone, Debug, Default)]
pub struct RailInfo {
    pub title: String,
    pub cwd: String,
    pub ctx_tokens: u64,
    pub ctx_window: u64,
    pub cost: Option<f64>,
    pub turns: usize,
    /// `(path, added, removed, new file)`
    pub changes: Vec<(String, usize, usize, bool)>,
    pub todos: Vec<Todo>,
    pub agents: usize,
}

fn rel(path: &str, cwd: &str) -> String {
    match path.strip_prefix(cwd) {
        Some(r) if !cwd.is_empty() => r.trim_start_matches('/').to_string(),
        _ => path.to_string(),
    }
}

pub fn draw(buf: &mut Buffer, rect: Rect, info: &RailInfo, p: &Palette, g: &Glyphs) {
    if rect.is_empty() {
        return;
    }
    fill(buf, rect, Style::new().bg(p.surface));
    if matches!(p.depth, Depth::Ansi16 | Depth::Mono) {
        for y in rect.top()..rect.bottom() {
            put_str(buf, rect.x, y, g.vline, p.s_faint(), rect);
        }
    }
    let x = rect.x + 2;
    let w = rect.width.saturating_sub(4) as usize;
    let inner = Rect::new(x, rect.y, w as u16, rect.height);
    let bg = Style::new().bg(p.surface);
    let mut y = rect.y + 1;
    let put = |buf: &mut Buffer, y: &mut u16, spans: &[(String, Style)]| {
        if *y >= rect.bottom() {
            return;
        }
        let mut cx = x;
        for (t, st) in spans {
            cx = put_str(buf, cx, *y, t, bg.patch(*st), inner);
        }
        *y += 1;
    };
    let head = |t: String| vec![(t, p.s_dim())];

    put(buf, &mut y, &head("Session".into()));
    let title = if info.title.is_empty() {
        "new session".to_string()
    } else {
        info.title.clone()
    };
    for l in wrap(&title, w).into_iter().take(2) {
        put(buf, &mut y, &[(l, p.s_text())]);
    }
    y += 1;

    put(buf, &mut y, &head("Context".into()));
    match ctx_percent(info.ctx_tokens, info.ctx_window) {
        Some(pct) => {
            let (on, off) = gauge(pct, g);
            put(
                buf,
                &mut y,
                &[
                    (on, gauge_style(pct, p)),
                    (off, p.s_faint()),
                    (format!(" {pct}%"), p.s_text()),
                    (
                        format!(
                            "  {} / {}",
                            fmt_tokens(info.ctx_tokens),
                            fmt_tokens(info.ctx_window)
                        ),
                        p.s_dim(),
                    ),
                ],
            );
        }
        None => put(
            buf,
            &mut y,
            &[(format!("{} tokens", fmt_tokens(info.ctx_tokens)), p.s_dim())],
        ),
    }
    y += 1;

    put(buf, &mut y, &head("Cost".into()));
    let cost = info.cost.map_or("n/a".to_string(), |c| format!("${c:.2}"));
    let turns = if info.turns == 1 {
        "1 turn".to_string()
    } else {
        format!("{} turns", info.turns)
    };
    put(
        buf,
        &mut y,
        &[
            (cost, p.s_text()),
            (format!(" {} {turns}", g.sep), p.s_dim()),
        ],
    );
    y += 1;

    if !info.changes.is_empty() {
        put(
            buf,
            &mut y,
            &head(format!("Changes {}", info.changes.len())),
        );
        for (path, add, del, new) in info.changes.iter().take(8) {
            let letter = if *new { "A" } else { "M" };
            let counts = if *del == 0 {
                format!("+{add}")
            } else {
                format!("+{add} -{del}")
            };
            let room = w.saturating_sub(2 + display_width(&counts) + 1);
            let name = truncate_left(&rel(path, &info.cwd), room);
            let pad = w.saturating_sub(2 + display_width(&name) + display_width(&counts));
            put(
                buf,
                &mut y,
                &[
                    (
                        format!("{letter} "),
                        if *new { p.s_ok() } else { p.s_dim() },
                    ),
                    (name, p.s_text()),
                    (" ".repeat(pad), Style::new()),
                    (format!("+{add}"), Style::new().fg(p.add_fg)),
                    (
                        if *del == 0 {
                            String::new()
                        } else {
                            format!(" -{del}")
                        },
                        Style::new().fg(p.del_fg),
                    ),
                ],
            );
        }
        if info.changes.len() > 8 {
            put(
                buf,
                &mut y,
                &[(format!("+{} more", info.changes.len() - 8), p.s_faint())],
            );
        }
        y += 1;
    }

    if !info.todos.is_empty() {
        let done = info
            .todos
            .iter()
            .filter(|t| t.status == TodoStatus::Completed)
            .count();
        put(
            buf,
            &mut y,
            &head(format!("Todos {done}/{}", info.todos.len())),
        );
        for t in info
            .todos
            .iter()
            .filter(|t| t.status != TodoStatus::Completed)
            .take(6)
        {
            let (glyph, gs, ts) = match t.status {
                TodoStatus::InProgress => (g.active, p.s_accent(), p.s_text()),
                _ => (g.pending, p.s_dim(), p.s_dim()),
            };
            put(
                buf,
                &mut y,
                &[
                    (format!("{glyph} "), gs),
                    (truncate(&t.text, w.saturating_sub(2)), ts),
                ],
            );
        }
        y += 1;
    }

    if info.agents > 0 {
        put(
            buf,
            &mut y,
            &head(format!("Agents {} running", info.agents)),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::palette::{Kind, UNICODE};
    use tuikit::testing::TestTerminal;

    #[test]
    fn rail_lists_sections_and_stays_inside_its_rect() {
        let p = Palette::new(Kind::Hearth, Depth::True);
        let info = RailInfo {
            title: "add --width flag and fix wrap test".into(),
            cwd: "/w".into(),
            ctx_tokens: 36_000,
            ctx_window: 200_000,
            cost: Some(0.31),
            turns: 12,
            changes: vec![
                ("/w/src/render.rs".into(), 3, 1, false),
                ("/w/src/cli.rs".into(), 9, 0, true),
            ],
            todos: vec![Todo {
                text: "Run the full suite".into(),
                status: TodoStatus::InProgress,
            }],
            agents: 0,
        };
        let mut t = TestTerminal::new(60, 24);
        t.draw(|b, _| draw(b, Rect::new(24, 0, 36, 24), &info, &p, &UNICODE));
        let s = t.plain();
        for want in [
            "Session",
            "add --width flag and fix",
            "Context",
            "━━────── 18%  36k / 200k",
            "$0.31 · 12 turns",
            "Changes 2",
            "M src/render.rs",
            "A src/cli.rs",
            "Todos 0/1",
            "◐ Run the full suite",
        ] {
            assert!(s.contains(want), "missing {want:?} in\n{s}");
        }
        for y in 0..24 {
            assert!(
                t.row(y).chars().take(24).all(|c| c == ' '),
                "row {y} spilled left of the rail"
            );
        }
    }
}
