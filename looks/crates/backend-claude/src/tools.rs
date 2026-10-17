//! Tool-call mapping shared by the live stream ([`crate::mapper`]) and the
//! transcript reader ([`crate::history`]).

use agent_core::{FileDiff, ToolCall, ToolKind, ToolStatus};
use serde_json::Value;
use std::path::Path;

/// Tool output longer than this is cut; frontends render a few screens at most.
pub(crate) const OUTPUT_CAP: usize = 20_000;

pub(crate) fn kind_of(name: &str) -> ToolKind {
    match name {
        "Bash" | "BashOutput" | "KillShell" | "KillBash" | "PowerShell" | "Monitor" => {
            ToolKind::Execute
        }
        "Read" | "NotebookRead" | "LS" => ToolKind::Read,
        "Edit" | "MultiEdit" | "Write" | "NotebookEdit" => ToolKind::Edit,
        "Glob" | "Grep" | "WebSearch" => ToolKind::Search,
        "WebFetch" => ToolKind::Fetch,
        // The subagent tool was called `Task` until it became `Agent`.
        "Task" | "Agent" => ToolKind::Think,
        _ => ToolKind::Other,
    }
}

fn str_of<'a>(v: &'a Value, k: &str) -> Option<&'a str> {
    v.get(k).and_then(Value::as_str)
}

fn rel(path: &str, cwd: &Path) -> String {
    match Path::new(path).strip_prefix(cwd) {
        Ok(p) if !p.as_os_str().is_empty() => p.to_string_lossy().into_owned(),
        _ => path.to_string(),
    }
}

fn or_else(a: &str, b: &str) -> String {
    let pick = if a.is_empty() { b } else { a };
    pick.to_string()
}

fn first_line(s: &str) -> String {
    let mut lines = s.lines().map(str::trim).filter(|l| !l.is_empty());
    let first = lines.next().unwrap_or("");
    if lines.next().is_some() {
        format!("{first} ...")
    } else {
        first.to_string()
    }
}

pub(crate) fn title_of(name: &str, input: &Value, cwd: &Path) -> String {
    let s = |k: &str| str_of(input, k).unwrap_or("");
    match name {
        "Bash" | "PowerShell" => first_line(s("command")),
        "Read" | "Edit" | "MultiEdit" | "Write" => rel(s("file_path"), cwd),
        "NotebookEdit" | "NotebookRead" => rel(s("notebook_path"), cwd),
        "Glob" | "Grep" => s("pattern").to_string(),
        "WebFetch" => s("url").to_string(),
        "WebSearch" | "ToolSearch" => s("query").to_string(),
        "Task" | "Agent" => or_else(s("description"), s("subagent_type")),
        "TaskCreate" => or_else(s("subject"), s("title")),
        "TaskUpdate" => {
            let id = str_of(input, "taskId")
                .or_else(|| str_of(input, "id"))
                .unwrap_or("");
            format!("#{id} {}", s("status")).trim().to_string()
        }
        "Skill" => s("skill").to_string(),
        "AskUserQuestion" => {
            let qs = input.get("questions").and_then(Value::as_array);
            let first = qs
                .and_then(|a| a.first())
                .and_then(|q| str_of(q, "question"))
                .unwrap_or("");
            match qs.map_or(0, Vec::len) {
                0 | 1 => first.to_string(),
                n => format!("{first} (+{} more)", n - 1),
            }
        }
        "ExitPlanMode" => s("plan")
            .lines()
            .map(|l| l.trim().trim_start_matches('#').trim())
            .find(|l| !l.is_empty())
            .unwrap_or("")
            .to_string(),
        "LS" => rel(s("path"), cwd),
        n if n.starts_with("mcp__") => n.trim_start_matches("mcp__").replacen("__", ":", 1),
        _ => String::new(),
    }
}

pub(crate) fn diff_of(name: &str, input: &Value) -> Option<FileDiff> {
    let s = |k: &str| str_of(input, k).map(str::to_string);
    match name {
        "Edit" => Some(FileDiff {
            path: s("file_path")?,
            old: s("old_string"),
            new: s("new_string").unwrap_or_default(),
        }),
        "Write" => Some(FileDiff {
            path: s("file_path")?,
            old: None,
            new: s("content").unwrap_or_default(),
        }),
        "MultiEdit" => {
            let edits = input.get("edits")?.as_array()?;
            let olds: Vec<&str> = edits
                .iter()
                .filter_map(|e| str_of(e, "old_string"))
                .collect();
            let news: Vec<&str> = edits
                .iter()
                .filter_map(|e| str_of(e, "new_string"))
                .collect();
            Some(FileDiff {
                path: s("file_path")?,
                old: Some(olds.join("\n")),
                new: news.join("\n"),
            })
        }
        "NotebookEdit" => Some(FileDiff {
            path: s("notebook_path")?,
            old: None,
            new: s("new_source").unwrap_or_default(),
        }),
        _ => None,
    }
}

