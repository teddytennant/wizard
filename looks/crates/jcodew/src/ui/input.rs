//! Everything under the transcript: queued prompts, the status line, the notice line, the
//! numbered composer, the session line, and the lists that open around the composer.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Span;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use super::line::{spans_width, truncate_spans};
use super::{clear, put, put_right};
use crate::app::App;
use crate::commands::{self, Suggestion};
use crate::theme;

pub const SUGGESTION_LIMIT: usize = 8;
/// The mark at the right of the composer: a prompt sent during a turn waits for it to end.
const SEND_ICON: &str = "⏳";

const TIPS: &[&str] = &[
    "Ctrl+J / Ctrl+K to jump between user prompts",
    "Shift+Enter or Alt+Enter inserts a newline",
    "Ctrl+S stashes the input and brings it back later",
    "Ctrl+G sets a scroll bookmark; press again to return",
    "Type / to see every command",
    "Up on an empty input walks your prompt history",
    "Alt+X hides or shows the pinned todo list",
];

pub fn format_elapsed(secs: u64) -> String {
    if secs >= 3600 {
        format!("{}h {}m", secs / 3600, (secs % 3600) / 60)
    } else if secs >= 60 {
        format!("{}m {}s", secs / 60, secs % 60)
    } else {
        format!("{secs}s")
    }
}

/// The composer's rows: the text hard-wrapped by cell, and where the cursor is in them.
pub struct InputLayout {
    pub rows: Vec<String>,
    pub cursor: (usize, usize),
}

pub fn prompt_number(app: &App) -> usize {
    app.user_count() + app.queue.len() + 1
}

fn prompt_len(app: &App) -> usize {
    prompt_number(app).to_string().len() + 2
}

pub fn input_layout(app: &App, width: u16) -> InputLayout {
    let reserved = SEND_ICON.len() + 1;
    let line_width = (width as usize)
        .saturating_sub(prompt_len(app) + reserved)
        .max(1);
    let text = app.ed.text();
    let cursor = app.ed.cursor().min(text.len());
    let mut rows = vec![String::new()];
    let mut w = 0usize;
    let mut cur = (0usize, 0usize);
    for (i, c) in text.char_indices() {
        if i == cursor {
            cur = (rows.len() - 1, w);
        }
        if c == '\n' {
            rows.push(String::new());
            w = 0;
            continue;
        }
        let cw = c.width().unwrap_or(0);
        if w + cw > line_width {
            rows.push(String::new());
            w = 0;
            if i == cursor {
                cur = (rows.len() - 1, 0);
            }
        }
        if let Some(r) = rows.last_mut() {
            r.push(c);
        }
        w += cw;
    }
    if cursor >= text.len() {
        cur = (rows.len() - 1, w);
    }
    InputLayout { rows, cursor: cur }
}

/// The hint row above the composer while a turn runs and something is typed.
pub fn hint(app: &App) -> Option<(&'static str, Color)> {
    if suggestions_active(app).is_some() {
        return None;
    }
    if app.busy() && !app.ed.is_empty() {
        return Some(("  Ctrl/Cmd+Enter to send now", theme::dim()));
    }
    None
}

pub fn input_height(app: &App, width: u16) -> u16 {
    let rows = input_layout(app, width).rows.len().min(10) as u16;
    rows + u16::from(hint(app).is_some())
}

pub fn draw_queued(buf: &mut Buffer, area: Rect, app: &App) {
    let start = app.user_count() + 1;
    let pending = app.queue.len();
    for (i, msg) in app.queue.iter().take(area.height as usize).enumerate() {
        let spans = vec![
            Span::styled(
                format!("{}", start + i),
                Style::default().fg(theme::rainbow_prompt(pending - i)),
            ),
            Span::raw(" "),
            Span::styled("⏳", Style::default().fg(theme::queued())),
            Span::raw(" "),
            Span::styled(
                msg.replace('\n', " "),
                Style::default()
                    .fg(theme::queued())
                    .add_modifier(Modifier::DIM),
            ),
        ];
        put(
            buf,
            area.x,
            area.y + i as u16,
            &truncate_spans(&spans, area.width as usize),
            area.width,
        );
    }
}

