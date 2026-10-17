//! The pinned todo list: a band at the top of the transcript that does not scroll.

use agent_core::TodoStatus;
use ratatui::style::Style;
use ratatui::text::Span;

use super::line::truncate_spans;
use crate::app::App;
use crate::theme;

/// The band's rows for a transcript `width` cells wide in a view `height` rows tall: a row of
/// pips, then one row per item, cut to a third of the view.
pub fn todo_band(app: &App, width: usize, height: usize) -> Vec<Vec<Span<'static>>> {
    let todos = &app.tr.todos;
    if !app.todos_pinned || todos.is_empty() || width < 16 || height < 3 {
        return Vec::new();
    }
    let fg = |r, g, b| Style::default().fg(theme::rgb(r, g, b));
    let count = |s: TodoStatus| todos.iter().filter(|t| t.status == s).count();
    let mut pips = vec![Span::raw("  ")];
    for (status, glyph, style) in [
        (TodoStatus::Completed, "●", fg(100, 180, 100)),
        (
            TodoStatus::InProgress,
            "●",
            Style::default().fg(theme::asap()),
        ),
        (TodoStatus::Pending, "○", fg(90, 90, 105)),
    ] {
        let n = count(status);
        if n > 0 {
            pips.push(Span::styled(glyph.repeat(n), style));
        }
    }
    let mut rows = vec![pips];
    for t in todos {
        let (glyph, gs, ts) = match t.status {
            TodoStatus::Pending => ("○", fg(135, 145, 160), fg(195, 202, 212)),
            TodoStatus::InProgress => ("●", fg(110, 210, 255), fg(225, 232, 240)),
            TodoStatus::Completed => ("✓", fg(105, 190, 125), fg(135, 150, 145)),
        };
        let spans = vec![
            Span::raw("    "),
            Span::styled(format!("{glyph} "), gs),
            Span::styled(t.text.clone(), ts),
        ];
        rows.push(truncate_spans(&spans, width.min(102)));
    }
    let budget = (height / 3).clamp(2, 12);
    if rows.len() > budget {
        let hidden = rows.len() - (budget - 1);
        rows.truncate(budget - 1);
        rows.push(vec![Span::styled(
            format!("  … +{hidden} more (todo)"),
            Style::default().fg(theme::dim()),
        )]);
    }
    rows
}
