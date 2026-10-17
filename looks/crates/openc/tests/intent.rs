//! Keys follow intent. Letters typed for the next message must never answer a permission
//! prompt, whatever the timing; a popup shows what Enter will do; the wheel does not change
//! which mode the keyboard is in. Round 1 interaction findings 2, 5 and 6.

mod common;

use agent_core::{Event, PermissionRequest, Request, ToolKind};
use common::Harness;
use crossterm::event::{KeyCode, KeyModifiers, MouseEvent, MouseEventKind};
use openc::app::Focus;

fn started() -> Harness {
    let mut h = Harness::new(100, 30);
    h.ready();
    h
}

fn bash_request(id: &str) -> Event {
    Event::Permission(PermissionRequest {
        id: id.into(),
        tool: "Bash".into(),
        kind: ToolKind::Execute,
        title: "make".into(),
        input: serde_json::json!({"command": "make"}),
        diff: None,
        rule: "Bash(make:*)".into(),
    })
}

fn decisions(h: &mut Harness) -> Vec<(bool, bool)> {
    h.sent()
        .into_iter()
        .filter_map(|r| match r {
            Request::Decide { allow, scope, .. } => {
                Some((allow, matches!(scope, agent_core::DecideScope::Always)))
            }
            _ => None,
        })
        .collect()
}

/// A turn is running and the user types the next message while claude works.
fn busy_turn(h: &mut Harness) {
    h.type_str("first");
    h.key(KeyCode::Enter);
    h.event(Event::TurnStart, 10);
    h.sent();
    // The first message was typed a while ago.
    h.advance(3000);
}

#[tokio::test]
async fn letters_typed_for_the_next_message_never_answer_a_permission() {
    let mut h = started();
    busy_turn(&mut h);
    h.type_str("yes pl");
    // The panel appears in the middle of a word.
    h.event(bash_request("p1"), 20);
    // The rest of the message keeps coming at typing speed, well past the 300 ms grace, and
    // it is full of y, n, e, a and d.
    h.type_str("ease and then add nested dirs yes");
    assert_eq!(decisions(&mut h), vec![], "typing decided a prompt");
    assert!(h.app.perm.is_some(), "the panel is still waiting");
    assert_eq!(
        h.app.composer.text(),
        "yes please and then add nested dirs yes"
    );
    let s = h.screen();
    assert!(s.contains("locked while you type"), "no visible lock:\n{s}");
    // A pause, then a deliberate key decides.
    h.advance(1600);
    assert!(
        !h.screen().contains("locked while you type"),
        "the lock should have ended"
    );
    h.key(KeyCode::Char('y'));
    assert_eq!(decisions(&mut h), vec![(true, false)]);
}

#[tokio::test]
async fn an_a_typed_into_a_locked_panel_makes_no_always_rule() {
    let mut h = started();
    busy_turn(&mut h);
    h.type_str("x");
    h.event(bash_request("p1"), 20);
    h.type_str("a");
    assert_eq!(decisions(&mut h), vec![]);
    assert_eq!(h.app.composer.text(), "xa");
}

#[tokio::test]
async fn enter_and_escape_do_nothing_while_the_lock_is_on() {
    let mut h = started();
    busy_turn(&mut h);
    h.type_str("draft");
    h.event(bash_request("p1"), 20);
    h.key(KeyCode::Enter);
    h.key(KeyCode::Char('n'));
    assert_eq!(decisions(&mut h), vec![]);
    assert!(h.sent().is_empty(), "enter must not send the draft either");
    assert_eq!(h.app.composer.text(), "draftn");
}

#[tokio::test]
async fn an_idle_user_answers_after_the_grace_window_as_before() {
    let mut h = started();
    busy_turn(&mut h);
    h.event(bash_request("p1"), 20);
    h.advance(350);
    assert!(!h.screen().contains("locked while you type"));
    h.key(KeyCode::Char('y'));
    assert_eq!(decisions(&mut h), vec![(true, false)]);
}

#[tokio::test]
async fn typing_long_before_the_panel_does_not_lock_it() {
    let mut h = started();
    busy_turn(&mut h);
    h.type_str("hello");
    h.advance(5000);
    h.event(bash_request("p1"), 20);
    h.advance(350);
    h.key(KeyCode::Char('n'));
    assert_eq!(decisions(&mut h), vec![(false, false)]);
}