pub fn draw_status(buf: &mut Buffer, area: Rect, app: &App) {
    let dim = Style::default().fg(theme::dim());
    let mut spans: Vec<Span<'static>> = Vec::new();
    if app.busy() {
        let secs = app.clock.elapsed().as_secs_f32();
        let elapsed = app
            .turn
            .started
            .map_or(0, |s| app.now().saturating_sub(s).as_secs());
        spans.push(Span::styled(
            theme::spinner(secs),
            Style::default().fg(theme::ai()),
        ));
        if let Some(call) = app.running_tool() {
            let (name, detail) = super::tools::running_detail(call);
            spans.push(Span::styled(format!(" running {name}"), dim));
            if !detail.is_empty() {
                spans.push(Span::styled(
                    format!(" · {detail}"),
                    Style::default()
                        .fg(theme::ai())
                        .add_modifier(Modifier::BOLD),
                ));
            }
            spans.push(Span::styled(format!(" · {}", format_elapsed(elapsed)), dim));
        } else if app.streaming() {
            spans.push(Span::styled(format!(" {}", format_elapsed(elapsed)), dim));
        } else if app.turn.open {
            spans.push(Span::styled(
                format!(" thinking… {}", format_elapsed(elapsed)),
                dim,
            ));
        } else {
            spans.push(Span::styled(
                format!(" sending… {}", format_elapsed(elapsed)),
                dim,
            ));
        }
        if !app.queue.is_empty() {
            spans.push(Span::styled(
                format!(" · +{} queued", app.queue.len()),
                Style::default().fg(theme::queued()),
            ));
        }
    } else if !app.clock.is_frozen() && area.width >= 16 {
        let secs = app.clock.elapsed().as_secs();
        if (28..40).contains(&(secs % 90)) {
            let tip = TIPS[(secs / 15) as usize % TIPS.len()];
            let text = format!("💡 {tip}");
            let text = if text.width() > area.width as usize {
                let mut cut = String::new();
                for c in text.chars() {
                    if cut.width() + c.width().unwrap_or(0) + 3 > area.width as usize {
                        break;
                    }
                    cut.push(c);
                }
                format!("{cut}...")
            } else {
                text
            };
            spans.push(Span::styled(text, dim));
        }
    }
    put(buf, area.x, area.y, &spans, area.width);
}

pub fn draw_notification(buf: &mut Buffer, area: Rect, app: &App) {
    let mut spans = Vec::new();
    if let Some((text, _)) = &app.notice {
        spans.push(Span::styled(
            text.clone(),
            Style::default().fg(theme::accent()),
        ));
    }
    if let Some((text, _)) = &app.hotkey_note {
        if !spans.is_empty() {
            spans.push(Span::styled(" · ", Style::default().fg(theme::dim())));
        }
        spans.push(Span::styled(
            text.clone(),
            Style::default().fg(theme::rgb(102, 204, 221)),
        ));
    }
    put(
        buf,
        area.x,
        area.y,
        &truncate_spans(&spans, area.width as usize),
        area.width,
    );
}

/// Draws the composer and returns where the terminal cursor goes.
pub fn draw_input(buf: &mut Buffer, area: Rect, app: &App) -> Option<(u16, u16)> {
    let mut y = area.y;
    let mut rows_left = area.height;
    if let Some((text, color)) = hint(app) {
        put(
            buf,
            area.x,
            y,
            &[Span::styled(text, Style::default().fg(color))],
            area.width,
        );
        y += 1;
        rows_left = rows_left.saturating_sub(1);
    }
    let lay = input_layout(app, area.width);
    let visible = rows_left.max(1) as usize;
    let scroll = if lay.rows.len() > visible {
        lay.cursor.0.saturating_sub(visible - 1)
    } else {
        0
    };
    let num = prompt_number(app).to_string();
    let plen = num.len() + 2;
    let shell = app.ed.text().trim_start().starts_with('!');
    let (glyph, color) = if shell {
        ("$ ", theme::shell())
    } else if app.busy() {
        ("… ", theme::queued())
    } else {
        ("> ", theme::user())
    };
    for (i, row) in lay.rows.iter().enumerate().skip(scroll).take(visible) {
        let ry = y + (i - scroll) as u16;
        let mut spans = if i == 0 {
            vec![
                Span::styled(num.clone(), Style::default().fg(theme::rainbow_prompt(0))),
                Span::styled(glyph, Style::default().fg(color)),
            ]
        } else {
            vec![Span::raw(" ".repeat(plen))]
        };
        spans.push(Span::raw(row.clone()));
        put(buf, area.x, ry, &spans, area.width);
    }
    let last_row = y + (lay.rows.len().min(visible) as u16).saturating_sub(1);
    if app.ready && area.width > 2 {
        let (icon, icon_color) = if shell {
            ("$", theme::shell())
        } else {
            (SEND_ICON, theme::queued())
        };
        put(
            buf,
            area.x + area.width.saturating_sub(icon.width() as u16),
            last_row,
            &[Span::styled(icon, Style::default().fg(icon_color))],
            icon.width() as u16,
        );
    }
    let cx = area.x + (plen + lay.cursor.1) as u16;
    let cy = y + (lay.cursor.0.saturating_sub(scroll)) as u16;
    (cx < area.x + area.width).then_some((cx, cy))
}

