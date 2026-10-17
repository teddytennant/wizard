//! Regression tests for the round 1 interaction critic (`docs/critics/openw-interaction-r1.md`).
//! The number in each test name is the finding.

mod common;

use std::time::{Duration, Instant};

use agent_core::{
    Config, Event, ModelOption, NoticeLevel, Request, StopReason, ToolCall, ToolKind, ToolStatus,
};
use common::*;
use crossterm::event::{KeyCode, KeyModifiers};
use openw::app::{AppOpts, Route};

const NONE: KeyModifiers = KeyModifiers::NONE;
const CTRL: KeyModifiers = KeyModifiers::CONTROL;

fn esc_esc(h: &mut Harness) {
    h.key(KeyCode::Esc, NONE);
    h.key(KeyCode::Esc, NONE);
}

fn running_tool(id: &str, cmd: &str) -> ToolCall {
    ToolCall {
        id: id.into(),
        name: "bash".into(),
        kind: ToolKind::Execute,
        title: cmd.into(),
        input: serde_json::json!({ "command": cmd }),
        status: ToolStatus::Running,
        ..Default::default()
    }
}

/// A turn with a tool that is still running.
fn busy_with_tool(h: &mut Harness) {
    h.app.send_prompt("run it".into());
    h.event(Event::TurnStart);
    h.event(Event::Tool(running_tool("t1", "sleep 40")));
    h.sent();
}

#[test]
fn f1_interrupt_says_so_at_once_and_stays_busy_until_the_turn_ends() {
    let mut h = Harness::new(120, 36);
    busy_with_tool(&mut h);
    assert!(h.text().contains("esc interrupt"));
    esc_esc(&mut h);
    assert_eq!(h.sent(), vec![Request::Cancel]);
    let t = h.text();
    assert!(t.contains("interrupting…"), "{t}");
    assert!(!t.contains("esc interrupt"), "{t}");
    assert!(
        h.app.transcript.busy,
        "still busy: the tool has not stopped"
    );
    // wizard ends the tool and the turn
    let mut done = running_tool("t1", "sleep 40");
    done.status = ToolStatus::Failed;
    h.event(Event::Tool(done));
    h.event(Event::TurnEnd(StopReason::Cancelled));
    let t = h.text();
    assert!(!t.contains("interrupting…"), "{t}");
    assert!(t.contains("interrupted"), "{t}");
    assert!(!h.app.transcript.busy);
}

#[test]
fn f1_a_second_interrupt_while_waiting_sends_cancel_again() {
    let mut h = Harness::new(120, 36);
    busy_with_tool(&mut h);
    esc_esc(&mut h);
    h.sent();
    esc_esc(&mut h);
    assert_eq!(
        h.sent(),
        vec![Request::Cancel],
        "this is the way to force it"
    );
}

#[test]
fn f2_queued_prompts_are_not_left_behind_as_sent_bubbles() {
    let mut h = Harness::new(120, 36);
    busy_with_tool(&mut h);
    h.type_str("queued: also say hi");
    h.key(KeyCode::Enter, NONE);
    assert!(h.text().contains("QUEUED"));
    h.sent();
    esc_esc(&mut h);
    assert_eq!(h.sent(), vec![Request::Cancel]);
    let t = h.text();
    assert!(!t.contains("QUEUED"), "{t}");
    // the bubble is gone and the words are back in the box, with a toast saying why
    assert!(
        !h.app
            .transcript
            .messages
            .iter()
            .skip(1)
            .any(|m| m.role == agent_core::transcript::Role::User),
        "only the first prompt is a user message"
    );
    assert_eq!(h.app.prompt.text(), "queued: also say hi");
    assert!(t.contains("1 queued message not sent"), "{t}");
}

#[test]
fn f2_with_text_in_the_box_the_toast_says_where_the_queue_went() {
    let mut h = Harness::new(120, 36);
    busy_with_tool(&mut h);
    h.type_str("first queued");
    h.key(KeyCode::Enter, NONE);
    h.type_str("second queued");
    h.key(KeyCode::Enter, NONE);
    h.type_str("typing now");
    esc_esc(&mut h);
    assert_eq!(h.app.prompt.text(), "typing now");
    let t = h.text();
    assert!(t.contains("2 queued messages not sent"), "{t}");
    assert!(!t.contains("first queued"), "{t}");
}

