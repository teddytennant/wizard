//! Keys, commands and backend requests, driven through the app without a terminal.

mod common;

use std::path::PathBuf;
use std::time::{Duration, Instant};

use agent_core::{Event, NoticeLevel, Request, SessionInfo, StopReason};
use common::*;
use crossterm::event::{KeyCode, KeyModifiers, MouseEvent, MouseEventKind};
use openw::app::{AppOpts, Route};

const NONE: KeyModifiers = KeyModifiers::NONE;
const CTRL: KeyModifiers = KeyModifiers::CONTROL;

fn enter(h: &mut Harness) {
    h.key(KeyCode::Enter, NONE);
}

#[test]
fn typing_and_enter_sends_the_prompt_and_shows_a_user_block() {
    let mut h = Harness::new(120, 36);
    h.sent();
    h.type_str("hello there");
    enter(&mut h);
    assert_eq!(h.sent(), vec![Request::Prompt("hello there".into())]);
    assert_eq!(h.app.route, Route::Session);
    assert!(h.text().contains("┃  hello there"));
    assert!(h.app.prompt.is_empty());
}

#[test]
fn double_esc_cancels_a_running_turn_and_single_esc_only_arms() {
    let mut h = Harness::new(120, 36);
    h.app.send_prompt("go".into());
    h.event(Event::TurnStart);
    h.sent();
    h.key(KeyCode::Esc, NONE);
    assert!(h.sent().is_empty());
    assert!(h.text().contains("esc again to interrupt"));
    h.key(KeyCode::Esc, NONE);
    assert_eq!(h.sent(), vec![Request::Cancel]);
    h.event(Event::TurnEnd(StopReason::Cancelled));
    assert!(h.text().contains("interrupted"));
    assert!(!h.text().contains("esc again"));
}

#[test]
fn esc_does_nothing_when_idle() {
    let mut h = Harness::new(120, 36);
    h.sent();
    h.key(KeyCode::Esc, NONE);
    assert!(h.sent().is_empty());
    assert!(!h.app.quit);
}

#[test]
fn ctrl_c_clears_text_then_quits_on_an_empty_prompt() {
    let mut h = Harness::new(120, 36);
    h.type_str("draft");
    h.key(KeyCode::Char('c'), CTRL);
    assert!(h.app.prompt.is_empty());
    assert!(!h.app.quit);
    h.key(KeyCode::Char('c'), CTRL);
    assert!(h.app.quit);
}

#[test]
fn ctrl_d_deletes_a_char_with_text_and_quits_when_empty() {
    let mut h = Harness::new(120, 36);
    h.type_str("ab");
    h.key(KeyCode::Home, NONE);
    h.key(KeyCode::Char('d'), CTRL);
    assert_eq!(h.app.prompt.text(), "b");
    assert!(!h.app.quit);
    h.key(KeyCode::Char('d'), CTRL);
    h.key(KeyCode::Char('d'), CTRL);
    assert!(h.app.quit);
}

#[test]
fn typed_exit_quits_without_calling_the_backend() {
    let mut h = Harness::new(120, 36);
    h.sent();
    h.type_str("exit");
    enter(&mut h);
    assert!(h.app.quit);
    assert!(h.sent().is_empty());
}

#[test]
fn slash_commands_run_frontend_actions() {
    let mut h = Harness::new(120, 36);
    h.type_str("/models");
    enter(&mut h);
    assert!(h.app.dialogs.is_open());
    assert_eq!(h.app.dialogs.top_title(), Some("Select model"));
    assert!(h.text().contains("grok-4.6"));
    h.key(KeyCode::Esc, NONE);
    assert!(!h.app.dialogs.is_open());
}

#[test]
fn model_with_an_argument_sets_it_directly() {
    let mut h = Harness::new(120, 36);
    h.sent();
    h.type_str("/models xai-oauth/grok-4.6-fast");
    enter(&mut h);
    assert_eq!(
        h.sent(),
        vec![Request::SetModel("xai-oauth/grok-4.6-fast".into())]
    );
}