/// `~`, `~/x`, `~/…/last`, or `…/a/b`.
pub fn dir_label_short(path: &str) -> String {
    let rel = super::header::home_relative(path);
    let parts: Vec<&str> = rel.split('/').filter(|p| !p.is_empty()).collect();
    if rel.starts_with('~') {
        match parts.len() {
            0 | 1 => "~".to_string(),
            2 => format!("~/{}", parts[1]),
            n => format!("~/…/{}", parts[n - 1]),
        }
    } else if parts.len() <= 2 {
        rel
    } else {
        format!("…/{}/{}", parts[parts.len() - 2], parts[parts.len() - 1])
    }
}

fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        format!("{}…", s.chars().take(max).collect::<String>())
    }
}

/// The right-aligned line under the composer: directory, branch, context, provider, model.
pub fn session_line(app: &App) -> Vec<Span<'static>> {
    let muted = Style::default().fg(theme::muted());
    let mut groups: Vec<Vec<Span<'static>>> = Vec::new();
    groups.push(vec![
        Span::styled(" ", Style::default().fg(theme::info())),
        Span::styled(dir_label_short(&app.cwd()), muted),
    ]);
    if let Some(b) = &app.git.branch {
        groups.push(vec![Span::styled(
            format!(" {}", truncate_chars(b, 24)),
            Style::default().fg(theme::rgb(150, 170, 140)),
        )]);
        let g = &app.git;
        let counts = [
            (g.modified, "~", theme::rgb(240, 200, 80)),
            (g.staged, "+", theme::success()),
            (g.untracked, "?", theme::muted()),
            (g.ahead, "↑", theme::success()),
            (g.behind, "↓", theme::rgb(255, 140, 100)),
        ];
        for (n, sign, color) in counts {
            if n > 0 {
                groups.push(vec![Span::styled(
                    format!("{sign}{n}"),
                    Style::default().fg(color),
                )]);
            }
        }
    }
    if let Some((used, limit)) = app.context() {
        let pct = if limit == 0 {
            0
        } else {
            ((used as f64 / limit as f64) * 100.0).round() as u64
        };
        let left = 100u64.saturating_sub(pct);
        let fill = if left <= 20 {
            theme::error()
        } else if left <= 50 {
            theme::warning()
        } else {
            theme::success()
        };
        let filled = ((pct as usize) / 10).min(10);
        let fmt = |n: u64| {
            if n >= 1_000_000 {
                format!("{:.1}M", n as f64 / 1_000_000.0)
            } else if n >= 100_000 {
                format!("{:.0}k", n as f64 / 1_000.0)
            } else if n >= 1_000 {
                let k = n as f64 / 1_000.0;
                if n < 10_000 && (k * 10.0).round() % 10.0 != 0.0 {
                    format!("{k:.1}k")
                } else {
                    format!("{k:.0}k")
                }
            } else {
                n.to_string()
            }
        };
        let mut g = vec![Span::styled(
            format!("{}/{} ", fmt(used), fmt(limit)),
            muted,
        )];
        if filled > 0 {
            g.push(Span::styled("▰".repeat(filled), Style::default().fg(fill)));
        }
        g.push(Span::styled(
            "▱".repeat(10 - filled),
            Style::default().fg(theme::rgb(50, 50, 60)),
        ));
        g.push(Span::styled(
            format!(" {pct}%"),
            Style::default().fg(fill).add_modifier(Modifier::BOLD),
        ));
        groups.push(g);
    }
    let provider = app.provider();
    if !provider.is_empty() {
        groups.push(vec![Span::styled(provider, muted)]);
    }
    let model = app.model_name();
    if !model.is_empty() {
        let mut g = vec![Span::styled(
            super::header::model_display_name(&model),
            Style::default()
                .fg(Color::Rgb(255, 135, 200))
                .add_modifier(Modifier::BOLD),
        )];
        let effort = app.config.effort.as_str();
        if !effort.is_empty() && effort != "default" {
            g.push(Span::styled(format!(" {effort}"), muted));
        }
        groups.push(g);
    }
    let mut out = Vec::new();
    for (i, g) in groups.into_iter().enumerate() {
        if i > 0 {
            out.push(Span::raw(" "));
        }
        out.extend(g);
    }
    out
}

