//! The status line, the task list, and the paths in the detailed transcript, put through the
//! app the way keys and backend events would.

mod common;

use agent_core::{Event, StopReason, Todo, TodoStatus, ToolCall, ToolKind, ToolStatus};
use claudew::statusline_run::Cfg;
use common::Harness;
use crossterm::event::KeyCode;
use ratatui::style::Color;

fn harness() -> Harness {
    Harness::new("static-120x40.jsonl", 120, 40)
}

fn status_line(hn: &mut Harness, lines: &[&str], padding: usize) {
    hn.app.status_line.cfg = Some(Cfg {
        command: "true".into(),
        padding,
        refresh: None,
    });
    hn.app.status_line.lines = lines.iter().map(ToString::to_string).collect();
}

fn rows(hn: &mut Harness) -> Vec<String> {
    hn.render().plain().lines().map(String::from).collect()
}

#[test]
fn the_status_line_sits_between_the_prompt_and_the_mode_line() {
    let mut hn = harness();
    hn.advance(11_000);
    status_line(&mut hn, &["main +2 -1", "second row"], 0);
    let screen = rows(&mut hn);
    let at = screen
        .iter()
        .rposition(|r| r == "  main +2 -1")
        .expect("the first row");
    assert_eq!(screen[at + 1], "  second row");
    assert!(
        screen[at + 2].starts_with("  ⏸ manual mode on"),
        "{screen:?}"
    );
    assert!(
        !screen[at + 2].contains("? for shortcuts"),
        "a status line takes the hint away: {}",
        screen[at + 2]
    );
    assert!(screen[at - 3].starts_with('─'), "{screen:?}");
    assert!(screen[at - 2].starts_with('❯'), "{screen:?}");
    assert!(screen[at - 1].starts_with('─'), "{screen:?}");
}

#[test]
fn padding_indents_both_sides_and_a_long_line_ends_in_an_ellipsis() {
    let mut hn = harness();
    hn.app.size = (60, 20);
    status_line(&mut hn, &[&"x".repeat(100)], 1);
    let screen = rows(&mut hn);
    let row = screen
        .iter()
        .find(|r| r.starts_with("   x"))
        .expect("the row");
    // two columns of margin, one of padding, on both sides: 60 - 6 text columns
    assert_eq!(row.chars().count(), 3 + 53 + 1, "{row:?}");
    assert!(row.ends_with('…'));
}

#[test]
fn a_command_with_nothing_to_say_still_holds_a_row() {
    let mut hn = harness();
    hn.advance(11_000);
    let before = rows(&mut hn);
    status_line(&mut hn, &[], 0);
    let after = rows(&mut hn);
    let mode = |s: &[String]| s.iter().position(|r| r.starts_with("  ⏸ manual mode on"));
    assert_eq!(
        mode(&after),
        mode(&before),
        "the mode line stays at the bottom"
    );
    let rule = after
        .iter()
        .rposition(|r| r.starts_with('─'))
        .expect("rule");
    assert_eq!(after[rule + 1], "", "{after:?}");
}

#[test]
fn the_shortcut_table_replaces_the_status_line() {
    let mut hn = harness();
    status_line(&mut hn, &["main +2 -1"], 0);
    hn.type_str("?");
    let screen = rows(&mut hn);
    assert!(screen.iter().any(|r| r.contains("! for shell mode")));
    assert!(
        !screen.iter().any(|r| r.contains("main +2 -1")),
        "{screen:?}"
    );
}

#[test]
fn a_colour_in_the_output_is_drawn_and_the_rest_stays_subtle() {
    let mut hn = harness();
    status_line(&mut hn, &["\x1b[32mgreen\x1b[0m rest"], 0);
    let t = hn.render();
    let (y, row) = (0..40)
        .map(|y| (y, t.row(y)))
        .find(|(_, r)| r.starts_with("  green rest"))
        .expect("the row");
    assert!(row.starts_with("  green"));
    assert_eq!(t.buffer()[(2, y)].fg, Color::Indexed(2));
    assert_eq!(t.buffer()[(9, y)].fg, Color::Indexed(246));
}

fn task(text: &str, status: TodoStatus) -> Todo {
    Todo {
        text: text.into(),
        status,
    }
}

fn three() -> Vec<Todo> {
    vec![
        task("Read main.rs", TodoStatus::Completed),
        task("Change hello to hi", TodoStatus::InProgress),
        task("Run the tests", TodoStatus::Pending),
    ]
}

