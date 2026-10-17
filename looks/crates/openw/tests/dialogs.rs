//! Dialog snapshots at the two reference sizes, plus behaviour tests. Text snapshots live in
//! `tests/snapshots/dlg-*.txt`; set `OPENW_DUMP_DIR` to also write `.ansi` files that
//! `tools/cmp-rect.py` can diff against `reference/extra/dialog-*.ansi`, at the sizes the
//! references were captured at.

mod common;

use agent_core::{Config, Event};
use common::*;
use crossterm::event::{KeyCode, KeyModifiers};
use openw::app::AppOpts;
use std::path::PathBuf;

const SIZES: [(u16, u16); 2] = [(120, 36), (150, 42)];

fn ctrl(h: &mut Harness, c: char) {
    h.key(KeyCode::Char(c), KeyModifiers::CONTROL);
}

fn press(h: &mut Harness, c: KeyCode) {
    h.key(c, KeyModifiers::NONE);
}

fn slash(h: &mut Harness, cmd: &str) {
    h.type_str(cmd);
    press(h, KeyCode::Enter);
}

/// Render at each reference size, compare with the snapshot and dump the ANSI.
fn each(name: &str, f: impl Fn(&mut Harness)) {
    for (w, h) in SIZES {
        shot(name, w, h, &f);
    }
}

/// One size only, for references captured at a size other than the two standard ones.
fn shot(name: &str, w: u16, h: u16, f: &impl Fn(&mut Harness)) {
    let mut hn = Harness::new(w, h);
    f(&mut hn);
    check(name, w, h, &mut hn);
}

fn check(name: &str, w: u16, h: u16, hn: &mut Harness) {
    let key = format!("dlg-{name}-{w}x{h}");
    hn.dump(&key);
    assert_snapshot(&key, &hn.text());
}

/// The model list of the reference captures, one provider, nothing else connected.
fn opencode_models(h: &mut Harness) {
    let m = |n: &str| agent_core::ModelOption {
        id: format!("opencode/{n}"),
        name: n.into(),
        provider: "opencode".into(),
    };
    let mut cfg = agent_core::mock::config();
    cfg.model = "opencode/Big Pickle".into();
    cfg.models = vec![m("Big Pickle"), m("MiMo-V2.6-Flash Free")];
    h.event(Event::ConfigChanged(cfg));
}

fn no_models(h: &mut Harness) {
    let mut cfg: Config = agent_core::mock::config();
    cfg.models.clear();
    cfg.model.clear();
    h.event(Event::ConfigChanged(cfg));
}

#[test]
fn palette() {
    each("palette-home", |h| ctrl(h, 'p'));
    // not connected, as in the reference capture: `Connect provider` is suggested
    shot("palette-home-tall", 120, 100, &|h| {
        no_models(h);
        ctrl(h, 'p');
    });
    each("palette-filtered", |h| {
        ctrl(h, 'p');
        h.type_str("th");
    });
    each("palette-session", |h| {
        h.turn("hi", "Hello there.");
        ctrl(h, 'p');
    });
    shot("palette-session-tall", 120, 90, &|h| {
        no_models(h);
        h.sessions(2);
        h.app.prompt.stash.push("stashed".into());
        h.app.flags.thinking_shown = true;
        h.turn("hi", "Hello there.");
        ctrl(h, 'p');
    });
}

#[test]
fn models() {
    each("models", |h| slash(h, "/models"));
    each("models-opencode", |h| {
        opencode_models(h);
        slash(h, "/models");
    });
    each("models-not-connected", |h| {
        no_models(h);
        slash(h, "/models");
    });
    each("models-filtered", |h| {
        slash(h, "/models");
        h.type_str("fast");
    });
    each("models-favorite", |h| {
        slash(h, "/models");
        ctrl(h, 'f');
        press(h, KeyCode::Esc);
        slash(h, "/models");
    });
    each("variant", |h| {
        let mut cfg = agent_core::mock::config();
        cfg.effort = "turbo".into();
        h.event(Event::ConfigChanged(cfg));
        slash(h, "/models");
        press(h, KeyCode::Down);
        press(h, KeyCode::Enter);
    });
}

#[test]
fn themes() {
    each("themes", |h| slash(h, "/themes"));
    each("themes-down", |h| {
        slash(h, "/themes");
        press(h, KeyCode::Down);
    });
}

#[test]
fn sessions() {
    each("sessions", |h| {
        h.sessions(3);
        slash(h, "/sessions");
    });
    each("sessions-delete-confirm", |h| {
        h.sessions(3);
        slash(h, "/sessions");
        press(h, KeyCode::Down);
        ctrl(h, 'd');
    });
    each("sessions-empty", |h| slash(h, "/sessions"));
    each("sessions-pinned", |h| {
        h.sessions(3);
        slash(h, "/sessions");
        press(h, KeyCode::Down);
        ctrl(h, 'f');
    });
    each("sessions-rename", |h| {
        h.sessions(3);
        slash(h, "/sessions");
        ctrl(h, 'r');
    });
}

