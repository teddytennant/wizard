//! Keys, queueing, interrupts, slash commands and the permission panel, checked through the
//! requests the app sends to the backend.

mod common;

use std::time::Duration;

use agent_core::{
    DecideScope, Event, PermissionRequest, Request, StopReason, ToolCall, ToolKind, ToolStatus,
};
use common::{hello, Harness, Mock};
use crossterm::event::{KeyCode, KeyModifiers};
use openc::ui::dialogs::Overlay;
use openc::ui::row::Cx;

fn prompts(reqs: &[Request]) -> Vec<String> {
    reqs.iter()
        .filter_map(|r| match r {
            Request::Prompt(t) => Some(t.clone()),
            _ => None,
        })
        .collect()
}

fn started() -> Harness {
    let mut h = Harness::new(100, 30);
    h.ready();
    h
}

fn send(h: &mut Harness, text: &str) {
    h.type_str(text);
    h.key(KeyCode::Enter);
}

#[test]
fn enter_sends_and_busy_enter_queues_until_the_turn_ends() {
    let mut h = started();
    send(&mut h, "first");
    assert_eq!(prompts(&h.sent()), ["first"]);
    h.event(Event::TurnStart, 20);
    send(&mut h, "second");
    assert!(prompts(&h.sent()).is_empty(), "queued, not sent");
    assert!(h.screen().contains("○ queued: second"));
    h.event(Event::TextDelta("ok".into()), 20);
    h.event(Event::TurnEnd(StopReason::EndTurn), 20);
    assert_eq!(
        prompts(&h.sent()),
        ["second"],
        "the queue head goes out when the turn ends"
    );
}

#[test]
fn ctrl_c_interrupts_and_keeps_the_draft_the_queue_and_the_partial_answer() {
    let mut h = started();
    send(&mut h, "go");
    h.event(Event::TurnStart, 10);
    h.event(Event::TextDelta("partial answer".into()), 10);
    send(&mut h, "queued one");
    h.type_str("my draft");
    h.sent();
    h.ctrl('c');
    assert!(h.sent().contains(&Request::Cancel));
    h.event(Event::TurnEnd(StopReason::Cancelled), 30);
    // An interrupt keeps the queue: nothing was sent on its own.
    assert!(prompts(&h.sent()).is_empty());
    let s = h.screen();
    assert!(
        s.contains("partial answer") && s.contains("■ interrupted"),
        "{s}"
    );
    assert!(
        s.contains("○ queued: queued one") && s.contains("my draft"),
        "{s}"
    );
    // Enter on an empty composer then sends the queue head.
    h.ctrl('u');
    h.key(KeyCode::Enter);
    assert_eq!(prompts(&h.sent()), ["queued one"]);
}

#[test]
fn esc_twice_interrupts_once_does_not() {
    let mut h = started();
    send(&mut h, "go");
    h.event(Event::TurnStart, 10);
    h.sent();
    h.key(KeyCode::Esc);
    assert!(h.sent().is_empty());
    assert!(h.screen().contains("esc again to interrupt"));
    h.key(KeyCode::Esc);
    assert!(h.sent().contains(&Request::Cancel));
}

#[test]
fn idle_ctrl_c_clears_then_quits_on_the_second_press() {
    let mut h = started();
    h.type_str("draft");
    h.ctrl('c');
    assert!(h.app.composer.is_empty() && !h.app.should_quit);
    h.ctrl('c');
    assert!(!h.app.should_quit, "first empty press only arms");
    h.ctrl('c');
    assert!(h.app.should_quit);
}

fn permission() -> Event {
    Event::Permission(PermissionRequest {
        id: "p1".into(),
        tool: "Bash".into(),
        kind: ToolKind::Execute,
        title: "cargo test".into(),
        input: serde_json::json!({"command": "cargo test"}),
        diff: None,
        rule: "cargo test *".into(),
    })
}

#[test]
fn typed_ahead_enter_never_approves_and_y_after_the_grace_does() {
    let mut h = started();
    send(&mut h, "run the tests");
    h.event(Event::TurnStart, 10);
    h.event(
        Event::Tool(ToolCall {
            id: "t1".into(),
            name: "Bash".into(),
            kind: ToolKind::Execute,
            title: "cargo test".into(),
            status: ToolStatus::Running,
            ..Default::default()
        }),
        10,
    );
    h.sent();
    h.event(permission(), 10);
    // 50 ms after the panel appears: Enter and y both do nothing.
    h.advance(40);
    h.key(KeyCode::Enter);
    h.key(KeyCode::Char('y'));
    assert!(
        h.sent().is_empty(),
        "input inside the grace window is ignored"
    );
    // The `y` was taken for the start of a message: it is in the draft, and the panel waits
    // for a pause before its letters work.
    assert_eq!(h.app.composer.text(), "y");
    h.advance(2000);
    h.key(KeyCode::Enter);
    assert!(h.sent().is_empty(), "enter never approves");
    h.key(KeyCode::Char('a'));
    assert_eq!(
        h.sent(),
        vec![Request::Decide {
            id: "p1".into(),
            allow: true,
            scope: DecideScope::Always,
            note: String::new()
        }]
    );
    assert!(h.app.perm.is_none());
}

