// OWNER: tools (per-tool renderers)
//! Tool calls as transcript blocks, after opencode's `routes/session/index.tsx`: one-line
//! "inline" rows for most tools, boxed "block" renderings for shell output, edits, writes and
//! todos. [`classify`] maps a backend's tool names onto opencode's tool vocabulary; the
//! per-tool renderers read opencode's argument names first and then wizard's, so a call looks
//! the same whichever backend sent it. See `docs/openw-spec.md` 3.6 and 3.7.

mod files;
mod misc;
mod rows;
mod shell;
pub mod text;

use agent_core::{ToolCall, ToolKind, ToolStatus};
use ratatui::style::Style;
use ratatui::text::Span;

pub use text::{collapse, fmt_path, input_args, titlecase};

/// Empty the per-call caches of this thread. They are keyed by call id and would otherwise keep
/// every call of every session the process has shown.
pub fn reset_caches() {
    text::reset_clock();
    files::reset_expanded();
}

use super::session::{Block, Gap, RenderCx};
use rows::{Inline, Phase};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tool {
    Bash,
    Read,
    Glob,
    Grep,
    List,
    Edit,
    Write,
    ApplyPatch,
    WebFetch,
    WebSearch,
    Task,
    Todo,
    Question,
    Skill,
    Generic,
}

/// Map a backend call onto opencode's tool set. Names decide first (wizard's `read_file`,
/// opencode's `read`, Claude's `Read` all land on [`Tool::Read`]); the ACP `kind` is the
/// fallback for a name nobody here knows.
pub fn classify(c: &ToolCall) -> Tool {
    let n = c.name.to_lowercase();
    match n.as_str() {
        "execute" | "bash" | "shell" | "run_command" | "git_status" | "git_diff" => {
            return Tool::Bash
        }
        "read_file" | "read" => return Tool::Read,
        "write_file" | "write" => return Tool::Write,
        "edit_file" | "edit" | "multiedit" | "str_replace" => return Tool::Edit,
        "apply_patch" | "patch" => return Tool::ApplyPatch,
        "search_files" | "grep" => return Tool::Grep,
        "glob" | "find_files" => return Tool::Glob,
        "list_files" | "list" | "ls" => {
            return if text::str_in(&c.input, &["glob"]).is_some() {
                Tool::Glob
            } else {
                Tool::List
            };
        }
        "web_fetch" | "webfetch" | "fetch" => return Tool::WebFetch,
        "web_search" | "websearch" | "x_search" | "codesearch" => return Tool::WebSearch,
        "todo" | "todowrite" | "todo_write" => return Tool::Todo,
        "task" | "spawn_subagent" | "subagent" | "agent" => return Tool::Task,
        "question" => return Tool::Question,
        "skill" => return Tool::Skill,
        _ => {}
    }
    if c.title.starts_with("task:") {
        return Tool::Task;
    }
    match c.kind {
        ToolKind::Execute => Tool::Bash,
        ToolKind::Read => Tool::Read,
        ToolKind::Search => Tool::Grep,
        ToolKind::Edit => {
            if c.diff.as_ref().is_some_and(|d| d.old.is_none()) {
                Tool::Write
            } else {
                Tool::Edit
            }
        }
        ToolKind::Fetch => Tool::WebFetch,
        _ => Tool::Generic,
    }
}

/// opencode's name for a tool, as the subagent progress row prints it (`↳ Read src/a.rs`).
pub fn display_name(c: &ToolCall) -> String {
    match classify(c) {
        Tool::Bash => "Bash".into(),
        Tool::Read => "Read".into(),
        Tool::Glob => "Glob".into(),
        Tool::Grep => "Grep".into(),
        Tool::List => "List".into(),
        Tool::Edit => "Edit".into(),
        Tool::Write => "Write".into(),
        Tool::ApplyPatch => "Apply_patch".into(),
        Tool::WebFetch => "Webfetch".into(),
        Tool::WebSearch => "Websearch".into(),
        Tool::Task => "Task".into(),
        Tool::Todo => "Todowrite".into(),
        Tool::Question => "Question".into(),
        Tool::Skill => "Skill".into(),
        Tool::Generic => titlecase(&c.name),
    }
}

/// Failed because someone said no, not because the tool broke.
pub fn denied(c: &ToolCall) -> bool {
    if c.status != ToolStatus::Failed {
        return false;
    }
    let o = c.output.as_deref().unwrap_or("").to_lowercase();
    [
        "rejected permission",
        "specified a rule",
        "user dismissed",
        "questionrejected",
        "user denied",
        "denied this tool",
        "permission denied by",
    ]
    .iter()
    .any(|k| o.contains(k))
}

/// Shared by the inline renderers: the error text a click opens.
fn error_of(c: &ToolCall) -> Option<String> {
    c.output
        .as_deref()
        .map(text::clean_output)
        .filter(|s| !s.is_empty())
}

