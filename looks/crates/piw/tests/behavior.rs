//! What keys and backend events do, without looking at the whole screen.

mod common;

use agent_core::{Event, HistoryItem, NoticeLevel, Request, SlashCommand, StopReason, ToolKind};
use common::*;
use crossterm::event::{KeyCode, KeyModifiers};

fn prompts(h: &mut Harness) -> Vec<String> {
    h.sent()
        .into_iter()
        .filter_map(|r| match r {
            Request::Prompt(t) => Some(t),
            _ => None,
        })
        .collect()
}

fn start_turn(h: &mut Harness, text: &str) {
    h.type_str(text);
    h.press(KeyCode::Enter);
    h.event(Event::TurnStart);
    h.event(Event::TextDelta("streaming".into()));
}

#[test]
fn enter_sends_the_prompt_and_shows_a_user_block() {
    let mut h = Harness::new(100, 30);
    h.type_str("hello there");
    h.press(KeyCode::Enter);
    assert_eq!(prompts(&mut h), ["hello there"]);
    let t = h.text();
    assert!(t.contains(" hello there"), "{t}");
    assert!(h.app.editor.is_empty());
}

#[test]
fn working_label_shows_in_the_top_rule_while_busy() {
    let mut h = Harness::new(100, 30);
    start_turn(&mut h, "go");
    let t = h.text();
    assert!(t.contains("── ⠋ Working ───"), "{t}");
    h.event(Event::TurnEnd(StopReason::EndTurn));
    assert!(!h.text().contains("Working"));
}

#[test]
fn enter_while_busy_queues_and_warns_once() {
    let mut h = Harness::new(100, 30);
    start_turn(&mut h, "go");
    h.sent();
    h.type_str("steer one");
    h.press(KeyCode::Enter);
    h.type_str("steer two");
    h.press(KeyCode::Enter);
    h.type_str("later");
    h.key(KeyCode::Enter, KeyModifiers::ALT);
    assert!(prompts(&mut h).is_empty(), "nothing is sent mid-turn");
    let t = h.text();
    assert!(t.contains("Steering: steer one"), "{t}");
    assert!(t.contains("Follow-up: later"), "{t}");
    assert!(t.contains("↳ Alt+Up to edit all queued messages"), "{t}");
    assert_eq!(
        t.matches("wizard cannot steer a running turn").count(),
        1,
        "{t}"
    );
    // delivered one at a time, steering first, once the turn ends
    h.event(Event::TurnEnd(StopReason::EndTurn));
    assert_eq!(prompts(&mut h), ["steer one"]);
    h.event(Event::TurnStart);
    h.event(Event::TurnEnd(StopReason::EndTurn));
    assert_eq!(prompts(&mut h), ["steer two"]);
    h.event(Event::TurnStart);
    h.event(Event::TurnEnd(StopReason::EndTurn));
    assert_eq!(prompts(&mut h), ["later"]);
}

#[test]
fn escape_cancels_and_puts_the_queue_back_in_the_editor() {
    let mut h = Harness::new(100, 30);
    start_turn(&mut h, "go");
    h.sent();
    h.type_str("one");
    h.press(KeyCode::Enter);
    h.type_str("two");
    h.key(KeyCode::Enter, KeyModifiers::ALT);
    h.press(KeyCode::Esc);
    assert!(h.sent().contains(&Request::Cancel));
    assert_eq!(h.app.editor.text(), "one\n\ntwo");
    h.event(Event::TurnEnd(StopReason::Cancelled));
    let t = h.text();
    assert!(t.contains("Operation aborted"), "{t}");
    assert!(t.contains("Restored 2 queued messages to editor"), "{t}");
    assert!(
        prompts(&mut h).is_empty(),
        "a cancelled turn flushes nothing"
    );
}

