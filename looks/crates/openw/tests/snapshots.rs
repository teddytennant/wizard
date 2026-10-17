//! Regression snapshots of every screen openw has, at the two sizes the reference captures use.
//! Update with `UPDATE_SNAPSHOTS=1 cargo test -p openw --test snapshots` and read the diff.

mod common;

use std::time::Duration;

use agent_core::{Event, StopReason, Todo, TodoStatus, ToolKind, ToolStatus, Usage};
use common::*;
use crossterm::event::{KeyCode, KeyModifiers};

const SIZES: [(u16, u16); 2] = [(120, 36), (150, 42)];

fn each(name: &str, f: impl Fn(&mut Harness)) {
    for (w, h) in SIZES {
        let mut hn = Harness::new(w, h);
        f(&mut hn);
        assert_snapshot(&format!("{name}-{w}x{h}"), &hn.text());
    }
}

fn press(h: &mut Harness, code: KeyCode, mods: KeyModifiers) {
    h.key(code, mods);
}

fn tools_turn(h: &mut Harness) {
    h.app.send_prompt("tools please".into());
    h.event(Event::TurnStart);
    h.event(Event::TextDelta("I'll look around first.".into()));
    let b = tool("1", "execute", ToolKind::Execute, "ls -la", serde_json::json!({"command": "ls -la", "description": "List files"}), Some("total 12\ndrwxr-xr-x 3 me me 4096 Oct  5 13:15 .\n-rw-r--r-- 1 me me   21 Oct  5 13:15 README.md\ndrwxr-xr-x 2 me me 4096 Oct  5 13:15 src"));
    h.event(Event::Tool(b));
    h.event(Event::Tool(tool(
        "2",
        "read_file",
        ToolKind::Read,
        "README.md",
        serde_json::json!({"path": "README.md"}),
        Some("# demo\nhello"),
    )));
    h.event(Event::Tool(tool(
        "3",
        "search_files",
        ToolKind::Search,
        "println",
        serde_json::json!({"pattern": "println"}),
        Some("src/main.rs:2"),
    )));
    let mut e = tool(
        "4",
        "edit_file",
        ToolKind::Edit,
        "src/main.rs",
        serde_json::json!({"path": "src/main.rs"}),
        Some("edited"),
    );
    e.diff = Some(agent_core::FileDiff {
        path: "src/main.rs".into(),
        old: Some("fn main() {\n    println!(\"hi\");\n}\n".into()),
        new: "fn main() {\n    let name = \"world\";\n    println!(\"hello, {name}\");\n}\n".into(),
    });
    h.event(Event::Tool(e));
    let mut w = tool(
        "5",
        "write_file",
        ToolKind::Edit,
        "notes.txt",
        serde_json::json!({"path": "notes.txt"}),
        Some("written"),
    );
    w.diff = Some(agent_core::FileDiff {
        path: "notes.txt".into(),
        old: None,
        new: "todo: add tests\n".into(),
    });
    h.event(Event::Tool(w));
    h.event(Event::Todos(vec![
        Todo {
            text: "Read the code".into(),
            status: TodoStatus::Completed,
        },
        Todo {
            text: "Fix greeting".into(),
            status: TodoStatus::InProgress,
        },
    ]));
    h.event(Event::TextDelta(
        "Done. I changed the greeting in `src/main.rs`.".into(),
    ));
    h.event(Event::Usage(Usage {
        input_tokens: 11_200,
        output_tokens: 640,
        context_tokens: 11_840,
        context_window: 200_000,
        cost_usd: Some(0.0412),
        ..Default::default()
    }));
    h.event(Event::TurnEnd(StopReason::EndTurn));
    h.fix_durations();
}

#[test]
fn home() {
    each("home", |_| {});
}

#[test]
fn home_with_sessions_shows_a_tip() {
    each("home-tip", |h| h.sessions(2));
}

#[test]
fn home_typed() {
    each("home-typed", |h| h.type_str("hello"));
}

#[test]
fn slash_popup() {
    each("slash", |h| h.type_str("/"));
    each("slash-filtered", |h| h.type_str("/mo"));
}

#[test]
fn at_popup() {
    each("at-files", |h| {
        h.app.files = vec![
            "README.md".into(),
            "notes.md".into(),
            "src/main.rs".into(),
            "src/lib.rs".into(),
        ];
        h.type_str("look at @R");
    });
}

#[test]
fn plan_agent() {
    each("plan", |h| press(h, KeyCode::Tab, KeyModifiers::NONE));
}