#[test]
fn between_turns_the_list_has_a_heading_and_the_tasks() {
    let mut hn = harness();
    hn.event(Event::Todos(three()));
    let screen = rows(&mut hn);
    let at = screen
        .iter()
        .position(|r| r == "  3 tasks (1 done, 1 in progress, 1 open)")
        .expect("heading");
    assert_eq!(screen[at + 1], "  ✔ Read main.rs");
    assert_eq!(screen[at + 2], "  ◼ Change hello to hi");
    assert_eq!(screen[at + 3], "  ◻ Run the tests");
    let last = screen.last().expect("a row");
    assert!(last.contains("ctrl+t to hide tasks"), "{last}");
    assert!(!last.contains("? for shortcuts"), "{last}");
    assert!(
        hn.app.blocks.is_empty(),
        "the list is a panel, not a transcript block"
    );
}

#[test]
fn ctrl_t_hides_the_list_and_a_finished_list_goes_by_itself() {
    let mut hn = harness();
    hn.event(Event::Todos(three()));
    hn.ctrl('t');
    let screen = rows(&mut hn);
    assert!(!screen.iter().any(|r| r.contains("3 tasks")), "{screen:?}");
    assert!(screen.last().expect("row").contains("ctrl+t to show tasks"));
    hn.ctrl('t');
    let done: Vec<Todo> = three()
        .into_iter()
        .map(|t| task(&t.text, TodoStatus::Completed))
        .collect();
    hn.event(Event::Todos(done));
    let screen = rows(&mut hn);
    assert!(
        !screen.iter().any(|r| r.contains("ctrl+t to")),
        "{screen:?}"
    );
    assert!(!screen.iter().any(|r| r.contains("✔ Read")), "{screen:?}");
}

#[test]
fn past_five_the_unfinished_come_first_and_the_rest_is_counted() {
    let mut hn = harness();
    let many: Vec<Todo> = (1..=9)
        .map(|n| {
            task(
                &format!("Step {n}"),
                match n {
                    1..=3 => TodoStatus::Completed,
                    4 => TodoStatus::InProgress,
                    _ => TodoStatus::Pending,
                },
            )
        })
        .collect();
    hn.event(Event::Todos(many));
    let screen = rows(&mut hn);
    let at = screen
        .iter()
        .position(|r| r.starts_with("  9 tasks"))
        .expect("heading");
    assert_eq!(screen[at + 1], "  ◼ Step 4");
    assert_eq!(screen[at + 5], "  ◻ Step 8");
    assert_eq!(screen[at + 6], "   … +1 pending, 3 completed");
}

#[test]
fn while_a_turn_runs_the_spinner_names_the_task_and_the_list_hangs_under_it() {
    let mut hn = harness();
    hn.type_str("go");
    hn.press(KeyCode::Enter);
    hn.event(Event::TurnStart);
    hn.event(Event::Todos(three()));
    hn.advance(2_000);
    let screen = rows(&mut hn);
    let at = screen
        .iter()
        .position(|r| r.contains("Change hello to hi…"))
        .unwrap_or_else(|| panic!("no spinner naming the task: {screen:?}"));
    assert_eq!(screen[at + 1], "  ⎿ \u{a0}✔ Read main.rs");
    assert_eq!(screen[at + 2], "     ◼ Change hello to hi");
    assert_eq!(screen[at + 3], "     ◻ Run the tests");
    let last = screen.last().expect("row");
    assert!(
        last.contains("esc to interrupt · ctrl+t to hide tasks"),
        "{last}"
    );
    hn.event(Event::TurnEnd(StopReason::EndTurn));
}

#[test]
fn the_detailed_transcript_writes_paths_from_the_working_directory() {
    let mut hn = harness();
    let cwd = hn.app.opts.cwd.to_string_lossy().to_string();
    let read = |id: &str, path: String| ToolCall {
        id: id.into(),
        name: "read_file".into(),
        kind: ToolKind::Read,
        title: path.clone(),
        input: serde_json::json!({ "path": path }),
        status: ToolStatus::Completed,
        output: Some("x\n".into()),
        ..Default::default()
    };
    hn.event(Event::TurnStart);
    hn.event(Event::Tool(read("a", format!("{cwd}/src/lib.rs"))));
    hn.event(Event::Tool(read(
        "b",
        format!(
            "{}/outside.txt",
            std::path::Path::new(&cwd)
                .parent()
                .expect("parent")
                .display()
        ),
    )));
    hn.event(Event::Tool(read("c", "/etc/hostname".into())));
    hn.event(Event::TurnEnd(StopReason::EndTurn));
    hn.ctrl('o');
    let screen = hn.render().plain();
    assert!(screen.contains("Read(./src/lib.rs)"), "{screen}");
    assert!(screen.contains("Read(./../outside.txt)"), "{screen}");
    assert!(screen.contains("Read(/etc/hostname)"), "{screen}");
}