#[test]
fn alt_up_restores_the_queue() {
    let mut h = Harness::new(100, 30);
    start_turn(&mut h, "go");
    h.type_str("one");
    h.press(KeyCode::Enter);
    h.key(KeyCode::Up, KeyModifiers::ALT);
    assert_eq!(h.app.editor.text(), "one");
    assert!(h.text().contains("Restored 1 queued message to editor"));
    h.key(KeyCode::Up, KeyModifiers::ALT);
    assert!(h.text().contains("No queued messages to restore"));
}

#[test]
fn ctrl_c_clears_then_exits_on_the_second_press() {
    let mut h = Harness::new(100, 30);
    h.type_str("draft");
    h.ctrl('c');
    assert!(h.app.editor.is_empty() && !h.app.quit);
    h.ctrl('c');
    assert!(h.app.quit);
}

#[test]
fn ctrl_d_exits_only_on_an_empty_editor() {
    let mut h = Harness::new(100, 30);
    h.type_str("ab");
    h.key(KeyCode::Left, KeyModifiers::NONE);
    h.ctrl('d');
    assert_eq!(h.app.editor.text(), "a");
    assert!(!h.app.quit);
    h.ctrl('a');
    h.ctrl('d');
    h.ctrl('d');
    assert!(h.app.quit);
}

#[test]
fn shift_tab_cycles_the_backends_levels() {
    let mut h = Harness::new(100, 30);
    h.press(KeyCode::BackTab);
    assert_eq!(h.sent(), [Request::SetEffort("high".into())]);
    assert!(h.text().contains("Thinking level: high"));
    h.press(KeyCode::BackTab);
    h.press(KeyCode::BackTab);
    assert_eq!(
        h.sent(),
        [
            Request::SetEffort("xhigh".into()),
            Request::SetEffort("low".into())
        ]
    );
}

#[test]
fn status_lines_replace_each_other() {
    let mut h = Harness::new(100, 30);
    h.ctrl('o');
    h.ctrl('o');
    let t = h.text();
    assert_eq!(t.matches("Tool output:").count(), 1, "{t}");
    assert!(t.contains("Tool output: collapsed"));
    h.ctrl('t');
    assert!(h.text().contains("Thinking blocks: hidden"));
}

#[test]
fn new_session_clears_the_chat_and_prints_the_accent_line() {
    let mut h = Harness::new(100, 30);
    h.turn("hello", "hi");
    h.type_str("/new");
    h.press(KeyCode::Enter);
    assert!(h.sent().contains(&Request::NewSession));
    h.event(Event::History {
        session_id: "s2".into(),
        items: vec![],
    });
    let t = h.text();
    assert!(t.contains("✓ New session started"), "{t}");
    assert!(!t.contains("hello"));
}

#[test]
fn resume_history_replays_into_the_transcript() {
    let mut h = Harness::new(100, 30);
    h.event(Event::History {
        session_id: "old".into(),
        items: vec![
            HistoryItem::User("fix the bug".into()),
            HistoryItem::Assistant("Fixed.".into()),
        ],
    });
    let t = h.text();
    assert!(t.contains("fix the bug") && t.contains("Fixed."), "{t}");
}

#[test]
fn model_command_opens_the_selector_or_switches_on_an_exact_match() {
    let mut h = Harness::new(100, 30);
    h.type_str("/model");
    h.press(KeyCode::Enter);
    assert!(h
        .text()
        .contains("Enter to select · Ctrl+S to set as default"));
    h.press(KeyCode::Enter);
    assert!(h
        .sent()
        .contains(&Request::SetModel("fake/fake-model".into())));
    h.type_str("/model fake-model");
    h.press(KeyCode::Esc); // closes the argument popup
    h.press(KeyCode::Enter);
    assert!(h
        .sent()
        .contains(&Request::SetModel("fake/fake-model".into())));
    assert!(h.text().contains("Model: fake-model"));
}