#[test]
fn picking_a_model_in_the_dialog_sends_set_model() {
    let mut h = Harness::new(120, 36);
    h.sent();
    h.key(KeyCode::Char('x'), CTRL);
    h.key(KeyCode::Char('m'), NONE);
    assert_eq!(h.app.dialogs.top_title(), Some("Select model"));
    h.key(KeyCode::Down, NONE);
    enter(&mut h);
    assert_eq!(
        h.sent(),
        vec![Request::SetModel("xai-oauth/grok-4.6-fast".into())]
    );
    assert!(!h.app.dialogs.is_open());
}

#[test]
fn backend_commands_complete_into_the_prompt_instead_of_running() {
    let mut h = Harness::new(120, 36);
    h.sent();
    h.type_str("/effo");
    enter(&mut h);
    assert_eq!(h.app.prompt.text(), "/effort ");
    assert!(h.sent().is_empty());
    h.type_str("high");
    enter(&mut h);
    assert_eq!(h.sent(), vec![Request::Prompt("/effort high".into())]);
}

#[test]
fn leader_sequences_open_dialogs_and_time_out() {
    let mut h = Harness::new(120, 36);
    h.sent();
    h.key(KeyCode::Char('x'), CTRL);
    assert!(h.app.leader_pending());
    h.key(KeyCode::Char('l'), NONE);
    assert!(!h.app.leader_pending());
    assert_eq!(h.app.dialogs.top_title(), Some("Sessions"));
    assert!(h.sent().contains(&Request::ListSessions));
    h.key(KeyCode::Esc, NONE);

    h.key(KeyCode::Char('x'), CTRL);
    h.app.on_tick(Instant::now() + Duration::from_millis(2100));
    assert!(!h.app.leader_pending());
    h.type_str("l");
    assert_eq!(h.app.prompt.text(), "l");
}

#[test]
fn an_unbound_key_after_the_leader_is_swallowed() {
    let mut h = Harness::new(120, 36);
    h.key(KeyCode::Char('x'), CTRL);
    h.key(KeyCode::Char('z'), NONE);
    assert!(h.app.prompt.is_empty());
    assert!(!h.app.leader_pending());
}

#[test]
fn new_session_goes_home_and_asks_the_backend() {
    let mut h = Harness::new(120, 36);
    h.turn("hi", "hello");
    assert_eq!(h.app.route, Route::Session);
    h.sent();
    h.type_str("/new");
    enter(&mut h);
    assert_eq!(h.sent(), vec![Request::NewSession]);
    assert_eq!(h.app.route, Route::Home);
    assert!(h.app.transcript.messages.is_empty());
}

#[test]
fn paste_of_many_lines_is_summarised_and_sent_in_full() {
    let mut h = Harness::new(120, 36);
    h.sent();
    h.paste("a\nb\nc\nd");
    // the token is followed by a space, as opencode inserts it
    assert_eq!(h.app.prompt.text(), "[Pasted ~4 lines] ");
    enter(&mut h);
    assert_eq!(h.sent(), vec![Request::Prompt("a\nb\nc\nd".into())]);
}

#[test]
fn backspace_removes_a_paste_summary_whole() {
    let mut h = Harness::new(120, 36);
    h.type_str("x ");
    h.paste("a\nb\nc");
    assert_eq!(h.app.prompt.text(), "x [Pasted ~3 lines] ");
    // the first backspace takes the space after the token, the second the token itself
    h.key(KeyCode::Backspace, NONE);
    assert_eq!(h.app.prompt.text(), "x [Pasted ~3 lines]");
    h.key(KeyCode::Backspace, NONE);
    assert_eq!(h.app.prompt.text(), "x ");
}

#[test]
fn short_pastes_go_in_as_text() {
    let mut h = Harness::new(120, 36);
    h.paste("one line");
    assert_eq!(h.app.prompt.text(), "one line");
}

#[test]
fn up_arrow_recalls_history() {
    let mut h = Harness::new(120, 36);
    h.type_str("first");
    enter(&mut h);
    h.type_str("second");
    enter(&mut h);
    h.key(KeyCode::Up, NONE);
    assert_eq!(h.app.prompt.text(), "second");
    h.key(KeyCode::Up, NONE);
    assert_eq!(h.app.prompt.text(), "first");
    // down goes to the end of the entry first, and only from there walks forward
    h.key(KeyCode::Down, NONE);
    assert_eq!(h.app.prompt.text(), "first");
    h.key(KeyCode::Down, NONE);
    assert_eq!(h.app.prompt.text(), "second");
    h.key(KeyCode::Down, NONE);
    assert_eq!(h.app.prompt.text(), "");
}