/// Status to row phase. `complete` is opencode's `complete` prop: the key argument is known.
fn phase(c: &ToolCall, complete: bool) -> Phase {
    match c.status {
        ToolStatus::Failed if denied(c) => Phase::Denied,
        ToolStatus::Failed => Phase::Failed,
        _ if complete => Phase::Ready,
        _ => Phase::Pending,
    }
}

/// An inline row for `c` with the status-derived fields filled in.
fn inline(
    c: &ToolCall,
    cx: &RenderCx,
    icon: &str,
    text: String,
    pending: &'static str,
    complete: bool,
) -> Inline {
    let mut i = Inline::new(icon, text, pending);
    i.phase = phase(c, complete);
    i.complete = complete;
    let err = error_of(c);
    i.error_open = cx.expanded.contains(&c.id);
    i.click = err.as_ref().map(|_| c.id.clone());
    i.error = err;
    i
}

/// Blocks for one tool call. `children` are the calls a subagent made.
pub fn render(c: &ToolCall, children: &[&ToolCall], cx: &RenderCx) -> Vec<Block> {
    // Completed calls disappear with the "tool details" toggle off; running, pending and
    // failed ones stay.
    if !cx.flags.tool_details && c.status == ToolStatus::Completed {
        return Vec::new();
    }
    match classify(c) {
        Tool::Bash => shell::bash(c, cx),
        Tool::Read => files::read(c, cx),
        Tool::Glob | Tool::Grep | Tool::List => misc::search(c, cx),
        Tool::Write => files::write(c, cx),
        Tool::Edit => files::edit(c, cx),
        Tool::ApplyPatch => files::apply_patch(c, cx),
        Tool::WebFetch | Tool::WebSearch => misc::web(c, cx),
        Tool::Task => misc::task(c, children, cx),
        Tool::Todo => misc::todo(c, cx),
        Tool::Question => misc::question(c, cx),
        Tool::Skill => misc::skill(c, cx),
        Tool::Generic => misc::generic(c, cx),
    }
}

/// `ctrl+x down view subagents`, the row opencode adds under a step that called `task`.
/// `paddingTop 1` inside the box, so the blank row is part of it and no margin is added.
pub fn subagent_hint(cx: &RenderCx) -> Block {
    let t = cx.theme;
    let line = ratatui::text::Line::from(vec![
        Span::raw("   "),
        Span::styled("ctrl+x down", Style::new().fg(t.text)),
        Span::styled(" view subagents", Style::new().fg(t.text_muted)),
    ]);
    Block {
        lines: vec![ratatui::text::Line::default(), line].into(),
        gap: Gap::Zero,
        always_sep: false,
        click: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(name: &str, kind: ToolKind) -> ToolCall {
        ToolCall {
            id: "1".into(),
            name: name.into(),
            kind,
            ..Default::default()
        }
    }

    #[test]
    fn classify_maps_wizard_names() {
        for (n, k, t) in [
            ("execute", ToolKind::Execute, Tool::Bash),
            ("read_file", ToolKind::Read, Tool::Read),
            ("write_file", ToolKind::Edit, Tool::Write),
            ("edit_file", ToolKind::Edit, Tool::Edit),
            ("search_files", ToolKind::Search, Tool::Grep),
            ("list_files", ToolKind::Read, Tool::List),
            ("git_status", ToolKind::Read, Tool::Bash),
            ("git_diff", ToolKind::Read, Tool::Bash),
            ("web_fetch", ToolKind::Fetch, Tool::WebFetch),
            ("web_search", ToolKind::Search, Tool::WebSearch),
            ("x_search", ToolKind::Search, Tool::WebSearch),
            ("todo", ToolKind::Other, Tool::Todo),
            ("spawn_subagent", ToolKind::Other, Tool::Task),
            ("memory", ToolKind::Other, Tool::Generic),
            ("generate_image", ToolKind::Other, Tool::Generic),
            ("github__create_issue", ToolKind::Other, Tool::Generic),
        ] {
            assert_eq!(classify(&call(n, k)), t, "{n}");
        }
        let mut l = call("list_files", ToolKind::Read);
        l.input = serde_json::json!({"path": "src", "glob": "*.rs"});
        assert_eq!(classify(&l), Tool::Glob);
    }

    #[test]
    fn unknown_names_fall_back_to_kind() {
        assert_eq!(classify(&call("Mystery", ToolKind::Execute)), Tool::Bash);
        assert_eq!(classify(&call("Mystery", ToolKind::Other)), Tool::Generic);
    }

    #[test]
    fn denial_is_not_failure() {
        let mut c = call("edit_file", ToolKind::Edit);
        c.status = ToolStatus::Failed;
        c.output = Some("The user denied this tool call.".into());
        assert!(denied(&c));
        c.output = Some("old_string not found".into());
        assert!(!denied(&c));
    }
}
