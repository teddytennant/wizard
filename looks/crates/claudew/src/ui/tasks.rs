//! The task list: a panel above the prompt, not part of the transcript. While a turn runs it
//! hangs under the spinner (the first five tasks in order); between turns it has a heading and
//! the five that matter most. Wizard's `todo` list fills it.

use agent_core::{Todo, TodoStatus};
use ratatui::style::{Modifier, Style};

use crate::app::App;
use crate::text::{truncate, Row};

/// Tasks shown before the rest are counted.
const SHOWN: usize = 5;
/// Columns of the row the task text leaves free: its text is cut at `width - 15`.
const MARGIN: usize = 15;

/// Whether the panel is on screen: there are tasks, some still to do or a turn running.
pub fn visible(app: &App) -> bool {
    app.show_todos
        && !app.todos.is_empty()
        && (app.busy.is_some() || app.todos.iter().any(|t| t.status != TodoStatus::Completed))
}

/// Whether there is a list to show or hide with ctrl+t, shown or not.
pub fn exists(app: &App) -> bool {
    !app.todos.is_empty()
        && (app.busy.is_some() || app.todos.iter().any(|t| t.status != TodoStatus::Completed))
}

/// The task a running turn is working on, for the spinner to name.
pub fn current(app: &App) -> Option<&str> {
    app.todos
        .iter()
        .find(|t| t.status == TodoStatus::InProgress)
        .map(|t| t.text.as_str())
}

fn count(todos: &[Todo], status: TodoStatus) -> usize {
    todos.iter().filter(|t| t.status == status).count()
}

fn item(app: &App, t: &Todo, lead: Row, w: usize) -> Row {
    let th = app.theme;
    let text = truncate(&t.text, w.saturating_sub(MARGIN).max(1));
    let (mark, mark_style, text_style) = match t.status {
        TodoStatus::Completed => (
            "✔",
            th.fg(th.success),
            th.dim().add_modifier(Modifier::CROSSED_OUT),
        ),
        TodoStatus::InProgress => (
            "◼",
            th.fg(th.brand),
            Style::new().add_modifier(Modifier::BOLD),
        ),
        TodoStatus::Pending => ("◻", Style::new(), Style::new()),
    };
    let mut row = lead;
    row.push(mark, mark_style);
    row.push(" ", Style::new());
    row.push(text, text_style);
    row
}

/// "+1 in progress, 3 pending, 3 completed" for the tasks that did not fit.
fn rest(hidden: &[&Todo]) -> String {
    let n = |s: TodoStatus| hidden.iter().filter(|t| t.status == s).count();
    let parts = [
        (n(TodoStatus::InProgress), "in progress"),
        (n(TodoStatus::Pending), "pending"),
        (n(TodoStatus::Completed), "completed"),
    ];
    let parts: Vec<String> = parts
        .iter()
        .filter(|(c, _)| *c > 0)
        .map(|(c, name)| format!("{c} {name}"))
        .collect();
    format!("+{}", parts.join(", "))
}

/// Under the spinner of a running turn: the first five in order, then what is left.
pub fn running_rows(app: &App, w: usize) -> Vec<Row> {
    let th = app.theme;
    let mut out = Vec::new();
    for (i, t) in app.todos.iter().take(SHOWN).enumerate() {
        let lead = if i == 0 {
            Row::of("  ⎿ \u{a0}", th.dim())
        } else {
            Row::of("     ", Style::new())
        };
        out.push(item(app, t, lead, w));
    }
    if app.todos.len() > SHOWN {
        let hidden: Vec<&Todo> = app.todos.iter().skip(SHOWN).collect();
        out.push(Row::of(format!("      … {}", rest(&hidden)), th.dim()));
    }
    out
}

/// Between turns: two blank rows, a heading, then the tasks. Up to five are listed in order; past that the
/// unfinished ones come first (in progress, pending, completed) and the rest is counted.
pub fn idle_rows(app: &App, w: usize) -> Vec<Row> {
    let th = app.theme;
    let todos = &app.todos;
    let done = count(todos, TodoStatus::Completed);
    let doing = count(todos, TodoStatus::InProgress);
    let open = count(todos, TodoStatus::Pending);
    let bold = th.dim().add_modifier(Modifier::BOLD);
    let plural = if todos.len() == 1 { "task" } else { "tasks" };
    let mut head = Row::of("  ", Style::new());
    head.push(todos.len().to_string(), bold);
    head.push(format!(" {plural} ("), th.dim());
    let mut first = true;
    for (n, name) in [(done, "done"), (doing, "in progress"), (open, "open")] {
        if n == 0 {
            continue;
        }
        if !first {
            head.push(", ", th.dim());
        }
        first = false;
        head.push(n.to_string(), bold);
        head.push(format!(" {name}"), th.dim());
    }
    head.push(")", th.dim());
    // two blank rows set the list off from the transcript
    let mut out = vec![Row::new(), Row::new(), head];
    let mut order: Vec<&Todo> = todos.iter().collect();
    if todos.len() > SHOWN {
        let rank = |t: &Todo| match t.status {
            TodoStatus::InProgress => 0,
            TodoStatus::Pending => 1,
            TodoStatus::Completed => 2,
        };
        order.sort_by_key(|t| rank(t));
    }
    for t in order.iter().take(SHOWN) {
        out.push(item(app, t, Row::of("  ", Style::new()), w));
    }
    if order.len() > SHOWN {
        out.push(Row::of(format!("   … {}", rest(&order[SHOWN..])), th.dim()));
    }
    out
}
