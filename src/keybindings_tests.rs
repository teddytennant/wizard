use super::*;

fn key(s: &str) -> Stroke {
    parse_stroke(s).expect("a key")
}

fn map(json: &str) -> Keymap {
    let mut km = Keymap::new();
    km.add_json(json, "test");
    km
}

const CHAT: &[&str] = &["Global", "Chat"];

#[test]
fn strokes_parse_with_aliases() {
    let k = key("Ctrl+Shift+K");
    assert!(k.ctrl && k.shift && !k.alt);
    assert_eq!(k.key, "k");
    assert!(key("opt+p").alt);
    assert!(key("meta+p").alt);
    assert!(key("cmd+k").sup);
    assert_eq!(key("esc").key, "escape");
    assert_eq!(key("return").key, "enter");
    assert_eq!(key("ctrl++").key, "+");
    assert!(parse_stroke("hyper+k").is_err());
    assert!(parse_stroke("").is_err());
}

#[test]
fn a_space_makes_a_chord() {
    let c = parse_chord("ctrl+x ctrl+k").expect("a chord");
    assert_eq!(c.len(), 2);
    assert_eq!(c[1], key("ctrl+k"));
}

#[test]
fn crossterm_events_become_strokes() {
    let ev = |code, m| Stroke::from(KeyEvent::new(code, m));
    assert_eq!(ev(KeyCode::Char('k'), KeyModifiers::CONTROL), key("ctrl+k"));
    assert_eq!(ev(KeyCode::Char('K'), KeyModifiers::NONE), key("shift+k"));
    assert_eq!(ev(KeyCode::BackTab, KeyModifiers::SHIFT), key("shift+tab"));
    assert_eq!(ev(KeyCode::Char(' '), KeyModifiers::NONE), key("space"));
    assert_eq!(ev(KeyCode::Esc, KeyModifiers::NONE), key("escape"));
    assert_eq!(ev(KeyCode::Char('p'), KeyModifiers::ALT), key("alt+p"));
}

#[test]
fn defaults_resolve_to_default() {
    let mut km = Keymap::new();
    let now = Instant::now();
    assert_eq!(km.resolve(CHAT, key("ctrl+r"), now), Resolved::Default);
    assert_eq!(km.resolve(CHAT, key("a"), now), Resolved::Default);
}

#[test]
fn a_user_binding_names_an_action() {
    let mut km = map(r#"{"bindings":[{"context":"Chat","bindings":{"ctrl+k":"chat:stash"}}]}"#);
    assert_eq!(
        km.resolve(CHAT, key("ctrl+k"), Instant::now()),
        Resolved::Action("chat:stash".into())
    );
}

#[test]
fn null_unbinds_a_default() {
    let mut km = map(r#"{"bindings":[{"context":"Chat","bindings":{"ctrl+s":null}}]}"#);
    assert_eq!(
        km.resolve(CHAT, key("ctrl+s"), Instant::now()),
        Resolved::Unbound
    );
}

#[test]
fn the_last_rule_wins() {
    let mut km = map(
        r#"{"bindings":[{"context":"Chat","bindings":{"ctrl+k":"chat:stash"}},
                        {"context":"Global","bindings":{"ctrl+k":"app:toggleTodos"}}]}"#,
    );
    assert_eq!(
        km.resolve(CHAT, key("ctrl+k"), Instant::now()),
        Resolved::Action("app:toggleTodos".into())
    );
}

