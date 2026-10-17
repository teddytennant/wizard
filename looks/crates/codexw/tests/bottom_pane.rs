//! The bottom pane on the headless terminal, against the Codex captures: composer, footer,
//! shortcut overlay, popups, history search, paste elements, shell mode, queued input.
//!
//! `reference/codex/<size>/<name>.txt` rows are the oracle. Screens with scripted text (the
//! assistant's answer) are compared only below it.

use std::path::PathBuf;

use agent_core::{Config, Event, Request, StopReason};
use codexw::app::AppOpts;
use codexw::testing::{Harness, harness, harness_with};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::style::{Color, Modifier};

/// The emulator reports ANSI colours by index, as crossterm writes them (`38;5;N`).
fn ansi(n: u8) -> Option<Color> {
    Some(Color::Indexed(n))
}

const PLACEHOLDERS: [&str; 8] = [
    "Explain this codebase",
    "Summarize recent commits",
    "Implement {feature}",
    "Find and fix a bug in @filename",
    "Write tests for @filename",
    "Improve documentation in @filename",
    "Run /review on my current changes",
    "Use /skills to list available skills",
];

/// A started app at `size` whose placeholder is the one the capture `name` drew.
fn start_like(cols: u16, rows: u16, size: &str, name: &str) -> Harness {
    let pick = reference(size, name)
        .iter()
        .find_map(|l| {
            let t = l.strip_prefix("› ")?;
            PLACEHOLDERS.iter().position(|p| *p == t)
        })
        .unwrap_or(7);
    start(cols, rows, pick)
}

fn ready(h: &mut Harness) {
    h.app.on_backend(Event::Ready {
        session_id: "s1".into(),
        config: Config {
            model: "gpt-5.5".into(),
            effort: "default".into(),
            efforts: ["default", "low", "medium", "high", "xhigh"]
                .map(String::from)
                .to_vec(),
            ..Default::default()
        },
    });
    h.draw();
}

fn start(cols: u16, rows: u16, placeholder: usize) -> Harness {
    let mut h = harness(cols, rows, placeholder);
    ready(&mut h);
    h
}

fn reference(size: &str, name: &str) -> Vec<String> {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../reference/codex")
        .join(size)
        .join(format!("{name}.txt"));
    let t = std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("{}: {e}", p.display()));
    t.lines().map(|l| l.trim_end().to_string()).collect()
}

/// Rows of the screen, padded to `n`.
fn rows(h: &Harness, n: usize) -> Vec<String> {
    let mut r = h.screen.rows();
    r.resize(n, String::new());
    r
}

fn assert_matches(h: &Harness, size: &str, name: &str) {
    let want = reference(size, name);
    let got = rows(h, want.len());
    let mut diffs = Vec::new();
    for (i, (w, g)) in want.iter().zip(&got).enumerate() {
        if w != g {
            diffs.push(format!("row {i}\n  want |{w}\n  got  |{g}"));
        }
    }
    assert!(diffs.is_empty(), "{name} at {size}:\n{}", diffs.join("\n"));
}

/// Same, but only rows at or after `from`.
fn assert_matches_from(h: &Harness, size: &str, name: &str, from: usize) {
    let want = reference(size, name);
    let got = rows(h, want.len());
    let mut diffs = Vec::new();
    for i in from..want.len() {
        if want[i] != got[i] {
            diffs.push(format!("row {i}\n  want |{}\n  got  |{}", want[i], got[i]));
        }
    }
    assert!(diffs.is_empty(), "{name} at {size}:\n{}", diffs.join("\n"));
}

fn press(h: &mut Harness, code: KeyCode, m: KeyModifiers) {
    h.app.on_key(KeyEvent::new(code, m));
}

fn enter(h: &mut Harness) {
    h.key(KeyCode::Enter);
}

fn running(h: &mut Harness, prompt: &str) {
    h.app.submit_prompt(prompt.into());
    h.app.on_backend(Event::TurnStart);
    h.draw();
}

/// The composer band row (the `›` line) of the current screen.
fn prompt_row(h: &Harness) -> usize {
    h.screen
        .rows()
        .iter()
        .rposition(|r| r.starts_with('›') || r == "!" || r.starts_with("! "))
        .expect("no prompt row")
}

