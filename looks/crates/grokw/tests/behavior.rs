//! Keys, commands and the requests they send, with no terminal.

mod common;

use agent_core::{Event, Request, StopReason};
use common::*;
use crossterm::event::{KeyCode, KeyModifiers};

#[test]
fn cancelled_turn_prints_its_marker() {
    let mut h = Harness::new(120, 36);
    h.app.send_prompt("go".into());
    h.event(Event::TurnStart);
    h.event(Event::TextDelta("working".into()));
    h.event(Event::TurnEnd(StopReason::Cancelled));
    h.took(36.0);
    let t = h.text();
    assert!(t.contains("Turn cancelled by user in 36s."), "{t}");
}

#[test]
fn ctrl_c_cancels_a_running_turn_and_clears_the_queue() {
    let mut h = Harness::new(120, 36);
    h.app.send_prompt("go".into());
    h.sent();
    h.type_str("next");
    h.press(KeyCode::Enter);
    assert_eq!(h.app.queue, ["next"]);
    h.ctrl('c');
    assert!(h.app.queue.is_empty());
    assert!(matches!(h.sent().as_slice(), [Request::Cancel]));
}

#[test]
fn enter_while_idle_sends_and_while_busy_queues() {
    let mut h = Harness::new(120, 36);
    h.type_str("hello");
    h.press(KeyCode::Enter);
    assert!(matches!(h.sent().as_slice(), [Request::Prompt(p)] if p == "hello"));
    h.type_str("again");
    h.press(KeyCode::Enter);
    assert!(h.sent().is_empty());
    assert_eq!(h.app.queue, ["again"]);
    // the turn ends: the queued prompt goes out
    h.event(Event::TurnStart);
    h.event(Event::TurnEnd(StopReason::EndTurn));
    assert!(matches!(h.sent().as_slice(), [Request::Prompt(p)] if p == "again"));
}

#[test]
fn double_ctrl_q_quits_and_any_other_key_disarms() {
    let mut h = Harness::new(120, 36);
    h.ctrl('q');
    assert!(!h.app.quit);
    assert!(h.text().contains("Ctrl+q:press again to quit"));
    h.type_str("x");
    h.ctrl('q');
    assert!(!h.app.quit);
    h.ctrl('q');
    assert!(h.app.quit);
}

#[test]
fn esc_mid_turn_toasts_and_never_cancels() {
    let mut h = Harness::new(120, 36);
    h.app.send_prompt("go".into());
    h.sent();
    h.press(KeyCode::Esc);
    assert!(h.sent().is_empty());
    assert!(h.text().contains("Press Ctrl+c to cancel the turn"));
}

#[test]
fn model_command_sets_the_model_and_effort() {
    let mut h = Harness::new(120, 36);
    h.type_str("/model Grok 4.6 low");
    h.press(KeyCode::Enter);
    let sent = h.sent();
    assert!(
        matches!(&sent[0], Request::SetModel(m) if m == "xai/grok-4.6"),
        "{sent:?}"
    );
    assert!(
        matches!(&sent[1], Request::SetEffort(e) if e == "low"),
        "{sent:?}"
    );
}

#[test]
fn unknown_slash_text_goes_to_the_model_as_a_prompt() {
    let mut h = Harness::new(120, 36);
    h.type_str("/zzz what");
    h.press(KeyCode::Enter);
    assert!(matches!(h.sent().as_slice(), [Request::Prompt(p)] if p == "/zzz what"));
}

#[test]
fn unsupported_commands_say_so() {
    let mut h = Harness::new(120, 36);
    h.type_str("/delete");
    h.press(KeyCode::Enter);
    h.press(KeyCode::Enter);
    assert!(h
        .text()
        .contains("Deleting a session is not supported by wizard"));
}

#[test]
fn new_session_clears_the_transcript() {
    let mut h = Harness::new(120, 36);
    h.turn("hi", "hello");
    h.type_str("/new");
    h.press(KeyCode::Enter);
    h.press(KeyCode::Enter);
    let sent = h.sent();
    assert!(
        sent.iter().any(|r| matches!(r, Request::NewSession)),
        "{sent:?}"
    );
    assert!(!h.text().contains("hello"));
}

#[test]
fn shift_enter_inserts_a_newline() {
    let mut h = Harness::new(120, 36);
    h.type_str("a");
    h.key(KeyCode::Enter, KeyModifiers::SHIFT);
    h.type_str("b");
    assert_eq!(h.app.ed.text(), "a\nb");
}

#[test]
fn first_key_on_home_leaves_home_with_the_key() {
    let mut h = Harness::home(120, 36);
    h.type_str("h");
    assert_eq!(h.app.screen, grokw::app::Screen::Session);
    assert_eq!(h.app.ed.text(), "h");
}

#[test]
fn idle_screens_do_not_ask_the_loop_to_wake() {
    // an idle agent screen parks the loop; a running turn asks for ticks
    let mut h = Harness::new(120, 36);
    h.app.clock = grokw::ui::anim::Clock::new();
    h.render();
    assert!(h.app.next_wake(std::time::Instant::now()).is_none());
    h.app.send_prompt("go".into());
    h.render();
    assert!(h.app.next_wake(std::time::Instant::now()).is_some());
}

#[test]
fn a_session_switch_puts_the_chosen_model_back() {
    let mut h = Harness::new(120, 36);
    h.type_str("/model Grok 4.6 low");
    h.press(KeyCode::Enter);
    h.sent();
    // wizard answers a session switch with its default config
    h.event(Event::Ready {
        session_id: "next".into(),
        config: config(),
    });
    let sent = h.sent();
    assert!(
        matches!(&sent[0], Request::SetModel(m) if m == "xai/grok-4.6"),
        "{sent:?}"
    );
    assert!(
        matches!(&sent[1], Request::SetEffort(e) if e == "low"),
        "{sent:?}"
    );
}

