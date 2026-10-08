//! Small overlays on the house frame: search, history, background output, keys.
//!
//! Each one is text on the terminal's own background. `Clear` blanks the cells
//! it covers back to `Reset`, which is the same as not painting a fill.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Paragraph, Wrap};

use super::{dim, muted};
use crate::app::App;
use crate::theme::{self, Token};
use crate::tools::tasks::TaskStatus;
use crate::transcript::TranscriptItem;

pub(super) fn draw(frame: &mut Frame, app: &App, body: Rect) {
    if body.width == 0 || body.height == 0 {
        return;
    }
    if app.keys_help {
        draw_text(frame, body, keys_lines());
    } else if app.task_view {
        draw_text(frame, body, task_lines(app));
    } else if let Some(query) = &app.history_search {
        draw_text(frame, body, history_lines(app, query));
    } else if let Some(query) = &app.find {
        let band = Rect { height: 1, ..body };
        frame.render_widget(Clear, band);
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled("find  ", dim()),
                Span::styled(query.clone(), muted()),
                Span::styled(format!("  {}", find_count(app, query)), dim()),
            ])),
            band,
        );
    }
}

fn draw_text(frame: &mut Frame, area: Rect, lines: Vec<Line<'static>>) {
    frame.render_widget(Clear, area);
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), area);
}

fn keys_lines() -> Vec<Line<'static>> {
    crate::app::help_keys()
        .lines()
        .map(|line| Line::from(Span::styled(line.to_string(), dim())))
        .collect()
}

fn history_lines(app: &App, query: &str) -> Vec<Line<'static>> {
    let mut lines = vec![Line::from(vec![
        Span::styled("history  ", dim()),
        Span::styled(query.to_string(), muted()),
    ])];
    let q = query.to_ascii_lowercase();
    for (index, line) in app
        .history
        .iter()
        .rev()
        .filter(|line| q.is_empty() || line.to_ascii_lowercase().contains(&q))
        .take(12)
        .enumerate()
    {
        let style = if index == app.overlay_index {
            theme::style(Token::Accent).add_modifier(ratatui::style::Modifier::BOLD)
        } else {
            muted()
        };
        lines.push(Line::from(Span::styled(line.clone(), style)));
    }
    lines
}

fn task_lines(app: &App) -> Vec<Line<'static>> {
    let Some(tasks) = &app.tasks else {
        return vec![Line::from(Span::styled("no background tasks", dim()))];
    };
    let listed = tasks.list();
    let Some(task) = listed.iter().max_by_key(|task| task.id) else {
        return vec![Line::from(Span::styled("no background tasks", dim()))];
    };
    let mut lines = vec![Line::from(vec![
        Span::styled(task.command.clone(), muted()),
        Span::styled(format!("  {}", task.status.describe()), dim()),
    ])];
    if let Some((_, output)) = tasks.output(task.id, 20_000) {
        let all: Vec<&str> = output.lines().collect();
        let start = all.len().saturating_sub(body_cap());
        for line in &all[start..] {
            let style = if matches!(task.status, TaskStatus::Done(0) | TaskStatus::Running) {
                muted()
            } else {
                theme::style(Token::Error)
            };
            lines.push(Line::from(Span::styled((*line).to_string(), style)));
        }
    }
    lines.push(Line::from(Span::styled("esc close  x stop", dim())));
    lines
}

fn body_cap() -> usize {
    40
}

fn find_count(app: &App, query: &str) -> String {
    if query.is_empty() {
        return String::new();
    }
    let q = query.to_ascii_lowercase();
    let hits = app
        .transcript
        .iter()
        .filter(|item| item_text(item).to_ascii_lowercase().contains(&q))
        .count();
    format!("{hits}")
}

fn item_text(item: &TranscriptItem) -> String {
    match item {
        TranscriptItem::User { text, .. }
        | TranscriptItem::Text(text)
        | TranscriptItem::Thinking(text)
        | TranscriptItem::Notice(text) => text.clone(),
        TranscriptItem::Tool(tool) => format!("{} {}", tool.name, tool.progress),
        _ => String::new(),
    }
}