#[test]
fn a_look_typed_in_full_after_ui_is_sent_on_the_first_enter() {
    let mut h = Harness::new(100, 30);
    h.type_str("/ui opencode");
    h.press(KeyCode::Enter);
    assert_eq!(prompts(&mut h), ["/ui opencode"]);
}

#[test]
fn wizard_commands_go_to_the_backend_without_a_user_block() {
    let mut h = Harness::new(100, 30);
    h.event(Event::Commands(vec![SlashCommand {
        name: "mode".into(),
        description: "switch mode".into(),
        input_hint: "<m>".into(),
    }]));
    h.type_str("/mode sovereign");
    h.press(KeyCode::Esc);
    h.press(KeyCode::Enter);
    assert_eq!(prompts(&mut h), ["/mode sovereign"]);
    assert!(!h.text().contains("/mode sovereign"));
    h.event(Event::TurnStart);
    h.event(Event::Notice {
        level: NoticeLevel::Info,
        text: "mode: sovereign".into(),
    });
    h.event(Event::TurnEnd(StopReason::EndTurn));
    assert!(h.text().contains("mode: sovereign"));
}

#[test]
fn unsupported_commands_say_so() {
    let mut h = Harness::new(100, 30);
    for c in ["/fork", "/tree", "/clone", "/export", "/login", "/trust"] {
        h.type_str(c);
        h.press(KeyCode::Esc);
        h.press(KeyCode::Enter);
    }
    let t = h.text();
    assert!(
        t.contains("Warning: wizard sessions do not branch; /fork is not supported."),
        "{t}"
    );
    assert!(t.contains("Warning: /export is not supported yet."), "{t}");
    assert!(
        t.contains("Warning: wizard signs in to its providers from ~/.wizard"),
        "{t}"
    );
    assert!(prompts(&mut h).is_empty());
}

#[tokio::test]
async fn bang_runs_a_command_and_the_output_rides_along_with_the_next_prompt() {
    let mut h = Harness::new(100, 30);
    h.app.opts.cwd = std::env::temp_dir();
    h.type_str("!echo piw-bang");
    h.press(KeyCode::Enter);
    h.settle_bash().await;
    h.type_str("!!echo secret-text");
    h.press(KeyCode::Enter);
    h.settle_bash().await;
    let t = h.text();
    assert!(
        t.contains("$ echo piw-bang") && t.contains("piw-bang"),
        "{t}"
    );
    assert!(prompts(&mut h).is_empty(), "a bang command is not a prompt");
    h.type_str("what did it print");
    h.press(KeyCode::Enter);
    let sent = prompts(&mut h);
    assert_eq!(sent.len(), 1);
    assert!(sent[0].contains("Ran `echo piw-bang`") && sent[0].contains("piw-bang"));
    assert!(
        !sent[0].contains("secret-text"),
        "!! stays out of the context"
    );
    assert!(sent[0].ends_with("what did it print"));
}

#[test]
fn up_and_down_walk_the_history_and_restore_the_draft() {
    let mut h = Harness::new(100, 30);
    h.turn("first", "a");
    h.type_str("first");
    h.press(KeyCode::Enter);
    h.event(Event::TurnEnd(StopReason::EndTurn));
    h.type_str("second");
    h.press(KeyCode::Enter);
    h.event(Event::TurnEnd(StopReason::EndTurn));
    h.type_str("draft");
    // the first Up on a line of text only jumps to its start, the second walks the history
    h.press(KeyCode::Up);
    assert_eq!(h.app.editor.text(), "draft");
    h.press(KeyCode::Up);
    assert_eq!(h.app.editor.text(), "second");
    h.press(KeyCode::Up);
    assert_eq!(h.app.editor.text(), "first");
    h.press(KeyCode::Down);
    assert_eq!(h.app.editor.text(), "second");
    h.press(KeyCode::Down);
    assert_eq!(h.app.editor.text(), "draft");
}