#[test]
#[cfg_attr(
    not(reference_captures),
    ignore = "needs the reference/ captures, not in this repository; see build.rs"
)]
fn shortcut_overlay_matches_start_05_at_every_size() {
    for (cols, rows, size) in [
        (120u16, 36u16, "120x36"),
        (150, 42, "150x42"),
        (80, 24, "80x24"),
        (60, 20, "60x20"),
        (40, 20, "40x20"),
    ] {
        let mut h = start_like(cols, rows, size, "start-05-shortcuts");
        h.type_str("?");
        h.draw();
        assert_matches(&h, size, "start-05-shortcuts");
    }
}

#[test]
#[cfg_attr(
    not(reference_captures),
    ignore = "needs the reference/ captures, not in this repository; see build.rs"
)]
fn a_second_question_mark_keeps_the_overlay_and_esc_does_too() {
    let mut h = start(120, 36, 7);
    h.type_str("?");
    h.draw();
    h.type_str("?");
    h.draw();
    assert_matches(&h, "120x36", "xg-04-shortcuts-toggle");
    h.key(KeyCode::Esc);
    h.draw();
    assert_matches(&h, "120x36", "xg-04-shortcuts-esc");
    h.type_str("x");
    h.draw();
    assert_matches(&h, "120x36", "xg-04-shortcuts-anykey");
}

#[test]
#[cfg_attr(
    not(reference_captures),
    ignore = "needs the reference/ captures, not in this repository; see build.rs"
)]
fn slash_popup_matches_start_06_07_08_at_120() {
    let mut h = start(120, 36, 7);
    h.type_str("/");
    h.draw();
    assert_matches(&h, "120x36", "start-06-slash");
    h.type_str("mo");
    h.draw();
    assert_matches(&h, "120x36", "start-07-slash-filtered");

    let mut h = start(120, 36, 7);
    h.type_str("/");
    for _ in 0..12 {
        h.key(KeyCode::Down);
    }
    h.draw();
    assert_matches(&h, "120x36", "start-08-slash-scrolled");
}

#[test]
#[cfg_attr(
    not(reference_captures),
    ignore = "needs the reference/ captures, not in this repository; see build.rs"
)]
fn slash_popup_end_of_list_and_wrap_around() {
    let mut h = start(120, 36, 7);
    h.type_str("/");
    for _ in 0..30 {
        h.key(KeyCode::Down);
    }
    h.draw();
    assert_matches(&h, "120x36", "popups-00-slash-end");

    let mut h = start(120, 36, 7);
    h.type_str("/");
    h.key(KeyCode::Up);
    h.draw();
    assert_matches(&h, "120x36", "xh-10-slash-up-wrap");
}

#[test]
#[cfg_attr(
    not(reference_captures),
    ignore = "needs the reference/ captures, not in this repository; see build.rs"
)]
fn slash_popup_wraps_descriptions_at_narrow_widths() {
    for (cols, rows, size) in [
        (150u16, 42u16, "150x42"),
        (80, 24, "80x24"),
        (60, 20, "60x20"),
    ] {
        let mut h = start_like(cols, rows, size, "start-06-slash");
        h.type_str("/");
        h.draw();
        assert_matches(&h, size, "start-06-slash");
    }
}

#[test]
#[cfg_attr(
    not(reference_captures),
    ignore = "needs the reference/ captures, not in this repository; see build.rs"
)]
fn slash_popup_is_clipped_when_taller_than_the_screen() {
    let mut h = start_like(40, 20, "40x20", "start-06-slash");
    h.type_str("/");
    h.draw();
    assert_matches(&h, "40x20", "start-06-slash");
}

#[test]
#[cfg_attr(
    not(reference_captures),
    ignore = "needs the reference/ captures, not in this repository; see build.rs"
)]
fn slash_filters_match_the_captures() {
    for (typed, name) in [
        ("/st", "xh-10-slash-st"),
        ("/p", "xh-10-slash-p"),
        ("/qu", "xh-10-slash-qu"),
    ] {
        let mut h = start(120, 36, 7);
        h.type_str(typed);
        h.draw();
        assert_matches(&h, "120x36", name);
    }
}

