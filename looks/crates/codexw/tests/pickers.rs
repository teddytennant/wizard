//! Screen-level tests for `/model`, its effort step, `/status`, `/new`, `/clear` and the
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

// ---- /status, /new, /clear -------------------------------------------------------------------

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
fn status_card_follows_the_echo_and_names_what_wizard_lacks() {
    let mut h = harness(120, 36, 7);
    ready(&mut h, catalog_config());
    h.app.on_backend(usage());
    h.type_str("/status");
    h.key(KeyCode::Enter);
    h.draw();
    let t = h.screen.full_text();
    let rows: Vec<&str> = t.lines().collect();
    let echo = rows.iter().position(|r| *r == "/status").expect("echo row");
    assert_eq!(rows[echo + 1], "");
    assert!(rows[echo + 2].starts_with("╭──"));
    assert!(t.contains("Permissions:") && t.contains("Full Access"));
    assert!(t.contains("never (wizard does not ask)"));
    assert!(t.contains("1.5K total  (1.2K input + 300 output)"), "{t}");
    assert!(t.contains("unknown (wizard does not report the window size)"));
    assert!(t.contains("Limits:") && t.contains("unknown (no subscription signed in)"));
}

#[test]
fn new_prints_the_old_sessions_summary_before_the_new_header() {
    let mut h = harness(120, 36, 7);
    ready(&mut h, catalog_config());
    h.app.on_backend(usage());
    h.type_str("/new");
    h.key(KeyCode::Enter);
    assert_eq!(h.sent(), vec![Request::NewSession]);
    h.app.on_backend(Event::History {
        session_id: "s2".into(),
        items: vec![],
    });
    h.app.on_backend(Event::Ready {
        session_id: "s2".into(),
        config: catalog_config(),
    });
    h.draw();
    let t = h.screen.full_text();
    let summary = t
        .find("Token usage: total=1,500 input=1,200 output=300")
        .expect("summary");
    let second_header = t.rfind("Wizard").expect("second header");
    let first_header = t.find("Wizard").expect("first header");
    assert!(first_header < summary && summary < second_header, "{t}");
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
fn new_and_clear_are_refused_while_a_turn_runs() {
    let mut h = harness(120, 36, 7);
    ready(&mut h, catalog_config());
    h.app.on_backend(Event::TurnStart);
    h.type_str("/new");
    h.key(KeyCode::Enter);
    h.draw();
    assert!(h.sent().is_empty());
    assert!(
        h.screen
            .full_text()
            .contains("'/new' is disabled while a task is in progress.")
    );
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

// ---- /rename ---------------------------------------------------------------------------------

fn named_home(tag: &str) -> std::path::PathBuf {
    let home = std::env::temp_dir().join(format!("cxw-rename-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&home);
    std::fs::create_dir_all(home.join(".wizard/sessions")).unwrap();
    home
}

#[test]
fn rename_with_an_argument_prints_the_confirmation_and_names_the_exit_hint() {
    let home = named_home("arg");
    std::fs::write(home.join(".wizard/sessions/s1.jsonl"), "{}\n").unwrap();
    let mut h = harness_with(
        120,
        36,
        AppOpts {
            home: Some(home.display().to_string()),
            placeholder: Some(7),
            ..Default::default()
        },
    );
    ready(&mut h, catalog_config());
    h.type_str("/rename my-thread");
    h.key(KeyCode::Enter);
    h.draw();
    let t = h.screen.full_text();
    assert!(
        t.contains("• Session renamed to my-thread. To resume this session run wizard-ui-codex resume, then select my-thread (s1)"),
        "{t}"
    );
    let lines = h.app.exit_lines(false);
    assert_eq!(
        lines,
        vec!["To continue this session, run wizard-ui-codex resume, then select my-thread (s1)"]
    );
    let _ = std::fs::remove_dir_all(home);
}

#[test]
fn rename_without_an_argument_opens_the_prompt_view() {
    let home = named_home("view");
    let mut h = harness_with(
        120,
        36,
        AppOpts {
            home: Some(home.display().to_string()),
            placeholder: Some(7),
            ..Default::default()
        },
    );
    ready(&mut h, catalog_config());
    h.type_str("/rename");
    h.key(KeyCode::Enter);
    h.draw();
    let rows = h.screen.rows();
    let at = rows
        .iter()
        .position(|r| r == "▌ Name thread")
        .expect("title");
    assert_eq!(rows[at + 1], "▌");
    assert_eq!(rows[at + 2], "▌ Type a name and press Enter");
    assert_eq!(rows[at + 4], "Press enter to confirm or esc to go back");
    h.type_str("work");
    h.key(KeyCode::Enter);
    h.draw();
    assert!(h.screen.full_text().contains("Session renamed to work."));
    assert!(!h.screen.text().contains("Name thread"));
    let _ = std::fs::remove_dir_all(home);
}

// ---- commands wizard cannot back -------------------------------------------------------------

#[test]
fn permissions_and_feedback_say_wizard_cannot() {
    let mut h = harness(120, 36, 7);
    ready(&mut h, catalog_config());
    h.type_str("/permissions");
    h.key(KeyCode::Enter);
    h.type_str("/feedback");
    h.key(KeyCode::Enter);
    h.draw();
    let t = h.screen.full_text();
    assert!(
        t.contains("■ '/permissions' is not supported by wizard."),
        "{t}"
    );
    assert!(t.contains("Use the Mode setting: chat mode has no file or shell tools."));
    assert!(t.contains("'/feedback' is not supported by wizard."));
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
