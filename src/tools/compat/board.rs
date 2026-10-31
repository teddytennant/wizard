//! `TaskCreate`, `TaskGet`, `TaskList`, `TaskUpdate` and `TaskStop`.
//!
//! The first four are Claude Code's structured task list: numbered tasks with
//! a subject, a description, an owner, and blocks/blockedBy edges. Wizard's
//! `todo` tool is a flat checklist with no ids, so the board is its own store
//! ([`TaskBoard`], in [`ToolContext::board`]) and mirrors its subjects and
//! statuses into the `todo` list after every change, which is what the TUI
//! panel and the status line draw. A `todo` write does not touch the board.
//!
//! `TaskStop` is the odd one out: it stops background work, and Wizard keeps
//! background shell commands and background subagents in two registries with
//! two id counters. It looks in the shell tasks first, and takes `agent:<n>`
//! or a subagent's name to reach the other.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Map, Value, json};

use super::{invalid, str_arg};
use crate::agent::AgentEvent;
use crate::tools::todo::{TodoItem, TodoStatus};
use crate::tools::{Tool, ToolAccess, ToolContext, ToolError, ToolOutput};

pub fn tools() -> Vec<Arc<dyn Tool>> {
    vec![
        Arc::new(TaskCreateTool),
        Arc::new(TaskGetTool),
        Arc::new(TaskListTool),
        Arc::new(TaskUpdateTool),
        Arc::new(TaskStopTool),
    ]
}

/// One task on the board.
#[derive(Debug, Clone)]
pub struct BoardTask {
    pub id: u32,
    pub subject: String,
    pub description: String,
    pub active_form: Option<String>,
    pub status: TodoStatus,
    pub blocks: Vec<u32>,
    pub blocked_by: Vec<u32>,
    pub owner: Option<String>,
    pub metadata: Map<String, Value>,
}

/// The session's task board.
#[derive(Debug, Default)]
pub struct TaskBoard {
    tasks: Vec<BoardTask>,
    next_id: u32,
}

impl TaskBoard {
    fn create(&mut self, subject: &str, description: &str, active_form: Option<&str>) -> u32 {
        self.next_id += 1;
        self.tasks.push(BoardTask {
            id: self.next_id,
            subject: subject.to_string(),
            description: description.to_string(),
            active_form: active_form.map(str::to_string),
            status: TodoStatus::Pending,
            blocks: Vec::new(),
            blocked_by: Vec::new(),
            owner: None,
            metadata: Map::new(),
        });
        self.next_id
    }

    fn get(&self, id: u32) -> Option<&BoardTask> {
        self.tasks.iter().find(|t| t.id == id)
    }

    fn get_mut(&mut self, id: u32) -> Option<&mut BoardTask> {
        self.tasks.iter_mut().find(|t| t.id == id)
    }

    /// Add the edge `blocker` blocks `blocked` on both tasks.
    fn link(&mut self, blocker: u32, blocked: u32) {
        if let Some(task) = self.get_mut(blocker)
            && !task.blocks.contains(&blocked)
        {
            task.blocks.push(blocked);
        }
        if let Some(task) = self.get_mut(blocked)
            && !task.blocked_by.contains(&blocker)
        {
            task.blocked_by.push(blocker);
        }
    }

    /// Remove a task and every edge pointing at it.
    fn delete(&mut self, id: u32) {
        self.tasks.retain(|t| t.id != id);
        for task in &mut self.tasks {
            task.blocks.retain(|b| *b != id);
            task.blocked_by.retain(|b| *b != id);
        }
    }

    /// The board as a `todo` list: subjects and statuses.
    fn as_todos(&self) -> Vec<TodoItem> {
        self.tasks
            .iter()
            .map(|t| TodoItem {
                content: t.subject.clone(),
                status: t.status,
                active_form: t.active_form.clone(),
            })
            .collect()
    }
}

fn status_name(status: TodoStatus) -> &'static str {
    match status {
        TodoStatus::Pending => "pending",
        TodoStatus::InProgress => "in_progress",
        TodoStatus::Completed => "completed",
    }
}

fn parse_status(text: &str) -> Option<TodoStatus> {
    match text {
        "pending" => Some(TodoStatus::Pending),
        "in_progress" => Some(TodoStatus::InProgress),
        "completed" => Some(TodoStatus::Completed),
        _ => None,
    }
}

fn parse_id(text: &str) -> Option<u32> {
    text.trim().trim_start_matches('#').parse().ok()
}