#[tokio::test]
async fn the_next_queued_panel_is_locked_too_when_you_are_typing() {
    let mut h = started();
    busy_turn(&mut h);
    h.event(bash_request("p1"), 20);
    h.event(bash_request("p2"), 20);
    h.advance(400);
    h.key(KeyCode::Char('y'));
    assert_eq!(decisions(&mut h), vec![(true, false)]);
    // The user starts the next message while p2 is on screen.
    h.type_str("n");
    h.type_str("ice");
    assert_eq!(decisions(&mut h), vec![]);
    assert_eq!(h.app.composer.text(), "nice");
}

fn wheel(h: &mut Harness, kind: MouseEventKind) {
    h.t += 40;
    let now = h.now();
    h.app.on_mouse(
        MouseEvent {
            kind,
            column: 20,
            row: 5,
            modifiers: KeyModifiers::NONE,
        },
        now,
    );
}

#[tokio::test]
async fn the_wheel_never_changes_which_mode_the_keyboard_is_in() {
    let mut h = started();
    for i in 0..6 {
        h.type_str(&format!("question {i}"));
        h.key(KeyCode::Enter);
        h.event(Event::TurnStart, 10);
        h.event(Event::TextDelta("long answer\n\n".repeat(30)), 10);
        h.event(Event::TurnEnd(agent_core::StopReason::EndTurn), 10);
    }
    h.draw();
    wheel(&mut h, MouseEventKind::ScrollUp);
    for _ in 0..5 {
        wheel(&mut h, MouseEventKind::ScrollDown);
    }
    assert_eq!(h.app.focus, Focus::Composer);
    // Typing after scrolling arrives whole: w and r are nav keys, i leaves nav mode.
    h.type_str("write tests please");
    assert_eq!(h.app.composer.text(), "write tests please");
}

#[tokio::test]
async fn scrolled_up_by_the_wheel_the_composer_still_takes_letters() {
    let mut h = started();
    for i in 0..6 {
        h.type_str(&format!("q{i}"));
        h.key(KeyCode::Enter);
        h.event(Event::TurnStart, 10);
        h.event(Event::TextDelta("long answer\n\n".repeat(30)), 10);
        h.event(Event::TurnEnd(agent_core::StopReason::EndTurn), 10);
    }
    h.draw();
    wheel(&mut h, MouseEventKind::ScrollUp);
    h.type_str("rewind");
    assert_eq!(h.app.composer.text(), "rewind");
    assert!(h.app.overlay.is_none());
}

#[tokio::test]
async fn a_resumed_call_that_never_finished_reads_as_interrupted() {
    use agent_core::{HistoryItem, ToolCall, ToolStatus};
    let mut h = started();
    let call = |id: &str, cmd: &str, out: &str| ToolCall {
        id: id.into(),
        name: "Bash".into(),
        kind: ToolKind::Execute,
        title: cmd.into(),
        input: serde_json::json!({"command": cmd}),
        status: ToolStatus::Failed,
        output: Some(out.into()),
        ..Default::default()
    };
    h.event(
        Event::History {
            session_id: "s".into(),
            items: vec![
                HistoryItem::User("run them".into()),
                HistoryItem::Tool(call("t1", "sleep 321", agent_core::INTERRUPTED_OUTPUT)),
                HistoryItem::Tool(call("t2", "sleep 322", agent_core::INTERRUPTED_OUTPUT)),
            ],
        },
        10,
    );
    let s = h.screen();
    let rows: Vec<&str> = s.lines().filter(|l| l.contains("sleep 32")).collect();
    assert_eq!(rows.len(), 2, "{s}");
    for r in rows {
        assert!(r.contains("interrupted"), "{r}");
        assert!(!r.contains('✓') && !r.contains("denied"), "{r}");
    }
}

#[tokio::test]
async fn ctrl_l_repaints_in_the_settings_popover_instead_of_moving_a_row() {
    let mut h = started();
    h.ctrl('t');
    assert!(matches!(
        h.app.overlay,
        Some(openc::ui::dialogs::Overlay::Settings { .. })
    ));
    let model = h.app.cfg.model.clone();
    for c in ['l', 'h', 'j', 'k'] {
        h.app.repaint = false;
        h.ctrl(c);
        if c == 'l' {
            assert!(h.app.repaint, "ctrl+l asks for a repaint");
        }
    }
    assert_eq!(h.app.cfg.model, model, "no ctrl chord changed the model");
    assert!(
        h.sent().is_empty(),
        "nothing reached the backend: ctrl+l/h/j/k are not vim keys here"
    );
}