#[test]
#[cfg_attr(
    not(reference_captures),
    ignore = "needs the reference/ captures, not in this repository; see build.rs"
)]
fn tab_completes_and_the_command_becomes_a_cyan_element() {
    let mut h = start(120, 36, 7);
    h.type_str("/re");
    h.key(KeyCode::Tab);
    h.draw();
    assert_matches(&h, "120x36", "xh-10-slash-re-tab");
    let style = h.screen.style_of("/review").unwrap();
    assert_eq!(style.fg, ansi(6));
    assert!(!style.add_modifier.contains(Modifier::BOLD));
}

#[test]
#[cfg_attr(
    not(reference_captures),
    ignore = "needs the reference/ captures, not in this repository; see build.rs"
)]
fn slash_element_esc_and_leading_space() {
    let mut h = start(120, 36, 7);
    h.type_str("/review ");
    h.draw();
    assert_matches(&h, "120x36", "xg-14-slash-element");

    let mut h = start(120, 36, 7);
    h.type_str("/mo");
    h.key(KeyCode::Esc);
    h.draw();
    assert_matches(&h, "120x36", "xg-14-slash-esc");
    h.type_str("d");
    h.draw();
    assert_matches(&h, "120x36", "xg-14-slash-token-changed");

    let mut h = start(120, 36, 7);
    h.type_str(" /model");
    h.draw();
    assert_matches(&h, "120x36", "xg-14-slash-leading-space");
}

#[test]
fn unknown_command_prints_an_info_cell_and_keeps_the_draft() {
    let mut h = start(120, 36, 7);
    h.type_str("/nonsense");
    enter(&mut h);
    h.draw();
    let r = h.screen.rows();
    assert!(
        r.iter().any(|l| l
            == "• Unrecognized command '/nonsense'. Type \"/\" for a list of supported commands.")
    );
    assert!(r.iter().any(|l| l == "› /nonsense"));
    assert!(h.sent().is_empty(), "nothing goes to the backend");
}

#[test]
#[cfg_attr(
    not(reference_captures),
    ignore = "needs the reference/ captures, not in this repository; see build.rs"
)]
fn at_popup_lists_a_file_row_with_tag_and_footer() {
    let mut h = start(120, 36, 7);
    h.app
        .pane
        .composer
        .set_file_index(codexw::ui::mention_popup::FileIndex::from_files(
            [
                "Cargo.toml",
                "README.md",
                "notes.txt",
                "src/lib.rs",
                "src/main.rs",
            ]
            .map(String::from)
            .to_vec(),
        ));
    h.type_str("@lib");
    h.draw();
    assert_matches(&h, "120x36", "start-10-at-filtered");
    // Enter inserts the path and a space and closes the popup.
    enter(&mut h);
    h.draw();
    assert!(h.screen.rows().iter().any(|l| l == "› src/lib.rs"));
    assert!(!h.screen.text().contains("switch search modes"));
}

#[test]
fn typed_mention_without_the_popup_stays_plain() {
    let mut h = start(120, 36, 7);
    h.type_str("@src/lib.rs");
    h.key(KeyCode::Esc);
    h.draw();
    let s = h.screen.style_of("@src/lib.rs").unwrap();
    assert_eq!(s.fg, None);
}

#[test]
#[cfg_attr(
    not(reference_captures),
    ignore = "needs the reference/ captures, not in this repository; see build.rs"
)]
fn composer_geometry_exactly_full_row_and_full_row_with_space() {
    let mut h = start(120, 36, 7);
    h.type_str(&"a".repeat(117));
    h.draw();
    assert_matches(&h, "120x36", "xg-07-textarea-exactly-full");

    let mut h = start(120, 36, 7);
    h.type_str(&format!("{} bb", "a".repeat(117)));
    h.draw();
    assert_matches(&h, "120x36", "xg-07-textarea-full-row");
}

#[test]
#[cfg_attr(
    not(reference_captures),
    ignore = "needs the reference/ captures, not in this repository; see build.rs"
)]
fn multiline_draft_wraps_at_every_size() {
    for (cols, rows, size) in [
        (120u16, 36u16, "120x36"),
        (150, 42, "150x42"),
        (80, 24, "80x24"),
        (60, 20, "60x20"),
        (40, 20, "40x20"),
    ] {
        let mut h = start_like(cols, rows, size, "start-04-multiline");
        h.type_str("first line");
        press(&mut h, KeyCode::Enter, KeyModifiers::SHIFT);
        h.type_str("second line");
        press(&mut h, KeyCode::Enter, KeyModifiers::SHIFT);
        h.type_str(
            "third line that is long enough to wrap around the right edge of the composer when the terminal is eighty columns wide or so",
        );
        h.draw();
        assert_matches(&h, size, "start-04-multiline");
    }
}