fn ids(args: &Value, key: &str) -> Vec<u32> {
    args.get(key)
        .and_then(Value::as_array)
        .map(|list| {
            list.iter()
                .filter_map(Value::as_str)
                .filter_map(parse_id)
                .collect()
        })
        .unwrap_or_default()
}

/// Push the board into the `todo` list and tell the surface.
async fn mirror(ctx: &ToolContext, todos: Vec<TodoItem>) {
    *ctx.todos.lock().expect("todo list lock poisoned") = todos.clone();
    if let Some(events) = &ctx.events {
        let _ = events.send(AgentEvent::TodoUpdated(todos)).await;
    }
}

/// `TaskCreate`.
pub struct TaskCreateTool;

#[async_trait]
impl Tool for TaskCreateTool {
    fn name(&self) -> &str {
        "TaskCreate"
    }

    fn description(&self) -> &str {
        "Add a task to the task list: a subject, what needs doing, and optionally the \
         present-continuous form shown while it runs. Tasks start pending. Mirrors into the \
         todo panel."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "subject": { "type": "string", "description": "A brief title for the task" },
                "description": { "type": "string", "description": "What needs to be done" },
                "activeForm": { "type": "string", "description": "Present continuous form shown while in progress (e.g. \"Running tests\")" },
                "metadata": { "type": "object", "description": "Arbitrary metadata to attach to the task" }
            },
            "required": ["subject", "description"]
        })
    }

    fn access(&self) -> ToolAccess {
        ToolAccess::ReadOnly
    }

    fn deferred_by_default(&self) -> bool {
        true
    }

    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        let (Some(subject), Some(description)) =
            (str_arg(&args, "subject"), str_arg(&args, "description"))
        else {
            return Ok(invalid(self.name(), "subject and description are required"));
        };
        let (id, todos) = {
            let mut board = ctx.board.lock().expect("task board lock poisoned");
            let id = board.create(subject, description, str_arg(&args, "activeForm"));
            if let Some(metadata) = args.get("metadata").and_then(Value::as_object)
                && let Some(task) = board.get_mut(id)
            {
                task.metadata = metadata.clone();
            }
            (id, board.as_todos())
        };
        mirror(ctx, todos).await;
        Ok(ToolOutput::ok(format!(
            "Task #{id} created successfully: {subject}"
        )))
    }
}

/// `TaskGet`.
pub struct TaskGetTool;

#[async_trait]
impl Tool for TaskGetTool {
    fn name(&self) -> &str {
        "TaskGet"
    }

    fn description(&self) -> &str {
        "Read one task: subject, status, description, and what blocks it or it blocks."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": { "taskId": { "type": "string", "description": "The ID of the task to retrieve" } },
            "required": ["taskId"]
        })
    }

    fn access(&self) -> ToolAccess {
        ToolAccess::ReadOnly
    }

    fn deferred_by_default(&self) -> bool {
        true
    }

    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        let Some(id) = str_arg(&args, "taskId").and_then(parse_id) else {
            return Ok(ToolOutput::ok("Task not found"));
        };
        let board = ctx.board.lock().expect("task board lock poisoned");
        let Some(task) = board.get(id) else {
            return Ok(ToolOutput::ok("Task not found"));
        };
        let hash = |ids: &[u32]| {
            ids.iter()
                .map(|id| format!("#{id}"))
                .collect::<Vec<_>>()
                .join(", ")
        };
        let mut lines = vec![
            format!("Task #{}: {}", task.id, task.subject),
            format!("Status: {}", status_name(task.status)),
            format!("Description: {}", task.description),
        ];
        if !task.blocked_by.is_empty() {
            lines.push(format!("Blocked by: {}", hash(&task.blocked_by)));
        }
        if !task.blocks.is_empty() {
            lines.push(format!("Blocks: {}", hash(&task.blocks)));
        }
        Ok(ToolOutput::ok(lines.join("\n")))
    }
}

/// `TaskList`.
pub struct TaskListTool;

#[async_trait]
impl Tool for TaskListTool {
    fn name(&self) -> &str {
        "TaskList"
    }

    fn description(&self) -> &str {
        "List every task with its status, owner, and open blockers."
    }

    fn parameters(&self) -> Value {
        json!({ "type": "object", "properties": {} })
    }

    fn access(&self) -> ToolAccess {
        ToolAccess::ReadOnly
    }

    fn deferred_by_default(&self) -> bool {
        true
    }