#[test]
fn small_dialogs() {
    each("agents", |h| slash(h, "/agents"));
    each("help", |h| slash(h, "/help"));
    each("status", |h| slash(h, "/status"));
    // the OS and terminal rows depend on the machine, so mask their values
    for (w, h) in SIZES {
        let mut hn = Harness::new(w, h);
        hn.turn("hi", "Hello there.");
        slash(&mut hn, "/debug");
        let key = format!("dlg-debug-{w}x{h}");
        hn.dump(&key);
        let masked: Vec<String> = hn
            .text()
            .lines()
            .map(|l| {
                for label in ["OS         ", "Terminal   "] {
                    if let Some(i) = l.find(label) {
                        return format!("{}{label}<machine>", &l[..i]);
                    }
                }
                l.to_string()
            })
            .collect();
        assert_snapshot(&key, &masked.join("\n"));
    }
    each("connect", |h| slash(h, "/connect"));
    each("connect-instructions", |h| {
        slash(h, "/connect");
        press(h, KeyCode::Enter);
    });
    each("rename", |h| {
        h.turn("Fix the parser", "Done.");
        ctrl(h, 'r');
    });
    each("export", |h| {
        h.turn("hi", "Hello there.");
        slash(h, "/export");
    });
    each("export-options", |h| {
        h.turn("hi", "Hello there.");
        slash(h, "/export");
        press(h, KeyCode::Tab);
        press(h, KeyCode::Tab);
        h.type_str(" ");
    });
}

fn with_wizard_dir(w: u16, h: u16) -> Harness {
    let dir = fixture_dir();
    Harness::with(
        w,
        h,
        AppOpts {
            cwd: PathBuf::from("/home/me/proj"),
            mock: true,
            seed: 2,
            wizard_dir: Some(dir),
            ..Default::default()
        },
    )
}

/// A throwaway `~/.wizard` with two skills, one MCP server and one plugin.
fn fixture_dir() -> PathBuf {
    // tests run in parallel inside one process, so every call gets its own directory
    static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let n = N.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!("openw-wizard-fixture-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    for (name, desc) in [
        (
            "common-sense",
            "Give the agent the judgment a competent engineer already has.",
        ),
        (
            "wrangler",
            "Run Wrangler. Shows status and budget, then does follow-up.",
        ),
    ] {
        let d = dir.join("skills").join(name);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(
            d.join("SKILL.md"),
            format!("---\nname: {name}\ndescription: {desc}\n---\n# {name}\n"),
        )
        .unwrap();
    }
    std::fs::create_dir_all(dir.join("plugins/tidy")).unwrap();
    std::fs::write(
        dir.join("mcp.toml"),
        "[[server]]\nname = \"playwright\"\ntransport = \"stdio\"\ncommand = \"npx\"\nargs = [\"-y\", \"@playwright/mcp@latest\"]\n",
    )
    .unwrap();
    dir
}

#[test]
fn wizard_dir_dialogs() {
    for (w, h) in SIZES {
        let mut hn = with_wizard_dir(w, h);
        slash(&mut hn, "/skills");
        check("skills", w, h, &mut hn);
        let mut hn = with_wizard_dir(w, h);
        slash(&mut hn, "/mcps");
        check("mcps", w, h, &mut hn);
        let mut hn = with_wizard_dir(w, h);
        slash(&mut hn, "/status");
        check("status-wizard", w, h, &mut hn);
    }
    each("skills-empty", |h| slash(h, "/skills"));
    each("mcps-empty", |h| slash(h, "/mcps"));
}

#[test]
fn stash_dialog() {
    each("stash", |h| {
        h.type_str("hello");
        ctrl(h, 'p');
        h.type_str("stash prompt");
        press(h, KeyCode::Enter);
        h.app
            .prompt
            .stash
            .push("a longer one\nwith two lines".into());
        h.app.stash_at.push(openw::app::FROZEN_NOW - 4000);
        ctrl(h, 'p');
        h.type_str("stash list");
        press(h, KeyCode::Enter);
    });
}

#[test]
fn message_dialogs() {
    let two = |h: &mut Harness| {
        h.turn(
            "Write a long, detailed 600-word story about a crab named Pip and the tide",
            "Once upon a tide.",
        );
        h.turn("queued second message", "Second answer.");
    };
    each("timeline", |h| {
        two(h);
        slash(h, "/timeline");
    });
    each("fork", |h| {
        two(h);
        slash(h, "/fork");
    });
}