#[test]
fn thirty_rows_grow_the_composer_one_to_one() {
    let mut h = start(120, 36, 7);
    h.type_str("row 1");
    for i in 2..=30 {
        press(&mut h, KeyCode::Enter, KeyModifiers::SHIFT);
        h.type_str(&format!("row {i}"));
    }
    h.draw();
    let r = h.screen.rows();
    let first = r.iter().position(|l| l == "› row 1").expect("first row");
    assert_eq!(r[first + 29], "  row 30");
}

#[test]
#[cfg_attr(
    not(reference_captures),
    ignore = "needs the reference/ captures, not in this repository; see build.rs"
)]
fn editing_keys_follow_the_capture() {
    let mut h = start(120, 36, 7);
    h.type_str("hello world foo bar");
    press(&mut h, KeyCode::Char('a'), KeyModifiers::CONTROL);
    h.type_str("X");
    h.draw();
    assert_matches(&h, "120x36", "xg-09-edit-ca");
    press(&mut h, KeyCode::Char('e'), KeyModifiers::CONTROL);
    h.type_str("Y");
    h.draw();
    assert_matches(&h, "120x36", "xg-09-edit-ce");
    press(&mut h, KeyCode::Char('b'), KeyModifiers::ALT);
    h.type_str("Z");
    h.draw();
    assert_matches(&h, "120x36", "xg-09-edit-mb");
    press(&mut h, KeyCode::Char('w'), KeyModifiers::CONTROL);
    h.draw();
    assert_matches(&h, "120x36", "xg-09-edit-cw");
    press(&mut h, KeyCode::Char('e'), KeyModifiers::CONTROL);
    press(&mut h, KeyCode::Char('h'), KeyModifiers::CONTROL);
    h.draw();
    let r = h.screen.rows();
    assert_eq!(r[12], "› Xhello world foo bar");
}

#[test]
#[cfg_attr(
    not(reference_captures),
    ignore = "needs the reference/ captures, not in this repository; see build.rs"
)]
fn large_pastes_become_cyan_elements_and_expand_on_submit() {
    let mut h = start(120, 36, 7);
    h.app.on_paste(&"x".repeat(1500));
    h.draw();
    assert_matches(&h, "120x36", "misc-11-paste-large");
    let s = h.screen.style_of("[Pasted Content").unwrap();
    assert_eq!(s.fg, ansi(6));
    assert!(!s.add_modifier.contains(Modifier::BOLD));
    h.type_str(" and more");
    h.draw();
    assert_matches(&h, "120x36", "misc-12-paste-large-more");
    enter(&mut h);
    let sent = h.sent();
    assert_eq!(
        sent,
        vec![Request::Prompt(format!("{} and more", "x".repeat(1500)))]
    );
}

#[test]
#[cfg_attr(
    not(reference_captures),
    ignore = "needs the reference/ captures, not in this repository; see build.rs"
)]
fn small_paste_is_verbatim_and_two_large_ones_are_numbered() {
    let mut h = start(120, 36, 7);
    h.app.on_paste("line one\nline two\nline three");
    h.draw();
    assert_matches(&h, "120x36", "misc-13-paste-small");

    let mut h = start(120, 36, 7);
    h.app.on_paste(&"x".repeat(1500));
    h.app.on_paste(&"y".repeat(1500));
    h.draw();
    assert_matches(&h, "120x36", "xg-10-paste-twice");
    h.key(KeyCode::Backspace);
    h.draw();
    assert_matches(&h, "120x36", "xg-10-paste-backspace");
}

#[test]
#[cfg_attr(
    not(reference_captures),
    ignore = "needs the reference/ captures, not in this repository; see build.rs"
)]
fn ctrl_c_clears_the_draft_and_the_next_one_quits() {
    let mut h = start(120, 36, 7);
    h.type_str("draft text");
    h.ctrl('c');
    h.draw();
    assert_matches(&h, "120x36", "start-14-ctrl-c-clears");
    assert!(!h.app.should_exit);
    h.ctrl('c');
    assert!(h.app.should_exit);
}