    async fn execute(&self, _args: Value, ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        let board = ctx.board.lock().expect("task board lock poisoned");
        if board.tasks.is_empty() {
            return Ok(ToolOutput::ok("No tasks found"));
        }
        let lines: Vec<String> = board
            .tasks
            .iter()
            .map(|t| {
                let owner = t
                    .owner
                    .as_ref()
                    .map(|o| format!(" ({o})"))
                    .unwrap_or_default();
                // A blocker that is done no longer blocks.
                let open: Vec<String> = t
                    .blocked_by
                    .iter()
                    .filter(|id| {
                        board
                            .get(**id)
                            .is_some_and(|b| b.status != TodoStatus::Completed)
                    })
                    .map(|id| format!("#{id}"))
                    .collect();
                let blocked = if open.is_empty() {
                    String::new()
                } else {
                    format!(" [blocked by {}]", open.join(", "))
                };
                format!(
                    "#{} [{}] {}{owner}{blocked}",
                    t.id,
                    status_name(t.status),
                    t.subject
                )
            })
            .collect();
        Ok(ToolOutput::ok(lines.join("\n")))
    }
}

/// `TaskUpdate`.
pub struct TaskUpdateTool;

#[async_trait]
impl Tool for TaskUpdateTool {
    fn name(&self) -> &str {
        "TaskUpdate"
    }

    fn description(&self) -> &str {
        "Change a task: subject, description, status (pending, in_progress, completed, or \
         deleted to remove it), owner, metadata, and blocking edges (addBlocks, addBlockedBy). \
         Mark a task completed as soon as it is done."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "taskId": { "type": "string", "description": "The ID of the task to update" },
                "subject": { "type": "string", "description": "New subject for the task" },
                "description": { "type": "string", "description": "New description for the task" },
                "activeForm": { "type": "string", "description": "Present continuous form shown while in progress" },
                "status": { "type": "string", "enum": ["pending", "in_progress", "completed", "deleted"], "description": "New status for the task" },
                "addBlocks": { "type": "array", "items": { "type": "string" }, "description": "Task IDs that this task blocks" },
                "addBlockedBy": { "type": "array", "items": { "type": "string" }, "description": "Task IDs that block this task" },
                "owner": { "type": "string", "description": "New owner for the task" },
                "metadata": { "type": "object", "description": "Metadata keys to merge into the task. Set a key to null to delete it." }
            },
            "required": ["taskId"]
        })
    }

    fn access(&self) -> ToolAccess {
        ToolAccess::ReadOnly
    }

    fn deferred_by_default(&self) -> bool {
        true
    }

    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        let Some(raw_id) = str_arg(&args, "taskId") else {
            return Ok(invalid(self.name(), "taskId is required"));
        };
        let Some(id) = parse_id(raw_id) else {
            return Ok(ToolOutput::ok("Task not found"));
        };
        let status_arg = str_arg(&args, "status");
        if let Some(status) = status_arg
            && status != "deleted"
            && parse_status(status).is_none()
        {
            return Ok(invalid(
                self.name(),
                format!(
                    "status must be pending, in_progress, completed or deleted, not {status:?}"
                ),
            ));
        }

        let outcome = {
            let mut board = ctx.board.lock().expect("task board lock poisoned");
            apply_update(&mut board, id, &args)
        };
        match outcome {
            Err(answer) => Ok(ToolOutput::ok(answer)),
            Ok((todos, summary)) => {
                mirror(ctx, todos).await;
                Ok(ToolOutput::ok(summary))
            }
        }
    }
}

