//! What keys do: sending, queueing, interrupting, quitting, completing commands, scrolling.

mod common;

use agent_core::{Event, Request, StopReason};
use common::*;
use crossterm::event::{KeyCode, KeyModifiers};
use jcodew::app::Overlay;

#[test]
fn enter_sends_the_prompt_and_numbers_the_next_one() {
    let mut h = Harness::new(120, 40);
    assert!(h.row_starting("1>").is_some());
    h.say("hello");
    assert_eq!(h.sent(), [Request::Prompt("hello".into())]);
    // while the turn runs the composer shows the next number and an ellipsis
    assert!(h.row_starting("2…").is_some());
    h.begin();
    h.event(Event::TextDelta("hi".into()));
    h.event(Event::TurnEnd(StopReason::EndTurn));
    assert!(h.row_starting("2>").is_some());
}

#[test]
fn the_composer_sits_under_the_transcript_until_the_screen_fills() {
    let mut h = Harness::new(120, 40);
    let (y, _) = h.row_starting("1>").unwrap();
    assert!(
        y < 30,
        "packed: the composer is at row {y}, not at the bottom"
    );
    h.turn("fake:long");
    let (y, _) = h.row_starting("2>").unwrap();
    assert_eq!(
        y, 38,
        "scrolling: the composer holds the second to last row"
    );
}

#[test]
fn a_prompt_typed_during_a_turn_waits_in_the_queue() {
    let mut h = Harness::new(120, 40);
    h.say("fake:slow");
    h.begin();
    h.sent();
    h.say("next one");
    assert!(h.sent().is_empty(), "nothing is sent while the turn runs");
    assert_eq!(h.app.queue, ["next one"]);
    assert!(h.text().contains("2 ⏳ next one"));
    assert!(h.row_starting("3…").is_some());
    h.event(Event::TurnEnd(StopReason::EndTurn));
    assert_eq!(h.sent(), [Request::Prompt("next one".into())]);
    assert!(h.app.queue.is_empty());
}

#[test]
fn ctrl_up_brings_queued_prompts_back() {
    let mut h = Harness::new(120, 40);
    h.say("fake:slow");
    h.begin();
    h.say("one");
    h.say("two");
    h.key(KeyCode::Up, KeyModifiers::CONTROL);
    assert_eq!(h.app.ed.text(), "one\n\ntwo");
    assert!(h.app.queue.is_empty());
    assert!(h
        .text()
        .contains("Retrieved 2 pending messages for editing"));
}

#[test]
fn escape_interrupts_a_turn_and_clears_the_input_when_idle() {
    let mut h = Harness::new(120, 40);
    h.say("fake:slow");
    h.begin();
    h.sent();
    h.press(KeyCode::Esc);
    assert_eq!(h.sent(), [Request::Cancel]);
    assert!(h.text().contains("Interrupting..."));
    h.event(Event::TurnEnd(StopReason::Cancelled));
    h.type_str("draft");
    h.press(KeyCode::Esc);
    assert!(h.app.ed.is_empty());
    assert!(h.text().contains("Input cleared - Ctrl+Z to restore"));
}

#[test]
fn ctrl_c_twice_quits_and_once_only_arms() {
    let mut h = Harness::new(120, 40);
    h.ctrl('c');
    assert!(!h.app.quit);
    assert!(h.text().contains("Press Ctrl+C again to quit"));
    h.advance(2_500);
    h.app.on_tick();
    h.ctrl('c');
    assert!(!h.app.quit, "the window had closed");
    h.ctrl('c');
    assert!(h.app.quit);
}

#[test]
fn ctrl_c_during_a_turn_interrupts_instead() {
    let mut h = Harness::new(120, 40);
    h.say("fake:slow");
    h.sent();
    h.ctrl('c');
    assert_eq!(h.sent(), [Request::Cancel]);
    assert!(!h.app.quit);
}

#[test]
fn enter_completes_a_partial_command_before_running_it() {
    let mut h = Harness::new(120, 40);
    h.type_str("/cle");
    h.press(KeyCode::Enter);
    assert_eq!(h.app.ed.text(), "/clear");
    assert!(h.sent().is_empty());
    h.press(KeyCode::Enter);
    assert_eq!(h.sent(), [Request::NewSession]);
    assert!(h.text().contains("Session cleared"));
}