#[test]
#[cfg_attr(
    not(reference_captures),
    ignore = "needs the reference/ captures, not in this repository; see build.rs"
)]
fn esc_with_a_draft_changes_nothing() {
    let mut h = start(120, 36, 7);
    h.type_str("draft text");
    h.key(KeyCode::Esc);
    h.draw();
    assert_matches(&h, "120x36", "start-12-esc-once");
    h.key(KeyCode::Esc);
    h.draw();
    assert_matches(&h, "120x36", "start-13-esc-twice");
}

/// Type `prompt`, send it and let the turn finish.
fn answered(h: &mut Harness, prompt: &str) {
    h.type_str(prompt);
    h.key(KeyCode::Enter);
    h.app.on_backend(Event::TurnStart);
    h.app.on_backend(Event::TextDelta("Hello.\n".into()));
    h.app.on_backend(Event::TurnEnd(StopReason::EndTurn));
    h.draw();
}

#[test]
fn esc_on_an_empty_composer_after_a_turn_shows_the_again_hint() {
    let mut h = start(120, 36, 1);
    answered(&mut h, "go fake:text");
    h.key(KeyCode::Esc);
    h.draw();
    let r = h.screen.rows();
    assert_eq!(r.last_footer(), "  esc again to edit previous message");
    let s = h.screen.style_of("esc again").unwrap();
    assert!(s.add_modifier.contains(Modifier::DIM));
}

trait LastFooter {
    fn last_footer(&self) -> String;
}

impl LastFooter for Vec<String> {
    /// The last non-empty row.
    fn last_footer(&self) -> String {
        self.iter()
            .rev()
            .find(|r| !r.is_empty())
            .cloned()
            .unwrap_or_default()
    }
}

#[test]
#[cfg_attr(
    not(reference_captures),
    ignore = "needs the reference/ captures, not in this repository; see build.rs"
)]
fn up_recalls_the_last_prompt_and_ctrl_r_searches() {
    let mut h = start(120, 36, 5);
    answered(&mut h, "first prompt fake:text");
    answered(&mut h, "second prompt fake:text");
    let _ = h.sent();
    h.key(KeyCode::Up);
    h.draw();
    assert_matches_from(&h, "120x36", "start-17-history-up", 22);
    let r = h.screen.rows();
    assert_eq!(r[prompt_row(&h)], "› second prompt fake:text");
    assert_eq!(
        r.last_footer(),
        "  gpt-5.5 default · ~/cxw-home/proj".replace("~/cxw-home/proj", "~/proj")
    );
    h.key(KeyCode::Down);
    h.key(KeyCode::Down);

    h.ctrl('r');
    h.draw();
    assert_matches_from(&h, "120x36", "start-15-ctrl-r-empty", 22);
    assert_eq!(h.screen.rows().last_footer(), "  reverse-i-search:");
    h.type_str("first");
    h.draw();
    assert_matches_from(&h, "120x36", "start-16-ctrl-r-query", 22);
    let r = h.screen.rows();
    assert_eq!(r[prompt_row(&h)], "› first prompt fake:text");
    assert_eq!(
        r.last_footer(),
        "  reverse-i-search: first  enter accept · esc cancel"
    );
    // The match is bold and reversed; the rest of the preview is not.
    let p = prompt_row(&h);
    let s = h.screen.cell(2, p).style;
    assert!(s.add_modifier.contains(Modifier::REVERSED | Modifier::BOLD));
    let rest = h.screen.cell(2 + "first ".len(), p).style;
    assert!(!rest.add_modifier.contains(Modifier::REVERSED));
    // The footer holds the cursor.
    let (cx, cy) = h.screen.cursor();
    assert_eq!(
        cy,
        h.screen.rows().len()
            - 1
            - h.screen
                .rows()
                .iter()
                .rev()
                .take_while(|r| r.is_empty())
                .count()
    );
    assert_eq!(cx, 2 + "reverse-i-search: first".len());

    h.key(KeyCode::Esc);
    h.draw();
    assert_eq!(
        h.screen.rows()[prompt_row(&h)],
        "› Improve documentation in @filename"
    );

    h.ctrl('r');
    h.type_str("zzz");
    h.draw();
    assert_eq!(
        h.screen.rows().last_footer(),
        "  reverse-i-search: zzz  no match"
    );
    let s = h.screen.style_of("no match").unwrap();
    assert_eq!(s.fg, ansi(1));
    h.key(KeyCode::Esc);
    h.ctrl('r');
    h.type_str("second");
    enter(&mut h);
    h.draw();
    assert_eq!(h.screen.rows()[prompt_row(&h)], "› second prompt fake:text");
    assert!(h.sent().is_empty());
}