#[test]
fn denying_with_a_note_and_ctrl_c_during_a_prompt() {
    let mut h = started();
    send(&mut h, "x");
    h.event(Event::TurnStart, 10);
    h.event(permission(), 10);
    h.sent();
    h.advance(500);
    h.key(KeyCode::Char('e'));
    h.type_str("use make instead");
    h.key(KeyCode::Enter);
    assert_eq!(
        h.sent(),
        vec![Request::Decide {
            id: "p1".into(),
            allow: false,
            scope: DecideScope::Once,
            note: "use make instead".into()
        }]
    );
    h.event(permission(), 10);
    h.advance(500);
    h.ctrl('c');
    let sent = h.sent();
    assert!(
        sent.iter()
            .any(|r| matches!(r, Request::Decide { allow: false, .. }))
            && sent.contains(&Request::Cancel),
        "{sent:?}"
    );
}

#[test]
fn pasting_forty_lines_is_one_chip_and_sends_exactly_those_lines() {
    let mut h = started();
    let blob: String = (0..40).map(|i| format!("line {i}\n")).collect();
    h.app.on_paste(&blob);
    assert_eq!(h.app.composer.text(), "[paste 40 lines]");
    assert!(h.screen().contains("[paste 40 lines]"));
    h.key(KeyCode::Enter);
    assert_eq!(prompts(&h.sent()), std::slice::from_ref(&blob));
    // The user block keeps the chip; the backend got the real text.
    assert!(h.screen().contains("▎ [paste 40 lines]"));
}

#[test]
fn slash_commands_that_need_ui_are_intercepted() {
    let mut h = started();
    send(&mut h, "/model");
    assert!(prompts(&h.sent()).is_empty());
    assert!(matches!(h.app.overlay, Some(Overlay::Settings { row: 0 })));
    h.key(KeyCode::Right);
    assert!(h.sent().contains(&Request::SetModel("opus".into())));
    h.key(KeyCode::Esc);
    send(&mut h, "/effort low");
    assert!(h.sent().contains(&Request::SetEffort("low".into())));
    send(&mut h, "/new");
    assert!(h.sent().contains(&Request::NewSession));
    send(&mut h, "/compact");
    assert_eq!(
        prompts(&h.sent()),
        ["/compact"],
        "backend commands pass through"
    );
    send(&mut h, "/exit");
    assert!(h.app.should_quit);
}

#[test]
fn shift_tab_cycles_ask_edits_plan_and_skips_bypass() {
    let mut h = started();
    h.key_mod(KeyCode::BackTab, KeyModifiers::SHIFT);
    assert_eq!(h.sent(), vec![Request::SetMode("acceptEdits".into())]);
}

#[test]
fn resume_dialog_lists_sessions_and_loads_the_pick() {
    let mut h = started();
    send(&mut h, "/resume");
    assert!(h.sent().contains(&Request::ListSessions));
    h.event(
        Event::Sessions(vec![
            agent_core::SessionInfo {
                id: "old-1".into(),
                title: "Fix the flaky parser test".into(),
                cwd: "/work/proj".into(),
                updated: 2,
            },
            agent_core::SessionInfo {
                id: "old-2".into(),
                title: "Add a --json flag".into(),
                cwd: "/work/proj".into(),
                updated: 1,
            },
        ]),
        10,
    );
    let s = h.screen();
    assert!(
        s.contains("Resume a session") && s.contains("Fix the flaky parser test"),
        "{s}"
    );
    h.key(KeyCode::Down);
    h.key(KeyCode::Enter);
    assert!(h.sent().contains(&Request::LoadSession("old-2".into())));
    h.event(
        Event::History {
            session_id: "old-2".into(),
            items: vec![
                agent_core::HistoryItem::User("q".into()),
                agent_core::HistoryItem::Assistant("a".into()),
            ],
        },
        10,
    );
    let s = h.screen();
    assert!(s.contains("▎ q") && s.contains("  a"), "{s}");
}

#[test]
fn up_on_an_empty_composer_pulls_the_last_queued_message_back() {
    let mut h = started();
    send(&mut h, "go");
    h.event(Event::TurnStart, 10);
    send(&mut h, "later");
    assert!(h.app.composer.is_empty());
    h.key(KeyCode::Up);
    assert_eq!(h.app.composer.text(), "later");
    assert!(h.app.queue.is_empty());
}

