//! The `todo` tool and its shared list state.
//!
//! The agent maintains a working todo list for multi-step tasks: action
//! `write` replaces the entire list, `read` returns it. State lives in
//! [`ToolContext::todos`](super::ToolContext) (agent-local, never on disk),
//! and every write emits [`AgentEvent::TodoUpdated`] so the TUI overlay
//! above the composer and the headless printer can mirror progress. The tool is classified
//! [`ToolAccess::ReadOnly`]: it touches nothing outside the agent, so todo
//! upkeep stays available in plan mode — planning is exactly when the list
//! gets drafted.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::agent::AgentEvent;

use super::{Tool, ToolAccess, ToolContext, ToolError, ToolOutput, parse_args};

/// Advertised name of the tool.
pub const TODO_TOOL_NAME: &str = "todo";

/// Progress state of one todo item.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TodoStatus {
    Pending,
    InProgress,
    Completed,
}

impl TodoStatus {
    /// Status glyph used by every surface (TUI panel, tool output).
    pub fn glyph(self) -> &'static str {
        match self {
            TodoStatus::Pending => "☐",
            TodoStatus::InProgress => "▸",
            TodoStatus::Completed => "☒",
        }
    }
}

/// One entry in the agent's working todo list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TodoItem {
    pub content: String,
    pub status: TodoStatus,
}

/// The agent's working todo list.
pub type TodoList = Vec<TodoItem>;

/// `(completed, total)` counts for a list.
pub fn progress(items: &[TodoItem]) -> (usize, usize) {
    let done = items
        .iter()
        .filter(|item| item.status == TodoStatus::Completed)
        .count();
    (done, items.len())
}

/// The first in-progress item, if any (what the agent is working on now).
pub fn current(items: &[TodoItem]) -> Option<&TodoItem> {
    items
        .iter()
        .find(|item| item.status == TodoStatus::InProgress)
}

/// Render a list as glyph-prefixed lines (the tool's `read` output).
pub fn render(items: &[TodoItem]) -> String {
    if items.is_empty() {
        return "(todo list is empty)".to_string();
    }
    items
        .iter()
        .map(|item| format!("{} {}", item.status.glyph(), item.content))
        .collect::<Vec<_>>()
        .join("\n")
}

/// One-line progress summary for headless surfaces:
/// `≡ todo: 2/5 done — current: <in_progress item>`.
pub fn summary_line(items: &[TodoItem]) -> String {
    let (done, total) = progress(items);
    match current(items) {
        Some(item) => format!("≡ todo: {done}/{total} done — current: {}", item.content),
        None => format!("≡ todo: {done}/{total} done"),
    }
}

/// What a `todo` call does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Action {
    Write,
    Read,
}

/// `todo` — maintain the session's working todo list.
pub struct TodoTool;

#[async_trait]
impl Tool for TodoTool {
    fn name(&self) -> &str {
        TODO_TOOL_NAME
    }

