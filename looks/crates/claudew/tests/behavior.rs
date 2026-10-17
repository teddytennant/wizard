//! Keys and backend events that change state rather than pixels.

mod common;

use agent_core::{Event, NoticeLevel, Request, StopReason};
use claudew::app::{Block, Dialog};
use common::Harness;
use crossterm::event::KeyCode;

fn harness() -> Harness {
    Harness::new("static-120x40.jsonl", 120, 40)
}

fn plan_answer(hn: &mut common::Harness, text: &str) {
    for ev in [
        Event::TurnStart,
        Event::Notice {
            level: NoticeLevel::Info,
            text: text.into(),
        },
        Event::TurnEnd(StopReason::EndTurn),
    ] {
        hn.event(ev);
    }
}

fn mode_confirmed(hn: &mut common::Harness, mode: &str) {
    let mut cfg = hn.app.cfg.clone();
    cfg.mode = mode.into();
    hn.event(Event::ConfigChanged(cfg));
}

#[test]
fn shift_tab_walks_the_modes_then_plan_and_leaves_no_trace() {
    let mut hn = harness();
    hn.press(KeyCode::BackTab);
    assert_eq!(hn.sent(), [Request::SetMode("accept edits".into())]);
    hn.press(KeyCode::BackTab);
    assert!(
        hn.sent().is_empty(),
        "the second step waits for wizard's answer"
    );
    assert_eq!(
        hn.app.mode_index(),
        Some(0),
        "the footer waits for wizard to confirm"
    );
    mode_confirmed(&mut hn, "accept edits");
    assert_eq!(hn.sent(), [Request::Prompt("/plan".into())]);
    plan_answer(&mut hn, "plan mode on");
    assert!(hn.app.plan);
    assert!(hn.app.blocks.is_empty(), "{:?}", hn.app.blocks);
    assert!(hn.app.busy.is_none());
    hn.press(KeyCode::BackTab);
    assert_eq!(hn.sent(), [Request::Prompt("/plan".into())]);
    plan_answer(&mut hn, "plan mode off");
    assert_eq!(hn.sent(), [Request::SetMode("manual".into())]);
    mode_confirmed(&mut hn, "manual");
    assert_eq!(hn.app.mode_index(), Some(0));
    assert!(hn.app.blocks.is_empty(), "{:?}", hn.app.blocks);
}

#[test]
fn a_refused_mode_change_is_retried_once_the_turn_ends() {
    let mut hn = harness();
    hn.press(KeyCode::BackTab);
    assert_eq!(hn.sent(), [Request::SetMode("accept edits".into())]);
    hn.event(Event::Notice {
        level: NoticeLevel::Info,
        text: "Options can only be changed between turns.".into(),
    });
    assert!(hn.app.blocks.is_empty(), "wizard's refusal is not printed");
    hn.event(Event::TurnStart);
    hn.event(Event::TurnEnd(StopReason::EndTurn));
    assert_eq!(hn.sent(), [Request::SetMode("accept edits".into())]);
    assert_eq!(hn.app.mode_index(), Some(0));
    mode_confirmed(&mut hn, "accept edits");
    assert_eq!(hn.app.mode_index(), Some(1));
}

#[test]
fn a_prompt_queued_behind_a_mode_change_waits_for_it() {
    let mut hn = harness();
    hn.type_str("first");
    hn.press(KeyCode::Enter);
    hn.event(Event::TurnStart);
    hn.press(KeyCode::BackTab);
    hn.type_str("second");
    hn.press(KeyCode::Enter);
    hn.sent();
    hn.event(Event::TurnEnd(StopReason::EndTurn));
    assert_eq!(
        hn.sent(),
        [Request::SetMode("accept edits".into())],
        "the mode change goes first, and the prompt waits"
    );
    mode_confirmed(&mut hn, "accept edits");
    assert_eq!(hn.sent(), [Request::Prompt("second".into())]);
}

#[test]
fn plan_mode_is_never_entered_on_chat() {
    let mut hn = harness();
    hn.app.cfg.modes = vec!["genie".into(), "sovereign".into(), "chat".into()];
    hn.app.cfg.mode = "chat".into();
    hn.press(KeyCode::BackTab);
    assert_eq!(hn.sent(), [Request::SetMode("genie".into())]);
    mode_confirmed(&mut hn, "genie");
    assert_eq!(hn.sent(), [Request::Prompt("/plan".into())]);
}