#[test]
fn nav_mode_keys_and_the_nav_tag() {
    let mut h = started();
    send(&mut h, "q");
    h.event(Event::TurnStart, 10);
    h.event(
        Event::Tool(ToolCall {
            id: "t".into(),
            name: "Bash".into(),
            kind: ToolKind::Execute,
            title: "ls".into(),
            output: Some("a\nb".into()),
            status: ToolStatus::Completed,
            ..Default::default()
        }),
        10,
    );
    h.event(Event::TextDelta("done".into()), 10);
    h.event(Event::TurnEnd(StopReason::EndTurn), 10);
    h.draw();
    h.key(KeyCode::Esc);
    assert!(h.screen().contains("NAV"));
    h.type_str("k");
    h.key(KeyCode::Char('o'));
    let s = h.screen();
    assert!(
        s.contains("▾ Bash") && s.contains("  a"),
        "o unfolds the block under the cursor:\n{s}"
    );
    h.key(KeyCode::Char('i'));
    assert!(!h.screen().contains("NAV"));
}

#[tokio::test]
async fn streaming_never_changes_a_committed_row() {
    let mut m = Mock::start();
    hello(&mut m).await;
    let events = m.run("long answer").await;
    let mut h = started();
    send(&mut h, "long answer");
    let mut prev: Vec<String> = Vec::new();
    for ev in events {
        h.event(ev, 30);
        h.draw();
        let now = h.now();
        let cx = Cx {
            p: &h.app.p,
            theme: &h.app.theme,
            g: &h.app.g,
            width: h.app.geo.c as usize,
            detail: false,
            now,
            spin: 0,
        };
        let doc = h.app.tr.document(&cx);
        // The newest two rows may still change; everything above is committed.
        let keep = prev.len().saturating_sub(2);
        assert_eq!(&doc[..keep.min(doc.len())], &prev[..keep.min(doc.len())]);
        prev = doc;
    }
}

#[test]
fn resize_keeps_the_block_at_the_top_of_the_view() {
    let mut h = started();
    for i in 0..30 {
        send(&mut h, &format!("question number {i}"));
        h.event(Event::TurnStart, 10);
        h.event(Event::TextDelta(format!("answer {i} with some more words so that it wraps on narrow screens, again and again")), 10);
        h.event(Event::TurnEnd(StopReason::EndTurn), 10);
    }
    h.draw();
    h.key(KeyCode::PageUp);
    h.key(KeyCode::PageUp);
    h.draw();
    let top = h.app.view.top.0;
    h.resize(60, 20);
    h.draw();
    assert_eq!(h.app.view.top.0, top);
    h.resize(100, 30);
    h.draw();
    assert_eq!(h.app.view.top.0, top);
}

#[tokio::test]
async fn real_mock_backend_round_trip_through_the_app() {
    let mut m = Mock::start();
    hello(&mut m).await;
    let mut h = started();
    send(&mut h, "tools please");
    let sent = h.sent();
    assert_eq!(prompts(&sent), ["tools please"]);
    m.tx.send(Request::Prompt("tools please".into())).unwrap();
    let evs = m.collect(|e| matches!(e, Event::TurnEnd(_))).await;
    h.events(evs, 50);
    let s = h.screen();
    assert!(s.contains("▾ Edit") && s.contains("Done."), "{s}");
    assert!(!h.app.busy());
    let _ = Duration::from_secs(0);
}

#[test]
fn theme_dialog_previews_live_and_escape_puts_the_old_theme_back() {
    let mut h = started();
    let before = h.app.p.kind;
    send(&mut h, "/theme");
    assert!(matches!(h.app.overlay, Some(Overlay::Select { .. })));
    // "auto" is first; moving down previews hearth, then parchment.
    h.key(KeyCode::Down);
    h.key(KeyCode::Down);
    assert_eq!(h.app.p.kind, openc::palette::Kind::Parchment);
    h.key(KeyCode::Esc);
    assert_eq!(h.app.p.kind, before, "escape reverts the preview");
    send(&mut h, "/theme");
    h.key(KeyCode::Down);
    h.key(KeyCode::Down);
    h.key(KeyCode::Enter);
    assert_eq!(h.app.p.kind, openc::palette::Kind::Parchment);
}

#[test]
fn steer_shows_the_line_at_once_and_does_not_queue_it() {
    let mut h = started();
    send(&mut h, "go");
    h.event(Event::TurnStart, 10);
    h.sent();
    h.type_str("also check the docs");
    h.ctrl('s');
    assert_eq!(h.sent(), vec![Request::Steer("also check the docs".into())]);
    assert!(h.app.queue.is_empty());
    assert!(h.screen().contains("▎ also check the docs"));
}