#[test]
fn bang_shell_mode_prompt_footer_and_result() {
    let dir = std::env::temp_dir().join(format!("cxw-bang-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    for f in ["Cargo.toml", "img.png", "notes.txt", "README.md", "src"] {
        std::fs::write(dir.join(f), "x").unwrap();
    }
    let mut h = harness_with(
        120,
        36,
        AppOpts {
            placeholder: Some(5),
            cwd: dir.clone(),
            ..Default::default()
        },
    );
    ready(&mut h);
    h.type_str("!ls");
    h.draw();
    let r = h.screen.rows();
    let p = prompt_row(&h);
    assert_eq!(r[p], "! ls");
    assert!(r[p + 2].ends_with("Shell mode"));
    assert_eq!(r[p + 2].trim_end().chars().count(), 118);
    let bang = h.screen.style_of("! ls").unwrap();
    assert_eq!(bang.fg, ansi(9));
    assert!(bang.add_modifier.contains(Modifier::BOLD));
    let label = h.screen.style_of("Shell mode").unwrap();
    assert_eq!(label.fg, ansi(9));

    enter(&mut h);
    h.draw();
    let r = h.screen.rows();
    let at = r
        .iter()
        .position(|l| l == "• You ran ls")
        .expect("result cell");
    let out: Vec<&str> = r[at + 1..at + 6].iter().map(String::as_str).collect();
    assert_eq!(out[0], "  └ Cargo.toml");
    assert!(out.contains(&"    README.md") && out.contains(&"    src"));
    assert_eq!(r[at + 6], "");
    assert_eq!(r[at + 7], "─".repeat(120));
    let bullet = h.screen.style_of("• You ran").unwrap();
    assert_eq!(bullet.fg, ansi(2));
    assert!(h.sent().is_empty(), "the shell never reaches the backend");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
#[cfg_attr(
    not(reference_captures),
    ignore = "needs the reference/ captures, not in this repository; see build.rs"
)]
fn bang_false_gets_a_red_bullet_and_esc_leaves_shell_mode() {
    let mut h = harness_with(
        120,
        36,
        AppOpts {
            placeholder: Some(0),
            cwd: std::env::temp_dir(),
            ..Default::default()
        },
    );
    ready(&mut h);
    h.type_str("!false");
    enter(&mut h);
    h.draw();
    assert!(h.screen.rows().iter().any(|l| l == "  └ (no output)"));
    assert_eq!(h.screen.style_of("• You ran false").unwrap().fg, ansi(1));

    let mut h = start(120, 36, 0);
    h.type_str("!");
    h.draw();
    assert_eq!(h.screen.rows()[prompt_row(&h)], "!");
    h.key(KeyCode::Esc);
    h.draw();
    assert_matches(&h, "120x36", "xg-12-bang-esc");
}

#[test]
fn enter_during_a_turn_is_a_steer_with_its_preview_block() {
    let mut h = start(120, 36, 0);
    running(&mut h, "go fake:slow");
    h.type_str("a queued follow up fake:text");
    enter(&mut h);
    h.draw();
    assert_eq!(
        h.sent(),
        vec![
            Request::Prompt("go fake:slow".into()),
            Request::Steer("a queued follow up fake:text".into())
        ]
    );
    let r = h.screen.rows();
    let at = r
        .iter()
        .position(|l| l.starts_with("• Working"))
        .expect("status row");
    assert_eq!(r[at + 1], "");
    assert_eq!(
        r[at + 2],
        "• Messages to be submitted after next tool call (press esc to interrupt and send immediately)"
    );
    assert_eq!(r[at + 3], "  ↳ a queued follow up fake:text");
    // The band follows the block with no blank row between.
    assert_eq!(r[at + 5], "› Explain this codebase");
    let arrow = h.screen.cell(2, at + 3).style;
    assert!(arrow.add_modifier.contains(Modifier::DIM), "{arrow:?}");
    assert!(!arrow.add_modifier.contains(Modifier::ITALIC));
}

