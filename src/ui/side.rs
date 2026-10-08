//! The wide-terminal rail: subagents, background commands, todos.
//!
//! One faint vertical rule, then the text. No fill — the terminal's own
//! background is the column. Under 120 columns this is not drawn; the one-row
//! summary under the composer is the same information, smaller.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use super::{accent, dim, muted};
use crate::app::{App, PaneStatus};
use crate::skin::glyphs;
use crate::theme::{self, Token};
use crate::tools::tasks::TaskStatus;
use crate::tools::todo::TodoStatus;

/// Default step budget the builtin worker runs under. The bar is progress
/// toward that, and the number next to it is the real count — a run with its
/// own cap still shows the count it has actually taken.
const DEFAULT_BUDGET: u32 = 50;

pub(super) fn draw(frame: &mut Frame, app: &App, area: Rect) {
    if area.width < 8 || area.height == 0 {
        return;
    }
    let width = area.width as usize;
    let mut lines: Vec<Line<'static>> = Vec::new();

    if !app.panes.is_empty() {
        for pane in &app.panes {
            lines.extend(agent_rows(app, pane, width));
        }
    }

    if let Some(tasks) = &app.tasks {
        let listed = tasks.list();
        if !listed.is_empty() && !lines.is_empty() {
            lines.push(rule_line(""));
        }
        for task in listed.iter().rev().take(4) {
            lines.extend(task_rows(app, task, tasks, width));
        }
    }

    if app.show_todos && !app.todos.is_empty() {
        if !lines.is_empty() {
            lines.push(rule_line(""));
        }
        for item in &app.todos {
            lines.push(todo_row(item));
        }
    }

    if lines.is_empty() {
        return;
    }
    let lines: Vec<Line<'static>> = lines
        .into_iter()
        .map(|line| super::truncate_line(line, width))
        .collect();
    frame.render_widget(Paragraph::new(lines), area);
}

fn agent_rows(app: &App, pane: &crate::app::SubagentPane, width: usize) -> Vec<Line<'static>> {
    let elapsed = pane.elapsed().as_secs();
    let clock = format!("{}:{:02}", elapsed / 60, elapsed % 60);
    let (glyph, style) = match pane.status {
        PaneStatus::Running => (
            pane.glyph(app.tick).to_string(),
            theme::style(Token::ToolRunning),
        ),
        PaneStatus::Done => ("●".to_string(), theme::style(Token::ToolDone)),
        PaneStatus::Failed => ("✕".to_string(), theme::style(Token::ToolFailed).bold()),
    };
    let focused = app
        .attached
        .is_some_and(|index| app.panes.get(index).is_some_and(|p| p.run == pane.run))
        || app
            .rail_focus
            .is_some_and(|index| app.panes.get(index).is_some_and(|p| p.run == pane.run));
    let name_style = if focused {
        accent().add_modifier(Modifier::BOLD)
    } else {
        muted()
    };
    let mut rows = vec![rule_line_spans(vec![
        Span::styled(format!("{glyph} "), style),
        Span::styled(pane.name.clone(), name_style),
        Span::styled(format!("  {clock}"), dim()),
    ])];
    let activity = pane
        .activity()
        .trim()
        .lines()
        .next()
        .unwrap_or("")
        .to_string();
    if !activity.is_empty() {
        rows.push(rule_line(format!("  {activity}")));
    }
    if pane.status == PaneStatus::Running {
        let bar_width = width.saturating_sub(10).clamp(6, 16);
        let filled =
            ((pane.steps.min(DEFAULT_BUDGET) as usize) * bar_width) / DEFAULT_BUDGET as usize;
        let bar = format!("{}{}", "━".repeat(filled), "─".repeat(bar_width - filled));
        rows.push(rule_line_spans(vec![
            Span::styled(format!("  {bar}  "), dim()),
            Span::styled(pane.steps.to_string(), dim()),
        ]));
    }
    rows
}

fn task_rows(
    app: &App,
    task: &crate::tools::tasks::Task,
    tasks: &crate::tools::tasks::TaskRegistry,
    _width: usize,
) -> Vec<Line<'static>> {
    let _ = app;
    let elapsed = task
        .finished
        .unwrap_or_else(std::time::Instant::now)
        .saturating_duration_since(task.started);
    let clock = super::fmt_elapsed(elapsed).unwrap_or_else(|| "0s".to_string());
    let (glyph, style) = match task.status {
        TaskStatus::Running => ("●", theme::style(Token::ToolRunning)),
        TaskStatus::Done(0) => ("●", theme::style(Token::ToolDone)),
        _ => ("✕", theme::style(Token::ToolFailed).bold()),
    };
    let state = match task.status {
        TaskStatus::Running => "running",
        TaskStatus::Done(0) => "done",
        other => {
            return vec![rule_line_spans(vec![
                Span::styled(format!("{glyph} "), style),
                Span::styled(first_word(&task.command).to_string(), muted()),
                Span::styled(format!("  {}", other.describe()), dim()),
            ])];
        }
    };
    let mut rows = vec![rule_line_spans(vec![
        Span::styled(format!("{glyph} "), style),
        Span::styled(first_word(&task.command).to_string(), muted()),
        Span::styled(format!("  {state}  {clock}"), dim()),
    ])];
    if let Some((_, output)) = tasks.output(task.id, 4_000)
        && let Some(last) = output.lines().rev().find(|line| !line.trim().is_empty())
    {
        let arm = glyphs::tool_arm().0.trim_end();
        rows.push(rule_line(format!("  {arm} {last}")));
    }
    rows
}

fn todo_row(item: &crate::tools::todo::TodoItem) -> Line<'static> {
    let (glyph, style) = match item.status {
        TodoStatus::Completed => ("●", dim().add_modifier(Modifier::CROSSED_OUT)),
        TodoStatus::InProgress => ("▸", accent()),
        TodoStatus::Pending => ("○", dim()),
    };
    let text_style = match item.status {
        TodoStatus::Completed => dim().add_modifier(Modifier::CROSSED_OUT),
        TodoStatus::InProgress => accent(),
        TodoStatus::Pending => muted(),
    };
    rule_line_spans(vec![
        Span::styled(format!("{glyph} "), style),
        Span::styled(item.content.clone(), text_style),
    ])
}

fn first_word(command: &str) -> &str {
    command.split_whitespace().next().unwrap_or(command)
}

fn rule_line(text: impl Into<String>) -> Line<'static> {
    rule_line_spans(vec![Span::styled(text.into(), dim())])
}

fn rule_line_spans(spans: Vec<Span<'static>>) -> Line<'static> {
    let rule = glyphs::adapt("│");
    let mut all = vec![Span::styled(format!("{rule} "), dim())];
    all.extend(spans);
    Line::from(all)
}
