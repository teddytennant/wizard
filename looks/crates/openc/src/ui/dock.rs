// OWNER: input
//! The dock above the composer: todos and queued messages.

use std::collections::VecDeque;

use agent_core::{Todo, TodoStatus};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::Span;
use tuikit::paint::{fill, put_str};
use tuikit::width::truncate;

use crate::palette::{Depth, Glyphs, Palette};

pub const DOCK_MAX: usize = 7;

/// Rows of the dock, each a list of spans starting at column 2 of the transcript column.
pub fn dock_rows(
    todos: &[Todo],
    todos_open: bool,
    queue: &VecDeque<String>,
    busy: bool,
    width: usize,
    p: &Palette,
    g: &Glyphs,
) -> Vec<Vec<Span<'static>>> {
    let mut rows: Vec<Vec<Span<'static>>> = Vec::new();
    let done = todos
        .iter()
        .filter(|t| t.status == TodoStatus::Completed)
        .count();
    let all_done = !todos.is_empty() && done == todos.len();
    if !todos.is_empty() && (!all_done || busy) {
        let active = todos.iter().find(|t| t.status == TodoStatus::InProgress);
        let glyph = if todos_open { g.unfolded } else { g.folded };
        let mut head = vec![
            Span::raw("  "),
            Span::styled(glyph, p.s_dim()),
            Span::raw(" "),
            Span::styled(format!("Todos {done}/{}", todos.len()), p.s_dim()),
        ];
        if let Some(a) = active {
            head.push(Span::styled(
                format!(" {} {} ", g.sep, g.active),
                p.s_faint(),
            ));
            head.push(Span::styled(
                truncate(&a.text, width.saturating_sub(24)),
                p.s_text(),
            ));
        }
        rows.push(head);
        if todos_open {
            let budget = DOCK_MAX.saturating_sub(1 + queue_rows(queue)).max(2);
            // Keep the active item in the window.
            let act = todos
                .iter()
                .position(|t| t.status == TodoStatus::InProgress)
                .unwrap_or(done.min(todos.len() - 1));
            let start = if todos.len() <= budget {
                0
            } else {
                act.saturating_sub(budget / 2).min(todos.len() - budget)
            };
            for t in todos.iter().skip(start).take(budget) {
                let (glyph, gs, ts) = match t.status {
                    TodoStatus::Completed => (g.ok, p.s_faint(), p.s_faint()),
                    TodoStatus::InProgress => (g.active, p.s_accent(), p.s_text()),
                    TodoStatus::Pending => (g.pending, p.s_dim(), p.s_dim()),
                };
                rows.push(vec![
                    Span::raw("      "),
                    Span::styled(glyph, gs),
                    Span::raw(" "),
                    Span::styled(truncate(&t.text, width.saturating_sub(10)), ts),
                ]);
            }
        }
    }
    let shown = queue.len().min(3);
    for q in queue.iter().take(shown) {
        let one: String = q.split_whitespace().collect::<Vec<_>>().join(" ");
        rows.push(vec![
            Span::raw("  "),
            Span::styled(g.pending, p.s_dim()),
            Span::styled(" queued: ", p.s_faint()),
            Span::styled(truncate(&one, width.saturating_sub(16)), p.s_dim()),
        ]);
    }
    if queue.len() > shown {
        rows.push(vec![
            Span::raw("    "),
            Span::styled(format!("+{} more", queue.len() - shown), p.s_faint()),
        ]);
    }
    rows.truncate(DOCK_MAX);
    rows
}

/// Paint the dock into `rect`: an edge row, then one row per entry on the `dock` surface.
///
/// The edge is what keeps the tray from reading as the end of the code block or tool body
/// above it (round 2 finding 2). With backgrounds it is a lower half block in the tray colour
/// on the page colour, so the tray starts half a row down and the row above it, whatever
/// that is, is never directly against a surface. Without backgrounds (16 colours, `NO_COLOR`,
/// the ASCII table) it is a rule in the dimmed `line` style.
pub fn paint(buf: &mut Buffer, rect: Rect, rows: &[Vec<Span<'static>>], p: &Palette, g: &Glyphs) {
    if rect.height == 0 || rect.width == 0 {
        return;
    }
    let tint = matches!(p.depth, Depth::True | Depth::Ansi256) && !g.edge.is_empty();
    let edge = Rect::new(rect.x, rect.y, rect.width, 1);
    if tint {
        fill(buf, edge, Style::new().bg(p.bg));
        for dx in 0..rect.width {
            put_str(
                buf,
                rect.x + dx,
                rect.y,
                g.edge,
                Style::new().fg(p.dock).bg(p.bg),
                edge,
            );
        }
    } else {
        for dx in 0..rect.width {
            put_str(buf, rect.x + dx, rect.y, g.rule, p.s_faint(), edge);
        }
    }
    let body = Rect::new(rect.x, rect.y + 1, rect.width, rect.height - 1);
    let bg = if tint { p.dock } else { p.bg };
    for (i, spans) in rows.iter().enumerate() {
        let y = body.y + i as u16;
        if y >= body.bottom() {
            break;
        }
        fill(
            buf,
            Rect::new(body.x, y, body.width, 1),
            Style::new().bg(bg),
        );
        let mut x = body.x;
        for s in spans {
            x = put_str(
                buf,
                x,
                y,
                &s.content,
                Style::new().bg(bg).patch(s.style),
                body,
            );
        }
    }
}

fn queue_rows(queue: &VecDeque<String>) -> usize {
    queue.len().min(3) + usize::from(queue.len() > 3)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::palette::{Depth, Kind, UNICODE};

    #[test]
    fn dock_collapsed_is_one_row_and_hidden_when_done_and_idle() {
        let p = Palette::new(Kind::Hearth, Depth::True);
        let todos = vec![
            Todo {
                text: "a".into(),
                status: TodoStatus::Completed,
            },
            Todo {
                text: "b".into(),
                status: TodoStatus::InProgress,
            },
        ];
        let q = VecDeque::new();
        let rows = dock_rows(&todos, false, &q, true, 80, &p, &UNICODE);
        assert_eq!(rows.len(), 1);
        let t: String = rows[0].iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(t, "  ▸ Todos 1/2 · ◐ b");
        let done = vec![Todo {
            text: "a".into(),
            status: TodoStatus::Completed,
        }];
        assert!(dock_rows(&done, false, &q, false, 80, &p, &UNICODE).is_empty());
    }

    #[test]
    fn queue_shows_three_rows_then_a_count() {
        let p = Palette::new(Kind::Hearth, Depth::True);
        let q: VecDeque<String> = (0..5).map(|i| format!("m{i}")).collect();
        let rows = dock_rows(&[], false, &q, true, 80, &p, &UNICODE);
        assert_eq!(rows.len(), 4);
    }
}