/// With `OPENW_DUMP_DIR` set, render the states of `reference/extra/dialog-*` at the size each
/// was captured at, so `tools/cmp-rect.py` can compare them cell by cell.
#[test]
fn reference_sizes() {
    if std::env::var_os("OPENW_DUMP_DIR").is_none() {
        return;
    }
    let dump = |name: &str, w: u16, h: u16, f: &dyn Fn(&mut Harness)| {
        let mut hn = Harness::new(w, h);
        f(&mut hn);
        hn.dump(&format!("ref-{name}"));
    };
    for (w, hh) in [(120u16, 36u16), (150, 42)] {
        let mut hn = Harness::new(w, hh);
        no_models(&mut hn);
        // the 120x36 captures had sessions, the 150x42 ones did not
        if w == 120 {
            hn.sessions(3);
        }
        ctrl(&mut hn, 'p');
        hn.dump(&format!("ref-06-palette-{w}x{hh}"));
        let mut hn = Harness::new(w, hh);
        no_models(&mut hn);
        if w == 120 {
            hn.sessions(3);
        }
        ctrl(&mut hn, 'p');
        hn.type_str("th");
        hn.dump(&format!("ref-07-palette-filtered-{w}x{hh}"));
    }
    dump("help", 120, 44, &|h| slash(h, "/help"));
    dump("status", 120, 44, &|h| slash(h, "/status"));
    dump("agents", 120, 44, &|h| slash(h, "/agents"));
    dump("mcps", 120, 44, &|h| slash(h, "/mcps"));
    dump("skills", 120, 44, &|h| slash(h, "/skills"));
    dump("debug", 120, 44, &|h| slash(h, "/debug"));
    dump("themes", 120, 44, &|h| slash(h, "/themes"));
    dump("themes-preview-orng", 120, 44, &|h| {
        slash(h, "/themes");
        while !h.text().contains("orng") || h.app.theme.name != "orng" {
            press(h, KeyCode::Down);
        }
    });
    dump("sessions-empty", 120, 44, &|h| slash(h, "/sessions"));
    dump("models", 120, 36, &|h| {
        no_models(h);
        slash(h, "/models");
    });
    dump("connect", 120, 36, &|h| slash(h, "/connect"));
    dump("palette-filtered", 120, 36, &|h| {
        h.type_str("hello");
        ctrl(h, 'p');
        h.type_str("stash");
    });
    dump("rename", 120, 36, &|h| {
        h.turn("Pip’s 600-Word Crab Adventure", "x");
        ctrl(h, 'r');
    });
    dump("export", 120, 36, &|h| {
        h.turn("hi", "x");
        slash(h, "/export");
    });
    dump("stash", 120, 36, &|h| {
        h.app
            .prompt
            .stash
            .push("hello [Pasted ~4 lines]  end".into());
        h.app.stash_at.push(openw::app::FROZEN_NOW);
        ctrl(h, 'p');
        h.type_str("stash list");
        press(h, KeyCode::Enter);
    });
    let ref_sessions = |h: &mut Harness| {
        let mk = |id: &str, t: &str, cwd: &str, ago: i64| agent_core::SessionInfo {
            id: id.into(),
            title: t.into(),
            cwd: cwd.into(),
            updated: openw::app::FROZEN_NOW - ago,
        };
        h.event(Event::Sessions(vec![
            mk(
                "ses_1234567890",
                "Pip’s 600-Word Crab Adventure",
                "/home/me/proj",
                10,
            ),
            mk(
                "b",
                "Update greet return value in util.py",
                "/home/me/projperm",
                20,
            ),
            mk(
                "c",
                "README.md count, example.com fetch, color prompt",
                "/home/me/proj",
                30,
            ),
            mk(
                "d",
                "Rename util.py greet and create notes.md",
                "/home/me/proj",
                40,
            ),
        ]));
        slash(h, "/sessions");
    };
    let two = |h: &mut Harness| {
        h.turn(
            "Write a long, detailed 600-word story about a crab named Pip and the tide",
            "Once upon a tide.",
        );
        h.turn("queued second message", "Second answer.");
    };
    dump("timeline", 120, 36, &|h| {
        two(h);
        slash(h, "/timeline");
    });
    dump("fork", 120, 36, &|h| {
        two(h);
        slash(h, "/fork");
    });
    dump("sessions", 120, 40, &ref_sessions);
    dump("sessions-confirm", 120, 40, &|h| {
        ref_sessions(h);
        press(h, KeyCode::Down);
        ctrl(h, 'd');
    });
}

#[test]
fn toast_reference() {
    if std::env::var_os("OPENW_DUMP_DIR").is_none() {
        return;
    }
    let mut h = Harness::new(120, 40);
    h.app.toast(
        tuikit::theme::Variant::Success,
        "Message copied to clipboard!",
    );
    h.dump("ref-toast-success");
}

fn bash_perm() -> agent_core::PermissionRequest {
    agent_core::PermissionRequest {
        id: "p1".into(),
        tool: "bash".into(),
        kind: agent_core::ToolKind::Execute,
        title: "ls -la && wc -l util.py".into(),
        input: serde_json::json!({"command": "ls -la && wc -l util.py"}),
        diff: None,
        rule: "ls *\nwc *".into(),
    }
}

