//! Sidebar, subagent bar and session hint row against the opencode captures in `reference/`.

mod common;

use agent_core::{Event, StopReason, Usage};
use common::*;

fn ref_rows(path: &str) -> Vec<String> {
    std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../reference")
            .join(path),
    )
    .expect("reference file")
    .lines()
    .map(|l| l.trim_end().to_string())
    .collect()
}

fn cut(s: &str, n: usize) -> String {
    s.chars().take(n).collect::<String>().trim_end().to_string()
}

#[test]
#[cfg_attr(
    not(reference_captures),
    ignore = "needs the reference/ captures, not in this repository; see build.rs"
)]
fn hint_row_clips_a_usage_text_that_does_not_fit_like_opencode() {
    let mut h = Harness::new(150, 42);
    h.app.cwd =
        "/tmp/claude-1000/-home-nixos/46dc526d-4fc6-4f47-b09e-820c7c4f2577/scratchpad/spec/proj"
            .into();
    h.app.send_prompt("hi".into());
    h.event(Event::TurnStart);
    h.event(Event::TextDelta("ok".into()));
    h.event(Event::Usage(Usage {
        context_tokens: 11_174,
        context_window: 200_000,
        cost_usd: Some(0.0412),
        ..Default::default()
    }));
    h.event(Event::TurnEnd(StopReason::EndTurn));
    let got: Vec<String> = h.text().lines().map(|l| l.trim_end().to_string()).collect();
    let want = ref_rows("extra/session-150x42-hint-cost.txt");
    let n = want.len();
    for i in [n - 3, n - 2] {
        assert_eq!(cut(&got[i], 108), cut(&want[i], 108), "row {i}");
    }
}

use agent_core::{FileDiff, Todo, TodoStatus, ToolKind};
use crossterm::event::{KeyCode, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};

fn sidebar_rows(h: &mut Harness, x0: usize) -> Vec<String> {
    h.text()
        .lines()
        .map(|l| {
            l.chars()
                .skip(x0)
                .collect::<String>()
                .trim_end()
                .to_string()
        })
        .collect()
}

fn edit(id: &str, path: &str, old: Option<&str>, new: &str) -> agent_core::ToolCall {
    let mut c = tool(
        id,
        "edit_file",
        ToolKind::Edit,
        path,
        serde_json::json!({"path": path}),
        Some("ok"),
    );
    c.diff = Some(FileDiff {
        path: path.into(),
        old: old.map(Into::into),
        new: new.into(),
    });
    c
}

#[test]
fn modified_files_come_from_the_edit_diffs_with_counts() {
    let mut h = Harness::new(150, 42);
    h.turn("go", "done");
    h.event(Event::Tool(edit(
        "e1",
        "/home/me/proj/src/main.rs",
        Some("a\nb\n"),
        "a\nc\nd\n",
    )));
    h.event(Event::Tool(edit("e2", "notes.txt", None, "one\n")));
    // a second edit of the same file adds to its row
    h.event(Event::Tool(edit(
        "e3",
        "src/main.rs",
        Some("x\n"),
        "x\ny\n",
    )));
    let rows = sidebar_rows(&mut h, 108);
    let at = rows
        .iter()
        .position(|r| r.trim() == "Modified Files")
        .expect("heading");
    assert_eq!(rows[at + 1], "  src/main.rs                     +3 -1");
    assert_eq!(rows[at + 2], "  notes.txt                          +1");
}

#[test]
fn todo_toggle_collapses_the_list_and_a_short_list_has_no_toggle() {
    let mut h = Harness::new(150, 42);
    h.turn("go", "done");
    let todo = |t: &str, s| Todo {
        text: t.into(),
        status: s,
    };
    h.event(Event::Todos(vec![
        todo("one", TodoStatus::Completed),
        todo("two", TodoStatus::InProgress),
    ]));
    let rows = sidebar_rows(&mut h, 108);
    assert!(rows.iter().any(|r| r == "  Todo"), "{rows:#?}");
    h.event(Event::Todos(vec![
        todo("one", TodoStatus::Completed),
        todo("two", TodoStatus::InProgress),
        todo("three", TodoStatus::Pending),
    ]));
    let rows = sidebar_rows(&mut h, 108);
    let at = rows.iter().position(|r| r == "  ▼ Todo").expect("toggle");
    assert_eq!(rows[at + 3], "  [ ] three");
    h.app.on_mouse(MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: 112,
        row: at as u16,
        modifiers: KeyModifiers::NONE,
    });
    let rows = sidebar_rows(&mut h, 108);
    assert!(rows.iter().any(|r| r == "  ▶ Todo"));
    assert!(!rows.iter().any(|r| r.contains("[ ] three")));
    // all done: the section disappears
    h.event(Event::Todos(vec![todo("one", TodoStatus::Completed)]));
    assert!(!sidebar_rows(&mut h, 108).iter().any(|r| r.contains("Todo")));
}

#[test]
fn lsp_shows_its_disabled_text_and_mcp_is_left_out() {
    let mut h = Harness::new(150, 42);
    h.turn("go", "done");
    let rows = sidebar_rows(&mut h, 108);
    // opencode draws `LSP` and `LSPs are disabled` under Context, then a blank row
    let at = rows.iter().position(|r| r == "  LSP").expect("LSP heading");
    assert_eq!(rows[at + 1], "  LSPs are disabled");
    assert_eq!(rows[at + 2], "");
    assert!(!rows.join("\n").contains("MCP"));
}