#[test]
fn steer_block_wraps_at_30_and_40_columns() {
    let mut h = start(40, 24, 0);
    running(&mut h, "go fake:slow");
    h.type_str("a queued follow up fake:text");
    enter(&mut h);
    h.draw();
    let r = h.screen.rows();
    let at = r.iter().position(|l| l.starts_with("• Messages")).unwrap();
    assert_eq!(r[at], "• Messages to be submitted after next");
    assert_eq!(r[at + 1], "  tool call (press esc to interrupt and");
    assert_eq!(r[at + 2], "  send immediately)");
    assert_eq!(r[at + 3], "  ↳ a queued follow up fake:text");

    let mut h = start(30, 24, 0);
    running(&mut h, "go fake:slow");
    h.type_str("a queued follow up fake:text");
    enter(&mut h);
    h.draw();
    let r = h.screen.rows();
    let at = r.iter().position(|l| l.starts_with("• Messages")).unwrap();
    assert_eq!(r[at], "• Messages to be submitted");
    assert_eq!(r[at + 1], "  after next tool call (press");
    assert_eq!(r[at + 2], "  esc to interrupt and send");
    assert_eq!(r[at + 3], "  immediately)");
    assert_eq!(r[at + 4], "  ↳ a queued follow up");
    assert_eq!(r[at + 5], "    fake:text");
}

#[test]
fn status_row_truncates_at_30_columns() {
    let mut h = start(30, 24, 0);
    running(&mut h, "go fake:slow");
    let r = h.screen.rows();
    assert!(
        r.iter().any(|l| l == "• Working (0s • esc to interr…"),
        "{r:?}"
    );
}

#[test]
fn tab_queues_while_running_and_alt_up_takes_it_back() {
    let mut h = start(120, 36, 2);
    running(&mut h, "go fake:slow");
    h.type_str("queued via tab");
    h.draw();
    assert_eq!(h.screen.rows().last_footer(), "  tab to queue message");
    h.key(KeyCode::Tab);
    h.draw();
    let r = h.screen.rows();
    let at = r
        .iter()
        .position(|l| l == "• Queued follow-up inputs")
        .unwrap();
    assert_eq!(r[at + 1], "  ↳ queued via tab");
    assert_eq!(r[at + 2], "    shift + ← edit last queued message");
    let item = h.screen.style_of("queued via tab").unwrap();
    assert!(item.add_modifier.contains(Modifier::DIM | Modifier::ITALIC));
    assert_eq!(r[at + 3], "");
    assert_eq!(r.last_footer(), "  gpt-5.5 default · ~/proj");
    // Nothing went to the backend yet.
    assert_eq!(h.sent().len(), 1);

    press(&mut h, KeyCode::Up, KeyModifiers::ALT);
    h.draw();
    let r = h.screen.rows();
    assert!(!r.iter().any(|l| l.contains("Queued follow-up")));
    assert_eq!(r[prompt_row(&h)], "› queued via tab");

    // Tab again, then the turn ends and the queue is sent.
    h.key(KeyCode::Tab);
    h.app.on_backend(Event::TurnEnd(StopReason::EndTurn));
    h.draw();
    assert_eq!(h.sent(), vec![Request::Prompt("queued via tab".into())]);
}

#[test]
fn esc_while_a_steer_is_pending_interrupts_and_sends_it() {
    let mut h = start(120, 36, 0);
    running(&mut h, "go fake:slow");
    h.type_str("a queued follow up fake:text");
    enter(&mut h);
    h.key(KeyCode::Esc);
    assert_eq!(h.sent().last(), Some(&Request::Cancel));
    h.app.on_backend(Event::TurnEnd(StopReason::Cancelled));
    h.draw();
    let r = h.screen.rows();
    assert!(
        r.iter()
            .any(|l| l == "• Model interrupted to submit steer instructions.")
    );
    assert!(r.iter().any(|l| l == "› a queued follow up fake:text"));
}

#[test]
fn commands_not_allowed_during_a_task_say_so() {
    let mut h = start(120, 36, 0);
    running(&mut h, "go fake:slow");
    h.type_str("/new");
    enter(&mut h);
    h.draw();
    let r = h.screen.rows();
    assert!(
        r.iter()
            .any(|l| l == "■ '/new' is disabled while a task is in progress.")
    );
    assert_eq!(h.screen.style_of("■ '/new'").unwrap().fg, ansi(1));
}

