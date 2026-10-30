//! Vim mode, keybindings.json and the permission prompt's auto mode option.

mod common;

use agent_core::Request;
use claudew::app::Dialog;
use claudew::keybindings::Keymap;
use claudew::vim::VimMode;
use common::Harness;
use crossterm::event::{KeyCode, KeyModifiers};

fn vim() -> Harness {
    let mut hn = Harness::new("static-120x40.jsonl", 120, 40);
    hn.app.vim.set_enabled(true);
    hn
}

fn row_containing(hn: &mut Harness, text: &str) -> bool {
    let t = hn.render();
    (0..t.area().height).any(|y| t.row(y).contains(text))
}

#[test]
fn vim_starts_in_insert_and_esc_moves_to_normal() {
    let mut hn = vim();
    assert_eq!(hn.app.vim.mode, VimMode::Insert);
    hn.type_str("hello");
    assert_eq!(hn.app.editor.text(), "hello");
    assert!(row_containing(&mut hn, "-- INSERT -- "));
    hn.press(KeyCode::Esc);
    assert_eq!(hn.app.vim.mode, VimMode::Normal);
    // NORMAL shows no label, as Claude Code does
    assert!(!row_containing(&mut hn, "-- NORMAL --"));
    assert!(!row_containing(&mut hn, "-- INSERT --"));
}

#[test]
fn normal_mode_keys_edit_instead_of_typing() {
    let mut hn = vim();
    hn.type_str("foo bar");
    hn.press(KeyCode::Esc);
    hn.type_str("0dw");
    assert_eq!(hn.app.editor.text(), "bar");
    hn.type_str("A!");
    assert_eq!(hn.app.editor.text(), "bar!");
    hn.press(KeyCode::Esc);
    hn.type_str("u");
    assert_eq!(hn.app.editor.text(), "bar");
    // a letter with no command is not typed
    hn.type_str("Z");
    assert_eq!(hn.app.editor.text(), "bar");
}

#[test]
fn enter_submits_from_normal_mode() {
    let mut hn = vim();
    hn.type_str("hi");
    hn.press(KeyCode::Esc);
    hn.press(KeyCode::Enter);
    assert!(hn.app.editor.is_empty());
    let sent = hn.sent();
    assert!(
        sent.iter()
            .any(|r| matches!(r, Request::Prompt(p) if p == "hi")),
        "{sent:?}"
    );
}

#[test]
fn visual_mode_is_named_and_selects() {
    let mut hn = vim();
    hn.type_str("one two");
    hn.press(KeyCode::Esc);
    hn.type_str("0ve");
    assert_eq!(hn.app.vim.mode, VimMode::Visual);
    assert!(row_containing(&mut hn, "-- VISUAL -- "));
    hn.type_str("d");
    assert_eq!(hn.app.editor.text(), " two");
    hn.type_str("V");
    assert!(row_containing(&mut hn, "-- VISUAL LINE -- "));
}

#[test]
fn the_slash_command_flips_the_mode() {
    let mut hn = Harness::new("static-120x40.jsonl", 120, 40);
    std::env::set_var("HOME", std::env::temp_dir().join("claudew-vim-home"));
    hn.type_str("/vim");
    hn.press(KeyCode::Enter);
    assert!(hn.app.vim.enabled);
    hn.type_str("/vim");
    hn.press(KeyCode::Enter);
    assert!(!hn.app.vim.enabled);
}

#[test]
fn k_on_the_first_line_recalls_history() {
    let mut hn = vim();
    hn.type_str("first");
    hn.press(KeyCode::Enter);
    hn.sent();
    hn.press(KeyCode::Esc);
    hn.type_str("k");
    assert_eq!(hn.app.editor.text(), "first");
}

fn with_bindings(json: &str) -> Harness {
    let mut hn = Harness::new("static-120x40.jsonl", 120, 40);
    let dir =
        std::env::temp_dir().join(format!("claudew-kb-{}-{}", std::process::id(), json.len()));
    std::fs::create_dir_all(dir.join(".claude")).expect("dir");
    std::fs::write(dir.join(".claude/keybindings.json"), json).expect("file");
    hn.app.keymap = Keymap::load(&dir, &dir);
    hn
}

#[test]
fn a_bound_command_runs_from_its_key() {
    let mut hn = with_bindings(
        r#"{"bindings":[{"context":"Chat","bindings":{"ctrl+k":"command:compact"}}]}"#,
    );
    hn.ctrl('k');
    let sent = hn.sent();
    assert!(
        sent.iter()
            .any(|r| matches!(r, Request::Prompt(p) if p == "/compact")),
        "{sent:?}"
    );
}

#[test]
fn null_silences_a_default_key() {
    let mut hn = with_bindings(r#"{"bindings":[{"context":"Chat","bindings":{"ctrl+s":null}}]}"#);
    hn.type_str("keep");
    hn.ctrl('s');
    // ctrl+s stashes the draft; unbound, the draft stays
    assert_eq!(hn.app.editor.text(), "keep");
}

#[test]
fn a_chord_runs_after_both_steps() {
    let mut hn = with_bindings(
        r#"{"bindings":[{"context":"Chat","bindings":{"ctrl+b ctrl+n":"chat:newline"}}]}"#,
    );
    hn.type_str("a");
    hn.ctrl('b');
    hn.type_str("");
    hn.ctrl('n');
    hn.type_str("b");
    assert_eq!(hn.app.editor.text(), "a\nb");
}

#[test]
fn an_action_rebinds_a_key() {
    let mut hn =
        with_bindings(r#"{"bindings":[{"context":"Chat","bindings":{"alt+j":"chat:newline"}}]}"#);
    hn.type_str("a");
    hn.key(KeyCode::Char('j'), KeyModifiers::ALT);
    hn.type_str("b");
    assert_eq!(hn.app.editor.text(), "a\nb");
}

#[test]
fn the_permission_prompt_offers_auto_mode_and_picking_it_switches_mode() {
    let mut hn = Harness::new("turn2-120x40.jsonl", 120, 40);
    hn.type_str("a");
    hn.press(KeyCode::Enter);
    hn.play();
    hn.type_str("b");
    hn.press(KeyCode::Enter);
    hn.play();
    let Some(Dialog::Permission { .. }) = hn.app.dialog else {
        panic!("no prompt");
    };
    assert!(row_containing(
        &mut hn,
        "3. Yes, and switch to auto mode \u{b7} auto mode handles these prompts for you"
    ));
    assert!(row_containing(&mut hn, "4. No"));
    assert!(row_containing(
        &mut hn,
        "Tip: auto mode handles these prompts for you"
    ));
    hn.sent();
    hn.press(KeyCode::Char('3'));
    let mut sent = hn.sent();
    assert!(
        sent.iter()
            .any(|r| matches!(r, Request::Decide { allow: true, .. })),
        "{sent:?}"
    );
    // wizard takes no mode change during a turn: it goes once the turn is over
    hn.play();
    hn.advance(3_000);
    sent.extend(hn.sent());
    assert!(
        sent.iter()
            .any(|r| matches!(r, Request::SetMode(m) if m == "auto")),
        "{sent:?}"
    );
}

#[test]
fn the_agents_hint_is_in_the_footer_for_ten_seconds_after_the_session_opens() {
    let mut hn = Harness::new("static-120x40.jsonl", 120, 40);
    assert!(row_containing(
        &mut hn,
        "? for shortcuts \u{b7} \u{2190} for agents"
    ));
    hn.advance(11_000);
    assert!(row_containing(&mut hn, "? for shortcuts"));
    assert!(!row_containing(&mut hn, "for agents"));
}