#[test]
fn up_steps_through_history_past_slash_entries() {
    let mut hn = harness();
    for p in ["line one", "/help"] {
        hn.app.submit(p.to_string());
        hn.app.dialog = None;
        hn.event(Event::TurnEnd(StopReason::EndTurn));
    }
    hn.press(KeyCode::Up);
    assert_eq!(hn.app.editor.text(), "/help");
    hn.press(KeyCode::Up);
    assert_eq!(hn.app.editor.text(), "line one");
}

#[test]
fn ctrl_c_clears_then_exits_and_the_window_closes() {
    let mut hn = harness();
    hn.type_str("draft");
    hn.ctrl('c');
    assert!(hn.app.editor.is_empty() && !hn.app.quit);
    hn.advance(900);
    hn.ctrl('c');
    assert!(!hn.app.quit, "the first press's window had closed");
    hn.advance(300);
    hn.ctrl('c');
    assert!(hn.app.quit);
}

#[test]
fn escape_twice_quickly_clears_the_prompt() {
    let mut hn = harness();
    hn.type_str("abc");
    hn.press(KeyCode::Esc);
    hn.advance(900);
    hn.press(KeyCode::Esc);
    assert_eq!(
        hn.app.editor.text(),
        "abc",
        "too slow to count as a double press"
    );
    hn.advance(300);
    hn.press(KeyCode::Esc);
    assert!(hn.app.editor.is_empty());
}

#[test]
fn enter_in_the_slash_menu_runs_the_selection() {
    let mut hn = harness();
    hn.type_str("/mo");
    hn.press(KeyCode::Enter);
    assert!(matches!(hn.app.dialog, Some(Dialog::Model { .. })));
    hn.press(KeyCode::Esc);
    assert!(matches!(
        hn.app.blocks.last(),
        Some(Block::Output { text, .. }) if text == "Kept model as Haiku 5.5"
    ));
}

#[test]
fn a_backend_command_goes_out_and_its_answer_hangs_under_it() {
    let mut hn = harness();
    hn.type_str("/compact");
    hn.press(KeyCode::Enter);
    assert_eq!(hn.sent(), [Request::Prompt("/compact".into())]);
    hn.event(Event::TurnStart);
    hn.event(Event::Notice {
        level: NoticeLevel::Info,
        text: "Compacted (ctrl+o to see full summary)".into(),
    });
    hn.event(Event::TurnEnd(StopReason::EndTurn));
    assert!(matches!(
        &hn.app.blocks[..],
        [Block::User { .. }, Block::Output { dim: true, .. }]
    ));
}

#[test]
fn switching_to_another_look_quits_this_one() {
    let mut hn = harness();
    hn.event(Event::Notice {
        level: NoticeLevel::Info,
        text: "saved [ui] skin = \"grok\". Quit this look to switch now.".into(),
    });
    assert!(hn.app.quit);
}

#[test]
fn a_permission_answer_goes_back_with_its_id() {
    let mut hn = Harness::new("turn-120x40.jsonl", 120, 40);
    hn.type_str("go");
    hn.press(KeyCode::Enter);
    hn.play();
    hn.sent();
    hn.type_str("edit it");
    hn.press(KeyCode::Enter);
    hn.play();
    assert!(matches!(hn.app.dialog, Some(Dialog::Permission { .. })));
    hn.press(KeyCode::Char('3'));
    let sent = hn.sent();
    assert!(
        sent.iter()
            .any(|r| matches!(r, Request::Decide { id, allow: false, .. } if id == "p1")),
        "{sent:?}"
    );
}

#[test]
fn the_commands_the_look_answers_are_listed_before_the_backend_is_up() {
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let (mtx, _mrx) = tokio::sync::mpsc::unbounded_channel();
    let app = claudew::app::App::new(Default::default(), tx, mtx);
    let names: Vec<&str> = app.commands.iter().map(|c| c.name.as_str()).collect();
    assert!(
        names.contains(&"help") && names.contains(&"resume"),
        "{names:?}"
    );
}

#[test]
fn esc_on_a_permission_prompt_turns_it_down_and_stops_the_turn() {
    let mut hn = Harness::new("turn2-120x40.jsonl", 120, 40);
    hn.type_str("md");
    hn.press(KeyCode::Enter);
    hn.play();
    hn.type_str("git");
    hn.press(KeyCode::Enter);
    hn.play();
    assert!(matches!(hn.app.dialog, Some(Dialog::Permission { .. })));
    hn.sent();
    hn.press(KeyCode::Esc);
    let sent = hn.sent();
    assert!(
        matches!(sent.first(), Some(Request::Decide { allow: false, .. })),
        "{sent:?}"
    );
    assert_eq!(sent.last(), Some(&Request::Cancel));
    hn.event(Event::TurnEnd(StopReason::Cancelled));
    let n = hn.app.blocks.len();
    assert!(matches!(
        &hn.app.blocks[n - 2],
        Block::Output { muted: true, text, .. } if text.starts_with("Interrupted")
    ));
    assert!(matches!(hn.app.blocks[n - 1], Block::TurnEnd { .. }));
}

