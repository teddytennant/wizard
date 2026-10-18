//! Screen-level tests for `/model`, its effort step, `/clear`, `/help` and the
//! resume picker, on the headless terminal. Reference rows are from the Codex capture in
//! `reference/codex/120x36`; the model names and descriptions are scripted, so the tests build a
//! config that carries Codex's catalog text where a row is compared literally.

use agent_core::{Config, Event, ModelOption, Request};
use codexw::testing::{Harness, harness};
use crossterm::event::KeyCode;

fn catalog_config() -> Config {
    let m = |name: &str, desc: &str| ModelOption {
        id: name.into(),
        name: name.into(),
        provider: desc.into(),
    };
    Config {
        model: "gpt-5.5".into(),
        models: vec![
            m("gpt-5.6-sol", "Latest frontier agentic coding model."),
            m(
                "gpt-5.6-terra",
                "Balanced agentic coding model for everyday work.",
            ),
            m("gpt-5.6-luna", "Fast and affordable agentic coding model."),
            m(
                "gpt-5.5",
                "Frontier model for complex coding, research, and real-world work.",
            ),
            m(
                "gpt-5.2",
                "Optimized for professional work and long-running agents.",
            ),
        ],
        effort: "default".into(),
        efforts: ["default", "low", "medium", "high", "xhigh"]
            .map(String::from)
            .to_vec(),
        ..Default::default()
    }
}

fn ready(h: &mut Harness, cfg: Config) {
    h.app.on_backend(Event::Ready {
        session_id: "s1".into(),
        config: cfg,
    });
    h.draw();
}

#[test]
fn model_picker_matches_the_capture_rows() {
    let mut h = harness(120, 36, 7);
    ready(&mut h, catalog_config());
    h.type_str("/model");
    h.key(KeyCode::Enter);
    h.draw();
    let rows = h.screen.rows();
    let title = rows
        .iter()
        .position(|r| r == "  Select Model and Effort")
        .expect("title row");
    // Subtitle is wizard's own text; the rest is the capture verbatim.
    let want = [
        "  1. gpt-5.6-sol (default)  Latest frontier agentic coding model.",
        "  2. gpt-5.6-terra          Balanced agentic coding model for everyday work.",
        "  3. gpt-5.6-luna           Fast and affordable agentic coding model.",
        "› 4. gpt-5.5 (current)      Frontier model for complex coding, research, and real-world work.",
        "  5. gpt-5.2                Optimized for professional work and long-running agents.",
    ];
    // gpt-5.6-sol is not marked default by wizard; compare the other rows and the structure.
    assert_eq!(rows[title + 2], "");
    assert!(
        rows[title + 3].starts_with("  1. gpt-5.6-sol "),
        "{:?}",
        rows[title + 3]
    );
    // No `(default)` marker on wizard's list, so the description column sits further left;
    // the words and the highlight are the capture's.
    let squash = |s: &str| s.split_whitespace().collect::<Vec<_>>().join(" ");
    for i in 1..5 {
        assert_eq!(squash(&rows[title + 3 + i]), squash(want[i]), "row {i}");
    }
    let col = |r: &str| {
        ["Latest", "Balanced", "Fast", "Frontier", "Optimized"]
            .iter()
            .find_map(|w| r.find(w))
            .map(|b| r[..b].chars().count())
    };
    for i in 3..8 {
        assert_eq!(col(&rows[title + i]), col(&rows[title + 4]), "row {i}");
    }
    assert_eq!(rows[title + 8], "");
    assert_eq!(
        rows[title + 9],
        "  Press enter to confirm or esc to go back"
    );
    // The band pad rows above the title and below the list are blank band rows.
    assert_eq!(rows[title - 1], "");
}

#[test]
fn model_down_then_enter_opens_the_effort_step_and_applies() {
    let mut h = harness(120, 36, 7);
    ready(&mut h, catalog_config());
    h.type_str("/model");
    h.key(KeyCode::Enter);
    h.key(KeyCode::Down);
    h.draw();
    assert!(h.screen.text().contains("› 5. gpt-5.2"));
    h.key(KeyCode::Enter);
    h.draw();
    let t = h.screen.text();
    assert!(t.contains("Select Reasoning Level for gpt-5.2"), "{t}");
    assert!(t.contains("› 1. Default"), "{t}");
    assert!(
        t.contains("Wizard sends the effort to models that accept one and ignores it on the rest.")
    );
    h.key(KeyCode::Down);
    h.key(KeyCode::Down);
    h.key(KeyCode::Enter);
    h.draw();
    let sent = h.sent();
    assert_eq!(
        sent,
        vec![
            Request::SetModel("gpt-5.2".into()),
            Request::SetEffort("medium".into())
        ]
    );
    assert!(
        h.screen
            .full_text()
            .contains("Model changed to gpt-5.2 medium")
    );
}