fn edit_perm() -> agent_core::PermissionRequest {
    agent_core::PermissionRequest {
        id: "p2".into(),
        tool: "edit".into(),
        kind: agent_core::ToolKind::Edit,
        title: "util.py".into(),
        input: serde_json::json!({"path": "util.py"}),
        diff: Some(agent_core::FileDiff {
            path: "util.py".into(),
            old: Some("def add(a, b):\n    return a + b\n\n\ndef greet(name):\n    return \"hello \" + name\n".into()),
            new: "def add(a, b):\n    return a + b\n\n\ndef greet(name):\n    return \"hi \" + name\n".into(),
        }),
        rule: "util.py".into(),
    }
}

fn session(h: &mut Harness) {
    // the reference sessions ran with the sidebar hidden
    h.app.sidebar_pref = openw::app::SidebarPref::Hide;
    h.turn(
        "Edit util.py: change the greet function",
        "Let me look at util.py first.",
    );
}

#[test]
fn permission_prompts() {
    each("permission-bash", |h| {
        session(h);
        h.event(Event::Permission(bash_perm()));
    });
    each("permission-edit", |h| {
        session(h);
        h.event(Event::Permission(edit_perm()));
    });
    each("permission-always", |h| {
        session(h);
        h.event(Event::Permission(bash_perm()));
        press(h, KeyCode::Right);
        press(h, KeyCode::Enter);
    });
    each("permission-fullscreen", |h| {
        session(h);
        h.event(Event::Permission(edit_perm()));
        ctrl(h, 'f');
    });
    each("permission-reject-selected", |h| {
        session(h);
        h.event(Event::Permission(edit_perm()));
        press(h, KeyCode::Right);
        press(h, KeyCode::Right);
    });
}

fn color_question() -> openw::ui::dialogs::ask::QuestionInfo {
    use openw::ui::dialogs::ask::{QuestionInfo, QuestionOption};
    QuestionInfo {
        header: "Color".into(),
        question: "Which color?".into(),
        options: vec![
            QuestionOption {
                label: "Red".into(),
                description: "Choose red".into(),
            },
            QuestionOption {
                label: "Blue".into(),
                description: "Choose blue".into(),
            },
        ],
        multiple: false,
        custom: true,
    }
}

#[test]
fn question_prompts() {
    use openw::ui::dialogs::ask::QuestionPrompt;
    each("question-single", |h| {
        session(h);
        h.app
            .ask_question(QuestionPrompt::new("q1", vec![color_question()]));
    });
    each("question-multi", |h| {
        session(h);
        let mut size = color_question();
        size.header = "Size".into();
        size.question = "Which sizes?".into();
        size.multiple = true;
        h.app
            .ask_question(QuestionPrompt::new("q2", vec![color_question(), size]));
        press(h, KeyCode::Enter);
    });
}

/// With `OPENW_DUMP_DIR` set, the states of `reference/extra/permission-*` at their size.
#[test]
fn permission_reference_sizes() {
    if std::env::var_os("OPENW_DUMP_DIR").is_none() {
        return;
    }
    let dump = |name: &str, f: &dyn Fn(&mut Harness)| {
        let mut hn = Harness::new(148, 42);
        f(&mut hn);
        hn.dump(&format!("ref-{name}"));
    };
    dump("permission-bash", &|h| {
        session(h);
        h.event(Event::Permission(bash_perm()));
    });
    dump("permission-edit-diff", &|h| {
        session(h);
        h.event(Event::Permission(edit_perm()));
    });
    dump("permission-always-stage", &|h| {
        session(h);
        h.event(Event::Permission(bash_perm()));
        press(h, KeyCode::Right);
        press(h, KeyCode::Enter);
    });
    dump("permission-fullscreen", &|h| {
        session(h);
        h.event(Event::Permission(edit_perm()));
        ctrl(h, 'f');
    });
    dump("session-question-prompt", &|h| {
        session(h);
        h.app
            .ask_question(openw::ui::dialogs::ask::QuestionPrompt::new(
                "q1",
                vec![color_question()],
            ));
    });
}

// ---- behaviour ---------------------------------------------------------------------------

/// Scratch directories made by this run, removed when the test process exits. A full run used
/// to leave twelve of them in /tmp each time.
static SCRATCH: std::sync::Mutex<Vec<PathBuf>> = std::sync::Mutex::new(Vec::new());

extern "C" fn remove_scratch() {
    if let Ok(dirs) = SCRATCH.lock() {
        for d in dirs.iter() {
            let _ = std::fs::remove_dir_all(d);
        }
    }
}

fn temp(name: &str) -> PathBuf {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        // SAFETY: registers a plain function that touches only a static.
        unsafe { libc::atexit(remove_scratch) };
    });
    let d = std::env::temp_dir().join(format!("openw-dlg-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    if let Ok(mut dirs) = SCRATCH.lock() {
        dirs.push(d.clone());
    }
    d
}

fn with_dirs(config: &std::path::Path, wizard: &std::path::Path) -> Harness {
    Harness::with(
        120,
        36,
        AppOpts {
            cwd: PathBuf::from("/home/me/proj"),
            mock: true,
            seed: 2,
            config_dir: Some(config.to_path_buf()),
            wizard_dir: Some(wizard.to_path_buf()),
            ..Default::default()
        },
    )
}