#[test]
fn tab_on_a_permission_prompt_sends_a_note_with_the_answer() {
    let mut hn = Harness::new("turn2-120x40.jsonl", 120, 40);
    hn.type_str("md");
    hn.press(KeyCode::Enter);
    hn.play();
    hn.type_str("git");
    hn.press(KeyCode::Enter);
    hn.play();
    hn.sent();
    hn.press(KeyCode::Tab);
    hn.type_str("use -b main");
    hn.press(KeyCode::Enter);
    let sent = hn.sent();
    assert!(
        matches!(sent.first(), Some(Request::Decide { allow: true, note, .. }) if note == "use -b main"),
        "{sent:?}"
    );
}

#[test]
fn the_effort_notice_goes_after_a_while_and_comes_back_for_a_turn() {
    let mut hn = harness();
    assert!(hn
        .app
        .notice
        .as_deref()
        .is_some_and(|n| n.contains("/effort")));
    hn.advance(31_000);
    assert_eq!(hn.app.notice, None);
    hn.type_str("hi");
    hn.press(KeyCode::Enter);
    assert!(hn.app.notice.is_some(), "shown again while the turn runs");
}

#[test]
fn the_header_and_status_show_wizards_version() {
    let mut hn = harness();
    let v = common::replay_version();
    assert_eq!(hn.app.wizard_version(), v);
    let screen = hn.render().plain();
    assert!(screen.contains(&format!("Wizard v{v}")), "{screen}");
    hn.type_str("/status");
    hn.press(KeyCode::Enter);
    let screen = hn.render().plain();
    assert!(
        screen.contains(&format!("Version:           {v}")),
        "{screen}"
    );
}

#[test]
fn before_wizard_answers_the_header_names_no_version() {
    let mut hn = harness();
    hn.app.cfg.backend.clear();
    assert_eq!(hn.app.wizard_version(), "");
    let screen = hn.render().plain();
    assert!(
        !screen.contains(&format!("v{}", claudew::app::VERSION)),
        "{screen}"
    );
}

#[test]
fn ctrl_r_finds_an_earlier_prompt() {
    let mut hn = harness();
    for p in ["alpha one", "beta two"] {
        hn.app.submit(p.to_string());
        hn.event(Event::TurnEnd(StopReason::EndTurn));
    }
    hn.ctrl('r');
    hn.type_str("alp");
    hn.press(KeyCode::Enter);
    assert_eq!(hn.app.editor.text(), "alpha one");
}

#[test]
fn completing_a_command_shows_what_it_takes() {
    let mut hn = Harness::new("wizard-120x40.jsonl", 120, 40);
    hn.type_str("/mod");
    hn.press(KeyCode::Tab);
    assert_eq!(hn.app.editor.text(), "/model ");
    let screen = hn.render().plain();
    assert!(screen.contains("❯\u{a0}/model  [tag]"), "{screen}");
}

#[test]
fn page_up_shows_the_way_back_down() {
    let mut hn = harness();
    for n in 0..30 {
        hn.app.submit(format!("prompt {n}"));
        hn.event(Event::TurnEnd(StopReason::EndTurn));
    }
    hn.render();
    hn.press(KeyCode::PageUp);
    assert!(hn.render().plain().contains("Jump to bottom (ctrl+End) ↓"));
    hn.key(KeyCode::End, crossterm::event::KeyModifiers::CONTROL);
    assert!(!hn.render().plain().contains("Jump to bottom"));
}

#[test]
fn after_a_fatal_error_nothing_looks_busy_and_the_exits_work() {
    let mut hn = harness();
    hn.type_str("hello");
    hn.press(KeyCode::Enter);
    hn.sent();
    hn.event(Event::Fatal("wizard exited".into()));
    assert!(hn.app.busy.is_none());
    let notices: Vec<_> = hn
        .app
        .blocks
        .iter()
        .filter_map(|b| match b {
            Block::Notice { text, .. } => Some(text.clone()),
            _ => None,
        })
        .collect();
    assert!(
        notices.iter().any(|t| t.contains("wizard setup")),
        "{notices:?}"
    );
    hn.type_str("again");
    hn.press(KeyCode::Enter);
    assert!(hn.sent().is_empty(), "nothing goes to a dead backend");
    assert!(hn.app.busy.is_none());
    hn.type_str("/exit");
    hn.press(KeyCode::Enter);
    assert!(hn.app.quit);
}