#[test]
fn suggestions_rank_prefix_matches_by_length_then_name() {
    let mut h = Harness::new(120, 40);
    h.type_str("/");
    let text = h.text();
    let rows: Vec<&str> = text
        .lines()
        .filter(|l| l.starts_with('/'))
        .map(|l| l.split_whitespace().next().unwrap())
        .collect();
    assert_eq!(rows[..4], ["/?", "/cls", "/cost", "/diff"]);
    assert_eq!(rows.len(), 8);
    assert!(text.contains(" more"));
    h.press(KeyCode::Down);
    h.press(KeyCode::Enter);
    assert_eq!(h.app.ed.text(), "/cls");
}

#[test]
fn tab_completes_and_walks_the_matches() {
    let mut h = Harness::new(120, 40);
    h.type_str("/mo");
    h.press(KeyCode::Tab);
    assert_eq!(h.app.ed.text(), "/mode");
    h.press(KeyCode::Tab);
    assert_eq!(h.app.ed.text(), "/model");
}

#[test]
fn tab_after_ui_offers_the_looks() {
    let mut h = Harness::new(120, 40);
    h.type_str("/ui op");
    h.press(KeyCode::Tab);
    assert_eq!(h.app.ed.text(), "/ui opencode");
}

#[test]
fn a_command_wizard_owns_goes_to_wizard_as_typed() {
    let mut h = Harness::new(120, 40);
    h.say("/compact");
    assert_eq!(h.sent(), [Request::Prompt("/compact".into())]);
    // a command is not a prompt: it gets no number and does not wait for a turn
    assert!(h.row_starting("1>").is_some());
    h.say("/ui grok");
    assert_eq!(h.sent(), [Request::Prompt("/ui grok".into())]);
}

#[test]
fn a_new_session_keeps_the_model_wizard_switched_to() {
    let mut h = Harness::new(120, 40);
    h.app.chosen_model = Some("alpha/a".into());
    let mut c = h.app.config.clone();
    c.model = "beta/b".into();
    h.event(Event::ConfigChanged(c.clone()));
    h.event(Event::Ready {
        session_id: "s2".into(),
        config: c,
    });
    assert_eq!(h.sent(), [Request::SetModel("beta/b".into())]);
}

#[test]
fn switching_to_another_look_quits_this_one() {
    let mut h = Harness::new(120, 40);
    h.event(Event::Notice {
        level: agent_core::NoticeLevel::Info,
        text: "saved [ui] skin = \"jcode\". Quit this look to switch now.".into(),
    });
    assert!(!h.app.quit, "it is already this look");
    h.event(Event::Notice {
        level: agent_core::NoticeLevel::Info,
        text: "saved [ui] skin = \"grok\". Quit this look to switch now.".into(),
    });
    assert!(h.app.quit);
}

#[test]
fn the_model_list_filters_and_picks() {
    let mut h = Harness::new(120, 40);
    h.type_str("/model");
    assert_eq!(h.app.ed.text(), "/model ", "a bare /model opens the list");
    let text = h.text();
    assert!(text.contains("▸ grok-4.7 current"));
    assert!(text.contains("↑↓ choose · Enter select · Esc cancel"));
    h.type_str("fast");
    let text = h.text();
    assert!(
        text.contains("▸ grok-4.7-fast") && !text.contains("grok-4.6"),
        "{text}"
    );
    h.press(KeyCode::Enter);
    assert_eq!(h.sent(), [Request::SetModel("grok-4.7-fast".into())]);
    assert!(h.app.ed.is_empty());
    assert!(h.text().contains("Model → grok-4.7-fast via fake"));
}

#[test]
fn up_walks_the_prompt_history_only_from_an_empty_composer() {
    let mut h = Harness::new(120, 40);
    h.turn("fake:text");
    h.turn("fake:bash");
    h.press(KeyCode::Up);
    assert_eq!(h.app.ed.text(), "fake:bash");
    h.press(KeyCode::Up);
    assert_eq!(h.app.ed.text(), "fake:text");
    h.press(KeyCode::Down);
    assert_eq!(h.app.ed.text(), "fake:bash");
    h.press(KeyCode::Down);
    assert!(h.app.ed.is_empty());
    h.type_str("draft");
    h.press(KeyCode::Up);
    assert_eq!(h.app.ed.text(), "draft", "a draft is not replaced");
}

#[test]
fn page_keys_scroll_and_escape_returns_to_the_tail() {
    let mut h = Harness::new(120, 40);
    h.turn("fake:long");
    h.render();
    let top = h.app.view.total - h.app.view.height;
    h.press(KeyCode::PageUp);
    assert_eq!(h.app.scroll, Some(top - 10));
    // the prompt that scrolled out of view is pinned to the first row
    assert!(h.text().lines().next().unwrap().contains("1› fake:long"));
    h.press(KeyCode::PageDown);
    assert_eq!(h.app.scroll, None);
    h.press(KeyCode::PageUp);
    h.press(KeyCode::Esc);
    assert_eq!(h.app.scroll, None);
}