fn saved(config: &std::path::Path) -> serde_json::Value {
    serde_json::from_str(&std::fs::read_to_string(config.join("state.json")).unwrap()).unwrap()
}

#[test]
fn picking_a_model_sends_it_and_remembers_it() {
    let cfg = temp("model-cfg");
    let mut h = with_dirs(&cfg, &temp("model-w"));
    slash(&mut h, "/models");
    h.type_str("fast");
    press(&mut h, KeyCode::Enter);
    assert!(h.sent().contains(&agent_core::Request::SetModel(
        "xai-oauth/grok-4.6-fast".into()
    )));
    assert!(!h.app.dialogs.is_open());
    assert_eq!(saved(&cfg)["recent"][0], "xai-oauth/grok-4.6-fast");
    // reopened, it is under Recent
    slash(&mut h, "/models");
    assert!(h.text().contains("Recent"));
}

#[test]
fn ctrl_f_favorites_the_selected_model_and_survives_a_restart() {
    let cfg = temp("fav-cfg");
    let mut h = with_dirs(&cfg, &temp("fav-w"));
    slash(&mut h, "/models");
    ctrl(&mut h, 'f');
    assert_eq!(saved(&cfg)["favorites"][0], "xai-oauth/grok-4.6");
    let mut again = with_dirs(&cfg, &temp("fav-w2"));
    slash(&mut again, "/models");
    assert!(again.text().contains("Favorites"));
    // pressing it again removes it
    ctrl(&mut again, 'f');
    assert_eq!(saved(&cfg)["favorites"].as_array().unwrap().len(), 0);
}

#[test]
fn a_model_with_an_unknown_effort_asks_for_a_variant() {
    let mut h = Harness::new(120, 36);
    let mut cfg = agent_core::mock::config();
    cfg.effort = "turbo".into();
    h.event(Event::ConfigChanged(cfg));
    slash(&mut h, "/models");
    press(&mut h, KeyCode::Down);
    press(&mut h, KeyCode::Enter);
    assert!(h.text().contains("Select variant"));
    h.sent();
    press(&mut h, KeyCode::Down); // Default -> low
    press(&mut h, KeyCode::Enter);
    assert!(h
        .sent()
        .contains(&agent_core::Request::SetEffort("low".into())));
    assert!(!h.app.dialogs.is_open());
}

#[test]
fn the_theme_dialog_previews_keeps_and_restores() {
    let cfg = temp("theme-cfg");
    let mut h = with_dirs(&cfg, &temp("theme-w"));
    assert_eq!(h.app.theme.name, "opencode");
    slash(&mut h, "/themes");
    press(&mut h, KeyCode::Down);
    let previewed = h.app.theme.name.clone();
    assert_ne!(previewed, "opencode", "moving applies the theme live");
    press(&mut h, KeyCode::Esc);
    assert_eq!(h.app.theme.name, "opencode", "esc restores it");
    assert!(!cfg.join("state.json").exists(), "a preview is never saved");
    slash(&mut h, "/themes");
    press(&mut h, KeyCode::Down);
    press(&mut h, KeyCode::Enter);
    assert_eq!(h.app.theme.name, previewed);
    assert_eq!(saved(&cfg)["theme"], previewed.as_str());
    // a new run starts in it
    let again = with_dirs(&cfg, &temp("theme-w2"));
    assert_eq!(again.app.theme.name, previewed);
}

/// Text on the selected row is coloured from the theme the dialog opened under (opencode reads
/// it once), so previewing rosepine keeps `#0a0a0a`, but a dialog opened after the switch uses
/// the new theme (fidelity round 1, finding 8; `tools/critic/fid/f02_dialogs.py`).
#[test]
fn the_selected_row_text_colour_is_fixed_when_the_theme_dialog_opens() {
    use ratatui::style::Color;
    let sel_fg = |h: &mut Harness| -> Color {
        let t = h.render();
        let (w, hh) = h.app.size;
        let primary = h.app.theme.primary;
        for y in 0..hh {
            for x in 0..w {
                let c = t.cell(x, y).unwrap();
                if c.bg == primary && c.symbol() != " " {
                    return c.fg;
                }
            }
        }
        panic!("no selected row")
    };
    let cfg = temp("selfg-cfg");
    let mut h = with_dirs(&cfg, &temp("selfg-w"));
    let opening = h.app.theme.background;
    slash(&mut h, "/themes");
    h.render(); // the frame that follows opening it
                // some neighbours share the default background; go on until one does not
    for _ in 0..12 {
        press(&mut h, KeyCode::Down);
        if h.app.theme.background != opening {
            break;
        }
    }
    assert_ne!(h.app.theme.background, opening);
    assert_eq!(sel_fg(&mut h), opening, "preview keeps the opening colour");
    press(&mut h, KeyCode::Enter);
    let now = h.app.theme.background;
    slash(&mut h, "/themes");
    assert_eq!(sel_fg(&mut h), now, "a new dialog reads the live theme");
}