#[test]
fn ctrl_c_during_a_turn_stops_it_and_a_second_press_exits() {
    let mut hn = harness();
    hn.type_str("hello");
    hn.press(KeyCode::Enter);
    hn.event(Event::TurnStart);
    hn.sent();
    hn.ctrl('c');
    assert_eq!(hn.sent(), [Request::Cancel]);
    hn.advance(200);
    hn.ctrl('c');
    assert!(hn.app.quit);
}

#[test]
fn exit_is_not_queued_behind_a_turn() {
    let mut hn = harness();
    hn.type_str("hello");
    hn.press(KeyCode::Enter);
    hn.event(Event::TurnStart);
    hn.type_str("/exit");
    hn.press(KeyCode::Enter);
    assert!(hn.app.quit);
}

#[test]
fn an_interrupt_without_tools_leaves_no_done_line() {
    let mut hn = harness();
    hn.type_str("hello");
    hn.press(KeyCode::Enter);
    hn.event(Event::TurnStart);
    hn.event(Event::TextDelta("partial".into()));
    hn.press(KeyCode::Esc);
    hn.event(Event::TurnEnd(StopReason::Cancelled));
    assert!(
        !hn.app
            .blocks
            .iter()
            .any(|b| matches!(b, Block::TurnEnd { .. })),
        "{:?}",
        hn.app.blocks
    );
}

#[test]
fn a_queued_prompt_shows_survives_esc_and_comes_back_on_up() {
    let mut hn = harness();
    hn.type_str("first");
    hn.press(KeyCode::Enter);
    hn.event(Event::TurnStart);
    hn.type_str("queued one");
    hn.press(KeyCode::Enter);
    let screen = hn.render().plain();
    assert!(screen.contains("❯ queued one"), "{screen}");
    assert!(screen.contains("ctrl+enter to send now"), "{screen}");
    assert!(
        screen.contains("Press up to edit queued messages"),
        "{screen}"
    );
    hn.press(KeyCode::Up);
    assert_eq!(hn.app.editor.text(), "queued one");
    assert!(hn.app.queued.is_empty());
    hn.press(KeyCode::Enter);
    hn.sent();
    hn.press(KeyCode::Esc);
    assert_eq!(hn.sent(), [Request::Cancel]);
    hn.event(Event::TurnEnd(StopReason::Cancelled));
    assert_eq!(hn.sent(), [Request::Prompt("queued one".into())]);
}

#[test]
fn effort_arrows_only_preview_until_the_pick_is_confirmed() {
    let mut hn = harness();
    hn.app.cfg.efforts = vec!["low".into(), "medium".into(), "high".into()];
    hn.app.cfg.effort = "medium".into();
    hn.type_str("/model");
    hn.press(KeyCode::Enter);
    hn.sent();
    hn.press(KeyCode::Right);
    assert!(hn.sent().is_empty());
    assert_eq!(hn.app.cfg.effort, "medium");
    hn.press(KeyCode::Esc);
    assert_eq!(hn.app.cfg.effort, "medium");
    hn.type_str("/model");
    hn.press(KeyCode::Enter);
    hn.press(KeyCode::Right);
    hn.press(KeyCode::Enter);
    assert!(hn.sent().contains(&Request::SetEffort("high".into())));
    let screen = hn.render().plain();
    assert!(
        screen.contains("for this session only with high effort"),
        "{screen}"
    );
}

#[test]
fn a_finished_plan_asks_and_the_answer_goes_back() {
    let mut hn = harness();
    hn.app.plan = true;
    hn.event(Event::TurnStart);
    hn.event(Event::Permission(agent_core::PermissionRequest {
        id: "9".into(),
        tool: "exit_plan".into(),
        kind: agent_core::ToolKind::Other,
        title: "Ready to code?".into(),
        input: serde_json::json!({ "plan": "1. Add `// entry point` to main.rs." }),
        diff: None,
        rule: String::new(),
    }));
    let screen = hn.render().plain();
    assert!(screen.contains("Ready to code?"), "{screen}");
    assert!(screen.contains("// entry point"), "{screen}");
    assert!(screen.contains("❯ 1. Yes, proceed"), "{screen}");
    assert!(screen.contains("2. No, keep planning"), "{screen}");
    hn.sent();
    hn.press(KeyCode::Enter);
    assert!(matches!(
        hn.sent().as_slice(),
        [Request::Decide { id, allow: true, .. }] if id == "9"
    ));
    assert!(!hn.app.plan, "an approved plan leaves plan mode");
}

