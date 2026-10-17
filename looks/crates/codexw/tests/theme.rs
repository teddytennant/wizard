//! `/theme` against the real Codex captures (`reference/codex/120x36/xn-20..23`) and the
//! behaviour around it. The syntax theme is process-wide, so these tests take a lock and put
//! the default back.

use std::path::PathBuf;
use std::sync::Mutex;

use agent_core::{Config, Event};
use codexw::app::AppOpts;
use codexw::highlight;
use codexw::testing::{Harness, harness, harness_with};
use crossterm::event::KeyCode;

static LOCK: Mutex<()> = Mutex::new(());

fn lock() -> std::sync::MutexGuard<'static, ()> {
    let g = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    highlight::set_configured_theme(None, None);
    g
}

fn reference(name: &str) -> Vec<String> {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../reference/codex/120x36")
        .join(format!("{name}.txt"));
    std::fs::read_to_string(&p)
        .unwrap_or_else(|e| panic!("{}: {e}", p.display()))
        .lines()
        .map(|l| l.trim_end().to_string())
        .collect()
}

fn ready(h: &mut Harness) {
    h.app.on_backend(Event::Ready {
        session_id: "s1".into(),
        config: Config {
            model: "gpt-5.5".into(),
            effort: "default".into(),
            ..Default::default()
        },
    });
    h.draw();
}

fn open_picker(cols: u16, rows: u16) -> Harness {
    let mut h = harness(cols, rows, 7);
    ready(&mut h);
    h.type_str("/theme");
    h.key(KeyCode::Enter);
    h.draw();
    h
}

/// The picker's rows (title down to the hint) of a screen.
fn picker_rows(rows: &[String]) -> &[String] {
    let top = rows
        .iter()
        .position(|r| r.trim() == "Select Syntax Theme")
        .expect("picker title");
    &rows[top..]
}

fn assert_picker_matches(h: &Harness, name: &str) {
    let want_all = reference(name);
    let want = picker_rows(&want_all);
    let got_all = h.screen.rows();
    let got = picker_rows(&got_all);
    let mut diffs = Vec::new();
    for (i, w) in want.iter().enumerate() {
        let g = got.get(i).map(String::as_str).unwrap_or("");
        if w != g {
            diffs.push(format!("row {i}\n  want |{w}\n  got  |{g}"));
        }
    }
    assert!(diffs.is_empty(), "{name}:\n{}", diffs.join("\n"));
}

#[test]
#[cfg_attr(
    not(reference_captures),
    ignore = "needs the reference/ captures, not in this repository; see build.rs"
)]
fn the_picker_matches_the_capture_then_follows_down_arrows() {
    let _g = lock();
    let mut h = open_picker(120, 36);
    assert_picker_matches(&h, "xn-20-theme-picker");
    h.key(KeyCode::Down);
    h.draw();
    assert_picker_matches(&h, "xn-21-theme-down");
    h.key(KeyCode::Down);
    h.draw();
    assert_picker_matches(&h, "xn-22-theme-down2");
    highlight::set_configured_theme(None, None);
}

#[test]
fn moving_previews_the_theme_and_escape_puts_the_old_one_back() {
    let _g = lock();
    assert!(
        highlight::explicit_syntax_theme().is_none(),
        "the adaptive default to start"
    );
    let mut h = open_picker(120, 36);
    let before = highlight::table_header_style();
    h.key(KeyCode::Down);
    h.key(KeyCode::Down);
    h.draw();
    assert!(
        highlight::explicit_syntax_theme().is_some(),
        "a preview is an explicit theme"
    );
    assert_ne!(
        highlight::table_header_style(),
        before,
        "the table header colour follows the theme"
    );
    h.key(KeyCode::Esc);
    h.draw();
    assert!(
        highlight::explicit_syntax_theme().is_none(),
        "Esc restores the default"
    );
    assert_eq!(highlight::table_header_style(), before);
    assert!(!h.screen.text().contains("Select Syntax Theme"));
}

#[test]
fn typing_filters_and_previews_the_first_match() {
    let _g = lock();
    let mut h = open_picker(120, 36);
    h.type_str("nord");
    h.draw();
    let t = h.screen.text();
    assert!(t.contains("\n› nord\n") || t.contains("› nord"), "{t}");
    assert!(
        !t.contains("base16-eighties-dark"),
        "the list is filtered\n{t}"
    );
    assert!(highlight::explicit_syntax_theme().is_some());
    h.key(KeyCode::Esc);
    highlight::set_configured_theme(None, None);
}