#[test]
fn ctrl_k_and_ctrl_j_jump_between_prompts() {
    let mut h = Harness::new(120, 40);
    h.turn("fake:text");
    h.turn("fake:long");
    h.render();
    let prompts = h.app.view.prompts.clone();
    assert_eq!(prompts.len(), 2);
    // a prompt lands under the pinned copy of the one before it
    h.ctrl('k');
    assert_eq!(h.app.scroll, Some(prompts[1] - 1));
    h.ctrl('k');
    assert_eq!(h.app.scroll, Some(prompts[0]));
    h.ctrl('j');
    assert_eq!(h.app.scroll, Some(prompts[1] - 1));
    h.ctrl('j');
    assert_eq!(
        h.app.scroll, None,
        "past the last prompt it follows the tail again"
    );
}

#[test]
fn ctrl_k_kills_to_the_end_of_a_non_empty_line() {
    let mut h = Harness::new(120, 40);
    h.type_str("hello world");
    for _ in 0..5 {
        h.press(KeyCode::Left);
    }
    h.ctrl('k');
    assert_eq!(h.app.ed.text(), "hello ");
    h.ctrl('u');
    assert!(h.app.ed.is_empty());
}

#[test]
fn stash_and_restore() {
    let mut h = Harness::new(120, 40);
    h.type_str("keep this");
    h.ctrl('s');
    assert!(h.app.ed.is_empty());
    assert!(h.text().contains("📋 Input stashed"));
    h.ctrl('s');
    assert_eq!(h.app.ed.text(), "keep this");
}

#[test]
fn a_trailing_backslash_makes_enter_a_newline() {
    let mut h = Harness::new(120, 40);
    h.type_str("one\\");
    h.press(KeyCode::Enter);
    assert_eq!(h.app.ed.text(), "one\n");
    assert!(h.sent().is_empty());
}

#[test]
fn help_scrolls_and_closes() {
    let mut h = Harness::new(80, 24);
    h.say("/help");
    assert!(matches!(h.app.overlay, Some(Overlay::Help { scroll: 0 })));
    assert!(h.text().contains(" Help "));
    h.press(KeyCode::PageDown);
    h.render();
    assert!(matches!(h.app.overlay, Some(Overlay::Help { scroll }) if scroll > 0));
    h.press(KeyCode::Esc);
    assert!(h.app.overlay.is_none());
}

#[test]
fn the_session_picker_loads_the_chosen_session() {
    let mut h = Harness::new(120, 40);
    h.say("/resume");
    assert_eq!(h.sent(), [Request::ListSessions]);
    h.event(Event::Sessions(jcodew::fake::sessions(
        std::path::Path::new(CWD),
    )));
    assert!(h.text().contains("Fix the flaky parser test"));
    h.press(KeyCode::Down);
    h.press(KeyCode::Enter);
    assert_eq!(h.sent(), [Request::LoadSession("fake-session-2".into())]);
    assert!(h.app.overlay.is_none());
}

#[test]
fn a_running_call_is_named_on_the_status_line_not_in_the_transcript() {
    let mut h = Harness::new(120, 40);
    h.say("fake:bash");
    h.begin();
    let steps = jcodew::fake::steps(std::path::Path::new(CWD), "fake:bash");
    let evs = jcodew::fake::events(&steps[1]);
    h.event(evs[0].clone());
    let text = h.text();
    assert!(text.contains(" running bash · $ printf"), "{text}");
    assert!(!text.contains("✓ bash"));
    h.event(evs[1].clone());
    assert!(h.text().contains("  ✓ bash $ printf"));
}

#[test]
fn the_view_clears_without_losing_the_session() {
    let mut h = Harness::new(120, 40);
    h.turn("fake:text");
    h.ctrl('l');
    let text = h.text();
    assert!(!text.contains("Hello from the scripted model"));
    assert!(text.contains("View cleared (context kept)"));
    assert!(h.sent().iter().all(|r| !matches!(r, Request::NewSession)));
    assert!(h.row_starting("2>").is_some());
}

#[test]
fn a_tiny_terminal_draws_nothing_rather_than_panicking() {
    for (w, h) in [(1, 1), (3, 3), (10, 4), (20, 5)] {
        let mut hn = Harness::new(w, h);
        hn.turn("fake:markdown");
        hn.type_str("/");
        hn.render();
    }
}