#[test]
fn sessions_load_pin_rename_and_delete() {
    let cfg = temp("sess-cfg");
    let wiz = temp("sess-w");
    std::fs::create_dir_all(wiz.join("sessions")).unwrap();
    std::fs::write(wiz.join("sessions/s1.jsonl"), "{}").unwrap();
    let mut h = with_dirs(&cfg, &wiz);
    h.sessions(3);
    slash(&mut h, "/sessions");
    // enter loads
    press(&mut h, KeyCode::Down);
    press(&mut h, KeyCode::Enter);
    assert!(h
        .sent()
        .contains(&agent_core::Request::LoadSession("s1".into())));
    // pin the first row: it moves to Pinned and takes slot 1
    h.sessions(3);
    slash(&mut h, "/sessions");
    ctrl(&mut h, 'f');
    assert_eq!(saved(&cfg)["pinned"][0], "s0");
    assert!(h.text().contains("Pinned"));
    assert!(h.text().contains("switch ctrl+x 1-9"));
    press(&mut h, KeyCode::Esc);
    // ctrl+x 1 opens the pinned one
    h.sent();
    h.key(KeyCode::Char('x'), KeyModifiers::CONTROL);
    press(&mut h, KeyCode::Char('1'));
    assert!(h
        .sent()
        .contains(&agent_core::Request::LoadSession("s0".into())));
    // rename
    slash(&mut h, "/sessions");
    ctrl(&mut h, 'r');
    assert!(h.text().contains("Rename Session"));
    for _ in 0..20 {
        press(&mut h, KeyCode::Backspace);
    }
    h.type_str("Fresh name");
    press(&mut h, KeyCode::Enter);
    assert_eq!(saved(&cfg)["titles"]["s0"], "Fresh name");
    // delete takes two presses and removes the file
    slash(&mut h, "/sessions");
    press(&mut h, KeyCode::Down);
    ctrl(&mut h, 'd');
    assert!(h.text().contains("Press ctrl+d again to confirm"));
    assert!(wiz.join("sessions/s1.jsonl").exists());
    press(&mut h, KeyCode::Up);
    assert!(
        !h.text().contains("Press ctrl+d again to confirm"),
        "moving cancels the confirmation"
    );
    press(&mut h, KeyCode::Down);
    ctrl(&mut h, 'd');
    ctrl(&mut h, 'd');
    assert!(!wiz.join("sessions/s1.jsonl").exists());
    assert!(!h.text().contains("Session 1"));
}

#[test]
fn the_sessions_dialog_refills_when_the_backend_answers() {
    let mut h = Harness::new(120, 36);
    slash(&mut h, "/sessions");
    assert!(h.text().contains("No results found"));
    h.sessions(2);
    assert!(h.text().contains("Session 1"));
    h.type_str("zzz");
    assert!(h.text().contains("No results found"));
}

#[test]
fn stash_enter_pops_and_ctrl_d_twice_deletes() {
    let mut h = Harness::new(120, 36);
    h.app.prompt.stash.push("first".into());
    h.app.prompt.stash.push("second one".into());
    h.app.stash_at.extend([openw::app::FROZEN_NOW; 2]);
    ctrl(&mut h, 'p');
    h.type_str("stash list");
    press(&mut h, KeyCode::Enter);
    assert!(h.text().contains("second one"));
    ctrl(&mut h, 'd');
    assert!(h.text().contains("Press ctrl+d again to confirm"));
    ctrl(&mut h, 'd');
    assert_eq!(h.app.prompt.stash, vec!["first".to_string()]);
    press(&mut h, KeyCode::Enter);
    assert_eq!(h.app.prompt.text(), "first");
    assert!(h.app.prompt.stash.is_empty());
}

#[test]
fn skills_insert_a_slash_command() {
    let mut h = with_wizard_dir(120, 36);
    slash(&mut h, "/skills");
    h.type_str("wrang");
    press(&mut h, KeyCode::Enter);
    assert_eq!(h.app.prompt.text(), "/wrangler ");
}

#[test]
fn connect_explains_the_login_and_never_asks_for_a_key() {
    let mut h = Harness::new(120, 36);
    slash(&mut h, "/connect");
    press(&mut h, KeyCode::Enter);
    let t = h.text();
    assert!(t.contains("wizard --login xai"), "{t}");
    assert!(!t.to_lowercase().contains("api key:"));
    press(&mut h, KeyCode::Enter);
    assert!(!h.app.dialogs.is_open());
}

#[test]
fn export_writes_the_chosen_file_and_honors_the_options() {
    let dir = temp("export");
    let path = dir.join("out.md");
    let mut h = Harness::new(120, 36);
    h.turn("hi", "Hello there.");
    slash(&mut h, "/export");
    for _ in 0..40 {
        press(&mut h, KeyCode::Backspace);
    }
    h.type_str(path.to_str().unwrap());
    press(&mut h, KeyCode::Enter);
    let md = std::fs::read_to_string(&path).unwrap();
    assert!(md.contains("## User") && md.contains("Hello there."));
}

