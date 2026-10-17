//! Fixtures shared by frontend tests and the `gallery` scenario.

use super::tool;
use crate::*;

/// One finished call per wizard tool, shaped like what `backend-wizard` delivers (title is
/// `name: detail`, no diffs except the ones it rebuilds from edit/write arguments). Frontends
/// use it for snapshot tests and the `gallery` scenario.
pub fn gallery_calls() -> Vec<ToolCall> {
    use serde_json::json;
    let done = ToolStatus::Completed;
    let seq: String = (1..=30)
        .map(|i| i.to_string())
        .collect::<Vec<_>>()
        .join("\n");
    let mut edit = tool(
        "g5",
        "edit_file",
        ToolKind::Edit,
        "src/main.rs",
        json!({"path": "src/main.rs", "old_string": "    println!(\"hi\");", "new_string": "    println!(\"hello\");"}),
        done,
        Some("Edited /home/me/proj/src/main.rs: replaced 1 occurrence (line 2)"),
    );
    edit.diff = Some(FileDiff {
        path: "src/main.rs".into(),
        old: Some("    println!(\"hi\");".into()),
        new: "    println!(\"hello\");".into(),
    });
    let mut write = tool(
        "g6",
        "write_file",
        ToolKind::Edit,
        "notes.txt",
        json!({"path": "notes.txt", "content": "Line one\nLine two\nLine three"}),
        done,
        Some("Created /home/me/proj/notes.txt (28 bytes)"),
    );
    write.diff = Some(FileDiff {
        path: "notes.txt".into(),
        old: None,
        new: "Line one\nLine two\nLine three".into(),
    });
    vec![
        tool("g1", "execute", ToolKind::Execute, "seq 1 30", json!({"command": "seq 1 30"}), done, Some(&seq)),
        tool("g2", "read_file", ToolKind::Read, "README.md", json!({"path": "README.md", "start_line": 2, "end_line": 3}), done, Some("     2\thello world\n     3\tthis is line three")),
        tool("g3", "list_files", ToolKind::Read, "src", json!({"path": "src", "glob": "**/*.rs"}), done, Some("src/lib.rs\nsrc/main.rs")),
        tool("g4", "search_files", ToolKind::Search, "println", json!({"pattern": "println", "path": "src"}), done, Some("src/main.rs:2:    println!(\"hi\");\nsrc/main.rs:4:    println!(\"{}\", x);")),
        edit,
        write,
        tool("g7", "web_fetch", ToolKind::Fetch, "https://example.com", json!({"url": "https://example.com"}), done, Some("Example Domain\n\nThis domain is for use in documentation examples.")),
        tool("g8", "execute", ToolKind::Execute, "ls /nonexistent_dir", json!({"command": "ls /nonexistent_dir"}), ToolStatus::Failed, Some("[stderr]\nls: cannot access '/nonexistent_dir': No such file or directory\nexit code: 2")),
        tool("g9", "web_search", ToolKind::Search, "rust ratatui layout", json!({"query": "rust ratatui layout", "count": 3}), done, Some("1. Layout | Ratatui\n   https://ratatui.rs/concepts/layout/\n   Layout is the process of dividing the terminal area.\n\n2. ratatui::layout\n   https://docs.rs/ratatui/latest/ratatui/layout/\n   Layout and constraint types.\n\n3. Ratatui layout examples\n   https://github.com/ratatui/ratatui\n   Examples.")),
        tool("g10", "x_search", ToolKind::Search, "ratatui 0.30", json!({"query": "ratatui 0.30"}), done, Some("1. @ratatui_rs: 0.30 is out\n\n2. @user: trying the new layout")),
        tool("g11", "git_status", ToolKind::Read, "git_status", json!({}), done, Some("## main\n?? notes.txt")),
        tool("g12", "git_diff", ToolKind::Read, "git_diff", json!({}), done, Some("diff --git a/src/main.rs b/src/main.rs\n--- a/src/main.rs\n+++ b/src/main.rs\n@@ -1,3 +1,3 @@\n fn main() {\n-    println!(\"hi\");\n+    println!(\"hello\");\n }")),
        tool(
            "g13",
            "todo",
            ToolKind::Other,
            "todo",
            json!({"action": "write", "items": [{"content": "Read the code", "status": "completed"}, {"content": "Fix greeting", "status": "in_progress"}, {"content": "Add tests", "status": "pending"}]}),
            done,
            Some("todo list updated: 1/3 done\n\u{2713} Read the code\n\u{25b8} Fix greeting\n\u{2610} Add tests"),
        ),
        tool("g14", "memory", ToolKind::Other, "memory", json!({"action": "save", "name": "test-style", "type": "project", "description": "Tests use insta snapshots"}), done, Some("saved memory test-style")),
        tool("g15", "generate_image", ToolKind::Other, "generate_image", json!({"prompt": "a lighthouse at dusk", "path": "generated/lighthouse.png"}), done, Some("Saved generated/lighthouse.png")),
        tool("g16", "github__create_issue", ToolKind::Other, "github__create_issue", json!({"owner": "me", "repo": "proj", "title": "Flaky parser test", "labels": ["bug"]}), done, Some("Created issue #42")),
        tool("g17", "spawn_subagent", ToolKind::Other, "spawn_subagent", json!({"subagent": "explorer", "task": "Find where the parser lives and report the file", "background": false}), done, Some("The parser lives in src/parser.rs.")),
    ]
}