#[tokio::test]
async fn the_resume_dialog_draws_at_every_tiny_size_without_overflowing() {
    // Debug builds panic on `r.bottom() - 2` at one row; release wrapped and drew garbage.
    for (w, hgt) in [(80, 1), (1, 1), (2, 2), (80, 2), (80, 3), (10, 4), (3, 30)] {
        let mut h = Harness::new(w, hgt);
        h.ready();
        let mut d = h.app.resume_dialog();
        d.set_rows(Vec::new(), true);
        h.app.overlay = Some(openc::ui::dialogs::Overlay::Resume(Box::new(d)));
        h.draw();
        h.resize(w, hgt.max(1));
        h.draw();
    }
}

#[tokio::test]
async fn a_message_sent_into_a_dead_backend_comes_back_when_the_restart_fails() {
    let mut h = Harness::new(100, 30);
    // claude never said hello: the row must not claim it is still starting once it failed.
    h.event(Event::Fatal("claude exited (exit status: 1)".into()), 10);
    let s = h.screen();
    assert!(
        s.contains("claude is not running") && !s.contains("starting claude"),
        "{s}"
    );
    h.type_str("say hi");
    h.key(KeyCode::Enter);
    h.sent();
    // The restart fails the same way.
    h.event(Event::Fatal("claude exited (exit status: 1)".into()), 10);
    assert_eq!(h.app.composer.text(), "say hi", "the message is not lost");
    assert!(h.app.backend_down);
    // A later success clears it.
    h.ready();
    assert!(!h.app.backend_down);
    h.event(Event::Fatal("died".into()), 10);
    h.type_str("again");
    h.key(KeyCode::Enter);
    h.event(Event::TurnStart, 10);
    h.event(Event::Fatal("died mid turn".into()), 10);
    assert!(
        h.app.composer.is_empty(),
        "a message that started a turn is not restored"
    );
}

#[tokio::test]
async fn a_tab_in_a_tool_title_does_not_move_the_outcome_column() {
    use agent_core::{ToolCall, ToolStatus};
    let mut h = started();
    let call = |id: &str, title: &str| ToolCall {
        id: id.into(),
        name: "Bash".into(),
        kind: ToolKind::Execute,
        title: title.into(),
        input: serde_json::json!({"command": title}),
        status: ToolStatus::Completed,
        output: Some("x".into()),
        ..Default::default()
    };
    h.type_str("go");
    h.key(KeyCode::Enter);
    h.event(Event::TurnStart, 10);
    h.event(Event::Tool(call("a", "cat /tmp/a/b/c/x.txt")), 10);
    h.event(Event::Tool(call("b", "cat /tmp/a\tb\tc/x.txt")), 10);
    h.event(Event::TurnEnd(agent_core::StopReason::EndTurn), 10);
    let s = h.screen();
    let ends: Vec<usize> = s
        .lines()
        .filter(|l| l.contains("x.txt"))
        .map(|l| tuikit::width::display_width(l.trim_end()))
        .collect();
    assert_eq!(ends.len(), 2, "{s}");
    assert_eq!(
        ends[0], ends[1],
        "rows end at different columns: {ends:?}\n{s}"
    );
}

#[tokio::test]
async fn ascii_mode_never_draws_the_unicode_ellipsis() {
    use openc::app::ThemeChoice;
    use openc::palette::{Depth, ASCII};
    // Narrow enough that the welcome row, the status line and a long tool title must be cut.
    let mut h = Harness::with_glyphs(44, 24, ThemeChoice::Hearth, Depth::True, ASCII);
    h.ready();
    h.type_str("a long question that needs a long tool title to show a cut");
    h.key(KeyCode::Enter);
    h.event(Event::TurnStart, 10);
    h.event(
        Event::Tool(agent_core::ToolCall {
            id: "t".into(),
            name: "Bash".into(),
            kind: ToolKind::Execute,
            title: "cargo test --workspace --all-targets -- --nocapture and more".into(),
            status: agent_core::ToolStatus::Completed,
            output: Some("ok".into()),
            ..Default::default()
        }),
        10,
    );
    h.event(Event::TurnEnd(agent_core::StopReason::EndTurn), 10);
    let s = h.screen();
    assert!(s.contains("..."), "something was cut:\n{s}");
    assert!(
        !s.contains('…'),
        "a unicode ellipsis on an ASCII screen:\n{s}"
    );
}