#[test]
fn f3_a_backend_that_never_answers_gets_a_way_out_and_a_hint() {
    let mut h = Harness::new(120, 36);
    h.app.send_prompt("hang please".into());
    h.event(Event::TurnStart);
    h.event(Event::TextDelta("thinking...".into()));
    assert!(!h.text().contains("not responding"));
    // a minute of silence with no tool running
    h.app.last_backend = Instant::now() - Duration::from_secs(61);
    let t = h.text();
    assert!(t.contains("not responding, esc esc interrupts"), "{t}");
    esc_esc(&mut h);
    let t = h.text();
    assert!(t.contains("interrupting…"), "{t}");
    // three seconds in, the footer offers the forced restart
    h.app.interrupt_at = Some(Instant::now() - Duration::from_secs(4));
    assert!(h.text().contains("esc esc to force"));
    // the restarted backend replays the session and the screen is usable again
    h.event(Event::History {
        session_id: "ses_1234567890".into(),
        items: vec![agent_core::HistoryItem::User("hang please".into())],
    });
    assert!(!h.app.transcript.busy);
    assert!(h.app.interrupt_at.is_none());
    h.type_str("next");
    h.key(KeyCode::Enter, NONE);
    assert_eq!(h.sent().last(), Some(&Request::Prompt("next".into())));
}

#[test]
fn f3_a_quiet_tool_is_not_a_stalled_backend() {
    let mut h = Harness::new(120, 36);
    busy_with_tool(&mut h);
    h.app.last_backend = Instant::now() - Duration::from_secs(300);
    assert!(!h.text().contains("not responding"));
}

#[test]
fn f5_rename_is_a_slash_command() {
    let mut h = Harness::new(120, 36);
    h.turn("hello", "hi");
    h.type_str("/rename");
    h.key(KeyCode::Enter, NONE);
    let t = h.text();
    assert!(t.contains("Rename Session"), "{t}");
    assert!(h.app.dialogs.is_open());
}

