//! `keybindings.json`, vim mode and the status line command in the house composer.

use super::*;
use crate::keybindings::Keymap;

fn with_bindings(json: &str) -> App {
    let mut app = app();
    app.keymap.add_json(json, "test");
    app
}

#[test]
fn a_rule_from_the_file_runs_its_action() {
    let mut app =
        with_bindings(r#"{"bindings":[{"context":"Chat","bindings":{"alt+j":"chat:newline"}}]}"#);
    type_str(&mut app, "a");
    press_mod(&mut app, KeyCode::Char('j'), KeyModifiers::ALT);
    type_str(&mut app, "b");
    assert_eq!(app.input, "a\nb");
}

#[test]
fn a_bound_command_is_submitted() {
    let mut app =
        with_bindings(r#"{"bindings":[{"context":"Chat","bindings":{"ctrl+k":"command:todos"}}]}"#);
    let action = press_ctrl(&mut app, 'k');
    assert!(matches!(action, Some(AppAction::Command(_))), "{action:?}");
}

#[test]
fn a_chord_needs_both_keys() {
    let mut app = with_bindings(
        r#"{"bindings":[{"context":"Chat","bindings":{"ctrl+b ctrl+n":"chat:newline"}}]}"#,
    );
    type_str(&mut app, "a");
    press_ctrl(&mut app, 'b');
    assert_eq!(app.input, "a");
    press_ctrl(&mut app, 'n');
    assert_eq!(app.input, "a\n");
}

#[test]
fn the_file_can_unbind_a_default_key() {
    let mut app = with_bindings(r#"{"bindings":[{"context":"Chat","bindings":{"ctrl+u":null}}]}"#);
    // ctrl+u is not a Claude default, so unbinding it changes nothing here
    type_str(&mut app, "keep");
    press_ctrl(&mut app, 'u');
    assert_eq!(app.input, "");
    let mut app =
        with_bindings(r#"{"bindings":[{"context":"Global","bindings":{"ctrl+t":null}}]}"#);
    let shown = app.show_todos;
    press_ctrl(&mut app, 't');
    assert_eq!(app.show_todos, shown);
}

#[test]
fn without_a_file_every_key_is_the_composers_own() {
    let mut app = app();
    assert!(!app.keymap.has_user_rules());
    type_str(&mut app, "ab");
    press_ctrl(&mut app, 'u');
    assert!(app.input.is_empty());
    let _ = Keymap::new();
}

#[test]
fn editor_mode_vim_in_claudes_settings_turns_vim_on() {
    let dir = std::env::temp_dir().join(format!("wz-edit-{}", std::process::id()));
    std::fs::create_dir_all(dir.join(".claude")).expect("dir");
    std::fs::write(dir.join(".claude/settings.json"), r#"{"editorMode":"vim"}"#).expect("write");
    let mut app = app();
    app.load_claude_input(&dir, &dir);
    assert!(app.vim.enabled);
    std::fs::write(
        dir.join(".claude/settings.json"),
        r#"{"editorMode":"normal"}"#,
    )
    .expect("write");
    let mut app = crate::app::tests::app();
    app.load_claude_input(&dir, &dir);
    assert!(!app.vim.enabled);
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn vim_normal_mode_follows_claude_for_a_few_keys() {
    let mut app = app();
    app.vim.set_enabled(true);
    type_str(&mut app, "one two three");
    press(&mut app, KeyCode::Esc);
    type_str(&mut app, "0wcwTWO");
    press(&mut app, KeyCode::Esc);
    assert_eq!(app.input, "one TWO three");
    // dot repeats the change on the next word
    type_str(&mut app, "w.");
    assert_eq!(app.input, "one TWO TWO");
    // a change is two undo steps, as in Claude Code: the typed text, then the deletion
    type_str(&mut app, "u");
    assert_eq!(app.input, "one TWO ");
    type_str(&mut app, "u");
    assert_eq!(app.input, "one TWO three");
}

#[test]
fn vim_visual_mode_deletes_a_selection() {
    let mut app = app();
    app.vim.set_enabled(true);
    type_str(&mut app, "hello world");
    press(&mut app, KeyCode::Esc);
    type_str(&mut app, "0vex");
    assert_eq!(app.input, " world");
}

#[test]
fn the_statusline_command_writes_and_removes_the_setting() {
    let dir = std::env::temp_dir().join(format!("wz-slc-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("dir");
    let mut app = app();
    app.statusline_command_in(&dir, "echo hi");
    let text = std::fs::read_to_string(dir.join(".claude/settings.json")).expect("settings");
    let doc: serde_json::Value = serde_json::from_str(&text).expect("json");
    assert_eq!(doc["statusLine"]["command"], "echo hi");
    assert_eq!(doc["statusLine"]["type"], "command");
    assert_eq!(
        app.status_line.cfg.as_ref().map(|c| c.command.as_str()),
        Some("echo hi")
    );
    app.statusline_command_in(&dir, "off");
    let text = std::fs::read_to_string(dir.join(".claude/settings.json")).expect("settings");
    assert!(!text.contains("statusLine"), "{text}");
    assert!(app.status_line.cfg.is_none());
    std::fs::remove_dir_all(&dir).ok();
}