#[test]
fn streaming_text_hides_the_spinner() {
    let mut hn = harness();
    hn.type_str("count");
    hn.press(KeyCode::Enter);
    hn.event(Event::TurnStart);
    assert!(
        hn.render().plain().contains("…"),
        "the spinner shows before any text"
    );
    hn.event(Event::TextDelta("1\n2\n3".into()));
    let screen = hn.render().plain();
    assert!(
        !screen.contains("esc to interrupt)") && !screen.contains("…"),
        "{screen}"
    );
}

#[test]
fn an_edit_snippet_is_placed_in_its_file() {
    let dir = std::env::temp_dir().join(format!("claudew-place-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("main.rs"),
        "fn main() {\n    println!(\"hi\");\n}\n",
    )
    .unwrap();
    let placed = claudew::app::place_diff(
        &dir,
        agent_core::FileDiff {
            path: "main.rs".into(),
            old: Some("hello".into()),
            new: "hi".into(),
        },
    );
    assert_eq!(
        placed.old.as_deref(),
        Some("fn main() {\n    println!(\"hello\");\n}\n")
    );
    assert_eq!(placed.new, "fn main() {\n    println!(\"hi\");\n}\n");
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn rewind_asks_to_confirm_and_esc_goes_back_to_the_list() {
    let mut hn = harness();
    for p in ["first prompt", "second prompt"] {
        hn.app.submit(p.to_string());
        hn.event(Event::TurnStart);
        hn.event(Event::TurnEnd(StopReason::EndTurn));
    }
    hn.app.dialog = Some(Dialog::Rewind { sel: 1 });
    hn.press(KeyCode::Enter);
    let screen = hn.render().plain();
    assert!(
        screen.contains("Confirm you want to restore to the point before you sent this message:"),
        "{screen}"
    );
    assert!(screen.contains("│ second prompt"), "{screen}");
    assert!(screen.contains("❯ 1. Restore conversation"), "{screen}");
    assert!(screen.contains("2. Never mind"), "{screen}");
    hn.press(KeyCode::Esc);
    assert!(matches!(hn.app.dialog, Some(Dialog::Rewind { sel: 1 })));
}

#[test]
fn a_local_command_takes_the_placeholder_away() {
    let mut hn = harness();
    assert!(
        hn.render().plain().contains("Try "),
        "a fresh prompt has a placeholder"
    );
    hn.type_str("/status");
    hn.press(KeyCode::Enter);
    hn.press(KeyCode::Esc);
    assert!(!hn.render().plain().contains("Try "));
}

#[test]
fn a_queued_prompt_joins_history_when_it_is_sent() {
    let mut hn = harness();
    hn.type_str("first");
    hn.press(KeyCode::Enter);
    hn.event(Event::TurnStart);
    hn.type_str("queued then taken back");
    hn.press(KeyCode::Enter);
    assert!(!hn.app.prompts.iter().any(|p| p == "queued then taken back"));
    hn.press(KeyCode::Up);
    hn.app.editor.set_text("");
    assert!(!hn.app.prompts.iter().any(|p| p == "queued then taken back"));
}

#[test]
fn typing_during_a_turn_drops_the_interrupt_hint() {
    let mut hn = harness();
    hn.type_str("go");
    hn.press(KeyCode::Enter);
    hn.event(Event::TurnStart);
    assert!(hn.render().plain().contains("esc to interrupt"));
    hn.type_str("next");
    assert!(!hn.render().plain().contains("esc to interrupt"));
}

#[test]
fn a_prompt_sent_right_after_shift_tab_waits_for_the_mode() {
    let mut hn = harness();
    hn.press(KeyCode::BackTab);
    hn.type_str("please write");
    hn.press(KeyCode::Enter);
    assert_eq!(
        hn.sent(),
        [Request::SetMode("accept edits".into())],
        "only the mode change goes out"
    );
    assert!(
        hn.render().plain().contains("accept edits on"),
        "the footer shows the pressed mode"
    );
    mode_confirmed(&mut hn, "accept edits");
    assert_eq!(hn.sent(), [Request::Prompt("please write".into())]);
}

#[test]
fn a_mode_change_wizard_refuses_does_not_wedge_the_queue() {
    let mut hn = harness();
    hn.press(KeyCode::BackTab);
    hn.type_str("please write");
    hn.press(KeyCode::Enter);
    hn.sent();
    // an unrelated config update is not the answer
    let cfg = hn.app.cfg.clone();
    hn.event(Event::ConfigChanged(cfg));
    assert!(hn.app.modes_pending());
    hn.event(Event::Notice {
        level: NoticeLevel::Error,
        text: "Could not change the setting: no such mode".into(),
    });
    assert!(!hn.app.modes_pending());
    assert!(
        hn.sent().is_empty(),
        "the held prompt is not sent in the old mode"
    );
    assert_eq!(
        hn.app.editor.text(),
        "please write",
        "it comes back to the input"
    );
}

#[test]
fn btw_answers_in_a_panel_and_leaves_the_transcript_alone() {
    let mut hn = harness();
    let before = hn.app.blocks.len();
    hn.type_str("/btw what is 2+2");
    hn.press(KeyCode::Enter);
    assert_eq!(hn.sent(), [Request::Prompt("/btw what is 2+2".into())]);
    hn.event(Event::TurnStart);
    hn.event(Event::Notice {
        level: NoticeLevel::Info,
        text: "2 + 2 = 4.".into(),
    });
    hn.event(Event::TurnEnd(StopReason::EndTurn));
    let screen = hn.render().plain();
    assert!(screen.contains("/btw what is 2+2"), "{screen}");
    assert!(screen.contains("2 + 2 = 4."), "{screen}");
    assert!(screen.contains("↑/↓ to scroll · Esc to close"), "{screen}");
    hn.press(KeyCode::Esc);
    assert_eq!(hn.app.blocks.len(), before, "{:?}", hn.app.blocks);
}

#[test]
fn plan_command_enables_plan_mode_through_the_mode_steps() {
    let mut hn = harness();
    hn.app.cfg.modes = vec!["genie".into(), "sovereign".into(), "chat".into()];
    hn.app.cfg.mode = "chat".into();
    hn.app.submit("/plan".into());
    assert_eq!(hn.sent(), [Request::SetMode("genie".into())]);
    assert!(hn.render().plain().contains("Enabled plan mode"));
}

#[test]
fn esc_on_a_plan_keeps_the_effort_notice() {
    let mut hn = harness();
    hn.app.submit("please plan".into());
    hn.event(Event::TurnStart);
    hn.event(Event::Permission(agent_core::PermissionRequest {
        id: "1".into(),
        tool: "exit_plan".into(),
        kind: agent_core::ToolKind::Other,
        title: "Ready to code?".into(),
        input: serde_json::json!({ "plan": "1. x" }),
        diff: None,
        rule: String::new(),
    }));
    hn.press(KeyCode::Esc);
    hn.event(Event::TurnEnd(StopReason::Cancelled));
    assert!(hn.app.notice.is_some(), "{:?}", hn.app.notice);
}

#[test]
fn a_resumed_prompt_and_its_tool_keep_their_gap() {
    let mut hn = harness();
    hn.event(Event::History {
        session_id: "s".into(),
        items: vec![
            agent_core::HistoryItem::User("edit it".into()),
            agent_core::HistoryItem::Tool(agent_core::ToolCall {
                id: "t".into(),
                name: "edit_file".into(),
                kind: agent_core::ToolKind::Edit,
                title: "main.rs".into(),
                input: serde_json::json!({ "path": "main.rs" }),
                status: agent_core::ToolStatus::Failed,
                output: Some("Error: not found".into()),
                diff: None,
                parent_id: None,
            }),
        ],
    });
    let screen = hn.render().plain();
    let lines: Vec<&str> = screen.lines().collect();
    let at = lines
        .iter()
        .position(|l| l.starts_with("❯ edit it"))
        .expect("prompt");
    assert_eq!(lines[at + 1].trim(), "", "{screen}");
}

#[test]
fn ui_tab_completes_the_looks() {
    let mut hn = harness();
    hn.app.editor.set_text("/ui op");
    hn.press(KeyCode::Tab);
    assert_eq!(hn.app.editor.text(), "/ui opencode");
    hn.press(KeyCode::Tab);
    assert_eq!(hn.app.editor.text(), "/ui pi");
}

/// Wizard opens a loaded or rewound session on its config's defaults: the look puts the picked
/// model and plan mode back, and a prompt sent meanwhile waits for both.
#[test]
fn a_reloaded_session_gets_its_model_and_plan_back_before_a_prompt() {
    let mut hn = harness();
    let defaults = hn.app.cfg.clone();
    let picked = hn.app.cfg.models[1].id.clone();
    hn.app.cfg.model = picked.clone();
    hn.app.plan = true;
    hn.type_str("/resume some-session");
    hn.press(KeyCode::Enter);
    assert_eq!(hn.sent(), [Request::LoadSession("some-session".into())]);
    hn.type_str("please write");
    hn.press(KeyCode::Enter);
    assert!(hn.sent().is_empty(), "the prompt waits for the session");
    hn.event(Event::Ready {
        session_id: "some-session".into(),
        config: defaults,
    });
    assert_eq!(hn.sent(), [Request::SetModel(picked.clone())]);
    assert!(
        hn.render().plain().contains("plan mode on"),
        "the footer keeps plan mode"
    );
    let mut cfg = hn.app.cfg.clone();
    cfg.model = picked;
    hn.event(Event::ConfigChanged(cfg));
    assert_eq!(hn.sent(), [Request::Prompt("/plan".into())]);
    plan_answer(&mut hn, "plan mode on");
    assert_eq!(hn.sent(), [Request::Prompt("please write".into())]);
}

/// The side question's turn is not the user's: it starts no spinner, and a prompt sent during
/// it waits for it instead of having its own turn ended by the side question's.
#[test]
fn a_prompt_sent_during_btw_waits_and_keeps_its_turn() {
    let mut hn = harness();
    hn.type_str("/btw what is 2+2");
    hn.press(KeyCode::Enter);
    hn.sent();
    hn.event(Event::TurnStart);
    assert!(hn.app.busy.is_none(), "no spinner for a side question");
    hn.press(KeyCode::Esc);
    hn.type_str("please write");
    hn.press(KeyCode::Enter);
    assert!(hn.sent().is_empty(), "held behind the side question");
    hn.event(Event::TurnEnd(StopReason::EndTurn));
    assert_eq!(hn.sent(), [Request::Prompt("please write".into())]);
    hn.event(Event::TurnStart);
    assert!(hn.app.busy.is_some(), "the prompt's own turn is running");
}

/// shift+tab before the session exists counts, and a prompt sent then waits for it.
#[test]
fn a_prompt_sent_before_the_session_waits_for_early_presses() {
    let (tx, mut requests) = tokio::sync::mpsc::unbounded_channel();
    let (mtx, _msgs) = tokio::sync::mpsc::unbounded_channel();
    let mut app = claudew::app::App::new(Default::default(), tx, mtx);
    app.on_key(crossterm::event::KeyEvent::from(KeyCode::BackTab));
    app.submit("early write".into());
    let mut sent = Vec::new();
    while let Ok(r) = requests.try_recv() {
        sent.push(r);
    }
    assert!(
        !sent.iter().any(|r| matches!(r, Request::Prompt(_))),
        "{sent:?}"
    );
    let config = agent_core::Config {
        mode: "genie".into(),
        modes: vec!["genie".into(), "sovereign".into(), "chat".into()],
        ..Default::default()
    };
    app.on_event(Event::Ready {
        session_id: "s".into(),
        config,
    });
    let first = requests.try_recv().ok();
    assert_eq!(first, Some(Request::SetMode("sovereign".into())));
    assert!(requests.try_recv().is_err(), "the prompt still waits");
}

/// `/plan <text>` turns plan mode on and sends the text once wizard is in it.
#[test]
fn plan_with_text_sends_the_text_in_plan_mode() {
    let mut hn = harness();
    hn.app.submit("/plan refine the approach".into());
    assert_eq!(hn.sent(), [Request::Prompt("/plan".into())]);
    plan_answer(&mut hn, "plan mode on");
    assert_eq!(hn.sent(), [Request::Prompt("refine the approach".into())]);
}

fn wizard_command(hn: &mut Harness, name: &str) {
    hn.app.commands.push(agent_core::SlashCommand {
        name: name.into(),
        description: String::new(),
        input_hint: String::new(),
    });
}

/// A typed `/omakase` is a turn of its own, and its answer is the only sign of the state it
/// put wizard in: it is shown. Only the plan-off line after an approved plan is left out.
#[test]
fn omakase_answers_are_shown_and_the_plan_off_line_is_not() {
    let mut hn = harness();
    wizard_command(&mut hn, "omakase");
    hn.app.submit("/omakase".into());
    assert_eq!(hn.sent(), [Request::Prompt("/omakase".into())]);
    plan_answer(&mut hn, "omakase on — chef's choice");
    assert!(
        hn.render().plain().contains("omakase on"),
        "the answer is shown"
    );
    assert!(hn.app.plan && hn.app.omakase);
    hn.app.submit("please plan".into());
    hn.event(Event::TurnStart);
    hn.event(Event::Notice {
        level: NoticeLevel::Info,
        text: "plan mode off".into(),
    });
    assert!(!hn.render().plain().contains("plan mode off"));
    assert!(!hn.app.plan && !hn.app.omakase);
}

/// A new session gets omakase back, not plain plan mode, so its plan is not put to a review.
#[test]
fn omakase_is_carried_into_a_new_session() {
    let mut hn = harness();
    wizard_command(&mut hn, "omakase");
    hn.app.submit("/omakase".into());
    hn.sent();
    plan_answer(&mut hn, "omakase on — chef's choice");
    let defaults = hn.app.cfg.clone();
    hn.app.submit("/clear".into());
    assert_eq!(hn.sent(), [Request::NewSession]);
    hn.event(Event::Ready {
        session_id: "new".into(),
        config: defaults,
    });
    assert_eq!(hn.sent(), [Request::Prompt("/omakase".into())]);
    plan_answer(&mut hn, "omakase on — chef's choice");
    assert!(hn.app.plan && hn.app.omakase);
}

/// The look does not judge a `/name` it does not run: wizard knows its own commands, the
/// user's, and which are paths, and answers an unknown one itself.
#[test]
fn a_slash_command_the_look_does_not_run_goes_to_wizard() {
    let mut hn = harness();
    hn.app.submit("/zzzfoo".into());
    assert_eq!(hn.sent(), [Request::Prompt("/zzzfoo".into())]);
}

/// A queued command the look answers itself starts no turn, so the queue moves on past it,
/// and a typed effort waits for wizard like a mode change does.
#[test]
fn the_queue_moves_past_a_local_command() {
    let mut hn = harness();
    hn.app.cfg.efforts = vec!["low".into(), "high".into()];
    hn.app.cfg.effort = "high".into();
    hn.app.submit("please wait".into());
    hn.sent();
    hn.event(Event::TurnStart);
    hn.app.submit("/effort low".into());
    hn.app.submit("fourth prompt".into());
    assert!(hn.sent().is_empty());
    hn.event(Event::TurnEnd(StopReason::EndTurn));
    assert_eq!(hn.sent(), [Request::SetEffort("low".into())]);
    let mut cfg = hn.app.cfg.clone();
    cfg.effort = "low".into();
    hn.event(Event::ConfigChanged(cfg));
    assert_eq!(hn.sent(), [Request::Prompt("fourth prompt".into())]);
    assert!(hn.app.queued.is_empty());
}

/// A typed `/model <name>` is echoed as typed.
#[test]
fn a_typed_model_is_echoed_as_typed() {
    let mut hn = harness();
    let name = hn.app.cfg.models[1].name.clone();
    hn.app.submit(format!("/model {name}"));
    assert!(hn.render().plain().contains(&format!("/model {name}")));
}

/// A queued `/model` opens the picker; the prompt queued after it waits for the answer
/// instead of going out on the old model behind it.
#[test]
fn the_queue_waits_for_a_picker_a_queued_command_opened() {
    let mut hn = harness();
    hn.app.submit("please wait".into());
    hn.sent();
    hn.event(Event::TurnStart);
    hn.app.submit("/model".into());
    hn.app.submit("say hi after model".into());
    hn.event(Event::TurnEnd(StopReason::EndTurn));
    assert!(matches!(hn.app.dialog, Some(Dialog::Model { .. })));
    assert!(hn.sent().is_empty(), "nothing goes out behind the picker");
    hn.press(KeyCode::Esc);
    assert_eq!(hn.sent(), [Request::Prompt("say hi after model".into())]);
}

/// A session switch wizard refuses releases what was held for it, with the reason shown.
#[test]
fn a_refused_session_switch_releases_the_queue() {
    let mut hn = harness();
    hn.app.submit("/clear".into());
    assert_eq!(hn.sent(), [Request::NewSession]);
    hn.app.submit("hello again".into());
    assert!(hn.sent().is_empty(), "held for the new session");
    hn.event(Event::Notice {
        level: NoticeLevel::Warn,
        text: "Finish or cancel the running turn before switching sessions.".into(),
    });
    assert_eq!(hn.sent(), [Request::Prompt("hello again".into())]);
    assert!(hn.render().plain().contains("Finish or cancel"));
}
