//! The modals: the cheatsheet, settings, context, usage, session info, tutorial, docs and the
//! doctor block, at the three sizes the captures use. `GROKW_DUMP_DIR` writes `.ansi` files for
//! `tools/grokw-cmp.py`; `UPDATE_SNAPSHOTS=1` rewrites the text snapshots.

mod common;

use common::*;
use crossterm::event::{KeyCode, KeyModifiers};

const SIZES: [(u16, u16); 3] = [(120, 36), (150, 42), (80, 24)];

fn each(name: &str, f: impl Fn(&mut Harness)) {
    for (w, h) in SIZES {
        let mut hn = Harness::new(w, h);
        hn.app.tr.usage.context_tokens = 1_475;
        hn.app.tr.usage.context_window = 256_000;
        f(&mut hn);
        hn.dump(&format!("{name}-{w}x{h}"));
        assert_snapshot(&format!("{name}-{w}x{h}"), &hn.text());
    }
}

#[test]
fn shortcuts_cheatsheet() {
    each("shortcuts", |h| h.ctrl('x'));
    // Input open: eight Downs onto its header, then Right
    each("shortcuts-expanded", |h| {
        h.ctrl('x');
        for _ in 0..8 {
            h.press(KeyCode::Down);
        }
        h.press(KeyCode::Right);
    });
    // the Dashboard group: Down to the last header, Right
    each("shortcuts-more", |h| {
        h.ctrl('x');
        for _ in 0..13 {
            h.press(KeyCode::Down);
        }
        h.press(KeyCode::Right);
    });
}

#[test]
fn shortcuts_search_filter_and_detail() {
    let mut h = Harness::new(120, 36);
    h.ctrl('x');
    h.type_str("/");
    h.type_str("stash");
    h.dump("shortcuts-search-120x36");
    assert_snapshot("shortcuts-search-120x36", &h.text());
    // Enter keeps the search; Down onto the row, Enter opens its page
    h.press(KeyCode::Enter);
    h.press(KeyCode::Down);
    h.press(KeyCode::Enter);
    h.dump("shortcuts-detail-120x36");
    assert_snapshot("shortcuts-detail-120x36", &h.text());
    h.press(KeyCode::Esc);
    h.press(KeyCode::Esc);
    assert!(h.app.modal.is_none());
    // f hides the dimmed rows
    h.ctrl('x');
    h.type_str("f");
    h.dump("shortcuts-filter-120x36");
    assert_snapshot("shortcuts-filter-120x36", &h.text());
    // Ctrl+. closes from anywhere
    h.key(KeyCode::Char('.'), KeyModifiers::CONTROL);
    assert!(h.app.modal.is_none());
}

fn after_turns(h: &mut Harness) {
    h.long_turn(5);
    h.pin_durations(&[5.9, 2.5, 1.0, 0.8], 8.8);
    h.app.tr.usage.context_tokens = 28_064;
    h.app.tr.usage.input_tokens = 492_589;
    h.app.tr.usage.cached_tokens = 413_952;
    h.app.tr.usage.output_tokens = 4_642;
    h.app.tr.usage.cost_usd = Some(0.1383);
}

#[test]
fn context_usage_and_session_info() {
    for (name, cmd) in [
        ("context", "/context"),
        ("usage", "/usage"),
        ("session-info", "/session-info"),
    ] {
        each(name, |h| {
            h.type_str(cmd);
            h.press(KeyCode::Enter);
            h.press(KeyCode::Esc);
            h.type_str(cmd);
            h.press(KeyCode::Enter);
        });
    }
    let mut h = Harness::new(120, 36);
    after_turns(&mut h);
    h.type_str("/context");
    h.press(KeyCode::Enter);
    h.dump("context-turns-120x36");
    assert_snapshot("context-turns-120x36", &h.text());
    h.press(KeyCode::Tab);
    h.dump("usage-turns-120x36");
    assert_snapshot("usage-turns-120x36", &h.text());
    h.press(KeyCode::Tab);
    h.dump("session-info-turns-120x36");
    assert_snapshot("session-info-turns-120x36", &h.text());
    h.press(KeyCode::Tab);
    assert!(matches!(h.app.modal, Some(grokw::ui::dialogs::Modal::Usage(ref m)) if m.tab == 0));
    h.press(KeyCode::Esc);
    assert!(h.app.modal.is_none());
}

#[test]
fn settings_modal() {
    each("settings", |h| h.key(KeyCode::F(2), KeyModifiers::NONE));
    // the list scrolled to `Confirm before rewind`
    let mut h = Harness::new(120, 36);
    h.app.tr.usage.context_tokens = 1_475;
    h.app.tr.usage.context_window = 256_000;
    h.press(KeyCode::F(2));
    for _ in 0..24 {
        h.press(KeyCode::Down);
    }
    h.dump("settings-scrolled-120x36");
    assert_snapshot("settings-scrolled-120x36", &h.text());
}

#[test]
fn tutorial_list_and_topic_pages() {
    each("tutorial", |h| {
        h.type_str("/tutorial");
        h.press(KeyCode::Enter);
    });
    let mut h = Harness::new(120, 36);
    h.type_str("/tutorial");
    h.press(KeyCode::Enter);
    h.press(KeyCode::Down);
    h.press(KeyCode::Enter);
    h.dump("tutorial-page-120x36");
    assert_snapshot("tutorial-page-120x36", &h.text());
    // Right reads on, Esc goes back to the list with the topics ticked
    h.press(KeyCode::Right);
    h.press(KeyCode::Esc);
    h.dump("tutorial-explored-120x36");
    assert_snapshot("tutorial-explored-120x36", &h.text());
    h.press(KeyCode::Esc);
    assert!(h.app.modal.is_none());
}