#[test]
fn overlay_while_running_swaps_two_entries() {
    let mut h = start(120, 36, 4);
    running(&mut h, "go fake:slow");
    h.type_str("?");
    h.draw();
    let t = h.screen.text();
    assert!(t.contains("tab to queue message"));
    assert!(t.contains("ctrl + c to interrupt"));
    assert!(!t.contains("tab to submit message"));
}

#[test]
fn shift_tab_switches_to_plan_mode_with_its_label() {
    let mut h = start(120, 36, 0);
    press(&mut h, KeyCode::BackTab, KeyModifiers::SHIFT);
    h.draw();
    let last = h.screen.rows().last_footer();
    assert!(last.starts_with("  gpt-5.5 default · ~/proj"));
    assert!(last.ends_with("Plan mode (shift+tab to cycle)"), "{last}");
    assert_eq!(last.chars().count(), 118);
    let s = h.screen.style_of("Plan mode (shift").unwrap();
    assert_eq!(s.fg, ansi(5));
    assert_eq!(h.sent(), vec![Request::Prompt("/plan".into())]);
    press(&mut h, KeyCode::BackTab, KeyModifiers::SHIFT);
    h.draw();
    assert!(!h.screen.text().contains("Plan mode ("));
}

#[test]
fn alt_period_steps_the_reasoning_effort() {
    let mut h = start(120, 36, 0);
    press(&mut h, KeyCode::Char('.'), KeyModifiers::ALT);
    assert_eq!(h.sent(), vec![Request::SetEffort("low".into())]);
    press(&mut h, KeyCode::Char(','), KeyModifiers::ALT);
    assert!(h.sent().is_empty(), "already at the lowest");
}

#[test]
fn vim_mode_footer_labels_and_editing() {
    let mut h = start(120, 36, 3);
    h.type_str("/vim");
    enter(&mut h);
    h.draw();
    assert!(h.screen.rows().iter().any(|l| l == "• Vim mode enabled."));
    let last = h.screen.rows().last_footer();
    assert!(last.ends_with("Vim: Normal"), "{last}");
    assert_eq!(last.chars().count(), 118);
    assert_eq!(h.screen.style_of("Vim: Normal").unwrap().fg, ansi(5));
    h.type_str("ihello");
    let bytes = h.draw();
    // Insert mode uses the steady bar cursor, as in the capture's raw stream.
    assert!(String::from_utf8_lossy(&bytes).contains("\x1b[6 q"));
    let r = h.screen.rows();
    assert_eq!(r[prompt_row(&h)], "› hello");
    assert!(r.last_footer().ends_with("Vim: Insert"));
    assert_eq!(h.screen.style_of("Vim: Insert").unwrap().fg, ansi(2));
    h.key(KeyCode::Esc);
    h.type_str("0x");
    h.draw();
    assert_eq!(h.screen.rows()[prompt_row(&h)], "› ello");
    h.type_str("dd");
    h.draw();
    assert_eq!(
        h.screen.rows()[prompt_row(&h)],
        "› Find and fix a bug in @filename"
    );
}

#[test]
fn composer_colours_match_the_capture() {
    let mut h = start(120, 36, 7);
    h.draw();
    let p = prompt_row(&h);
    let glyph = h.screen.cell(0, p).style;
    assert!(glyph.add_modifier.contains(Modifier::BOLD));
    let ph = h.screen.cell(2, p).style;
    assert!(ph.add_modifier.contains(Modifier::DIM));
    // The band is tinted on its pad rows too.
    let pad = h.screen.cell(10, p - 1).style;
    assert_eq!(pad.bg, Some(Color::Rgb(30, 30, 30)));
    let footer_row = p + 2;
    assert_eq!(
        h.screen.cell(2, footer_row).style.fg,
        Some(Color::Rgb(246, 226, 183))
    );
}

#[test]
fn cursor_sits_in_the_text_and_follows_typing() {
    let mut h = start(120, 36, 7);
    let p = prompt_row(&h);
    assert_eq!(h.screen.cursor(), (2, p));
    h.type_str("hello");
    h.draw();
    assert_eq!(h.screen.cursor(), (7, p));
}