#[test]
fn up_in_the_middle_goes_to_the_start_before_history() {
    let mut h = Harness::new(120, 36);
    h.type_str("older");
    enter(&mut h);
    h.type_str("draft");
    h.key(KeyCode::Up, NONE);
    assert_eq!(h.app.prompt.text(), "draft");
    assert_eq!(h.app.prompt.editor.cursor(), 0);
    // a draft that is not a history entry blocks walking
    h.key(KeyCode::Up, NONE);
    assert_eq!(h.app.prompt.text(), "draft");
}

#[test]
fn shell_mode_is_entered_with_bang_and_left_with_esc() {
    let mut h = Harness::new(120, 36);
    h.type_str("!");
    assert!(h.app.prompt.shell);
    assert!(h.app.prompt.is_empty());
    assert!(h.text().contains("esc exit shell mode"));
    h.key(KeyCode::Esc, NONE);
    assert!(!h.app.prompt.shell);
}

#[test]
fn tab_toggles_plan_and_the_confirmation_becomes_a_toast() {
    let mut h = Harness::new(120, 36);
    h.sent();
    h.key(KeyCode::Tab, NONE);
    assert_eq!(h.sent(), vec![Request::Prompt("/plan".into())]);
    assert_eq!(h.app.agent, openw::app::AgentKind::Plan);
    h.event(Event::Notice {
        level: NoticeLevel::Info,
        text: "plan mode on".into(),
    });
    assert!(h.app.transcript.messages.is_empty());
    assert!(h.app.toasts.current().is_some());
}

#[test]
fn info_notices_become_assistant_text_and_warnings_become_toasts() {
    let mut h = Harness::new(120, 36);
    h.event(Event::Notice {
        level: NoticeLevel::Info,
        text: "cost: $0.01".into(),
    });
    assert_eq!(h.app.route, Route::Session);
    assert!(h.text().contains("cost: $0.01"));
    h.event(Event::Notice {
        level: NoticeLevel::Warn,
        text: "retrying".into(),
    });
    assert!(h.app.toasts.current().unwrap().message.contains("retrying"));
}

#[test]
fn continue_loads_the_newest_session_for_this_directory() {
    let opts = AppOpts {
        cwd: PathBuf::from("/home/me/proj"),
        mock: true,
        continue_latest: true,
        seed: 2,
        ..Default::default()
    };
    let mut h = Harness::with(120, 36, opts);
    h.sent();
    let s = |id: &str, cwd: &str, updated: i64| SessionInfo {
        id: id.into(),
        title: id.into(),
        cwd: cwd.into(),
        updated,
    };
    h.event(Event::Sessions(vec![
        s("old", "/home/me/proj", 10),
        s("other", "/elsewhere", 99),
        s("new", "/home/me/proj", 50),
    ]));
    assert_eq!(h.sent(), vec![Request::LoadSession("new".into())]);
    // a later list does not trigger another load
    h.event(Event::Sessions(vec![s("new", "/home/me/proj", 50)]));
    assert!(h.sent().is_empty());
}

#[test]
fn history_event_switches_to_the_session_screen() {
    let mut h = Harness::new(120, 36);
    h.event(Event::History {
        session_id: "s".into(),
        items: vec![
            agent_core::HistoryItem::User("old question".into()),
            agent_core::HistoryItem::Assistant("old answer".into()),
        ],
    });
    assert_eq!(h.app.route, Route::Session);
    let t = h.text();
    assert!(t.contains("old question") && t.contains("old answer"));
}

#[test]
fn theme_dialog_previews_and_esc_restores() {
    let mut h = Harness::new(120, 36);
    let before = h.app.theme.name.clone();
    h.key(KeyCode::Char('x'), CTRL);
    h.key(KeyCode::Char('t'), NONE);
    h.key(KeyCode::Down, NONE);
    assert_ne!(
        h.app.theme.name, before,
        "moving the selection previews a theme"
    );
    h.key(KeyCode::Esc, NONE);
    assert_eq!(h.app.theme.name, before);
    h.key(KeyCode::Char('x'), CTRL);
    h.key(KeyCode::Char('t'), NONE);
    h.key(KeyCode::Down, NONE);
    let picked = h.app.theme.name.clone();
    enter(&mut h);
    assert_eq!(h.app.theme.name, picked);
}

