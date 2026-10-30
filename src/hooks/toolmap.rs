//! Names and argument shapes: Wizard's tools as Claude Code spells them.
//!
//! A Claude-format hook script expects `tool_name: "Bash"` and
//! `tool_input.command`; permission rules are written against `Bash(...)`,
//! `Read(...)` and `mcp__server__tool`. Wizard's tools are `execute`,
//! `read_file` and `server__tool`. This module is the one place that
//! translates, in both directions.

use serde_json::{Map, Value};

/// What kind of thing a tool call touches, for rule matching.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolClass {
    /// Runs a shell command line (`Bash(...)`).
    Shell,
    /// Reads or searches files (`Read(...)`, `Glob`, `Grep`).
    Read,
    /// Writes files (`Edit(...)`, which also covers `Write`).
    Edit,
    /// Fetches or searches the web.
    Web,
    /// A tool from an MCP server.
    Mcp,
    /// Anything else.
    Other,
}

/// Claude Code's name for a Wizard tool. MCP tools (`server__tool`) become
/// `mcp__server__tool`; a tool with no Claude counterpart keeps its name.
pub fn claude_name(tool: &str) -> String {
    let known = match tool {
        "execute" => "Bash",
        "read_file" => "Read",
        "write_file" => "Write",
        "edit_file" | "apply_patch" => "Edit",
        "list_files" => "Glob",
        "search_files" => "Grep",
        "web_fetch" => "WebFetch",
        "web_search" => "WebSearch",
        "todo" => "TodoWrite",
        "spawn_subagent" => "Agent",
        "exit_plan" => "ExitPlanMode",
        "interview" => "AskUserQuestion",
        _ => "",
    };
    if !known.is_empty() {
        return known.to_string();
    }
    if tool.contains("__") && !tool.starts_with("mcp__") {
        return format!("mcp__{tool}");
    }
    tool.to_string()
}

/// Wizard's name for a Claude tool name, when there is exactly one.
pub fn wizard_name(claude: &str) -> Option<&'static str> {
    Some(match claude {
        "Bash" => "execute",
        "Read" => "read_file",
        "Write" => "write_file",
        "Edit" => "edit_file",
        "Glob" => "list_files",
        "Grep" => "search_files",
        "WebFetch" => "web_fetch",
        "WebSearch" => "web_search",
        "TodoWrite" => "todo",
        "Agent" | "Task" => "spawn_subagent",
        "ExitPlanMode" => "exit_plan",
        "AskUserQuestion" => "interview",
        _ => return None,
    })
}

/// The tool under Wizard's own name when it was called by a Claude alias
/// (`Bash` runs `execute`), so rules see one spelling.
fn canonical(tool: &str) -> &str {
    wizard_name(tool).unwrap_or(tool)
}

/// The class of a tool, called by either spelling.
pub fn class(tool: &str) -> ToolClass {
    match canonical(tool) {
        "execute" | "PowerShell" => ToolClass::Shell,
        "read_file" | "list_files" | "search_files" => ToolClass::Read,
        "write_file" | "edit_file" | "apply_patch" | "NotebookEdit" => ToolClass::Edit,
        "web_fetch" | "web_search" | "x_search" => ToolClass::Web,
        _ if tool.contains("__") => ToolClass::Mcp,
        _ => ToolClass::Other,
    }
}

/// Tool arguments as a Claude hook script expects to read them
/// (`path` becomes `file_path` on the file tools).
pub fn to_claude_input(tool: &str, args: &Value) -> Value {
    rename_keys(tool, args, "path", "file_path")
}

/// The inverse of [`to_claude_input`], for an `updatedInput` coming back.
pub fn from_claude_input(tool: &str, input: &Value) -> Value {
    rename_keys(tool, input, "file_path", "path")
}

fn rename_keys(tool: &str, value: &Value, from: &str, to: &str) -> Value {
    let renames = matches!(
        tool,
        "read_file" | "write_file" | "edit_file" | "list_files" | "search_files"
    );
    let (Some(object), true) = (value.as_object(), renames) else {
        return value.clone();
    };
    let mut out = Map::new();
    for (key, val) in object {
        let key = if key == from { to } else { key.as_str() };
        out.insert(key.to_string(), val.clone());
    }
    Value::Object(out)
}

/// The file paths a call names, as written. `apply_patch` carries its paths
/// inside the patch text.
pub fn paths(tool: &str, args: &Value) -> Vec<String> {
    match canonical(tool) {
        "NotebookEdit" => match args.get("notebook_path") {
            Some(Value::String(path)) => vec![path.clone()],
            _ => Vec::new(),
        },
        "apply_patch" => {
            let text = ["patch", "input", "diff"]
                .iter()
                .find_map(|key| args.get(*key).and_then(Value::as_str))
                .unwrap_or("");
            text.lines()
                .filter_map(|line| {
                    ["*** Update File:", "*** Add File:", "*** Delete File:"]
                        .iter()
                        .find_map(|marker| line.strip_prefix(marker))
                })
                .map(|path| path.trim().to_string())
                .collect()
        }
        "read_file" | "write_file" | "edit_file" | "list_files" | "search_files" => {
            match args.get("path").or_else(|| args.get("file_path")) {
                Some(Value::String(path)) => vec![path.clone()],
                _ => Vec::new(),
            }
        }
        _ => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn wizard_tools_get_their_claude_names() {
        assert_eq!(claude_name("execute"), "Bash");
        assert_eq!(claude_name("read_file"), "Read");
        assert_eq!(claude_name("write_file"), "Write");
        assert_eq!(claude_name("edit_file"), "Edit");
        assert_eq!(claude_name("apply_patch"), "Edit");
        assert_eq!(claude_name("memory"), "memory", "no counterpart, kept");
    }

    #[test]
    fn mcp_tools_get_the_mcp_prefix() {
        assert_eq!(
            claude_name("github__list_issues"),
            "mcp__github__list_issues"
        );
        assert_eq!(claude_name("mcp__github__x"), "mcp__github__x");
        assert_eq!(class("github__list_issues"), ToolClass::Mcp);
    }

    #[test]
    fn claude_names_map_back() {
        assert_eq!(wizard_name("Bash"), Some("execute"));
        assert_eq!(wizard_name("Glob"), Some("list_files"));
        assert_eq!(wizard_name("Nope"), None);
    }

    #[test]
    fn path_is_file_path_on_the_way_out_and_path_on_the_way_in() {
        let args = json!({"path": "a.rs", "content": "x"});
        let out = to_claude_input("write_file", &args);
        assert_eq!(out, json!({"file_path": "a.rs", "content": "x"}));
        assert_eq!(from_claude_input("write_file", &out), args);
        let shell = json!({"command": "ls"});
        assert_eq!(to_claude_input("execute", &shell), shell);
    }

    #[test]
    fn apply_patch_paths_come_from_the_patch() {
        let args = json!({"patch": "*** Update File: a.rs\n@@\n-x\n+y\n*** Add File: b.rs\n+z\n"});
        assert_eq!(paths("apply_patch", &args), vec!["a.rs", "b.rs"]);
        assert_eq!(paths("read_file", &json!({"path": "c"})), vec!["c"]);
        assert!(paths("execute", &json!({"command": "ls"})).is_empty());
    }
}