#[test]
fn an_overflowing_sidebar_gets_a_scrollbar_and_scrolls_with_the_wheel() {
    let mut h = Harness::new(150, 16);
    h.turn("go", "done");
    let todo = |t: &str| Todo {
        text: t.into(),
        status: TodoStatus::Pending,
    };
    h.event(Event::Todos(
        (0..8).map(|i| todo(&format!("item {i}"))).collect(),
    ));
    let t = h.render();
    let bar = &t.buffer()[(147, 1)];
    assert_eq!(bar.bg, h.app.theme.background, "track");
    assert_eq!(bar.fg, h.app.theme.border_active, "thumb");
    let first = sidebar_rows(&mut h, 108)[1].clone();
    h.app.on_mouse(MouseEvent {
        kind: MouseEventKind::ScrollDown,
        column: 120,
        row: 5,
        modifiers: KeyModifiers::NONE,
    });
    assert_ne!(sidebar_rows(&mut h, 108)[1], first);
}

#[test]
fn the_sidebar_shows_at_121_columns_and_not_at_120_until_toggled() {
    let mut h = Harness::new(121, 40);
    h.turn("go", "done");
    assert!(h.text().contains("Context"));
    let mut h = Harness::new(120, 40);
    h.turn("go", "done");
    assert!(!h.text().contains("Context"));
    h.key(KeyCode::Char('x'), KeyModifiers::CONTROL);
    h.key(KeyCode::Char('b'), KeyModifiers::NONE);
    assert!(h.text().contains("Context"), "overlay");
    h.key(KeyCode::Char('x'), KeyModifiers::CONTROL);
    h.key(KeyCode::Char('b'), KeyModifiers::NONE);
    assert!(!h.text().contains("Context"));
}

/// The mock backend's `sub` scenario: a task call with two nested calls.
fn sub_run(h: &mut Harness) {
    h.app.send_prompt("sub please".into());
    h.event(Event::TurnStart);
    let mut t = tool(
        "t1",
        "task",
        ToolKind::Other,
        "explore: find the parser",
        serde_json::json!({"description": "find the parser"}),
        Some("The parser lives in src/parser.rs"),
    );
    t.status = agent_core::ToolStatus::Completed;
    h.event(Event::Tool(t));
    for (i, n) in ["search_files", "read_file"].iter().enumerate() {
        let mut c = tool(
            &format!("s{i}"),
            n,
            ToolKind::Search,
            "x",
            serde_json::json!({}),
            Some("ok"),
        );
        c.parent_id = Some("t1".into());
        h.event(Event::Tool(c));
    }
    h.event(Event::TurnEnd(StopReason::EndTurn));
}

#[test]
fn a_child_session_swaps_the_prompt_for_the_subagent_bar_and_drops_the_sidebar() {
    let mut h = Harness::new(150, 42);
    sub_run(&mut h);
    assert!(h.text().contains("Context"));
    // up, left and right belong to the prompt until a child is open
    h.key(KeyCode::Right, KeyModifiers::NONE);
    assert!(h.app.child.is_none());
    h.key(KeyCode::Char('x'), KeyModifiers::CONTROL);
    h.key(KeyCode::Down, KeyModifiers::NONE);
    assert_eq!(h.app.child, Some(0));
    let text = h.text();
    let rows: Vec<&str> = text.lines().collect();
    let bar = rows
        .iter()
        .position(|r| r.contains("Subagent"))
        .expect("bar");
    assert!(
        rows[bar].starts_with("  ┃  Subagent (1 of 1)"),
        "{}",
        rows[bar]
    );
    assert!(rows[bar]
        .trim_end()
        .ends_with("Parent up  Prev left  Next right"));
    assert!(rows[bar - 1].trim_end() == "  ┃" && rows[bar + 1].trim_end() == "  ┃");
    assert_eq!(bar, 42 - 3, "bar sits above the bottom padding row");
    assert!(!text.contains("Context"), "no sidebar in a child session");
    assert!(text.contains("Grep") || text.contains("search_files") || text.contains("x"));
    // one child: prev and next wrap onto itself; up returns
    h.key(KeyCode::Right, KeyModifiers::NONE);
    assert_eq!(h.app.child, Some(0));
    h.key(KeyCode::Up, KeyModifiers::NONE);
    assert!(h.app.child.is_none());
    assert!(h.text().contains("Context"));
}

#[test]
fn sidebar_with_todos_and_modified_files_at_170x45() {
    let mut h = Harness::new(170, 45);
    h.turn("tidy the greeting", "Done.");
    let todo = |t: &str, s| Todo {
        text: t.into(),
        status: s,
    };
    h.event(Event::Todos(vec![
        todo("Read the code", TodoStatus::Completed),
        todo(
            "Fix the greeting and add a test that covers the long wrapped item text",
            TodoStatus::InProgress,
        ),
        todo("Add tests", TodoStatus::Pending),
    ]));
    h.event(Event::Tool(edit(
        "e1",
        "src/main.rs",
        Some("a\nb\n"),
        "a\nc\nd\n",
    )));
    h.event(Event::Tool(edit(
        "e2",
        "crates/some/very/deep/path/to/a/long_file_name.rs",
        None,
        "one\n",
    )));
    assert_snapshot("sidebar-todos-files-170x45", &h.text());
}

#[test]
fn child_session_bar_at_150x42() {
    let mut h = Harness::new(150, 42);
    sub_run(&mut h);
    h.key(KeyCode::Char('x'), KeyModifiers::CONTROL);
    h.key(KeyCode::Down, KeyModifiers::NONE);
    assert_snapshot("subagent-bar-150x42", &h.text());
}