#[test]
fn f7_the_model_picked_last_time_comes_back() {
    let mut cfg = agent_core::mock::config();
    let other = cfg
        .models
        .iter()
        .find(|m| m.id != cfg.model)
        .expect("the mock has two models")
        .id
        .clone();
    let default = cfg.model.clone();
    let dir = std::env::temp_dir().join(format!("openw-r1-model-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let prefs = openw::ui::dialogs::prefs::Prefs {
        recent: vec![other.clone()],
        ..Default::default()
    };
    prefs.save(&dir).unwrap();
    let mut h = Harness::with(
        120,
        36,
        AppOpts {
            cwd: "/home/me/proj".into(),
            mock: true,
            seed: 2,
            config_dir: Some(dir.clone()),
            ..Default::default()
        },
    );
    // the harness already delivered `Ready` with wizard's default
    assert!(h.sent().contains(&Request::SetModel(other.clone())));
    // a session switch resets wizard's model to its default; the choice is made again
    cfg.model = default;
    h.event(Event::Ready {
        session_id: "ses_2".into(),
        config: Config { ..cfg.clone() },
    });
    assert!(h.sent().contains(&Request::SetModel(other.clone())));
    // already on it: nothing to send
    cfg.model = other.clone();
    h.event(Event::Ready {
        session_id: "ses_3".into(),
        config: cfg,
    });
    assert!(!h.sent().iter().any(|r| matches!(r, Request::SetModel(_))));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_model_switched_on_wizards_side_survives_a_new_session() {
    let mut h = Harness::new(120, 36);
    let mut cfg = h.app.config.clone();
    let default = cfg.model.clone();
    let other = cfg
        .models
        .iter()
        .map(|m| m.id.clone())
        .find(|id| *id != default)
        .expect("a second model");
    h.sent();
    cfg.model = other.clone();
    h.event(Event::ConfigChanged(cfg.clone()));
    cfg.model = default;
    h.event(Event::Ready {
        session_id: "ses_2".into(),
        config: cfg,
    });
    assert!(h.sent().contains(&Request::SetModel(other)));
}

#[test]
fn theme_with_a_name_switches_and_is_saved_and_share_says_it_is_not_there() {
    let dir = std::env::temp_dir().join(format!("openw-theme-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let mut h = Harness::with(
        120,
        36,
        AppOpts {
            cwd: "/home/me/proj".into(),
            mock: true,
            config_dir: Some(dir.clone()),
            ..Default::default()
        },
    );
    h.type_str("/theme dracula");
    h.key(KeyCode::Enter, KeyModifiers::NONE);
    assert_eq!(h.app.prefs.theme.as_deref(), Some("dracula"));
    let saved = openw::ui::dialogs::prefs::Prefs::load(&dir);
    assert_eq!(saved.theme.as_deref(), Some("dracula"));

    h.sent();
    h.type_str("/share");
    h.key(KeyCode::Enter, KeyModifiers::NONE);
    h.key(KeyCode::Enter, KeyModifiers::NONE);
    assert!(h.text().contains("wizard has no session sharing"));
    assert!(!h.sent().iter().any(|r| matches!(r, Request::Prompt(_))));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_typed_plan_moves_the_agent_badge() {
    let mut h = Harness::new(120, 36);
    h.event(Event::Notice {
        level: agent_core::NoticeLevel::Info,
        text: "plan mode on — the agent investigates read-only".into(),
    });
    assert_eq!(h.app.agent, openw::app::AgentKind::Plan);
    h.event(Event::Notice {
        level: agent_core::NoticeLevel::Info,
        text: "plan mode off".into(),
    });
    assert_eq!(h.app.agent, openw::app::AgentKind::Build);
}

#[test]
fn f7_a_recent_model_wizard_no_longer_offers_is_ignored() {
    let dir = std::env::temp_dir().join(format!("openw-r1-model2-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let prefs = openw::ui::dialogs::prefs::Prefs {
        recent: vec!["gone/model".into()],
        ..Default::default()
    };
    prefs.save(&dir).unwrap();
    let mut h = Harness::with(
        120,
        36,
        AppOpts {
            cwd: "/home/me/proj".into(),
            mock: true,
            seed: 2,
            config_dir: Some(dir.clone()),
            ..Default::default()
        },
    );
    assert!(!h.sent().iter().any(|r| matches!(r, Request::SetModel(_))));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn f7_picking_a_model_with_ctrl_t_is_remembered_too() {
    let mut h = Harness::new(120, 36);
    h.app.prefs.recent = vec![h.app.config.model.clone()];
    let other = h
        .app
        .config
        .models
        .iter()
        .find(|m| m.id != h.app.config.model)
        .unwrap()
        .id
        .clone();
    h.app.prefs.recent.push(other.clone());
    h.sent();
    h.app.run_action(openw::keys::Action::ModelCycle);
    assert_eq!(h.sent(), vec![Request::SetModel(other.clone())]);
    assert_eq!(h.app.prefs.recent.first(), Some(&other));
}

#[test]
fn f8_a_name_with_a_space_is_mentioned_and_goes_out_quoted() {
    let mut h = Harness::new(120, 36);
    h.app.files = vec![
        "my file.txt".into(),
        "src/main.rs".into(),
        "日本語.txt".into(),
    ];
    h.type_str("read @myfile");
    h.key(KeyCode::Enter, NONE);
    assert_eq!(h.app.prompt.text(), "read @my file.txt ");
    let out = h.app.prompt.outgoing();
    assert_eq!(out.files.len(), 1, "the chip is there");
    assert_eq!(out.files[0].name, "my file.txt");
    assert_eq!(out.sent, "read @\"my file.txt\" ");
    // a plain or non-ASCII name goes out as it is
    h.app.prompt.clear();
    h.type_str("see @日本");
    h.key(KeyCode::Enter, NONE);
    assert_eq!(h.app.prompt.outgoing().sent, "see @日本語.txt ");
}

#[test]
fn f9_the_leader_exit_chord_keeps_a_draft() {
    let mut h = Harness::new(120, 36);
    h.type_str("an unsent draft that took a while to write");
    h.key(KeyCode::Char('x'), CTRL);
    h.key(KeyCode::Char('q'), NONE);
    assert!(!h.app.quit, "ctrl+x q must not quit over a draft");
    assert_eq!(
        h.app.prompt.text(),
        "an unsent draft that took a while to write"
    );
    h.app.prompt.clear();
    h.key(KeyCode::Char('x'), CTRL);
    h.key(KeyCode::Char('q'), NONE);
    assert!(h.app.quit);
}

#[test]
fn f10_redo_works_with_ctrl_dot_and_ctrl_shift_z_does_not_suspend() {
    let mut h = Harness::new(120, 36);
    h.type_str("abc");
    h.key(KeyCode::Char('-'), CTRL);
    assert_eq!(h.app.prompt.text(), "");
    h.key(KeyCode::Char('.'), CTRL);
    assert_eq!(h.app.prompt.text(), "abc");
    h.key(KeyCode::Char('-'), CTRL);
    h.key(KeyCode::Char('z'), CTRL | KeyModifiers::SHIFT);
    assert_eq!(h.app.prompt.text(), "abc");
    assert!(h.app.pending.is_empty(), "no suspend queued");
    // plain ctrl+z still suspends
    h.key(KeyCode::Char('z'), CTRL);
    assert_eq!(h.app.pending, vec![openw::app::Deferred::Suspend]);
}

#[test]
fn f10_a_paste_summary_moves_and_deletes_as_one_unit() {
    let mut h = Harness::new(120, 36);
    h.paste("one\ntwo\nthree\nfour\nfive");
    let text = h.app.prompt.text().to_string();
    assert!(text.starts_with("[Pasted ~"), "{text}");
    let token = text.trim_end().to_string();
    h.type_str("x");
    // left, left (over the x, then the space), left again lands before the whole token
    h.key(KeyCode::Left, NONE);
    h.key(KeyCode::Left, NONE);
    h.key(KeyCode::Left, NONE);
    assert_eq!(
        h.app.prompt.editor.cursor(),
        0,
        "left jumps over the summary"
    );
    h.key(KeyCode::Right, NONE);
    assert_eq!(h.app.prompt.editor.cursor(), token.len());
    h.key(KeyCode::Left, NONE);
    h.key(KeyCode::Delete, NONE);
    assert_eq!(h.app.prompt.text(), " x", "delete took the whole summary");
    h.key(KeyCode::Enter, NONE);
    let sent = h.sent();
    assert!(
        !format!("{sent:?}").contains("[Pasted"),
        "a half summary was sent: {sent:?}"
    );
}

#[test]
fn f11_diff_says_it_runs_wizards_command() {
    let mut h = Harness::new(120, 36);
    h.app.run_action(openw::keys::Action::Diff);
    assert!(h.sent().contains(&Request::Prompt("/diff".into())));
    assert!(h.text().contains("no diff viewer"));
}

#[test]
fn f12_the_compact_chord_does_nothing_on_the_home_screen() {
    let mut h = Harness::new(120, 36);
    h.sent();
    h.key(KeyCode::Char('x'), CTRL);
    h.key(KeyCode::Char('c'), NONE);
    assert!(h.sent().is_empty());
    assert_eq!(h.app.route, Route::Home);
}

#[test]
fn f13_a_dead_backend_leaves_no_spinner_and_keeps_the_draft() {
    let mut h = Harness::new(120, 36);
    busy_with_tool(&mut h);
    h.event(Event::Fatal(
        "wizard acp exited (signal: 9 (SIGKILL))".into(),
    ));
    let calls: Vec<ToolStatus> = h
        .app
        .transcript
        .messages
        .iter()
        .flat_map(|m| m.parts.iter())
        .filter_map(|p| match p {
            agent_core::transcript::Part::Tool(c) => Some(c.status),
            _ => None,
        })
        .collect();
    assert_eq!(calls, vec![ToolStatus::Failed]);
    assert!(!h.app.transcript.busy);
    h.type_str("do you hear me");
    h.key(KeyCode::Enter, NONE);
    assert_eq!(
        h.app.prompt.text(),
        "do you hear me",
        "the draft is not eaten"
    );
    assert!(h.text().contains("wizard is not running"));
    assert!(h.sent().is_empty());
}

#[test]
fn f16_a_changed_effort_shows_next_to_the_model() {
    let mut h = Harness::new(120, 36);
    assert!(h.app.variant_shown().is_none());
    let mut cfg = h.app.config.clone();
    cfg.effort = "xhigh".into();
    h.event(Event::ConfigChanged(cfg));
    assert_eq!(h.app.variant_shown(), Some("xhigh"));
    assert!(h.text().contains("xai-oauth · xhigh"), "{}", h.text());
}

#[test]
fn f20_a_settings_file_that_cannot_be_written_is_told_once() {
    let dir = std::env::temp_dir().join(format!("openw-r1-ro-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    // a file where the directory should be
    let blocked = dir.join("config");
    std::fs::write(&blocked, "").unwrap();
    let mut h = Harness::with(
        120,
        36,
        AppOpts {
            cwd: "/home/me/proj".into(),
            mock: true,
            seed: 2,
            config_dir: Some(blocked),
            ..Default::default()
        },
    );
    h.app.run_action(openw::keys::Action::ToggleAnimations);
    assert!(h.text().contains("Could not save settings"), "{}", h.text());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn unrated_undo_against_a_backend_that_answers_rewind_as_text_does_not_stay_busy() {
    let mut h = Harness::new(120, 36);
    h.turn("write a file", "done");
    h.sent();
    h.app.run_action(openw::keys::Action::Undo);
    assert_eq!(h.sent(), vec![Request::Prompt("/rewind".into())]);
    h.event(Event::TurnStart);
    h.event(Event::TextDelta("short answer".into()));
    h.event(Event::TurnEnd(StopReason::EndTurn));
    assert!(!h.app.transcript.busy, "still busy after the exchange");
    assert!(h.text().contains("Nothing to undo"), "{}", h.text());
}

#[test]
fn f2_notice_level_is_untouched_by_the_drop() {
    // a warning during the interrupt does not bring the queue back or crash the footer
    let mut h = Harness::new(120, 36);
    busy_with_tool(&mut h);
    esc_esc(&mut h);
    h.event(Event::Notice {
        level: NoticeLevel::Warn,
        text:
            "wizard did not stop after the interrupt, so it was restarted and the session reloaded"
                .into(),
    });
    h.text();
}

#[allow(dead_code)]
fn unused(_: ModelOption) {}