    fn description(&self) -> &str {
        "Maintain your working todo list for the current task. Action \"write\" replaces the \
         entire list (pass every item, including completed ones, each as {content, status}); \
         action \"read\" returns the current list. Statuses: pending, in_progress, completed — \
         keep exactly one item in_progress at a time and mark items completed as soon as they \
         are done."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["write", "read"],
                    "description": "write replaces the whole list; read returns it"
                },
                "items": {
                    "type": "array",
                    "description": "The full todo list (write only)",
                    "items": {
                        "type": "object",
                        "properties": {
                            "content": { "type": "string" },
                            "status": {
                                "type": "string",
                                "enum": ["pending", "in_progress", "completed"]
                            }
                        },
                        "required": ["content", "status"]
                    }
                }
            },
            "required": ["action"]
        })
    }

    /// Mutates only agent-local state (never the filesystem), so it stays
    /// usable in plan mode — drafting the todo list is part of planning.
    fn access(&self) -> ToolAccess {
        ToolAccess::ReadOnly
    }

    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        #[derive(Deserialize)]
        struct Args {
            action: Action,
            #[serde(default)]
            items: Option<Vec<TodoItem>>,
        }
        let args: Args = parse_args(TODO_TOOL_NAME, args)?;

        match args.action {
            Action::Read => {
                let items = ctx.todos.lock().expect("todo list lock poisoned").clone();
                Ok(ToolOutput::ok(render(&items)))
            }
            Action::Write => {
                let Some(items) = args.items else {
                    return Err(ToolError::InvalidArgs {
                        tool: TODO_TOOL_NAME.to_string(),
                        message: "action \"write\" requires `items` (the full list)".to_string(),
                    });
                };
                *ctx.todos.lock().expect("todo list lock poisoned") = items.clone();
                if let Some(events) = &ctx.events {
                    let _ = events.send(AgentEvent::TodoUpdated(items.clone())).await;
                }
                let (done, total) = progress(&items);
                Ok(ToolOutput::ok(format!(
                    "todo list updated — {done}/{total} done\n{}",
                    render(&items)
                )))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use tokio::sync::mpsc;

    use super::*;

    fn ctx() -> ToolContext {
        ToolContext::new(std::env::temp_dir())
    }

    fn item(content: &str, status: TodoStatus) -> TodoItem {
        TodoItem {
            content: content.to_string(),
            status,
        }
    }

    #[tokio::test]
    async fn write_replaces_the_list_and_read_returns_it() {
        let tool = TodoTool;
        let ctx = ctx();

        let out = tool
            .execute(json!({ "action": "read" }), &ctx)
            .await
            .expect("read ok");
        assert_eq!(out.content, "(todo list is empty)");

        let out = tool
            .execute(
                json!({
                    "action": "write",
                    "items": [
                        { "content": "first", "status": "completed" },
                        { "content": "second", "status": "in_progress" },
                        { "content": "third", "status": "pending" }
                    ]
                }),
                &ctx,
            )
            .await
            .expect("write ok");
        assert!(!out.is_error);
        assert!(out.content.contains("1/3 done"), "{}", out.content);

        let out = tool
            .execute(json!({ "action": "read" }), &ctx)
            .await
            .expect("read ok");
        assert_eq!(out.content, "☒ first\n▸ second\n☐ third");

        // A second write replaces, never appends.
        tool.execute(
            json!({ "action": "write", "items": [{ "content": "only", "status": "pending" }] }),
            &ctx,
        )
        .await
        .expect("write ok");
        assert_eq!(ctx.todos.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn write_emits_todo_updated_on_the_event_channel() {
        let tool = TodoTool;
        let (tx, mut rx) = mpsc::channel(8);
        let ctx = ctx().with_events(tx);

        tool.execute(
            json!({ "action": "write", "items": [{ "content": "a", "status": "pending" }] }),
            &ctx,
        )
        .await
        .expect("write ok");

        match rx.try_recv() {
            Ok(AgentEvent::TodoUpdated(items)) => {
                assert_eq!(items, vec![item("a", TodoStatus::Pending)]);
            }
            other => panic!("expected TodoUpdated, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn write_without_items_is_invalid_args() {
        let err = TodoTool
            .execute(json!({ "action": "write" }), &ctx())
            .await
            .expect_err("missing items");
        assert!(matches!(err, ToolError::InvalidArgs { .. }));
    }

    #[tokio::test]
    async fn unknown_action_or_status_is_invalid_args() {
        let err = TodoTool
            .execute(json!({ "action": "append" }), &ctx())
            .await
            .expect_err("bad action");
        assert!(matches!(err, ToolError::InvalidArgs { .. }));

        let err = TodoTool
            .execute(
                json!({ "action": "write", "items": [{ "content": "x", "status": "doing" }] }),
                &ctx(),
            )
            .await
            .expect_err("bad status");
        assert!(matches!(err, ToolError::InvalidArgs { .. }));
    }

    #[test]
    fn summary_line_names_the_current_item() {
        let items = vec![
            item("done thing", TodoStatus::Completed),
            item("active thing", TodoStatus::InProgress),
            item("later thing", TodoStatus::Pending),
        ];
        assert_eq!(
            summary_line(&items),
            "≡ todo: 1/3 done — current: active thing"
        );
        let all_done = vec![item("a", TodoStatus::Completed)];
        assert_eq!(summary_line(&all_done), "≡ todo: 1/1 done");
    }

    #[test]
    fn tool_is_read_only_for_the_plan_gate() {
        assert_eq!(TodoTool.access(), ToolAccess::ReadOnly);
    }
}