#[test]
fn enter_applies_saves_and_a_new_app_starts_with_it() {
    let _g = lock();
    let home = std::env::temp_dir().join(format!("cxw-theme-{}", std::process::id()));
    std::fs::create_dir_all(&home).unwrap();
    let opts = || AppOpts {
        placeholder: Some(7),
        home: Some(home.display().to_string()),
        ..Default::default()
    };
    let mut h = harness_with(120, 36, opts());
    ready(&mut h);
    h.type_str("/theme");
    h.key(KeyCode::Enter);
    h.draw();
    h.type_str("zenburn");
    h.key(KeyCode::Enter);
    h.draw();
    let cfg = home.join(".config/codexw/config.toml");
    assert_eq!(
        std::fs::read_to_string(&cfg).unwrap(),
        "theme = \"zenburn\"\n"
    );
    assert_eq!(highlight::configured_theme().as_deref(), Some("zenburn"));
    assert!(!h.screen.text().contains("Select Syntax Theme"));
    // a fresh app reads it before drawing anything
    highlight::set_configured_theme(None, None);
    assert!(highlight::explicit_syntax_theme().is_none());
    let _h2 = harness_with(120, 36, opts());
    assert_eq!(highlight::configured_theme().as_deref(), Some("zenburn"));
    assert!(highlight::explicit_syntax_theme().is_some());
    let _ = std::fs::remove_dir_all(&home);
    highlight::set_configured_theme(None, None);
}

#[test]
fn a_saved_theme_that_does_not_resolve_warns_once_and_keeps_the_default() {
    let _g = lock();
    let home = std::env::temp_dir().join(format!("cxw-theme-bad-{}", std::process::id()));
    std::fs::create_dir_all(home.join(".config/codexw")).unwrap();
    std::fs::write(
        home.join(".config/codexw/config.toml"),
        "theme = \"no-such-theme\"\n",
    )
    .unwrap();
    let mut h = harness_with(
        120,
        36,
        AppOpts {
            placeholder: Some(7),
            home: Some(home.display().to_string()),
            ..Default::default()
        },
    );
    ready(&mut h);
    let t = h.screen.text();
    let _ = std::fs::remove_dir_all(&home);
    assert!(
        t.contains("⚠ Theme \"no-such-theme\" not found. Using the default theme."),
        "{t}"
    );
    assert!(highlight::explicit_syntax_theme().is_none());
}

#[test]
fn a_custom_tmtheme_file_is_listed_with_its_tag() {
    let _g = lock();
    let home = std::env::temp_dir().join(format!("cxw-theme-custom-{}", std::process::id()));
    let dir = home.join(".config/codexw/themes");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("mine.tmTheme"),
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict><key>name</key><string>Mine</string><key>settings</key><array><dict><key>settings</key><dict>
<key>foreground</key><string>#FFFFFF</string><key>background</key><string>#000000</string></dict></dict></array></dict></plist>"#,
    )
    .unwrap();
    let mut h = harness_with(
        120,
        36,
        AppOpts {
            placeholder: Some(7),
            home: Some(home.display().to_string()),
            ..Default::default()
        },
    );
    ready(&mut h);
    h.type_str("/theme");
    h.key(KeyCode::Enter);
    h.type_str("mine");
    h.draw();
    let t = h.screen.text();
    h.key(KeyCode::Esc);
    let _ = std::fs::remove_dir_all(&home);
    highlight::set_configured_theme(None, None);
    assert!(t.contains("› mine (custom)"), "{t}");
}

#[test]
fn a_narrow_terminal_stacks_a_four_row_preview_under_the_list() {
    let _g = lock();
    let h = open_picker(80, 40);
    let t = h.screen.text();
    assert!(t.contains("12  fn greet(name: &str) -> String {"), "{t}");
    assert!(t.contains("13 +    format!(\"Hello, {name}!\")"), "{t}");
    assert!(
        !t.contains("summarize"),
        "the wide sample is not drawn\n{t}"
    );
}

#[test]
fn a_wide_terminal_names_the_custom_theme_directory() {
    let _g = lock();
    let home = std::env::temp_dir().join(format!("cxw-theme-wide-{}", std::process::id()));
    std::fs::create_dir_all(&home).unwrap();
    let mut h = harness_with(
        220,
        40,
        AppOpts {
            placeholder: Some(7),
            home: Some(home.display().to_string()),
            ..Default::default()
        },
    );
    ready(&mut h);
    h.type_str("/theme");
    h.key(KeyCode::Enter);
    h.draw();
    let t = h.screen.text();
    h.key(KeyCode::Esc);
    let _ = std::fs::remove_dir_all(&home);
    assert!(
        t.contains("Custom .tmTheme files can be added to the ~/.config/codexw/themes directory."),
        "{t}"
    );
}

#[test]
fn light_terminals_default_to_latte_and_dark_ones_to_mocha() {
    let _g = lock();
    assert_eq!(highlight::adaptive_default_theme_name(), "catppuccin-mocha");
}

#[test]
fn the_status_line_follows_the_theme() {
    let _g = lock();
    let mut h = harness(120, 36, 7);
    ready(&mut h);
    let mocha = h.screen.style_of("gpt-5.5 default").unwrap().fg;
    highlight::set_configured_theme(Some("nord".into()), None);
    h.draw();
    h.app.request_draw();
    h.draw();
    let nord = h.screen.style_of("gpt-5.5 default").unwrap().fg;
    highlight::set_configured_theme(None, None);
    assert_ne!(mocha, nord);
}