/// Apply a `TaskUpdate` to the board: the new `todo` list and the result line,
/// or a plain answer when the task or a task it names is not there.
fn apply_update(
    board: &mut TaskBoard,
    id: u32,
    args: &Value,
) -> Result<(Vec<TodoItem>, String), String> {
    if board.get(id).is_none() {
        return Err("Task not found".to_string());
    }
    let status_arg = str_arg(args, "status");
    if status_arg == Some("deleted") {
        board.delete(id);
        return Ok((board.as_todos(), format!("Updated task #{id} deleted")));
    }
    let blocks = ids(args, "addBlocks");
    let blocked_by = ids(args, "addBlockedBy");
    if let Some(other) = blocks
        .iter()
        .chain(&blocked_by)
        .find(|other| board.get(**other).is_none())
    {
        return Err(format!("Task #{other} not found"));
    }
    let mut updated: Vec<&str> = Vec::new();
    let task = board.get_mut(id).expect("checked above");
    if let Some(subject) = str_arg(args, "subject") {
        task.subject = subject.to_string();
        updated.push("subject");
    }
    if let Some(description) = str_arg(args, "description") {
        task.description = description.to_string();
        updated.push("description");
    }
    if let Some(form) = str_arg(args, "activeForm") {
        task.active_form = Some(form.to_string());
        updated.push("activeForm");
    }
    if let Some(status) = status_arg.and_then(parse_status) {
        task.status = status;
        updated.push("status");
    }
    if let Some(owner) = str_arg(args, "owner") {
        task.owner = Some(owner.to_string());
        updated.push("owner");
    }
    if let Some(metadata) = args.get("metadata").and_then(Value::as_object) {
        for (key, value) in metadata {
            if value.is_null() {
                task.metadata.remove(key);
            } else {
                task.metadata.insert(key.clone(), value.clone());
            }
        }
        updated.push("metadata");
    }
    if !blocks.is_empty() {
        for blocked in &blocks {
            board.link(id, *blocked);
        }
        updated.push("blocks");
    }
    if !blocked_by.is_empty() {
        for blocker in &blocked_by {
            board.link(*blocker, id);
        }
        updated.push("blockedBy");
    }
    Ok((
        board.as_todos(),
        format!("Updated task #{id} {}", updated.join(", ")),
    ))
}

/// `TaskStop`.
pub struct TaskStopTool;

/// What a `task_id` names.
#[derive(Debug, PartialEq, Eq)]
enum Target {
    Shell(u32),
    Agent(u32),
    AgentNamed(String),
}

fn parse_target(text: &str) -> Target {
    let text = text.trim();
    if let Some(rest) = text.strip_prefix("agent:") {
        return match parse_id(rest) {
            Some(id) => Target::Agent(id),
            None => Target::AgentNamed(rest.to_string()),
        };
    }
    match parse_id(text) {
        Some(id) => Target::Shell(id),
        None => Target::AgentNamed(text.to_string()),
    }
}

#[async_trait]
impl Tool for TaskStopTool {
    fn name(&self) -> &str {
        "TaskStop"
    }