#[test]
fn wheel_scrolls_the_transcript_only_in_a_session() {
    let mut h = Harness::new(80, 20);
    let wheel = |h: &mut Harness, up: bool| {
        h.app.on_mouse(MouseEvent {
            kind: if up {
                MouseEventKind::ScrollUp
            } else {
                MouseEventKind::ScrollDown
            },
            column: 5,
            row: 5,
            modifiers: NONE,
        });
    };
    wheel(&mut h, true);
    for i in 0..8 {
        h.turn(&format!("q{i}"), "some answer text");
    }
    h.text();
    let bottom = h.app.scroll.offset();
    wheel(&mut h, true);
    h.text();
    assert_eq!(h.app.scroll.offset(), bottom - 3);
    wheel(&mut h, false);
    h.text();
    assert_eq!(h.app.scroll.offset(), bottom);
}

#[test]
fn idle_app_has_no_wakeups_and_a_busy_one_animates() {
    let mut h = Harness::new(120, 36);
    h.text();
    assert!(h.app.next_wake(Instant::now()).is_none());
    h.app.send_prompt("go".into());
    h.event(Event::TurnStart);
    assert!(h.app.animating());
    assert!(h.app.next_wake(Instant::now()).is_some());
    h.event(Event::TurnEnd(StopReason::EndTurn));
    assert!(h.app.next_wake(Instant::now()).is_none());
}

#[test]
fn toast_expires_on_its_deadline() {
    let mut h = Harness::new(120, 36);
    h.app.toast(tuikit::theme::Variant::Info, "hi");
    let when = h
        .app
        .next_wake(Instant::now())
        .expect("toast has a deadline");
    h.app.on_tick(when + Duration::from_millis(1));
    assert!(h.app.toasts.current().is_none());
}

#[test]
fn resize_redraws_at_the_new_size() {
    let mut h = Harness::new(120, 36);
    h.app.on_resize(80, 24);
    let t = h.render();
    assert_eq!((t.area().width, t.area().height), (80, 24));
    // no tip here, so the free height is 2 and the wordmark moves down one row from the
    // reference (which has a tip row); the prompt is 75 wide at col 3 either way
    assert!(t.row(6).trim().starts_with("█   █ █"));
    assert_eq!(
        t.cell(3, 11).map(|c| c.symbol().to_string()),
        Some("┃".into())
    );
}

#[test]
fn a_dead_backend_is_reported_not_swallowed() {
    let mut h = Harness::new(120, 36);
    h.event(Event::Fatal("wizard acp exited".into()));
    assert!(h.text().contains("wizard acp exited"));
    h.type_str("hello");
    enter(&mut h);
    assert!(h
        .app
        .toasts
        .current()
        .unwrap()
        .message
        .contains("not running"));
}

#[test]
fn rename_dialog_sets_a_local_title() {
    let mut h = Harness::new(120, 36);
    h.turn("hi", "hello");
    h.key(KeyCode::Char('r'), CTRL);
    assert_eq!(h.app.dialogs.top_title(), Some("Rename Session"));
    for _ in 0..20 {
        h.key(KeyCode::Backspace, NONE);
    }
    h.type_str("My title");
    enter(&mut h);
    assert_eq!(h.app.session_title(), "My title");
    assert_eq!(h.app.window_title(), "OC | My title");
}

#[test]
fn tiny_terminals_do_not_panic_on_any_screen() {
    for (w, h) in [
        (1, 1),
        (2, 2),
        (10, 3),
        (20, 5),
        (30, 8),
        (40, 10),
        (60, 12),
    ] {
        let mut hn = Harness::new(w, h);
        hn.text();
        hn.type_str("/");
        hn.text();
        hn.key(KeyCode::Esc, NONE);
        hn.turn(
            "hello",
            "# Title\n\n- a\n- b\n\n```rust\nfn main() {}\n```\n\n| a | b |\n|---|---|\n| 1 | 2 |",
        );
        hn.text();
        hn.key(KeyCode::Char('p'), CTRL);
        hn.text();
        hn.type_str("th");
        hn.text();
        hn.key(KeyCode::Esc, NONE);
        for cmd in [
            "/models",
            "/themes",
            "/sessions",
            "/status",
            "/help",
            "/agents",
            "/export",
        ] {
            hn.type_str(cmd);
            enter(&mut hn);
            hn.text();
            hn.key(KeyCode::Esc, NONE);
        }
        hn.app.toast(
            tuikit::theme::Variant::Error,
            "a toast that is much wider than the terminal can possibly be",
        );
        hn.text();
        hn.key(KeyCode::Char('x'), CTRL);
        hn.key(KeyCode::Char('b'), NONE);
        hn.text();
    }
}