#[tokio::test]
async fn the_nav_cursor_never_lands_on_the_welcome_block() {
    let mut h = started();
    // Nothing but the welcome rows: jump to the top, then walk both ways.
    h.key(KeyCode::Home);
    for c in ['j', 'k', 'j'] {
        h.key(KeyCode::Char(c));
    }
    assert_eq!(h.app.view.cursor, None);
    let s = h.screen();
    let welcome: Vec<&str> = s.lines().filter(|l| l.contains("o p e n c")).collect();
    assert_eq!(welcome.len(), 1, "{s}");
    assert!(
        !welcome[0].trim_start().starts_with('▎'),
        "the cursor bar is drawn over the welcome row: {:?}",
        welcome[0]
    );
}

#[tokio::test]
async fn a_family_emoji_in_a_resume_title_stays_one_glyph() {
    let mut h = started();
    let mut d = h.app.resume_dialog();
    let row = backend_claude::sessions::SessionDetail {
        id: "s1".into(),
        title: "fix 👨‍👩‍👧 emoji".into(),
        ..Default::default()
    };
    d.set_rows(vec![row.clone().into()], false);
    d.set_rows(vec![row.into()], true);
    h.app.overlay = Some(openc::ui::dialogs::Overlay::Resume(Box::new(d)));
    let s = h.screen();
    assert!(s.contains("fix 👨‍👩‍👧 emoji"), "{s}");
}

#[tokio::test]
async fn copy_with_an_argument_that_is_not_a_count_says_so_instead_of_copying_the_last_answer() {
    let mut h = started();
    h.type_str("hello");
    h.key(KeyCode::Enter);
    h.event(Event::TurnStart, 10);
    h.event(Event::TextDelta("the answer".into()), 10);
    h.event(Event::TurnEnd(agent_core::StopReason::EndTurn), 10);
    h.sent();
    let now = h.now();
    h.app.composer.set_text("/copy abc");
    h.key(KeyCode::Enter);
    let _ = now;
    assert!(
        h.screen().contains("usage: /copy"),
        "a usage line, not a silent copy"
    );
    h.app.composer.set_text("/copy");
    h.key(KeyCode::Enter);
    assert!(h.screen().contains("copied"), "{}", h.screen());
}

fn retry_notice(attempt: u32, delay_ms: u64) -> Event {
    Event::Notice {
        level: agent_core::NoticeLevel::Warn,
        text: backend_claude::mapper::RetryNotice {
            reason: "529 (overloaded)".into(),
            attempt,
            max: 10,
            delay_ms,
        }
        .text(),
    }
}

#[tokio::test]
async fn api_retries_are_one_error_row_with_a_countdown_that_ticks() {
    let mut h = started();
    h.type_str("hi");
    h.key(KeyCode::Enter);
    h.event(Event::TurnStart, 10);
    h.event(retry_notice(1, 8_000), 10);
    let s = h.screen();
    assert!(s.contains("✗ Request failed · 529 (overloaded)"), "{s}");
    assert!(s.contains("retry 1 of 10 in 8s"), "{s}");
    // Three seconds on, the row says 5, and the app asked to wake for it.
    let wake = h.app.next_wake(h.now()).expect("a wake for the countdown");
    assert!(wake > h.now() && wake <= h.now() + std::time::Duration::from_secs(1));
    h.advance(3_000);
    let now = h.now();
    h.app.on_wake(now);
    assert!(h.screen().contains("retry 1 of 10 in 5s"), "{}", h.screen());
    // The next attempt rewrites the row.
    h.event(retry_notice(2, 4_000), 10);
    h.event(retry_notice(3, 9_000), 10);
    let s = h.screen();
    assert_eq!(
        s.matches("Request failed").count(),
        1,
        "one row, not a stack:\n{s}"
    );
    assert!(s.contains("retry 3 of 10 in 9s"), "{s}");
    // Once the answer arrives it stops counting and says what happened.
    h.event(Event::TextDelta("done".into()), 10);
    h.advance(20_000);
    let s = h.screen();
    assert!(s.contains("retried 3 of 10") && !s.contains("in 9s"), "{s}");
    assert!(
        h.app.tr.retry_wake(h.now()).is_none(),
        "a settled row asks for no ticks"
    );
}