#[test]
fn replayed_messages_carry_no_timestamp() {
    let mut h = Harness::new(120, 36);
    h.event(Event::History {
        session_id: "old".into(),
        items: vec![
            agent_core::HistoryItem::User("hi".into()),
            agent_core::HistoryItem::Assistant("hello".into()),
        ],
    });
    let t = h.text();
    assert!(!t.contains("PM") && !t.contains("AM"), "{t}");
}

/// No terminal size, however small, may panic a draw: debug builds trap every u16 underflow.
#[test]
fn tiny_and_odd_sizes_never_panic() {
    let sizes = [
        (1, 1),
        (2, 2),
        (5, 3),
        (10, 4),
        (12, 6),
        (20, 8),
        (24, 10),
        (30, 12),
        (40, 14),
        (60, 16),
        (79, 23),
        (89, 30),
        (90, 30),
        (200, 60),
        (300, 100),
    ];
    for (w, h) in sizes {
        // home, with the picker open
        let mut hn = Harness::home(w, h);
        hn.text();
        hn.ctrl('r');
        hn.event(Event::Sessions(vec![session("a", "say hi", 1_790_003_600)]));
        hn.text();
        // a session with a long turn, a popup, the palette, the todo pane and a toast
        let mut hn = Harness::new(w, h);
        hn.long_turn(5);
        hn.text();
        hn.type_str("/mo");
        hn.text();
        hn.press(KeyCode::Esc);
        hn.ctrl('u');
        hn.press(KeyCode::Tab);
        hn.press(KeyCode::Up);
        hn.press(KeyCode::Right);
        hn.text();
        hn.press(KeyCode::Tab);
        hn.ctrl('p');
        hn.text();
        hn.press(KeyCode::Esc);
        hn.press(KeyCode::Esc);
        hn.app.compact_mode = true;
        hn.text();
        hn.app
            .toast("a toast that is rather long for a narrow window");
        hn.text();
        hn.app.send_prompt("again".into());
        hn.event(Event::TurnStart);
        hn.event(Event::ThoughtDelta("thinking about it\n\nmore".into()));
        hn.text();
        // the todo pane, the in-session resume modal and the file picker
        hn.event(Event::Todos(vec![agent_core::Todo {
            text: "one".into(),
            status: agent_core::TodoStatus::Pending,
        }]));
        hn.ctrl('t');
        hn.text();
        hn.press(KeyCode::Esc);
        hn.ctrl('t');
        hn.app.files = vec!["a.rs".into(), "src/".into()];
        hn.app.ed.clear();
        hn.type_str("see @");
        hn.text();
        hn.app.ed.clear();
        hn.ctrl('r');
        hn.event(Event::Sessions(vec![session("a", "say hi", 1_790_003_600)]));
        hn.text();
    }
}

#[test]
fn a_dropped_stream_shows_as_a_retry_on_the_turn_row_only() {
    let mut h = Harness::new(120, 36);
    h.app.send_prompt("go".into());
    h.event(Event::TurnStart);
    h.event(Event::TextDelta("partial".into()));
    h.event(Event::Notice {
        level: agent_core::NoticeLevel::Warn,
        text: "the response stream dropped; it restarts below".into(),
    });
    let t = h.text();
    assert!(
        t.contains("Connection failed | Retrying (attempt 1)..."),
        "{t}"
    );
    assert!(
        !t.contains("stream dropped"),
        "the transcript keeps no trace:\n{t}"
    );
    // new text from the retried stream ends the retry label
    h.event(Event::TextDelta(" more".into()));
    assert!(h.text().contains("Responding…"));
}

#[test]
fn multiline_puts_its_name_in_the_border() {
    let mut h = Harness::new(120, 36);
    h.type_str("/multiline");
    h.press(KeyCode::Enter);
    h.press(KeyCode::Enter);
    let t = h.text();
    assert!(t.contains("Grok 4.7 (high) ─multiline ─╯"), "{t}");
    assert!(t.contains("✓ Multiline: on"));
}

#[test]
fn compact_mode_drops_prompt_padding_and_prefix() {
    let mut h = Harness::new(120, 36);
    h.app.compact_mode = true;
    h.turn("hello", "hi");
    let t = h.text();
    let rows: Vec<&str> = t.lines().collect();
    // header on row 0, the prompt on row 1 with no `❯`
    assert!(rows[0].contains("main"), "{t}");
    assert!(rows[1].trim_start().starts_with("hello"), "{t}");
    assert!(!rows[1].contains('❯'));
}

#[test]
fn bang_runs_a_shell_command_without_blocking_and_prints_its_output() {
    let mut h = Harness::new(120, 36);
    h.type_str("!");
    assert!(h.text().contains("Run shell command"));
    h.type_str("echo hi");
    h.press(KeyCode::Enter);
    // nothing was sent to the model, and the composer left shell mode
    assert!(h.sent().is_empty());
    assert!(!h.app.shell_mode);
    h.app.update(grokw::app::Msg::Shell {
        id: "bang-1".into(),
        cmd: "echo hi".into(),
        output: "hi\n".into(),
        ok: true,
    });
    let t = h.text();
    assert!(t.contains("Run (user) echo hi"), "{t}");
    assert!(t.contains("$ echo hi") && t.contains("\n  ┃  hi"), "{t}");
}