#[test]
fn model_change_is_refused_mid_turn() {
    let mut h = harness(120, 36, 7);
    ready(&mut h, catalog_config());
    h.app.on_backend(Event::TurnStart);
    h.type_str("/model");
    h.key(KeyCode::Enter);
    h.key(KeyCode::Enter);
    h.key(KeyCode::Enter);
    h.draw();
    assert!(h.sent().is_empty());
    assert!(
        h.screen
            .full_text()
            .contains("Wizard changes the model between turns")
    );
}

#[test]
fn escape_closes_the_picker_and_brings_the_composer_back() {
    let mut h = harness(120, 36, 7);
    ready(&mut h, catalog_config());
    h.type_str("/model");
    h.key(KeyCode::Enter);
    h.draw();
    h.key(KeyCode::Esc);
    h.draw();
    let t = h.screen.text();
    assert!(!t.contains("Select Model and Effort"));
    assert!(t.contains("› "), "composer back: {t}");
}

// ---- /clear ----------------------------------------------------------------------------------

use agent_core::{HistoryItem, SessionInfo, Usage};
use codexw::app::AppOpts;
use codexw::testing::harness_with;

fn usage() -> Event {
    Event::Usage(Usage {
        input_tokens: 1_200,
        output_tokens: 300,
        ..Default::default()
    })
}

#[test]
fn clear_wipes_the_screen_and_leaves_header_tip_and_summary() {
    let mut h = harness(120, 36, 7);
    ready(&mut h, catalog_config());
    h.app.on_backend(usage());
    h.type_str("/clear");
    h.key(KeyCode::Enter);
    h.app.on_backend(Event::Ready {
        session_id: "s2".into(),
        config: catalog_config(),
    });
    h.draw();
    let rows = h.screen.rows();
    assert!(rows[0].starts_with("╭"), "{rows:?}");
    assert!(rows[1].contains("Wizard"));
    let t = h.screen.full_text();
    assert_eq!(t.matches("Wizard").count(), 1, "{t}");
    assert!(t.contains("Token usage: total=1,500 input=1,200 output=300"));
}

#[test]
fn a_wizard_command_typed_during_a_turn_goes_to_wizard() {
    let mut h = harness(120, 36, 7);
    ready(&mut h, catalog_config());
    h.app.on_backend(Event::TurnStart);
    h.type_str("/compact");
    h.key(KeyCode::Enter);
    h.draw();
    assert_eq!(h.sent(), vec![Request::Prompt("/compact".into())]);
    assert!(!h.screen.full_text().contains("disabled while a task"));
}

// ---- resume ----------------------------------------------------------------------------------

fn sessions() -> Vec<SessionInfo> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    let s = |id: &str, title: &str, cwd: &str, ago: i64| SessionInfo {
        id: id.into(),
        title: title.into(),
        cwd: cwd.into(),
        updated: now - ago,
    };
    vec![
        s("old", "first session fake:text", "/tmp/cxw-home/proj", 300),
        s("new", "second session fake:text", "/tmp/cxw-home/proj", 20),
        s("far", "elsewhere", "/tmp/other", 5),
    ]
}

