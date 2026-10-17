//! Full-screen views: help and the session picker.

use agent_core::SessionInfo;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::Span;

use super::line::truncate_spans;
use super::put;
use crate::app::App;
use crate::theme;

enum HelpRow {
    Section(&'static str),
    Command(String, String),
    Key(&'static str, &'static str),
    Rule,
    Blank,
}

const NAVIGATION: &[(&str, &str)] = &[
    ("PageUp / PageDown", "Scroll history"),
    ("Up / Down", "Scroll history (when input empty)"),
    ("Ctrl+J / Ctrl+K", "Jump to next / previous user prompt"),
    ("Alt+U / Alt+D", "Scroll history up / down ten lines"),
    ("Ctrl+G", "Set a scroll bookmark, press again to return"),
];

const EDITING: &[(&str, &str)] = &[
    ("Ctrl+C / Ctrl+D", "Quit (press twice to confirm)"),
    ("Ctrl+U", "Clear input line"),
    ("Ctrl+K", "Delete to end of input"),
    (
        "Alt+Backspace / Alt+Delete",
        "Delete previous word in input",
    ),
    ("Ctrl+S", "Stash / pop input (save for later)"),
    ("Ctrl+Backspace", "Delete previous word in input"),
    ("Ctrl+B / Ctrl+F", "Move by word left / right"),
    ("Ctrl+Left / Right", "Move by word left / right"),
    ("Shift+Enter / Alt+Enter", "Insert newline in input"),
    ("Trailing \\ then Enter", "Insert newline"),
    ("Ctrl+Enter", "Stop the turn and send now"),
    ("Ctrl+Up", "Retrieve pending message for editing"),
    ("Ctrl+L", "Clear the view, keeping context"),
    ("Alt+X", "Show / hide the pinned todo list"),
    ("Alt+Left / Alt+Right", "Cycle reasoning effort"),
    ("Alt+R", "Open the /resume session picker"),
];

fn help_rows(app: &App) -> Vec<HelpRow> {
    let mut rows = vec![HelpRow::Blank, HelpRow::Section("Commands"), HelpRow::Blank];
    for (name, desc) in crate::commands::LOCAL {
        rows.push(HelpRow::Command(format!("/{name}"), (*desc).to_string()));
    }
    rows.extend([HelpRow::Blank, HelpRow::Rule, HelpRow::Blank]);
    rows.extend([HelpRow::Section("More commands"), HelpRow::Blank]);
    for c in &app.commands {
        if crate::commands::LOCAL.iter().any(|(n, _)| *n == c.name) {
            continue;
        }
        let name = if c.input_hint.is_empty() {
            format!("/{}", c.name)
        } else {
            format!("/{} {}", c.name, c.input_hint)
        };
        rows.push(HelpRow::Command(name, c.description.clone()));
    }
    rows.extend([HelpRow::Blank, HelpRow::Rule, HelpRow::Blank]);
    rows.extend([HelpRow::Section("Navigation"), HelpRow::Blank]);
    rows.extend(NAVIGATION.iter().map(|(k, d)| HelpRow::Key(k, d)));
    rows.extend([HelpRow::Blank, HelpRow::Rule, HelpRow::Blank]);
    rows.extend([HelpRow::Section("Input & Editing"), HelpRow::Blank]);
    rows.extend(EDITING.iter().map(|(k, d)| HelpRow::Key(k, d)));
    rows.push(HelpRow::Blank);
    rows
}

fn border(buf: &mut Buffer, area: Rect, top: &[Span<'static>], bottom: &[Span<'static>]) {
    let st = Style::default().fg(theme::dim());
    let (x0, x1, y0, y1) = (area.x, area.right() - 1, area.y, area.bottom() - 1);
    for x in x0..=x1 {
        buf[(x, y0)].set_symbol("─").set_style(st);
        buf[(x, y1)].set_symbol("─").set_style(st);
    }
    for y in y0..=y1 {
        buf[(x0, y)].set_symbol("│").set_style(st);
        buf[(x1, y)].set_symbol("│").set_style(st);
    }
    buf[(x0, y0)].set_symbol("┌");
    buf[(x1, y0)].set_symbol("┐");
    buf[(x0, y1)].set_symbol("└");
    buf[(x1, y1)].set_symbol("┘");
    let room = area.width.saturating_sub(2);
    put(buf, x0 + 1, y0, top, room);
    put(buf, x0 + 1, y1, bottom, room);
}

/// Draws the help view and returns the furthest it can scroll.
pub fn draw_help(buf: &mut Buffer, area: Rect, app: &App, scroll: usize) -> usize {
    let rows = help_rows(app);
    let inner_h = area.height.saturating_sub(2) as usize;
    let max = rows.len().saturating_sub(inner_h);
    let scroll = scroll.min(max);
    let title_style = Style::default()
        .fg(theme::rgb(200, 200, 220))
        .add_modifier(Modifier::BOLD);
    let title = match (scroll * 100).checked_div(max) {
        Some(pct) => format!(" Help  {pct}%  "),
        None => " Help ".to_string(),
    };
    border(
        buf,
        area,
        &[Span::styled(title, title_style)],
        &[Span::styled(
            " Esc to close · mouse wheel/j/k scroll · Space/PageUp page ",
            Style::default().fg(theme::dim()),
        )],
    );
    let inner_w = area.width.saturating_sub(2);
    for (i, row) in rows.iter().skip(scroll).take(inner_h).enumerate() {
        let y = area.y + 1 + i as u16;
        let spans: Vec<Span<'static>> = match row {
            HelpRow::Blank => continue,
            HelpRow::Section(name) => vec![Span::styled(
                format!("  {name}"),
                Style::default()
                    .fg(theme::accent())
                    .add_modifier(Modifier::BOLD),
            )],
            HelpRow::Command(cmd, desc) => vec![
                Span::raw("    "),
                Span::styled(cmd.clone(), Style::default().fg(theme::rgb(230, 230, 240))),
                Span::raw("  "),
                Span::styled(desc.clone(), Style::default().fg(theme::rgb(150, 150, 165))),
            ],
            HelpRow::Key(key, desc) => vec![
                Span::raw("    "),
                Span::styled(
                    format!("{key:<22}"),
                    Style::default().fg(theme::rgb(200, 180, 120)),
                ),
                Span::styled(
                    (*desc).to_string(),
                    Style::default().fg(theme::rgb(150, 150, 165)),
                ),
            ],
            HelpRow::Rule => vec![
                Span::raw("  "),
                Span::styled("─".repeat(49), Style::default().fg(theme::rgb(50, 50, 55))),
            ],
        };
        put(
            buf,
            area.x + 1,
            y,
            &truncate_spans(&spans, inner_w as usize),
            inner_w,
        );
    }
    max
}