#[test]
fn leader_pending() {
    each("leader", |h| {
        h.type_str("abc");
        press(h, KeyCode::Char('x'), KeyModifiers::CONTROL);
    });
}

#[test]
fn paste_summary() {
    each("paste", |h| h.paste("one\ntwo\nthree\nfour"));
}

#[test]
fn shell_mode() {
    each("shell", |h| h.type_str("!"));
}

#[test]
fn turn_with_tools() {
    each("turn-tools", tools_turn);
}

#[test]
fn turn_long_markdown() {
    const LONG: &str = "# Plan\n\nHere is what I found in **`src/`**, with a few *notes*.\n\n## Findings\n\n1. `main.rs` prints a greeting\n2. There is no test directory\n   - add `tests/smoke.rs`\n3. [Docs](https://example.com/docs) are missing\n\n> Keep the change small.\n\n| file | lines |\n|------|------:|\n| src/main.rs | 3 |\n\n```rust\nfn main() {\n    println!(\"hello\");\n}\n```\n\n---\n\nThat is all.";
    each("turn-long", |h| h.turn("long answer", LONG));
}

#[test]
fn turn_with_thinking() {
    each("turn-think", |h| {
        h.app.send_prompt("think about it".into());
        h.event(Event::TurnStart);
        h.event(Event::ThoughtDelta(
            "**Reading the parser**\nThe lookahead consumes one token too many.".into(),
        ));
        h.event(Event::TextDelta("The bug is in the lookahead.".into()));
        h.event(Event::TurnEnd(StopReason::EndTurn));
        h.fix_durations();
    });
    each("turn-think-shown", |h| {
        h.app.send_prompt("think about it".into());
        h.event(Event::TurnStart);
        h.event(Event::ThoughtDelta(
            "**Reading the parser**\nThe lookahead consumes one token too many.".into(),
        ));
        h.event(Event::TextDelta("The bug is in the lookahead.".into()));
        h.event(Event::TurnEnd(StopReason::EndTurn));
        h.fix_durations();
        h.app.run_action(openw::keys::Action::ToggleThinking);
    });
}

#[test]
fn busy_turn() {
    each("busy", |h| {
        h.app.send_prompt("slow please".into());
        h.event(Event::TurnStart);
        h.event(Event::TextDelta("Working through item 0 of 40. ".into()));
    });
    each("busy-esc-once", |h| {
        h.app.send_prompt("slow please".into());
        h.event(Event::TurnStart);
        h.event(Event::TextDelta("Working through item 0 of 40. ".into()));
        press(h, KeyCode::Esc, KeyModifiers::NONE);
    });
}

#[test]
fn interrupted_turn() {
    each("interrupted", |h| {
        h.app.send_prompt("slow please".into());
        h.event(Event::TurnStart);
        h.event(Event::TextDelta("Working".into()));
        h.event(Event::TurnEnd(StopReason::Cancelled));
        h.fix_durations();
    });
}

#[test]
fn error_turn() {
    each("error", |h| {
        h.app.send_prompt("error please".into());
        h.event(Event::TurnStart);
        let mut c = tool(
            "e1",
            "execute",
            ToolKind::Execute,
            "cargo test",
            serde_json::json!({"command": "cargo test"}),
            Some("error[E0425]: cannot find value `x` in this scope\n --> src/main.rs:3:5"),
        );
        c.status = ToolStatus::Failed;
        h.event(Event::Tool(c));
        h.event(Event::TextDelta(
            "The build failed on an undefined `x`.".into(),
        ));
        h.event(Event::Notice {
            level: agent_core::NoticeLevel::Error,
            text: "provider returned 500".into(),
        });
        h.event(Event::TurnEnd(StopReason::Error));
        h.fix_durations();
    });
}

#[test]
fn queued_message() {
    each("queued", |h| {
        h.app.send_prompt("first".into());
        h.event(Event::TurnStart);
        h.event(Event::TextDelta("working".into()));
        h.app.send_prompt("second".into());
    });
}

#[test]
fn toast() {
    each("toast", |h| {
        h.app.toast(
            tuikit::theme::Variant::Success,
            "Message copied to clipboard!",
        )
    });
}

#[test]
fn wide_session_has_a_sidebar() {
    let mut h = Harness::new(150, 42);
    h.turn("hi", "Hello there.");
    assert_snapshot("sidebar-150x42", &h.text());
}