#[test]
fn resume_opens_the_picker_on_the_alternate_screen_and_loads_the_pick() {
    let mut h = harness(120, 36, 7);
    ready(&mut h, catalog_config());
    h.app.on_backend(usage());
    h.type_str("/resume");
    h.key(KeyCode::Enter);
    assert_eq!(h.sent(), vec![Request::ListSessions]);
    h.app.on_backend(Event::Sessions(sessions()));
    h.draw();
    assert!(h.screen.in_alt_screen());
    let rows = h.screen.rows();
    assert_eq!(rows[0], " Resume a previous session");
    // Newest first, the other directory filtered out by the default Cwd filter.
    assert!(
        rows[4].starts_with("  ❯ ") && rows[4].contains("second session fake:text"),
        "{:?}",
        rows[4]
    );
    assert!(rows[5].contains("first session fake:text"));
    assert!(!h.screen.text().contains("elsewhere"));
    assert!(rows[33].contains("esc exit") && rows[33].contains("ctrl+c exit"));
    h.key(KeyCode::Down);
    h.key(KeyCode::Enter);
    assert_eq!(h.sent(), vec![Request::LoadSession("old".into())]);
    h.draw();
    assert!(!h.screen.in_alt_screen());
    // The backend replays the session, then confirms it.
    h.app.on_backend(Event::History {
        session_id: "old".into(),
        items: vec![
            HistoryItem::User("first session fake:text".into()),
            HistoryItem::Assistant("Hello again.".into()),
        ],
    });
    h.app.on_backend(Event::Ready {
        session_id: "old".into(),
        config: catalog_config(),
    });
    h.draw();
    let t = h.screen.full_text();
    let header = t.rfind("Wizard").unwrap();
    let user = t
        .find("› first session fake:text")
        .expect("replayed user row");
    let summary = t.find("Token usage: total=1,500").expect("summary");
    assert!(
        header < user && user < summary,
        "header, replay, then the old session's summary:\n{t}"
    );
}

#[test]
fn escape_closes_the_in_session_picker_and_leaves_the_chat_alone() {
    let mut h = harness(120, 36, 7);
    ready(&mut h, catalog_config());
    h.type_str("/resume");
    h.key(KeyCode::Enter);
    h.app.on_backend(Event::Sessions(sessions()));
    h.draw();
    h.key(KeyCode::Esc);
    h.draw();
    assert!(!h.screen.in_alt_screen());
    assert!(
        h.sent()
            .iter()
            .all(|r| !matches!(r, Request::LoadSession(_)))
    );
}

#[test]
fn resume_with_an_id_loads_it_without_a_picker() {
    let mut h = harness(120, 36, 7);
    ready(&mut h, catalog_config());
    h.type_str("/resume abc");
    h.key(KeyCode::Enter);
    assert_eq!(h.sent(), vec![Request::LoadSession("abc".into())]);
}

#[test]
fn startup_picker_holds_the_header_until_a_choice() {
    let mut h = harness_with(
        120,
        36,
        AppOpts {
            resume_picker: true,
            placeholder: Some(7),
            ..Default::default()
        },
    );
    ready(&mut h, catalog_config());
    assert_eq!(h.sent(), vec![Request::ListSessions]);
    h.app.on_backend(Event::Sessions(sessions()));
    h.draw();
    assert!(h.screen.in_alt_screen());
    assert!(
        h.screen.rows()[33].contains("esc start new")
            && h.screen.rows()[33].contains("ctrl+c quit")
    );
    // Esc carries on with the new session: the header shows.
    h.key(KeyCode::Esc);
    h.draw();
    assert!(!h.screen.in_alt_screen());
    assert!(h.screen.full_text().contains("Wizard"));
}

#[test]
fn startup_picker_ctrl_c_quits() {
    let mut h = harness_with(
        120,
        36,
        AppOpts {
            resume_picker: true,
            placeholder: Some(7),
            ..Default::default()
        },
    );
    ready(&mut h, catalog_config());
    h.app.on_backend(Event::Sessions(sessions()));
    h.draw();
    h.ctrl('c');
    assert!(h.app.should_exit);
}

#[test]
fn startup_pick_loads_without_a_summary() {
    let mut h = harness_with(
        120,
        36,
        AppOpts {
            resume_picker: true,
            placeholder: Some(7),
            ..Default::default()
        },
    );
    ready(&mut h, catalog_config());
    let _ = h.sent();
    h.app.on_backend(Event::Sessions(sessions()));
    h.draw();
    h.key(KeyCode::Enter);
    assert_eq!(h.sent(), vec![Request::LoadSession("new".into())]);
    h.app.on_backend(Event::History {
        session_id: "new".into(),
        items: vec![HistoryItem::User("second session fake:text".into())],
    });
    h.app.on_backend(Event::Ready {
        session_id: "new".into(),
        config: catalog_config(),
    });
    h.draw();
    let t = h.screen.full_text();
    assert_eq!(t.matches("Wizard").count(), 1, "{t}");
    assert!(t.contains("› second session fake:text"));
    assert!(!t.contains("To continue this session"));
}