pub(crate) fn new_call(
    id: &str,
    name: &str,
    input: Value,
    cwd: &Path,
    parent: Option<&str>,
) -> ToolCall {
    ToolCall {
        id: id.to_string(),
        name: name.to_string(),
        kind: kind_of(name),
        title: title_of(name, &input, cwd),
        diff: diff_of(name, &input),
        input,
        status: ToolStatus::Running,
        output: None,
        parent_id: parent.map(str::to_string),
    }
}

pub(crate) fn cap(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n... (truncated)", &s[..end])
}

/// Text of a `tool_result` content field: a string, or blocks of text and images.
pub(crate) fn flatten_content(c: &Value) -> String {
    match c {
        Value::String(s) => s.clone(),
        Value::Array(blocks) => blocks
            .iter()
            .filter_map(|b| match str_of(b, "type") {
                Some("text") => str_of(b, "text").map(str::to_string),
                Some("image") => Some("[image]".to_string()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// Fold a finished `tool_result` into its call. `tur` is the sibling
/// `tool_use_result` object the CLI attaches (`Null` in old transcripts).
pub(crate) fn apply_result(tool: &mut ToolCall, text: &str, is_error: bool, tur: &Value) {
    let text = text
        .replace("<tool_use_error>", "")
        .replace("</tool_use_error>", "");
    let text = text.trim();
    if is_error {
        tool.status = ToolStatus::Failed;
        tool.output = Some(cap(text, OUTPUT_CAP));
        return;
    }
    if (tool.name == "Agent" || tool.name == "Task")
        && tur.get("isAsync").and_then(Value::as_bool) == Some(true)
    {
        tool.status = ToolStatus::Running;
        tool.output = Some("running in the background".to_string());
        return;
    }
    tool.status = ToolStatus::Completed;
    let mut out = text.to_string();
    if tool.name == "Agent" || tool.name == "Task" {
        // The plain `content` is the subagent's report; the tool_result text wraps it in harness framing.
        if let Some(blocks) = tur.get("content") {
            let report = flatten_content(blocks);
            if !report.is_empty() {
                out = report;
            }
        }
    }
    if tool.name == "Write" && str_of(tur, "type") == Some("update") {
        if let (Some(orig), Some(d)) = (str_of(tur, "originalFile"), tool.diff.as_mut()) {
            d.old = Some(orig.to_string());
        }
    }
    tool.output = Some(cap(&out, OUTPUT_CAP));
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn titles() {
        let cwd = Path::new("/work/proj");
        assert_eq!(
            title_of("Read", &json!({"file_path": "/work/proj/src/a.rs"}), cwd),
            "src/a.rs"
        );
        assert_eq!(
            title_of("Read", &json!({"file_path": "/etc/hosts"}), cwd),
            "/etc/hosts"
        );
        assert_eq!(
            title_of("Bash", &json!({"command": "ls -la\necho hi"}), cwd),
            "ls -la ..."
        );
        assert_eq!(
            title_of("Grep", &json!({"pattern": "fn main"}), cwd),
            "fn main"
        );
        assert_eq!(
            title_of("mcp__brave__browser_click", &json!({}), cwd),
            "brave:browser_click"
        );
    }

    #[test]
    fn question_and_plan_titles() {
        let cwd = Path::new("/w");
        let q = json!({"questions": [{"question": "Which one?"}, {"question": "And?"}]});
        assert_eq!(title_of("AskUserQuestion", &q, cwd), "Which one? (+1 more)");
        assert_eq!(
            title_of(
                "ExitPlanMode",
                &json!({"plan": "\n## Add the flag\n- x"}),
                cwd
            ),
            "Add the flag"
        );
    }

    #[test]
    fn multiedit_diff_joins_edits() {
        let d = diff_of("MultiEdit", &json!({"file_path": "/a", "edits": [{"old_string": "a", "new_string": "b"}, {"old_string": "c", "new_string": "d"}]})).unwrap();
        assert_eq!((d.old.as_deref(), d.new.as_str()), (Some("a\nc"), "b\nd"));
    }

    #[test]
    fn cap_respects_char_boundaries() {
        let s = "é".repeat(10);
        assert!(cap(&s, 5).starts_with("éé"));
    }
}