#[test]
fn a_permission_decision_goes_back_to_the_backend_and_clears_the_prompt() {
    let mut h = Harness::new(120, 36);
    session(&mut h);
    h.event(Event::Permission(bash_perm()));
    assert!(h.text().contains("Permission required"));
    // typing goes to the prompt that is waiting, not the box under it
    h.type_str("zzz");
    assert!(h.app.prompt.text().is_empty());
    h.sent();
    press(&mut h, KeyCode::Enter);
    assert_eq!(
        h.sent(),
        vec![agent_core::Request::Decide {
            id: "p1".into(),
            allow: true,
            scope: agent_core::DecideScope::Once,
            note: String::new()
        }]
    );
    assert!(!h.text().contains("Permission required"));
    // always, confirmed
    h.event(Event::Permission(bash_perm()));
    press(&mut h, KeyCode::Char('l'));
    press(&mut h, KeyCode::Enter);
    assert!(h.text().contains("Always allow"));
    h.sent();
    press(&mut h, KeyCode::Enter);
    assert!(matches!(
        h.sent().as_slice(),
        [agent_core::Request::Decide {
            allow: true,
            scope: agent_core::DecideScope::Always,
            ..
        }]
    ));
    // esc rejects
    h.event(Event::Permission(edit_perm()));
    press(&mut h, KeyCode::Esc);
    assert!(matches!(
        h.sent().as_slice(),
        [agent_core::Request::Decide { allow: false, .. }]
    ));
}

#[test]
fn a_dismissed_question_is_reported() {
    use openw::ui::dialogs::ask::{AskOutcome, QuestionPrompt};
    let mut h = Harness::new(120, 36);
    session(&mut h);
    h.app
        .ask_question(QuestionPrompt::new("q1", vec![color_question()]));
    assert!(h.text().contains("Which color?"));
    press(&mut h, KeyCode::Esc);
    assert_eq!(
        h.app.question_results,
        vec![AskOutcome::Reject { id: "q1".into() }]
    );
    assert!(!h.text().contains("Which color?"));
}

#[test]
fn clicking_the_dim_layer_closes_and_clicking_a_row_picks_it() {
    use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
    let mut h = Harness::new(120, 36);
    slash(&mut h, "/themes");
    h.render();
    let click = |h: &mut Harness, x: u16, y: u16, kind| {
        h.app.on_mouse(MouseEvent {
            kind,
            column: x,
            row: y,
            modifiers: KeyModifiers::NONE,
        });
    };
    // outside the panel
    click(&mut h, 2, 2, MouseEventKind::Down(MouseButton::Left));
    click(&mut h, 2, 2, MouseEventKind::Up(MouseButton::Left));
    assert!(!h.app.dialogs.is_open());
    // on a row: down selects, up submits
    slash(&mut h, "/themes");
    h.render();
    click(&mut h, 40, 16, MouseEventKind::Moved);
    click(&mut h, 40, 16, MouseEventKind::Down(MouseButton::Left));
    click(&mut h, 40, 16, MouseEventKind::Up(MouseButton::Left));
    assert!(!h.app.dialogs.is_open());
    assert!(h.app.theme.name != "opencode" || h.text().contains("Ask anything"));
}

#[test]
fn toasts_have_the_reference_duration_and_replace_each_other() {
    use std::time::Duration;
    let mut h = Harness::new(120, 36);
    let mut cfg = agent_core::mock::config();
    cfg.models.truncate(1);
    h.event(Event::ConfigChanged(cfg));
    h.app.run_action(openw::keys::Action::ModelCycle);
    let t = h.app.toasts.current().unwrap();
    assert_eq!(t.message, "Add a favorite model to use this shortcut");
    assert_eq!(t.duration, Duration::from_secs(3));
    h.app
        .toast(tuikit::theme::Variant::Success, "Copied to clipboard");
    let t = h.app.toasts.current().unwrap();
    assert_eq!(t.message, "Copied to clipboard");
    assert_eq!(t.duration, Duration::from_millis(5000));
}

#[test]
fn model_cycle_walks_recent_models() {
    let mut h = Harness::new(120, 36);
    h.sent();
    h.app.run_action(openw::keys::Action::ModelCycle);
    // nothing recent yet: it walks every model
    assert!(matches!(
        h.sent().as_slice(),
        [agent_core::Request::SetModel(m)] if m == "xai-oauth/grok-4.6-fast"
    ));
}