#[test]
fn resume_last_picks_the_newest_session_of_this_directory() {
    let mut h = harness_with(
        120,
        36,
        AppOpts {
            resume_last: true,
            placeholder: Some(7),
            ..Default::default()
        },
    );
    ready(&mut h, catalog_config());
    assert_eq!(h.sent(), vec![Request::ListSessions]);
    h.app.on_backend(Event::Sessions(sessions()));
    assert_eq!(h.sent(), vec![Request::LoadSession("new".into())]);
}

#[test]
fn resume_last_with_nothing_to_resume_starts_a_new_session() {
    let mut h = harness_with(
        120,
        36,
        AppOpts {
            resume_last: true,
            placeholder: Some(7),
            ..Default::default()
        },
    );
    ready(&mut h, catalog_config());
    h.app.on_backend(Event::Sessions(vec![]));
    h.draw();
    assert!(
        h.sent()
            .iter()
            .all(|r| !matches!(r, Request::LoadSession(_)))
    );
    assert!(h.screen.full_text().contains("Wizard"));
}

// ---- the shared commands and wizard's ----------------------------------------------------

#[test]
fn removed_codex_names_go_to_wizard_as_typed() {
    let mut h = harness(120, 36, 7);
    ready(&mut h, catalog_config());
    let wizards = |name: &str| agent_core::SlashCommand {
        name: name.into(),
        description: String::new(),
        input_hint: String::new(),
    };
    h.app
        .on_backend(Event::Commands(vec![wizards("status"), wizards("diff")]));
    for typed in [
        "/permissions",
        "/feedback",
        "/new",
        "/status",
        "/diff",
        "/goal ship it",
    ] {
        h.type_str(typed);
        h.key(KeyCode::Enter);
        assert_eq!(h.sent(), vec![Request::Prompt(typed.into())], "{typed}");
    }
    h.draw();
    let t = h.screen.full_text();
    assert!(!t.contains("not supported by wizard"), "{t}");
    assert!(!t.contains("Unrecognized command"), "{t}");
}

#[test]
fn help_lists_wizards_commands_the_shared_ones_and_codexws() {
    let mut h = harness(120, 36, 7);
    ready(&mut h, catalog_config());
    h.app
        .on_backend(Event::Commands(vec![agent_core::SlashCommand {
            name: "compact".into(),
            description: "compact the conversation".into(),
            input_hint: String::new(),
        }]));
    h.type_str("/help");
    h.key(KeyCode::Enter);
    h.draw();
    assert!(h.sent().is_empty());
    let t = h.screen.full_text();
    assert!(
        t.lines()
            .any(|l| l.starts_with("• /compact ") && l.ends_with("compact the conversation")),
        "{t}"
    );
    assert!(t.contains("/resume [id]"), "{t}");
    assert!(t.contains("/raw [on|off]"), "{t}");
    assert!(!t.contains("/review"), "{t}");
}

#[test]
fn a_paste_goes_to_the_picker_search_while_it_is_open() {
    let mut h = harness(120, 36, 7);
    ready(&mut h, catalog_config());
    h.type_str("/resume");
    h.key(KeyCode::Enter);
    h.app.on_backend(Event::Sessions(sessions()));
    h.app.on_paste("second\nsession");
    h.draw();
    assert!(
        h.screen.rows()[2].starts_with(" Search: second session"),
        "{:?}",
        h.screen.rows()[2]
    );
    h.key(KeyCode::Esc);
    h.key(KeyCode::Esc);
    h.draw();
    assert!(h.app.pane.composer.is_empty());
}

#[test]
fn export_writes_the_conversation_and_settings_answers_here() {
    let mut h = harness(120, 36, 7);
    ready(&mut h, catalog_config());
    h.type_str("hi");
    h.key(KeyCode::Enter);
    h.app.on_backend(Event::TurnStart);
    h.app.on_backend(Event::TextDelta("hello".into()));
    h.app
        .on_backend(Event::TurnEnd(agent_core::StopReason::EndTurn));
    h.sent();
    let out = std::env::temp_dir().join(format!("codexw-export-{}.md", std::process::id()));
    h.type_str(&format!("/export {}", out.display()));
    h.key(KeyCode::Enter);
    assert!(h.sent().is_empty());
    let md = std::fs::read_to_string(&out).unwrap();
    std::fs::remove_file(&out).unwrap();
    assert_eq!(md, "## You\n\nhi\n\n## Wizard\n\nhello\n\n");
    h.type_str("/settings");
    h.key(KeyCode::Enter);
    assert!(h.sent().is_empty());
    h.draw();
    let t = h.screen.full_text();
    assert!(t.contains("This look has no settings view"), "{t}");
}
