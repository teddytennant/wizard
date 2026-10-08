//! The wide-terminal rail: subagents, background commands, todos.
//!
//! One faint vertical rule, then the text. No fill — the terminal's own
//! background is the column. Under 120 columns this is not drawn; the one-row
//! summary under the composer is the same information, smaller.
//!
//! Every row is wrapped to the columns *inside* the rule and then prefixed
//! with `│ `, so a long activity line continues indented under its text
//! instead of wrapping back onto the rule.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Modifier;
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::Paragraph;

use super::{accent, dim, muted};
use crate::app::{App, PaneStatus};
use crate::skin::glyphs;
use crate::theme::{self, Token};
use crate::tools::tasks::TaskStatus;
use crate::tools::todo::TodoStatus;

/// Default step budget the builtin worker runs under. The bar is progress
/// toward that, and the number next to it is `steps/budget`.
const DEFAULT_BUDGET: u32 = 50;

pub(super) fn draw(frame: &mut Frame, app: &App, area: Rect) {
    if area.width < 8 || area.height == 0 {
        return;
    }
    // Two columns for `│ `. Everything else — including a wrapped
    // continuation — stays to the right of that.
    let inner = (area.width as usize).saturating_sub(2).max(1);
    let mut lines: Vec<Line<'static>> = Vec::new();

    if !app.panes.is_empty() {
        for pane in &app.panes {
            lines.extend(agent_rows(app, pane, inner));
        }
    }

    if let Some(tasks) = &app.tasks {
        let listed = tasks.list();
        if !listed.is_empty() && !lines.is_empty() {
            lines.push(Line::default());
        }
        for task in listed.iter().rev().take(4) {
            lines.extend(task_rows(task, tasks, inner));
        }
    }

    if app.show_todos && !app.todos.is_empty() {
        if !lines.is_empty() {
            lines.push(Line::default());
        }
        for item in &app.todos {
            lines.extend(wrapped(todo_row(item), inner));
        }
    }

    if lines.is_empty() {
        return;
    }
    let width = area.width as usize;
    let lines: Vec<Line<'static>> = with_rule(lines)
        .into_iter()
        .map(|line| super::truncate_line(line, width))
        .collect();
    frame.render_widget(Paragraph::new(lines), area);
}

fn agent_rows(app: &App, pane: &crate::app::SubagentPane, inner: usize) -> Vec<Line<'static>> {
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
    let mut rows = wrapped(
        Line::from(vec![
            Span::styled(format!("{glyph} "), style),
            Span::styled(pane.name.clone(), name_style),
            Span::styled(format!("  {clock}"), dim()),
        ]),
        inner,
    );
    let activity = pane
        .activity()
        .trim()
        .lines()
        .next()
        .unwrap_or("")
        .to_string();
    if !activity.is_empty() {
        rows.extend(wrapped(
            Line::from(Span::styled(format!("  {activity}"), dim())),
            inner,
        ));
    }
    if pane.status == PaneStatus::Running {
        rows.extend(wrapped(progress_line(pane.steps, inner), inner));
    }
    rows
}

/// `steps/budget` beside a short bar. The budget is the default the worker
/// runs under, so a run that has not moved yet reads `0/50` rather than a
/// bare `0` with no scale.
fn progress_line(steps: u32, inner: usize) -> Line<'static> {
    let fraction = format!("{steps}/{DEFAULT_BUDGET}");
    let reserved = 2 + 2 + fraction.chars().count();
    let bar_width = inner.saturating_sub(reserved).min(12);
    if bar_width < 4 {
        return Line::from(Span::styled(format!("  {fraction}"), dim()));
    }
    let filled = ((steps.min(DEFAULT_BUDGET) as usize) * bar_width) / DEFAULT_BUDGET as usize;
    let bar = format!("{}{}", "━".repeat(filled), "─".repeat(bar_width - filled));
    Line::from(vec![
        Span::styled(format!("  {bar}  "), dim()),
        Span::styled(fraction, dim()),
    ])
}

fn task_rows(
    task: &crate::tools::tasks::Task,
    tasks: &crate::tools::tasks::TaskRegistry,
    inner: usize,
) -> Vec<Line<'static>> {
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
            return wrapped(
                Line::from(vec![
                    Span::styled(format!("{glyph} "), style),
                    Span::styled(first_word(&task.command).to_string(), muted()),
                    Span::styled(format!("  {}", other.describe()), dim()),
                ]),
                inner,
            );
        }
    };
    let mut rows = wrapped(
        Line::from(vec![
            Span::styled(format!("{glyph} "), style),
            Span::styled(first_word(&task.command).to_string(), muted()),
            Span::styled(format!("  {state}  {clock}"), dim()),
        ]),
        inner,
    );
    if let Some((_, output)) = tasks.output(task.id, 4_000)
        && let Some(last) = output.lines().rev().find(|line| !line.trim().is_empty())
    {
        let arm = glyphs::tool_arm().0.trim_end();
        rows.extend(wrapped(
            Line::from(Span::styled(format!("  {arm} {last}"), dim())),
            inner,
        ));
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
    Line::from(vec![
        Span::styled(format!("{glyph} "), style),
        Span::styled(item.content.clone(), text_style),
    ])
}

fn first_word(command: &str) -> &str {
    command.split_whitespace().next().unwrap_or(command)
}

/// Wrap `line` to `inner` columns. Continuation rows keep the line's hanging
/// indent, which for rail body text is the two spaces under the glyph.
fn wrapped(line: Line<'static>, inner: usize) -> Vec<Line<'static>> {
    super::wrap_lines(Text::from(vec![line]), inner.max(1))
}

/// Put the faint rule in front of every row, including wrapped continuations.
fn with_rule(rows: Vec<Line<'static>>) -> Vec<Line<'static>> {
    let rule = format!("{} ", glyphs::adapt("│"));
    rows.into_iter()
        .map(|line| {
            let mut spans = Vec::with_capacity(line.spans.len() + 1);
            spans.push(Span::styled(rule.clone(), dim()));
            spans.extend(line.spans);
            Line::from(spans)
        })
        .collect()
}