#[test]
fn scrolled_up() {
    let mut h = Harness::new(120, 36);
    for i in 0..6 {
        h.turn(
            &format!("question {i}"),
            &format!("answer {i} with some words to fill the line"),
        );
    }
    h.app.scroll.scroll_by(-10);
    h.app.frozen = Some(Duration::ZERO);
    assert_snapshot("scrolled-120x36", &h.text());
}

#[test]
fn tool_variety() {
    each("tools-variety", |h| {
        h.app.send_prompt("everything".into());
        h.event(Event::TurnStart);
        let mut pending = tool(
            "p1",
            "read_file",
            ToolKind::Read,
            "",
            serde_json::json!({}),
            None,
        );
        pending.status = ToolStatus::Pending;
        h.event(Event::Tool(pending));
        let mut running = tool(
            "p2",
            "read_file",
            ToolKind::Read,
            "src/lib.rs",
            serde_json::json!({"path": "src/lib.rs", "offset": 10}),
            None,
        );
        running.status = ToolStatus::Running;
        h.event(Event::Tool(running));
        let mut failed = tool(
            "p3",
            "read_file",
            ToolKind::Read,
            "missing.rs",
            serde_json::json!({"path": "missing.rs"}),
            Some("No such file or directory"),
        );
        failed.status = ToolStatus::Failed;
        h.event(Event::Tool(failed));
        h.event(Event::Tool(tool(
            "p4",
            "web_fetch",
            ToolKind::Fetch,
            "https://example.com",
            serde_json::json!({"url": "https://example.com"}),
            Some("ok"),
        )));
        h.event(Event::Tool(tool(
            "p5",
            "glob",
            ToolKind::Search,
            "**/*.rs",
            serde_json::json!({"pattern": "**/*.rs", "path": "src"}),
            Some("a.rs\nb.rs\nc.rs"),
        )));
        h.event(Event::Tool(tool(
            "p6",
            "mystery_tool",
            ToolKind::Other,
            "",
            serde_json::json!({"depth": 3, "name": "x"}),
            Some("generic output"),
        )));
        let todos = serde_json::json!({"todos": [
            {"content": "Read the code", "status": "completed"},
            {"content": "Fix greeting", "status": "in_progress"},
            {"content": "Add tests", "status": "pending"}]});
        h.event(Event::Tool(tool(
            "p7",
            "todo_write",
            ToolKind::Other,
            "",
            todos,
            Some("ok"),
        )));
        let long: String = (1..=14).map(|i| format!("line {i}\n")).collect();
        h.event(Event::Tool(tool(
            "p8",
            "execute",
            ToolKind::Execute,
            "seq 14",
            serde_json::json!({"command": "seq 14"}),
            Some(&long),
        )));
        let mut task = tool(
            "t1",
            "task",
            ToolKind::Other,
            "explore: find the parser",
            serde_json::json!({"description": "find the parser", "subagent_type": "explore"}),
            None,
        );
        task.status = ToolStatus::Completed;
        h.event(Event::Tool(task));
        let mut kid = tool(
            "t2",
            "read_file",
            ToolKind::Read,
            "src/parser.rs",
            serde_json::json!({}),
            Some("x"),
        );
        kid.parent_id = Some("t1".into());
        h.event(Event::Tool(kid));
        h.event(Event::TurnEnd(StopReason::EndTurn));
        h.fix_durations();
    });
}

#[test]
fn bash_output_expands_on_click() {
    let mut h = Harness::new(120, 36);
    h.app.send_prompt("go".into());
    h.event(Event::TurnStart);
    let long: String = (1..=14).map(|i| format!("line {i}\n")).collect();
    h.event(Event::Tool(tool(
        "b1",
        "execute",
        ToolKind::Execute,
        "seq 14",
        serde_json::json!({"command": "seq 14"}),
        Some(&long),
    )));
    h.event(Event::TurnEnd(StopReason::EndTurn));
    h.fix_durations();
    let before = h.text();
    assert!(before.contains("Click to expand") && !before.contains("line 14"));
    let row = before
        .lines()
        .position(|l| l.contains("Click to expand"))
        .unwrap() as u16;
    // opencode toggles on mouse up, so a drag over a block can select text instead
    for kind in [
        crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left),
        crossterm::event::MouseEventKind::Up(crossterm::event::MouseButton::Left),
    ] {
        h.app.on_mouse(crossterm::event::MouseEvent {
            kind,
            column: 6,
            row,
            modifiers: KeyModifiers::NONE,
        });
    }
    let after = h.text();
    assert!(
        after.contains("Click to collapse") && after.contains("line 14"),
        "{after}"
    );
}