#[test]
fn docs_list_and_guide_page() {
    let dir = std::env::temp_dir().join("grokw-docs-test");
    let guides = dir.join("docs/user-guide");
    std::fs::create_dir_all(&guides).unwrap();
    std::fs::write(
        guides.join("01-getting-started.md"),
        "# Getting Started\n\nGrok Build is a terminal-based AI coding assistant.\n\n## Installation\n\nRun the installer.\n",
    )
    .unwrap();
    std::fs::write(
        guides.join("03-keyboard-shortcuts.md"),
        "# Keyboard Shortcuts\n\nKeys.\n",
    )
    .unwrap();
    std::env::set_var("GROK_HOME", &dir);
    // `/docs` takes an argument, so the first Enter completes it and the second runs it
    each("docs", |h| {
        h.type_str("/docs");
        h.press(KeyCode::Enter);
        h.press(KeyCode::Enter);
    });
    let mut h = Harness::new(120, 36);
    h.type_str("/docs");
    h.press(KeyCode::Enter);
    h.press(KeyCode::Enter);
    h.press(KeyCode::Enter);
    h.dump("docs-page-120x36");
    assert_snapshot("docs-page-120x36", &h.text());
    h.press(KeyCode::Esc);
    h.press(KeyCode::Esc);
    assert!(h.app.modal.is_none());
    // /docs <title> opens the guide directly
    h.type_str("/docs keyboard");
    h.press(KeyCode::Enter);
    assert!(h.text().contains("Keyboard Shortcuts"));
    std::env::remove_var("GROK_HOME");
    std::fs::remove_dir_all(&dir).ok();
    // without the directory the list says so
    std::env::set_var("GROK_HOME", dir.join("none"));
    let mut h = Harness::new(120, 36);
    h.type_str("/docs");
    h.press(KeyCode::Enter);
    h.press(KeyCode::Enter);
    assert!(h.text().contains("No guides found"));
    std::env::remove_var("GROK_HOME");
}

#[test]
fn doctor_block_in_the_transcript() {
    use grokw::ui::dialogs::doctor::{report, Env};
    let env = Env {
        terminal: "Unknown".into(),
        multiplexer: "tmux",
        ssh: false,
        truecolor: true,
        native_clipboard: false,
        tmux_clipboard: Some(true),
        tmux_passthrough: Some(false),
        tmux_extended_keys: Some(true),
    };
    each("doctor", |h| {
        h.app.enter_session();
        h.app.note(report(&env));
    });
    // /doctor fix is refused in words
    let mut h = Harness::new(120, 36);
    h.type_str("/doctor fix ssh-wrap");
    h.press(KeyCode::Enter);
    assert!(h.text().contains("/doctor fix is not supported by grokw"));
}

#[test]
fn settings_rows_act_and_remember() {
    let mut h = Harness::new(120, 36);
    h.press(KeyCode::F(2));
    // Enter on `Compact mode` toggles it and says so
    h.press(KeyCode::Enter);
    assert!(h.app.compact_mode);
    assert!(h
        .app
        .toast
        .as_ref()
        .is_some_and(|t| t.text == "✓ Compact mode: on"));
    // Space on a row grokw only remembers
    h.press(KeyCode::Down); // Default screen mode, an enum
    h.press(KeyCode::Down); // Show timestamps
    h.press(KeyCode::Char(' '));
    assert!(!h.app.timestamps);
    // d asks first, y resets
    h.press(KeyCode::Char('d'));
    h.dump("settings-reset-120x36");
    assert_snapshot("settings-reset-120x36", &h.text());
    h.press(KeyCode::Char('y'));
    assert!(h.app.timestamps);
    // an enum opens a chooser, Esc backs out of it, a second Esc closes the window
    h.press(KeyCode::Up);
    h.press(KeyCode::Enter);
    h.dump("settings-chooser-120x36");
    assert_snapshot("settings-chooser-120x36", &h.text());
    h.press(KeyCode::Esc);
    assert!(h.app.modal.is_some());
    // an integer opens a stepper
    for _ in 0..10 {
        h.press(KeyCode::Down);
    }
    h.press(KeyCode::Enter);
    h.press(KeyCode::Up);
    h.press(KeyCode::Up);
    h.dump("settings-stepper-120x36");
    assert_snapshot("settings-stepper-120x36", &h.text());
    h.press(KeyCode::Enter);
    assert_eq!(h.app.prefs_get_int("max_thoughts_width"), Some(130));
    // / filters by label and keywords
    h.type_str("/");
    h.type_str("vim");
    h.dump("settings-search-120x36");
    assert_snapshot("settings-search-120x36", &h.text());
    h.press(KeyCode::Esc);
    h.press(KeyCode::Esc);
    h.press(KeyCode::Esc);
    assert!(h.app.modal.is_none());
}

#[test]
fn theme_chooser_previews_and_reverts() {
    let mut h = Harness::new(120, 36);
    h.press(KeyCode::F(2));
    for _ in 0..7 {
        h.press(KeyCode::Down);
    }
    h.press(KeyCode::Enter);
    let before = h.app.theme.kind;
    h.press(KeyCode::Down);
    h.press(KeyCode::Down);
    assert_ne!(h.app.theme.kind, before);
    h.press(KeyCode::Esc);
    assert_eq!(h.app.theme.kind, before);
}