pub fn ago(now: i64, then: i64) -> String {
    let d = (now - then).max(0);
    match d {
        0..=59 => "just now".to_string(),
        60..=3599 => format!("{}m ago", d / 60),
        3600..=86_399 => format!("{}h ago", d / 3600),
        _ => format!("{}d ago", d / 86_400),
    }
}

fn session_title(s: &SessionInfo) -> String {
    if s.title.trim().is_empty() {
        s.id.chars().take(8).collect()
    } else {
        s.title.clone()
    }
}

fn rounded(
    buf: &mut Buffer,
    area: Rect,
    color: ratatui::style::Color,
    top: &[Span<'static>],
    bottom: &[Span<'static>],
) {
    let st = Style::default().fg(color);
    let (x0, x1, y0, y1) = (area.x, area.right() - 1, area.y, area.bottom() - 1);
    for x in x0..=x1 {
        buf[(x, y0)].set_symbol("─").set_style(st);
        buf[(x, y1)].set_symbol("─").set_style(st);
    }
    for y in y0..=y1 {
        buf[(x0, y)].set_symbol("│").set_style(st);
        buf[(x1, y)].set_symbol("│").set_style(st);
    }
    buf[(x0, y0)].set_symbol("╭");
    buf[(x1, y0)].set_symbol("╮");
    buf[(x0, y1)].set_symbol("╰");
    buf[(x1, y1)].set_symbol("╯");
    let room = area.width.saturating_sub(2);
    put(buf, x0 + 1, y0, top, room);
    put(buf, x0 + 1, y1, bottom, room);
}

/// The `/resume` picker: sessions on the left, the selected one on the right.
pub fn draw_sessions(buf: &mut Buffer, area: Rect, app: &App, sel: usize, loading: bool) {
    let dim = Style::default().fg(theme::dim());
    let faint = Style::default().fg(theme::rgb(70, 70, 70));
    let list_w = (area.width * 40 / 100).max(20).min(area.width);
    let list = Rect::new(area.x, area.y, list_w, area.height);
    let preview = Rect::new(area.x + list_w, area.y, area.width - list_w, area.height);
    let title = if loading {
        vec![Span::styled(
            " loading sessions ",
            Style::default().fg(theme::rgb(120, 120, 120)),
        )]
    } else {
        vec![
            Span::raw(" "),
            Span::styled(
                app.sessions.len().to_string(),
                Style::default()
                    .fg(theme::rgb(200, 200, 200))
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(" "),
            Span::styled("sessions ", Style::default().fg(theme::rgb(120, 120, 120))),
        ]
    };
    rounded(
        buf,
        list,
        theme::rgb(130, 130, 160),
        &title,
        &[Span::styled(" Enter in place · ↑↓ · q ", dim)],
    );
    if preview.width > 2 {
        rounded(
            buf,
            preview,
            theme::rgb(70, 70, 70),
            &[Span::styled(" Preview ", faint)],
            &[],
        );
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64);
    let inner_w = list.width.saturating_sub(2);
    let per = 3usize;
    let room = (list.height.saturating_sub(2) as usize / per).max(1);
    let start = if sel >= room { sel + 1 - room } else { 0 };
    if app.sessions.is_empty() && !loading {
        put(
            buf,
            list.x + 1,
            list.y + 1,
            &[Span::styled("  No sessions found.", dim)],
            inner_w,
        );
    }
    let bg = theme::rgb(40, 44, 52);
    for (i, s) in app.sessions.iter().enumerate().skip(start).take(room) {
        let y = list.y + 1 + ((i - start) * per) as u16;
        let selected = i == sel;
        let mut base = Style::default();
        if selected {
            base = base.bg(bg).add_modifier(Modifier::BOLD);
            for yy in y..(y + per as u16 - 1).min(list.bottom() - 1) {
                for x in list.x + 1..list.right() - 1 {
                    buf[(x, yy)].set_style(Style::default().bg(bg));
                }
            }
        }
        let when = if s.updated > 0 {
            ago(now, s.updated)
        } else {
            String::new()
        };
        let mut l1 = vec![
            Span::styled(
                if selected { "● " } else { "○ " },
                base.fg(theme::rgb(90, 90, 90)),
            ),
            Span::styled(session_title(s), base.fg(theme::rgb(140, 220, 160))),
        ];
        if !when.is_empty() {
            l1.push(Span::styled(
                format!("  {when}"),
                base.fg(theme::rgb(100, 100, 100)),
            ));
        }
        if s.id == app.tr.session_id {
            l1.push(Span::styled(
                "  ◀ current",
                base.fg(theme::rgb(120, 200, 140)),
            ));
        }
        put(
            buf,
            list.x + 1,
            y,
            &truncate_spans(&l1, inner_w as usize),
            inner_w,
        );
        let mut l2 = vec![Span::styled("     ", base)];
        if !when.is_empty() {
            l2.push(Span::styled(
                format!("active {when}  "),
                base.fg(theme::rgb(70, 70, 70)),
            ));
        }
        l2.push(Span::styled(
            format!("📁 {}", s.cwd),
            base.fg(theme::rgb(120, 200, 140)),
        ));
        put(
            buf,
            list.x + 1,
            y + 1,
            &truncate_spans(&l2, inner_w as usize),
            inner_w,
        );
    }
    if preview.width > 4 {
        let px = preview.x + 1;
        let pw = preview.width.saturating_sub(2);
        match app.sessions.get(sel) {
            None => {
                put(
                    buf,
                    px,
                    preview.y + 1,
                    &[Span::styled("No session selected", dim)],
                    pw,
                );
            }
            Some(s) => {
                let rows = [
                    vec![Span::styled(
                        session_title(s),
                        Style::default()
                            .fg(theme::rgb(255, 255, 255))
                            .add_modifier(Modifier::BOLD),
                    )],
                    vec![Span::styled(format!("📁 {}", s.cwd), dim)],
                    vec![Span::styled(s.id.clone(), dim)],
                ];
                for (i, spans) in rows.iter().enumerate() {
                    put(
                        buf,
                        px,
                        preview.y + 1 + i as u16,
                        &truncate_spans(spans, pw as usize),
                        pw,
                    );
                }
            }
        }
    }
}