#[test]
fn a_context_that_is_not_active_does_not_apply() {
    let mut km =
        map(r#"{"bindings":[{"context":"Transcript","bindings":{"ctrl+k":"transcript:exit"}}]}"#);
    assert_eq!(
        km.resolve(CHAT, key("ctrl+k"), Instant::now()),
        Resolved::Default
    );
}

#[test]
fn chords_finish_or_cancel() {
    let mut km =
        map(r#"{"bindings":[{"context":"Chat","bindings":{"ctrl+b ctrl+n":"chat:newline"}}]}"#);
    let t = Instant::now();
    assert_eq!(km.resolve(CHAT, key("ctrl+b"), t), Resolved::Pending);
    assert_eq!(
        km.resolve(CHAT, key("ctrl+n"), t),
        Resolved::Action("chat:newline".into())
    );
    assert_eq!(km.resolve(CHAT, key("ctrl+b"), t), Resolved::Pending);
    assert_eq!(km.resolve(CHAT, key("x"), t), Resolved::Cancelled);
    assert_eq!(km.resolve(CHAT, key("x"), t), Resolved::Default);
}

#[test]
fn a_chord_times_out_after_three_seconds() {
    let mut km =
        map(r#"{"bindings":[{"context":"Chat","bindings":{"ctrl+b ctrl+n":"chat:newline"}}]}"#);
    let t = Instant::now();
    assert_eq!(km.resolve(CHAT, key("ctrl+b"), t), Resolved::Pending);
    let late = t + CHORD_TIMEOUT + Duration::from_millis(1);
    assert_eq!(km.resolve(CHAT, key("ctrl+n"), late), Resolved::Default);
}

#[test]
fn unknown_actions_contexts_and_reserved_keys_are_skipped_with_a_warning() {
    let km = map(
        r#"{"bindings":[{"context":"Chat","bindings":{"ctrl+k":"chat:nope","ctrl+c":"chat:stash","ctrl+j":"chat:newline"}},
                        {"context":"Nowhere","bindings":{"ctrl+k":"chat:stash"}}]}"#,
    );
    assert_eq!(km.warnings.len(), 3, "{:?}", km.warnings);
    assert!(km.has_user_rules());
}

#[test]
fn command_actions_name_a_slash_command() {
    let mut km =
        map(r#"{"bindings":[{"context":"Chat","bindings":{"ctrl+k":"command:compact"}}]}"#);
    assert_eq!(
        km.resolve(CHAT, key("ctrl+k"), Instant::now()),
        Resolved::Action("command:compact".into())
    );
}

#[test]
fn hints_follow_a_rebinding() {
    let mut km = Keymap::new();
    assert_eq!(km.shortcut(CHAT, "chat:stash").as_deref(), Some("ctrl+s"));
    km.add_json(
        r#"{"bindings":[{"context":"Chat","bindings":{"ctrl+k":"chat:stash"}}]}"#,
        "test",
    );
    assert_eq!(km.shortcut(CHAT, "chat:stash").as_deref(), Some("ctrl+k"));
}

#[test]
fn a_file_is_read_from_home_and_the_project_and_reloaded() {
    let dir = std::env::temp_dir().join(format!("wz-kb-{}", std::process::id()));
    let home = dir.join("home");
    let proj = dir.join("proj");
    std::fs::create_dir_all(home.join(".claude")).expect("home");
    std::fs::create_dir_all(proj.join(".claude")).expect("proj");
    std::fs::write(
        home.join(".claude/keybindings.json"),
        r#"{"bindings":[{"context":"Chat","bindings":{"ctrl+k":"chat:stash"}}]}"#,
    )
    .expect("write");
    let mut km = Keymap::load(&home, &proj);
    assert_eq!(
        km.resolve(CHAT, key("ctrl+k"), Instant::now()),
        Resolved::Action("chat:stash".into())
    );
    std::thread::sleep(Duration::from_millis(20));
    std::fs::write(
        proj.join(".claude/keybindings.json"),
        r#"{"bindings":[{"context":"Chat","bindings":{"ctrl+k":"chat:undo"}}]}"#,
    )
    .expect("write");
    assert!(km.reload());
    assert_eq!(
        km.resolve(CHAT, key("ctrl+k"), Instant::now()),
        Resolved::Action("chat:undo".into())
    );
    assert!(!km.reload());
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn malformed_json_warns_and_keeps_defaults() {
    let km = map("{ not json");
    assert_eq!(km.warnings.len(), 1);
    assert!(!km.has_user_rules());
}