#[test]
fn generic_alert_confirm_and_prompt() {
    use openw::ui::dialogs::panel::{Alert, Confirm, Prompt};
    use openw::ui::dialogs::Effect;
    each("alert", |h| {
        h.app.dialogs.push(Box::new(Alert::new(
            "Heads up",
            "wizard is not running; restart openw.",
        )));
    });
    each("confirm", |h| {
        h.app.dialogs.push(Box::new(Confirm::new(
            "Delete session",
            "This removes the session file and cannot be undone.",
            vec![Effect::Toast(tuikit::theme::Variant::Info, "gone".into())],
        )));
    });
    each("confirm-cancel", |h| {
        h.app.dialogs.push(Box::new(Confirm::new(
            "Delete session",
            "This removes the session file and cannot be undone.",
            vec![],
        )));
        press(h, KeyCode::Left);
    });
    each("prompt", |h| {
        let mut p = Prompt::new("Install plugin", "", "npm package name", Effect::Rename);
        p.description = Some("scope: local".into());
        h.app.dialogs.push(Box::new(p));
    });
}

#[test]
fn confirm_runs_its_effects_only_on_confirm() {
    use openw::ui::dialogs::panel::Confirm;
    use openw::ui::dialogs::Effect;
    let mut h = Harness::new(120, 36);
    let ask = || {
        Box::new(Confirm::new(
            "Sure?",
            "x",
            vec![Effect::Toast(tuikit::theme::Variant::Info, "done".into())],
        ))
    };
    h.app.dialogs.push(ask());
    press(&mut h, KeyCode::Left);
    press(&mut h, KeyCode::Enter);
    assert!(h.app.toasts.current().is_none(), "cancel runs nothing");
    h.app.dialogs.push(ask());
    press(&mut h, KeyCode::Enter);
    assert_eq!(h.app.toasts.current().unwrap().message, "done");
}

#[test]
fn more_dialogs() {
    each("message-actions", |h| {
        h.turn("Fix the parser", "Done.");
        slash(h, "/timeline");
        press(h, KeyCode::Enter);
    });
    each("subagent", |h| {
        h.app.dialogs.push(openw::ui::dialogs::timeline::subagent());
    });
    for (w, hh) in SIZES {
        let mut hn = with_wizard_dir(w, hh);
        ctrl(&mut hn, 'p');
        hn.type_str("plugins");
        press(&mut hn, KeyCode::Enter);
        check("plugins", w, hh, &mut hn);
        let mut hn = with_wizard_dir(w, hh);
        ctrl(&mut hn, 'p');
        hn.type_str("install plugin");
        press(&mut hn, KeyCode::Enter);
        check("install-plugin", w, hh, &mut hn);
    }
}

#[test]
fn a_skills_directory_that_cannot_be_read_says_so() {
    let dir = temp("skills-err");
    std::fs::write(dir.join("skills"), "not a directory").unwrap();
    for (w, h) in SIZES {
        let mut hn = Harness::with(
            w,
            h,
            AppOpts {
                cwd: PathBuf::from("/home/me/proj"),
                mock: true,
                seed: 2,
                wizard_dir: Some(dir.clone()),
                ..Default::default()
            },
        );
        slash(&mut hn, "/skills");
        let t = hn.text();
        assert!(t.contains("Could not load skills"), "{t}");
        check("skills-error", w, h, &mut hn);
    }
}

#[test]
fn message_actions_revert_rewinds_to_that_turn_and_restores_the_text() {
    let mut h = Harness::new(120, 36);
    h.turn("first question", "a");
    h.turn("second question", "b");
    slash(&mut h, "/timeline");
    // newest first: pick the older one
    press(&mut h, KeyCode::Down);
    press(&mut h, KeyCode::Enter);
    h.sent();
    press(&mut h, KeyCode::Enter); // Revert
    assert!(h
        .sent()
        .contains(&agent_core::Request::Prompt("/rewind 1".into())));
    assert_eq!(h.app.prompt.text(), "first question");
}

#[test]
fn the_ok_and_confirm_buttons_take_clicks() {
    use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
    use openw::ui::dialogs::panel::{Alert, Confirm};
    use openw::ui::dialogs::Effect;
    let up = |h: &mut Harness, x: u16, y: u16| {
        h.app.on_mouse(MouseEvent {
            kind: MouseEventKind::Up(MouseButton::Left),
            column: x,
            row: y,
            modifiers: KeyModifiers::NONE,
        });
    };
    // find the button by its text on screen
    let find = |h: &mut Harness, word: &str| -> (u16, u16) {
        let t = h.render();
        let (y, row) = (0..36)
            .map(|y| (y, t.row(y)))
            .find(|(_, r)| r.contains(word))
            .unwrap();
        (row[..row.find(word).unwrap()].chars().count() as u16, y)
    };
    let mut h = Harness::new(120, 36);
    h.app.dialogs.push(Box::new(Alert::new("Heads up", "msg")));
    let (x, y) = find(&mut h, "ok");
    up(&mut h, x, y);
    assert!(!h.app.dialogs.is_open());
    h.app.dialogs.push(Box::new(Confirm::new(
        "Sure?",
        "x",
        vec![Effect::Toast(tuikit::theme::Variant::Info, "done".into())],
    )));
    let (x, y) = find(&mut h, "Confirm");
    up(&mut h, x + 1, y);
    assert_eq!(h.app.toasts.current().unwrap().message, "done");
    assert!(!h.app.dialogs.is_open());
}