#[test]
fn trailing_backslash_makes_a_newline() {
    let mut h = Harness::new(100, 30);
    h.type_str("line one\\");
    h.press(KeyCode::Enter);
    h.type_str("line two");
    assert_eq!(h.app.editor.text(), "line one\nline two");
    assert!(prompts(&mut h).is_empty());
}

#[test]
fn enter_on_a_slash_row_runs_the_command() {
    let mut h = Harness::new(100, 30);
    h.type_str("/ne");
    h.press(KeyCode::Enter);
    assert!(h.sent().contains(&Request::NewSession));
    assert!(h.app.editor.is_empty());
}

#[test]
fn compaction_shows_the_label_then_the_summary_box() {
    let mut h = Harness::new(100, 30);
    h.turn("hello", "hi");
    h.sent();
    h.type_str("/compact");
    h.press(KeyCode::Enter);
    assert_eq!(prompts(&mut h), ["/compact"]);
    h.event(Event::TurnStart);
    assert!(h
        .text()
        .contains("Compacting context... (escape to cancel)"));
    h.event(Event::Notice {
        level: NoticeLevel::Info,
        text: "Compacted from 1,261 tokens\n\nSummary text".into(),
    });
    h.event(Event::TurnEnd(StopReason::EndTurn));
    let t = h.text();
    assert!(t.contains("[compaction]"), "{t}");
    assert!(
        t.contains("Compacted from 1,261 tokens (ctrl+o to expand)"),
        "{t}"
    );
    assert!(
        t.contains("?/200k (auto)"),
        "the context figure is unknown after a compaction: {t}"
    );
}

#[test]
fn nothing_to_compact_is_not_a_compaction() {
    let mut h = Harness::new(100, 30);
    h.type_str("/compact");
    h.press(KeyCode::Enter);
    h.event(Event::TurnStart);
    h.event(Event::Notice {
        level: NoticeLevel::Info,
        text: "nothing to compact yet".into(),
    });
    h.event(Event::TurnEnd(StopReason::EndTurn));
    let t = h.text();
    assert!(!t.contains("[compaction]"), "{t}");
    assert!(t.contains("nothing to compact yet"), "{t}");
}

#[test]
fn page_up_shows_the_jump_indicator_and_ctrl_end_hides_it() {
    let mut h = Harness::new(100, 20);
    for i in 0..8 {
        h.turn(&format!("question {i}"), &"A long answer line. ".repeat(12));
    }
    assert!(!h.text().contains("Jump to latest"));
    h.press(KeyCode::PageUp);
    assert!(h.text().contains("↓ Jump to latest message · Ctrl+End"));
    h.key(KeyCode::End, KeyModifiers::CONTROL);
    assert!(!h.text().contains("Jump to latest"));
}

#[test]
fn errors_and_fatal_events_become_lines() {
    let mut h = Harness::new(100, 30);
    h.type_str("go");
    h.press(KeyCode::Enter);
    h.event(Event::TurnStart);
    h.event(Event::TurnEnd(StopReason::Error));
    assert!(h.text().contains("Error: Request failed"));
    h.event(Event::Fatal("wizard exited".into()));
    let t = h.text();
    assert!(t.contains("Error: wizard exited"), "{t}");
    assert!(
        t.contains("Press ctrl+c to exit, or start a new session with /new"),
        "{t}"
    );
}

#[test]
fn tiny_terminals_do_not_panic() {
    for (w, h_) in [(1, 1), (3, 2), (10, 3), (20, 5), (40, 8)] {
        let mut h = Harness::new(w, h_);
        h.turn("hello", "# Title\n\n| a | b |\n|---|---|\n| 1 | 2 |\n");
        h.event(Event::Tool(tool(
            "1",
            "execute",
            ToolKind::Execute,
            "ls",
            serde_json::json!({"command": "ls"}),
            Some("a\nb"),
        )));
        h.type_str("/");
        let _ = h.text();
        h.ctrl('l');
        let _ = h.text();
    }
}