pub fn draw_session_line(buf: &mut Buffer, area: Rect, app: &App) {
    let spans = session_line(app);
    let spans = if spans_width(&spans) > area.width as usize {
        let mut cut = truncate_spans(&spans, area.width as usize);
        if let Some(last) = cut.last_mut() {
            last.style = Style::default().fg(theme::rgb(100, 100, 110));
        }
        cut
    } else {
        spans
    };
    put_right(buf, area, &spans);
}

/// The command list, when the composer holds a command being typed.
pub fn suggestions_active(app: &App) -> Option<Vec<Suggestion>> {
    let text = app.ed.text();
    if commands::model_filter(text).is_some() {
        return None;
    }
    let list = commands::suggestions(app, text);
    (!list.is_empty()).then_some(list)
}

fn lift(c: Color, matched: bool) -> Color {
    match c {
        Color::Rgb(r, g, b) if matched => {
            let up = |v: u8| v + (255 - v) / 2;
            theme::rgb(up(r), up(g), up(b))
        }
        Color::Rgb(r, g, b) => theme::rgb(r / 2, g / 2, b / 2),
        other => other,
    }
}

/// Rows under the composer (above it when there is no room): eight commands at most, the
/// selected one in yellow.
pub fn draw_suggestions(
    buf: &mut Buffer,
    input: Rect,
    screen: Rect,
    app: &App,
    list: &[Suggestion],
) {
    let needle = app.ed.text();
    let sel = app.sugg_sel.min(list.len() - 1);
    let start = if sel >= SUGGESTION_LIMIT {
        sel + 1 - SUGGESTION_LIMIT
    } else {
        0
    };
    let shown: Vec<&Suggestion> = list.iter().skip(start).take(SUGGESTION_LIMIT).collect();
    let count = shown.len() as u16;
    let below = (screen.y + screen.height).saturating_sub(input.y + input.height);
    let y0 = if below >= count {
        input.y + input.height
    } else {
        input.y.saturating_sub(count)
    };
    let more = list.len().saturating_sub(start + shown.len());
    for (i, s) in shown.iter().enumerate() {
        let y = y0 + i as u16;
        clear(buf, Rect::new(input.x, y, input.width, 1));
        let selected = start + i == sel;
        let (cmd_c, desc_c) = if selected || list.len() == 1 {
            (theme::selected(), theme::selected())
        } else {
            (theme::unselected(), theme::dim())
        };
        let mut spans: Vec<Span<'static>> = Vec::new();
        if needle == "/" {
            spans.push(Span::styled(s.command.clone(), Style::default().fg(cmd_c)));
        } else {
            let base = match cmd_c {
                Color::Rgb(..) => cmd_c,
                _ => Color::Rgb(128, 203, 196),
            };
            for (ci, ch) in s.command.chars().enumerate() {
                let m = s.matched.contains(&ci);
                let mut st = Style::default().fg(lift(base, m));
                if m {
                    st = st.add_modifier(Modifier::BOLD);
                }
                match spans.last_mut() {
                    Some(last) if last.style == st => last.content.to_mut().push(ch),
                    _ => spans.push(Span::styled(ch.to_string(), st)),
                }
            }
        }
        spans.push(Span::styled(
            format!("  {}", s.description),
            Style::default().fg(desc_c),
        ));
        if i == 0 && start > 0 {
            spans.push(Span::styled(
                format!("  ↑{start}"),
                Style::default().fg(theme::dim()),
            ));
        }
        if i + 1 == shown.len() && more > 0 {
            spans.push(Span::styled(
                format!("  +{more} more"),
                Style::default().fg(theme::dim()),
            ));
        }
        put(
            buf,
            input.x,
            y,
            &truncate_spans(&spans, input.width as usize),
            input.width,
        );
    }
}