    fn description(&self) -> &str {
        "Stop a running background task by id: a background shell command or Monitor (its \
         number), or a background subagent (`agent:<number>` or its name)."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "task_id": { "type": "string", "description": "The ID of the background task to stop." },
                "shell_id": { "type": "string", "description": "Deprecated: use task_id." }
            }
        })
    }

    fn access(&self) -> ToolAccess {
        ToolAccess::Execute
    }

    fn deferred_by_default(&self) -> bool {
        true
    }

    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        let Some(raw) = str_arg(&args, "task_id").or_else(|| str_arg(&args, "shell_id")) else {
            return Ok(invalid(self.name(), "Missing required parameter: task_id"));
        };
        match parse_target(raw) {
            Target::Shell(id) => {
                let Some((task, _)) = ctx.tasks.output(id, 0) else {
                    return Ok(invalid(
                        self.name(),
                        format!("No task found with ID: {raw}"),
                    ));
                };
                if task.status.is_finished() {
                    return Ok(invalid(
                        self.name(),
                        format!(
                            "Task {id} is not running (status: {})",
                            task.status.describe()
                        ),
                    ));
                }
                ctx.tasks.kill(id);
                Ok(ToolOutput::ok(format!(
                    "Successfully stopped task: {id} ({})",
                    task.command
                )))
            }
            target => {
                let snapshots = ctx.subagents.list();
                let found = match &target {
                    Target::Agent(id) => snapshots.iter().find(|s| s.id == *id),
                    Target::AgentNamed(name) => snapshots
                        .iter()
                        .rev()
                        .find(|s| &s.name == name && s.result.is_none()),
                    Target::Shell(_) => None,
                };
                let Some(agent) = found else {
                    return Ok(invalid(
                        self.name(),
                        format!("No task found with ID: {raw}"),
                    ));
                };
                if agent.result.is_some() {
                    return Ok(invalid(
                        self.name(),
                        format!("Task {raw} is not running (status: {})", agent.describe()),
                    ));
                }
                ctx.subagents.kill(agent.id);
                Ok(ToolOutput::ok(format!(
                    "Successfully stopped task: agent:{} ({})",
                    agent.id, agent.task
                )))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> ToolContext {
        ToolContext::new(std::env::temp_dir())
    }

    async fn call(tool: &dyn Tool, ctx: &ToolContext, args: Value) -> ToolOutput {
        tool.execute(args, ctx).await.unwrap()
    }

    /// The calls and answers of Claude Code 2.1.314 (`CLAUDE_CODE_ENABLE_TODO_TOOLS=1`, driven
    /// through `claude -p` against a fake Messages server), one pair per step.
    #[tokio::test]
    async fn the_answers_match_claude_code_2_1_314_step_by_step() {
        let ctx = ctx();
        let steps: Vec<(&str, Value, &str)> = vec![
            (
                "TaskCreate",
                json!({"subject":"Write the parser","description":"Parse the input","activeForm":"Writing the parser"}),
                "Task #1 created successfully: Write the parser",
            ),
            (
                "TaskCreate",
                json!({"subject":"Add tests","description":"Cover the parser","metadata":{"area":"tests"}}),
                "Task #2 created successfully: Add tests",
            ),
            (
                "TaskList",
                json!({}),
                "#1 [pending] Write the parser\n#2 [pending] Add tests",
            ),
            (
                "TaskGet",
                json!({"taskId":"1"}),
                "Task #1: Write the parser\nStatus: pending\nDescription: Parse the input",
            ),
            (
                "TaskUpdate",
                json!({"taskId":"1","status":"in_progress"}),
                "Updated task #1 status",
            ),
            (
                "TaskUpdate",
                json!({"taskId":"2","addBlockedBy":["1"],"owner":"bob"}),
                "Updated task #2 owner, blockedBy",
            ),
            (
                "TaskList",
                json!({}),
                "#1 [in_progress] Write the parser\n#2 [pending] Add tests (bob) [blocked by #1]",
            ),
            (
                "TaskGet",
                json!({"taskId":"2"}),
                "Task #2: Add tests\nStatus: pending\nDescription: Cover the parser\nBlocked by: #1",
            ),
            (
                "TaskUpdate",
                json!({"taskId":"1","status":"completed"}),
                "Updated task #1 status",
            ),
            (
                "TaskList",
                json!({}),
                "#1 [completed] Write the parser\n#2 [pending] Add tests (bob)",
            ),
            ("TaskGet", json!({"taskId":"99"}), "Task not found"),
            (
                "TaskUpdate",
                json!({"taskId":"99","status":"completed"}),
                "Task not found",
            ),
            (
                "TaskUpdate",
                json!({"taskId":"1","subject":"Write the lexer","metadata":{"area":null,"k":"v"}}),
                "Updated task #1 subject, metadata",
            ),
            (
                "TaskGet",
                json!({"taskId":"1"}),
                "Task #1: Write the lexer\nStatus: completed\nDescription: Parse the input\nBlocks: #2",
            ),
            (
                "TaskUpdate",
                json!({"taskId":"2","status":"deleted"}),
                "Updated task #2 deleted",
            ),
            ("TaskList", json!({}), "#1 [completed] Write the lexer"),
            ("TaskUpdate", json!({"taskId":"1"}), "Updated task #1 "),
            (
                "TaskCreate",
                json!({"subject":"","description":"x"}),
                "Task #3 created successfully: ",
            ),
        ];
        let tools = tools();
        for (name, args, want) in steps {
            let tool = tools.iter().find(|t| t.name() == name).expect(name);
            let got = call(tool.as_ref(), &ctx, args.clone()).await;
            assert_eq!(got.content, want, "{name} {args}");
        }
    }

    #[tokio::test]
    async fn create_get_list_follow_claude_codes_wording() {
        let ctx = ctx();
        let made = call(
            &TaskCreateTool,
            &ctx,
            json!({ "subject": "Write tests", "description": "cover the parser" }),
        )
        .await;
        assert_eq!(made.content, "Task #1 created successfully: Write tests");
        call(
            &TaskCreateTool,
            &ctx,
            json!({ "subject": "Ship", "description": "release it" }),
        )
        .await;

        let got = call(&TaskGetTool, &ctx, json!({ "taskId": "1" })).await;
        assert_eq!(
            got.content,
            "Task #1: Write tests\nStatus: pending\nDescription: cover the parser"
        );
        let missing = call(&TaskGetTool, &ctx, json!({ "taskId": "9" })).await;
        assert_eq!(missing.content, "Task not found");

        let listed = call(&TaskListTool, &ctx, json!({})).await;
        assert_eq!(
            listed.content,
            "#1 [pending] Write tests\n#2 [pending] Ship"
        );
    }

    #[tokio::test]
    async fn an_empty_board_says_so() {
        assert_eq!(
            call(&TaskListTool, &ctx(), json!({})).await.content,
            "No tasks found"
        );
    }

    #[tokio::test]
    async fn update_changes_fields_blocks_and_mirrors_into_todos() {
        let ctx = ctx();
        for subject in ["a", "b"] {
            call(
                &TaskCreateTool,
                &ctx,
                json!({ "subject": subject, "description": "d" }),
            )
            .await;
        }
        let out = call(
            &TaskUpdateTool,
            &ctx,
            json!({ "taskId": "2", "status": "in_progress", "addBlockedBy": ["1"], "owner": "me" }),
        )
        .await;
        assert_eq!(out.content, "Updated task #2 status, owner, blockedBy");

        let listed = call(&TaskListTool, &ctx, json!({})).await;
        assert_eq!(
            listed.content,
            "#1 [pending] a\n#2 [in_progress] b (me) [blocked by #1]"
        );
        let got = call(&TaskGetTool, &ctx, json!({ "taskId": "1" })).await;
        assert!(got.content.ends_with("Blocks: #2"), "{}", got.content);

        let todos = ctx.todos.lock().unwrap().clone();
        assert_eq!(todos.len(), 2);
        assert_eq!(todos[1].status, TodoStatus::InProgress);

        // A finished blocker stops blocking.
        call(
            &TaskUpdateTool,
            &ctx,
            json!({ "taskId": "1", "status": "completed" }),
        )
        .await;
        let listed = call(&TaskListTool, &ctx, json!({})).await;
        assert!(!listed.content.contains("blocked by"), "{}", listed.content);
    }

    #[tokio::test]
    async fn deleting_removes_the_task_and_its_edges() {
        let ctx = ctx();
        for subject in ["a", "b"] {
            call(
                &TaskCreateTool,
                &ctx,
                json!({ "subject": subject, "description": "d" }),
            )
            .await;
        }
        call(
            &TaskUpdateTool,
            &ctx,
            json!({ "taskId": "2", "addBlockedBy": ["1"] }),
        )
        .await;
        let out = call(
            &TaskUpdateTool,
            &ctx,
            json!({ "taskId": "1", "status": "deleted" }),
        )
        .await;
        assert_eq!(out.content, "Updated task #1 deleted");
        let got = call(&TaskGetTool, &ctx, json!({ "taskId": "2" })).await;
        assert!(!got.content.contains("Blocked by"));
        assert_eq!(ctx.todos.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn updating_a_missing_task_is_a_plain_answer_not_an_error() {
        let out = call(
            &TaskUpdateTool,
            &ctx(),
            json!({ "taskId": "7", "status": "completed" }),
        )
        .await;
        assert!(!out.is_error);
        assert_eq!(out.content, "Task not found");
    }

    #[test]
    fn task_stop_targets() {
        assert_eq!(parse_target("3"), Target::Shell(3));
        assert_eq!(parse_target("#3"), Target::Shell(3));
        assert_eq!(parse_target("agent:2"), Target::Agent(2));
        assert_eq!(parse_target("worker"), Target::AgentNamed("worker".into()));
    }

    #[tokio::test]
    async fn task_stop_stops_a_running_shell_task_and_reports_the_rest() {
        let ctx = ctx();
        let id = ctx.tasks.add("sleep 100");

        let missing = call(&TaskStopTool, &ctx, json!({ "task_id": "999" })).await;
        assert_eq!(
            missing.content,
            "Tool TaskStop input is invalid: No task found with ID: 999"
        );
        let none = call(&TaskStopTool, &ctx, json!({})).await;
        assert_eq!(
            none.content,
            "Tool TaskStop input is invalid: Missing required parameter: task_id"
        );

        let child = tokio::process::Command::new("sleep")
            .arg("30")
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let live = ctx.tasks.spawn("sleep 30", child);
        let out = call(&TaskStopTool, &ctx, json!({ "task_id": live.to_string() })).await;
        assert_eq!(
            out.content,
            format!("Successfully stopped task: {live} (sleep 30)")
        );
        for _ in 0..100 {
            if ctx.tasks.status(live).is_some_and(|s| s.is_finished()) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        let again = call(&TaskStopTool, &ctx, json!({ "task_id": live.to_string() })).await;
        assert!(
            again.content.contains("is not running (status: killed)"),
            "{}",
            again.content
        );
        let _ = id;
    }
}