#[test]
fn sidebar_overlay_on_a_narrow_terminal_dims_the_page() {
    let mut h = Harness::new(100, 30);
    h.turn("hi", "hello");
    h.key(KeyCode::Char('x'), CTRL);
    h.key(KeyCode::Char('b'), NONE);
    let t = h.render();
    // panel at the right edge, 42 wide, on backgroundPanel
    assert_eq!(
        t.cell(100 - 42, 5).map(|c| c.bg),
        Some(ratatui::style::Color::Rgb(20, 20, 20))
    );
    // the page left of it is dimmed (alpha 70: #0a0a0a becomes #070707)
    assert_eq!(
        t.cell(0, 0).map(|c| c.bg),
        Some(ratatui::style::Color::Rgb(7, 7, 7))
    );
    h.key(KeyCode::Char('x'), CTRL);
    h.key(KeyCode::Char('b'), NONE);
    let t = h.render();
    assert_eq!(
        t.cell(0, 0).map(|c| c.bg),
        Some(ratatui::style::Color::Rgb(10, 10, 10))
    );
}

#[test]
fn shell_mode_runs_locally_and_shows_a_bash_block_without_a_duration() {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .unwrap();
    let _guard = rt.enter();
    let (mtx, mut mrx) = tokio::sync::mpsc::unbounded_channel();
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let opts = AppOpts {
        cwd: std::env::temp_dir(),
        mock: true,
        seed: 2,
        ..Default::default()
    };
    let mut app = openw::app::App::new(opts, tx, mtx);
    app.size = (100, 30);
    app.frozen = Some(Duration::ZERO);
    for c in "!echo hi-from-shell".chars() {
        app.on_key(crossterm::event::KeyEvent::new(KeyCode::Char(c), NONE));
    }
    app.on_key(crossterm::event::KeyEvent::new(KeyCode::Enter, NONE));
    // output chunks first, then the end
    rt.block_on(async {
        loop {
            let msg = tokio::time::timeout(Duration::from_secs(10), mrx.recv())
                .await
                .expect("shell finished")
                .expect("a message");
            let done = matches!(msg, openw::app::Msg::Shell { .. });
            app.update(msg);
            if done {
                break;
            }
        }
    });
    let mut t = tuikit::testing::TestTerminal::new(100, 30);
    t.draw(|buf, _| {
        openw::ui::draw(buf, &mut app);
    });
    let text = t.plain();
    assert!(text.contains("$ echo hi-from-shell"), "{text}");
    assert!(text.contains("hi-from-shell"));
    assert!(text.contains("▣  Build") && !text.contains("ms"), "{text}");
}

#[test]
fn page_keys_scroll_half_a_screen_and_ctrl_alt_keys_a_quarter() {
    let mut h = Harness::new(80, 24);
    for i in 0..10 {
        h.turn(&format!("q{i}"), "some answer");
    }
    h.text();
    let bottom = h.app.scroll.offset();
    let vp = h.app.scroll.viewport();
    h.key(KeyCode::PageUp, NONE);
    h.text();
    assert_eq!(h.app.scroll.offset(), bottom - vp / 2);
    h.key(KeyCode::Char('d'), CTRL | KeyModifiers::ALT);
    h.text();
    assert_eq!(h.app.scroll.offset(), bottom - vp / 2 + vp / 4);
    h.key(KeyCode::Home, NONE);
    assert_eq!(
        h.app.scroll.offset(),
        0,
        "home jumps to the first message when the prompt is empty"
    );
    h.key(KeyCode::End, NONE);
    h.text();
    assert_eq!(h.app.scroll.offset(), bottom);
}