/// The `/model` list: borderless rows right above the composer.
pub fn draw_model_picker(buf: &mut Buffer, input: Rect, app: &App, filter: &str) {
    let dim = Style::default().fg(theme::dim());
    let matches = commands::model_matches(app, filter);
    let mut rows: Vec<Vec<Span<'static>>> = Vec::new();
    if matches.is_empty() {
        let text = if filter.trim().is_empty() {
            "No matching models".to_string()
        } else {
            format!("No matching models for {}", filter.trim())
        };
        rows.push(vec![Span::styled(text, dim)]);
    } else {
        let sel = app.picker_sel.min(matches.len() - 1);
        let room = (input.y as usize)
            .saturating_sub(1)
            .clamp(1, SUGGESTION_LIMIT);
        let start = if sel >= room { sel + 1 - room } else { 0 };
        let label = |i: usize| {
            let m = &app.config.models[i];
            let name = if m.name.is_empty() { &m.id } else { &m.name };
            (name.clone(), m.id == app.config.model)
        };
        let widest = matches
            .iter()
            .skip(start)
            .take(room)
            .map(|&i| {
                let (n, cur) = label(i);
                n.width() + if cur { " current".len() } else { 0 }
            })
            .max()
            .unwrap_or(0);
        let visible: Vec<usize> = matches.iter().copied().skip(start).take(room).collect();
        let more = matches.len().saturating_sub(start + visible.len());
        for (vi, &i) in visible.iter().enumerate() {
            let selected = start + vi == sel;
            let (name, current) = label(i);
            let c = if selected {
                theme::selected()
            } else {
                theme::unselected()
            };
            let mut spans = vec![Span::styled(
                format!("{} {name}", if selected { "▸" } else { " " }),
                Style::default().fg(c),
            )];
            let mut w = name.width();
            if current {
                spans.push(Span::styled(" current", dim));
                w += " current".len();
            }
            let provider = &app.config.models[i].provider;
            if !provider.is_empty() {
                spans.push(Span::raw(" ".repeat(widest - w + 2)));
                spans.push(Span::styled(
                    provider.clone(),
                    if selected {
                        Style::default().fg(theme::selected())
                    } else {
                        dim
                    },
                ));
            }
            if vi == 0 && start > 0 {
                spans.push(Span::styled(format!("  ↑{start}"), dim));
            }
            if vi + 1 == visible.len() && more > 0 {
                spans.push(Span::styled(format!("  +{more} more"), dim));
            }
            rows.push(spans);
        }
    }
    rows.push(vec![Span::styled(
        "↑↓ choose · Enter select · Esc cancel",
        dim,
    )]);
    let y0 = input.y.saturating_sub(rows.len() as u16);
    for (i, spans) in rows.iter().enumerate() {
        let y = y0 + i as u16;
        if y >= input.y {
            break;
        }
        clear(buf, Rect::new(input.x, y, input.width, 1));
        put(
            buf,
            input.x,
            y,
            &truncate_spans(spans, input.width as usize),
            input.width,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn elapsed_and_dirs() {
        assert_eq!(format_elapsed(7), "7s");
        assert_eq!(format_elapsed(75), "1m 15s");
        assert_eq!(format_elapsed(3700), "1h 1m");
        std::env::set_var("HOME", "/home/me");
        assert_eq!(dir_label_short("/home/me"), "~");
        assert_eq!(dir_label_short("/home/me/x"), "~/x");
        assert_eq!(dir_label_short("/home/me/a/b/c"), "~/…/c");
        assert_eq!(dir_label_short("/tmp/a/w/proj"), "…/w/proj");
        assert_eq!(dir_label_short("/tmp/a"), "/tmp/a");
    }
}
