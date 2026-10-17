//! Wave 2 features on the headless terminal, against captures of the real Codex 0.147.0
//! (`reference/codex/120x36/xn-*`, from `tools/harvest-codex.sh xn`): the external editor, image
//! chips, and the rest as they land.

use std::path::PathBuf;

use agent_core::{Config, Event, Request};
use codexw::editor::EditorError;
use codexw::testing::{Harness, harness};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

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

/// A started app whose home is its own temp directory, for tests that save settings.
fn started_in_private_home(placeholder: usize) -> (Harness, PathBuf) {
    static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let home = std::env::temp_dir().join(format!("cxw-priv-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&home).unwrap();
    let mut h = harness_with(
        120,
        36,
        AppOpts {
            placeholder: Some(placeholder),
            home: Some(home.display().to_string()),
            cwd: home.join("proj"),
            ..Default::default()
        },
    );
    h.app.on_backend(Event::Ready {
        session_id: "s1".into(),
        config: Config {
            model: "gpt-5.5".into(),
            effort: "default".into(),
            ..Default::default()
        },
    });
    h.draw();
    (h, home)
}

fn started(placeholder: usize) -> Harness {
    let mut h = harness(120, 36, placeholder);
    h.app.on_backend(Event::Ready {
        session_id: "s1".into(),
        config: Config {
            model: "gpt-5.5".into(),
            effort: "default".into(),
            ..Default::default()
        },
    });
    h.draw();
    h
}

/// The capture's rows from the header on (it may carry a startup warning above it).
fn want(name: &str) -> Vec<String> {
    let rows = reference(name);
    let top = rows.iter().position(|r| r.starts_with('╭')).unwrap_or(0);
    rows[top..].to_vec()
}

fn assert_screen(h: &Harness, name: &str) {
    let want = want(name);
    let mut got = h.screen.rows();
    got.resize(want.len().max(got.len()), String::new());
    let mut diffs = Vec::new();
    for (i, w) in want.iter().enumerate() {
        if *w != got[i] {
            diffs.push(format!("row {i}\n  want |{w}\n  got  |{}", got[i]));
        }
    }
    assert!(diffs.is_empty(), "{name}:\n{}", diffs.join("\n"));
}

fn ctrl(h: &mut Harness, c: char) {
    h.app
        .on_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL));
}

// ---- external editor --------------------------------------------------------------------------

#[test]
#[cfg_attr(
    not(reference_captures),
    ignore = "needs the reference/ captures, not in this repository; see build.rs"
)]
fn ctrl_g_puts_the_hint_in_the_footer_like_codex() {
    let mut h = started(0);
    h.type_str("draft text");
    ctrl(&mut h, 'g');
    assert!(h.app.editor_requested);
    h.draw();
    assert_screen(&h, "xn-01-editor-hint");
}

#[test]
#[cfg_attr(
    not(reference_captures),
    ignore = "needs the reference/ captures, not in this repository; see build.rs"
)]
fn what_the_editor_writes_replaces_the_draft_with_trailing_blank_lines_trimmed() {
    let mut h = started(7);
    ctrl(&mut h, 'g');
    h.app
        .finish_editor(Ok("edited by the editor\nsecond line\n\n".into()));
    h.draw();
    assert!(!h.app.editor_requested);
    assert_screen(&h, "xn-03-editor-wrote");
    let mut h = started(7);
    h.type_str("seed text");
    ctrl(&mut h, 'g');
    h.app.finish_editor(Ok("seed text and more".into()));
    h.draw();
    assert_screen(&h, "xn-04-editor-appended");
}

#[test]
#[cfg_attr(
    not(reference_captures),
    ignore = "needs the reference/ captures, not in this repository; see build.rs"
)]
fn a_missing_editor_is_an_error_cell_and_the_draft_survives() {
    let mut h = started(7);
    ctrl(&mut h, 'g');
    h.app.finish_editor(Err(EditorError::Missing));
    h.draw();
    assert_screen(&h, "xn-05-editor-missing");
    let mut h = started(7);
    h.type_str("keep me");
    ctrl(&mut h, 'g');
    h.app.finish_editor(Err(EditorError::Missing));
    h.draw();
    assert!(h.screen.text().contains("› keep me"));
    assert!(!h.screen.text().contains("Save and close"));
}

#[test]
#[cfg_attr(
    not(reference_captures),
    ignore = "needs the reference/ captures, not in this repository; see build.rs"
)]
fn a_failing_editor_reports_its_status_in_rusts_words() {
    let mut h = started(7);
    ctrl(&mut h, 'g');
    let st = std::process::Command::new("false").status().unwrap();
    h.app
        .finish_editor(Err(EditorError::Status(st.to_string())));
    h.draw();
    assert_screen(&h, "xn-06-editor-fails");
}

#[test]
fn ctrl_g_does_nothing_while_a_popup_or_view_is_open() {
    let mut h = started(7);
    h.type_str("/");
    ctrl(&mut h, 'g');
    assert!(!h.app.editor_requested, "slash popup open");
    let mut h = started(7);
    h.type_str("/skills");
    h.key(KeyCode::Enter);
    h.draw();
    ctrl(&mut h, 'g');
    assert!(!h.app.editor_requested, "a bottom view is open");
}

#[test]
fn ctrl_g_works_while_a_turn_runs() {
    let mut h = started(7);
    h.app.submit_prompt("go".into());
    h.app.on_backend(Event::TurnStart);
    ctrl(&mut h, 'g');
    assert!(h.app.editor_requested);
}

#[cfg(unix)]
#[test]
fn launch_editor_runs_the_program_and_applies_its_output() {
    // One test owns the process environment here; nothing else in this binary reads it.
    use std::os::unix::fs::PermissionsExt;
    let dir = std::env::temp_dir().join(format!("cxw-wave2-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let sh = dir.join("ed.sh");
    std::fs::write(
        &sh,
        "#!/bin/sh\nprintf '%s + typed in the editor\\n\\n' \"$(cat \"$1\")\" > \"$1\"\n",
    )
    .unwrap();
    std::fs::set_permissions(&sh, std::fs::Permissions::from_mode(0o755)).unwrap();
    // SAFETY: single-threaded access to these two variables within this test binary.
    unsafe {
        std::env::set_var("VISUAL", &sh);
        std::env::remove_var("EDITOR");
    }
    let mut h = started(7);
    h.type_str("draft");
    ctrl(&mut h, 'g');
    h.app.launch_editor().unwrap();
    h.draw();
    let _ = std::fs::remove_dir_all(&dir);
    assert!(
        h.screen.text().contains("› draft + typed in the editor"),
        "{}",
        h.screen.text()
    );
    assert!(!h.screen.text().contains("Save and close"));
    assert!(!h.app.editor_requested);
}

// ---- image chips ------------------------------------------------------------------------------

const PNG: &[u8] = &[
    0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0, 0, 0, 0x0d, b'I', b'H', b'D', b'R', 0, 0, 0,
    1, 0, 0, 0, 1, 8, 6, 0, 0, 0, 0x1f, 0x15, 0xc4, 0x89,
];

fn png_in(dir: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("cxw-img-{dir}-{}", std::process::id()));
    std::fs::create_dir_all(&d).unwrap();
    let p = d.join("img.png");
    std::fs::write(&p, PNG).unwrap();
    p
}

#[test]
#[cfg_attr(
    not(reference_captures),
    ignore = "needs the reference/ captures, not in this repository; see build.rs"
)]
fn pasting_an_image_path_shows_a_chip_like_codex() {
    let p = png_in("chip");
    let mut h = started(7);
    h.app.on_paste(&p.display().to_string());
    h.draw();
    assert_screen(&h, "xn-10-image-pasted-path");
    h.type_str("what is this");
    h.draw();
    assert_screen(&h, "xn-11-image-and-text");
}

#[test]
fn a_chip_is_cyan_and_deleting_it_drops_the_attachment() {
    let p = png_in("cyan");
    let mut h = started(7);
    h.app.on_paste(&p.display().to_string());
    h.draw();
    let st = h.screen.style_of("[Image #1]").unwrap();
    assert_eq!(st.fg, Some(ratatui::style::Color::Indexed(6)));
    // Backspace over the space, then over the chip: one keypress removes the whole element
    h.key(KeyCode::Backspace);
    h.key(KeyCode::Backspace);
    h.type_str("x");
    h.key(KeyCode::Enter);
    h.draw();
    let sent: Vec<Request> = h.sent();
    assert_eq!(sent, vec![Request::Prompt("x".into())]);
}

#[test]
fn a_message_with_a_chip_shows_the_chip_and_sends_the_path() {
    let p = png_in("send");
    let mut h = started(7);
    h.app.on_paste(&p.display().to_string());
    h.type_str(" describe it");
    h.key(KeyCode::Enter);
    h.draw();
    let sent = h.sent();
    assert_eq!(
        sent,
        vec![Request::Prompt(format!("@{}  describe it", p.display()))]
    );
    let text = h.screen.full_text();
    assert!(text.contains("› [Image #1]  describe it"), "{text}");
    assert!(
        text.contains("Wizard takes text only over ACP"),
        "the note is said once\n{text}"
    );
    // a second image message does not repeat the note
    h.app.on_backend(Event::TurnStart);
    h.app
        .on_backend(Event::TurnEnd(agent_core::StopReason::EndTurn));
    h.app.on_paste(&p.display().to_string());
    h.type_str("again");
    h.key(KeyCode::Enter);
    h.draw();
    assert_eq!(
        h.screen
            .full_text()
            .matches("Wizard takes text only")
            .count(),
        1
    );
}

#[test]
fn two_images_are_numbered_in_order_and_renumbered_when_one_goes() {
    let p = png_in("two");
    let mut h = started(7);
    h.app.on_paste(&p.display().to_string());
    h.app.on_paste(&p.display().to_string());
    h.draw();
    assert!(
        h.screen.text().contains("› [Image #1] [Image #2]"),
        "{}",
        h.screen.text()
    );
    // delete the first chip: the second becomes #1
    h.key(KeyCode::Home);
    h.key(KeyCode::Delete);
    h.draw();
    assert!(
        h.screen.text().contains("[Image #1]"),
        "{}",
        h.screen.text()
    );
    assert!(!h.screen.text().contains("[Image #2]"));
}

#[test]
fn text_that_is_not_an_image_path_pastes_as_text() {
    let mut h = started(7);
    h.app.on_paste("/no/such/file.png");
    h.draw();
    assert!(h.screen.text().contains("› /no/such/file.png"));
}

#[test]
fn ctrl_v_without_a_clipboard_image_is_a_red_error_cell() {
    let mut h = started(7);
    // The headless box has no display; with one, a clipboard without an image says so as well.
    h.app
        .on_key(KeyEvent::new(KeyCode::Char('v'), KeyModifiers::CONTROL));
    h.draw();
    let t = h.screen.text();
    assert!(t.contains("■ Failed to paste image: "), "{t}");
}

// ---- raw output mode --------------------------------------------------------------------------

use codexw::fake::scenario_events;

/// A turn of `go fake:<scenario>` through the whole app, on a screen of `rows` rows.
fn turn(placeholder: usize, rows: u16, scenario: &str) -> Harness {
    let mut h = harness(120, rows, placeholder);
    h.app.log.notes = false;
    h.app.on_backend(Event::Ready {
        session_id: "s1".into(),
        config: Config {
            model: "gpt-5.5".into(),
            effort: "default".into(),
            backend: "wizard 3.8.12".into(),
            ..Default::default()
        },
    });
    h.draw();
    h.type_str(&format!("go fake:{scenario}"));
    h.key(KeyCode::Enter);
    h.draw();
    for ev in scenario_events(scenario, "/tmp/cxw-home/proj").expect("scenario") {
        h.app.on_backend(ev);
        h.draw();
    }
    h.draw();
    h
}

fn slash(h: &mut Harness, cmd: &str) {
    h.type_str(cmd);
    h.key(KeyCode::Enter);
    h.draw();
}

fn full_ref(name: &str) -> Vec<String> {
    reference(name)
}

#[test]
#[cfg_attr(
    not(reference_captures),
    ignore = "needs the reference/ captures, not in this repository; see build.rs"
)]
fn raw_mode_rewrites_the_transcript_as_source_text_like_codex() {
    let mut h = turn(7, 60, "markdown");
    slash(&mut h, "/raw");
    let want = full_ref("xn-30-raw-on.full");
    let got: Vec<String> = h
        .screen
        .full_text()
        .lines()
        .map(|l| l.trim_end().to_string())
        .collect();
    // The capture's trailing rows are the composer and the footer; compare the transcript.
    let upto = want
        .iter()
        .position(|l| l.starts_with("• Raw output mode on"))
        .unwrap()
        + 1;
    let want: Vec<&String> = want[..upto].iter().skip_while(|l| l.is_empty()).collect();
    let got: Vec<&String> = got
        .iter()
        .skip_while(|l| l.is_empty())
        .take(want.len())
        .collect();
    let mut diffs = Vec::new();
    for (i, (w, g)) in want.iter().zip(&got).enumerate() {
        if w != g {
            diffs.push(format!("row {i}\n  want |{w}\n  got  |{g}"));
        }
    }
    assert_eq!(
        want.len(),
        got.len(),
        "row count\n{}",
        got.iter()
            .map(|s| s.as_str())
            .collect::<Vec<_>>()
            .join("\n")
    );
    assert!(diffs.is_empty(), "{}", diffs.join("\n"));
}

#[test]
fn raw_off_brings_the_rich_cells_back() {
    let mut h = turn(7, 60, "markdown");
    slash(&mut h, "/raw");
    slash(&mut h, "/raw");
    let t = h.screen.full_text();
    assert!(
        t.contains("• Raw output mode on: transcript text is shown for clean terminal selection.")
    );
    assert!(t.contains("• Raw output mode off: rich transcript rendering restored."));
    assert!(
        t.contains("│ >_ Wizard (v3.8.12)"),
        "the header box is back"
    );
    assert!(t.contains("› go fake:markdown"));
    assert!(t.contains("• ## Summary") || t.contains("  I read src/lib.rs"));
}

#[test]
fn raw_arguments_and_the_usage_error() {
    let mut h = started(7);
    slash(&mut h, "/raw on");
    assert!(h.app.raw_output);
    slash(&mut h, "/raw on");
    assert!(h.app.raw_output, "on stays on");
    slash(&mut h, "/raw maybe");
    assert!(h.screen.text().contains("■ Usage: /raw [on|off]"));
    assert!(h.app.raw_output);
    slash(&mut h, "/raw OFF");
    assert!(!h.app.raw_output);
}

#[test]
fn raw_lines_are_not_wrapped_by_the_app() {
    let mut h = turn(7, 40, "markdown");
    slash(&mut h, "/raw on");
    // The 121-column tip reaches the terminal whole; the emulator wraps at its edge.
    let t = h.screen.full_text();
    assert!(t.contains("offers\n."), "{t}");
}

#[test]
fn alt_r_toggles_without_a_notice() {
    let mut h = started(7);
    h.app
        .on_key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::ALT));
    assert!(h.app.raw_output);
    h.draw();
    assert!(!h.screen.text().contains("Raw output mode"));
    h.app
        .on_key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::ALT));
    assert!(!h.app.raw_output);
}

// ---- backtrack and /rewind ---------------------------------------------------------------------

use agent_core::{HistoryItem, StopReason};
use codexw::app::AppOpts;
use codexw::testing::harness_with;

/// An app whose home holds a wizard session file with the given turn markers.
fn with_session(markers: &[(u64, &str)]) -> (Harness, PathBuf) {
    static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let home = std::env::temp_dir().join(format!("cxw-rewind-{}-{n}", std::process::id()));
    let dir = home.join(".wizard/sessions");
    std::fs::create_dir_all(&dir).unwrap();
    let mut file = String::from("{\"cwd\":\"/tmp/cxw-home/proj\",\"version\":1}\n");
    for (turn, prompt) in markers {
        file.push_str(&format!("{{\"turn\":{turn},\"prompt\":\"{prompt}\"}}\n"));
    }
    std::fs::write(dir.join("s1.jsonl"), file).unwrap();
    let mut h = harness_with(
        120,
        36,
        AppOpts {
            placeholder: Some(7),
            home: Some(home.display().to_string()),
            ..Default::default()
        },
    );
    h.app.log.notes = false;
    h.app.on_backend(Event::Ready {
        session_id: "s1".into(),
        config: Config {
            model: "gpt-5.5".into(),
            effort: "default".into(),
            ..Default::default()
        },
    });
    h.draw();
    (h, home)
}

fn say(h: &mut Harness, prompt: &str) {
    h.app.submit_prompt(prompt.into());
    h.app.on_backend(Event::TurnStart);
    h.app.on_backend(Event::TextDelta("Hello!".into()));
    h.app.on_backend(Event::TurnEnd(StopReason::EndTurn));
    h.draw();
    h.sent();
}

#[test]
fn enter_on_a_prompt_asks_then_rewinds_through_wizard_and_reloads_the_session() {
    let (mut h, home) = with_session(&[
        (1, "first question"),
        (2, "second question"),
        (3, "third question"),
    ]);
    for p in ["first question", "second question", "third question"] {
        say(&mut h, p);
    }
    h.key(KeyCode::Esc);
    h.key(KeyCode::Esc);
    h.draw();
    h.key(KeyCode::Enter);
    h.draw();
    let t = h.screen.text();
    assert!(t.contains("Rewind to before this message?"), "{t}");
    assert!(
        t.contains("› 2. No, keep the conversation"),
        "the safe row is highlighted\n{t}"
    );
    assert!(h.sent().is_empty(), "nothing is sent before the answer");
    h.key(KeyCode::Up);
    h.key(KeyCode::Enter);
    h.draw();
    assert_eq!(h.sent(), vec![Request::Prompt("/rewind 3".into())]);
    // the command turn: its answer is read, not printed
    h.app.on_backend(Event::TurnStart);
    h.app.on_backend(Event::Notice {
        level: agent_core::NoticeLevel::Info,
        text: "rewound to before turn 3: no files needed restoring; conversation truncated".into(),
    });
    h.app.on_backend(Event::TurnEnd(StopReason::EndTurn));
    h.draw();
    assert!(!h.screen.full_text().contains("rewound to before turn"));
    assert_eq!(h.sent(), vec![Request::LoadSession("s1".into())]);
    // wizard replays the cut session
    h.app.on_backend(Event::History {
        session_id: "s1".into(),
        items: vec![
            HistoryItem::User("first question".into()),
            HistoryItem::Assistant("Hello!".into()),
            HistoryItem::User("second question".into()),
            HistoryItem::Assistant("Hello!".into()),
        ],
    });
    h.app.on_backend(Event::Ready {
        session_id: "s1".into(),
        config: Config {
            model: "gpt-5.5".into(),
            effort: "default".into(),
            ..Default::default()
        },
    });
    h.draw();
    let t = h.screen.full_text();
    let _ = std::fs::remove_dir_all(&home);
    assert!(t.contains("› second question"), "{t}");
    assert_eq!(
        t.matches("› third question").count(),
        1,
        "only the composer holds the cut prompt\n{t}"
    );
    assert!(
        t.contains("• You\u{2019}re continuing from this point."),
        "{t}"
    );
    assert!(
        !t.contains("Token usage"),
        "no summary of a session that has not ended\n{t}"
    );
    assert!(
        h_text_has_composer_prompt(&t),
        "the prompt is in the composer\n{t}"
    );
}

fn h_text_has_composer_prompt(t: &str) -> bool {
    let rows: Vec<&str> = t.lines().collect();
    let i = rows.iter().rposition(|r| *r == "› third question");
    i.is_some_and(|i| rows.get(i + 2).is_some_and(|r| r.contains("gpt-5.5")))
}

#[test]
fn saying_no_changes_nothing() {
    let (mut h, home) = with_session(&[(1, "only question")]);
    say(&mut h, "only question");
    h.key(KeyCode::Esc);
    h.key(KeyCode::Esc);
    h.draw();
    h.key(KeyCode::Enter);
    h.draw();
    h.key(KeyCode::Enter);
    h.draw();
    let _ = std::fs::remove_dir_all(&home);
    assert!(h.sent().is_empty());
    assert!(!h.screen.text().contains("Rewind to before"));
    assert!(h.app.rewind.is_none());
}

#[test]
fn a_prompt_with_no_turn_marker_comes_back_with_an_honest_error() {
    let (mut h, home) = with_session(&[]);
    say(&mut h, "no marker for me");
    h.key(KeyCode::Esc);
    h.key(KeyCode::Esc);
    h.draw();
    h.key(KeyCode::Enter);
    h.draw();
    let _ = std::fs::remove_dir_all(&home);
    let t = h.screen.text();
    assert!(
        t.contains("■ Failed to branch before the selected prompt: wizard has no turn marker"),
        "{t}"
    );
    assert!(t.contains("› no marker for me"), "{t}");
    assert!(h.sent().is_empty());
}

#[test]
fn a_refused_rewind_puts_the_prompt_back_with_wizards_reason() {
    let (mut h, home) = with_session(&[(1, "q")]);
    say(&mut h, "q");
    h.app.start_rewind(1, "q".into());
    h.sent();
    h.app.on_backend(Event::TurnStart);
    h.app.on_backend(Event::Notice {
        level: agent_core::NoticeLevel::Info,
        text: "nothing to rewind yet".into(),
    });
    h.app.on_backend(Event::TurnEnd(StopReason::EndTurn));
    h.draw();
    let _ = std::fs::remove_dir_all(&home);
    let t = h.screen.text();
    assert!(
        t.contains("■ Failed to branch before the selected prompt: nothing to rewind yet"),
        "{t}"
    );
    assert!(t.contains("› q"), "{t}");
    assert!(h.sent().is_empty(), "no reload after a refusal");
}

#[test]
fn slash_rewind_opens_the_picker_and_a_bad_argument_says_usage() {
    let (mut h, home) = with_session(&[(1, "q")]);
    say(&mut h, "q");
    slash(&mut h, "/rewind");
    assert!(h.screen.in_alt_screen(), "the backtrack overlay is open");
    h.key(KeyCode::Char('q'));
    h.draw();
    slash(&mut h, "/rewind abc");
    let _ = std::fs::remove_dir_all(&home);
    assert!(h.screen.text().contains("■ Usage: /rewind [turn]"));
}

#[test]
fn rewind_is_not_in_the_popup_but_resolves_when_typed() {
    let mut h = started(7);
    h.type_str("/rew");
    h.draw();
    assert!(
        !h.screen.text().contains("go back to before"),
        "hidden from the popup"
    );
    assert!(codexw::commands::lookup("rewind").is_some());
}

// ---- hook cells -------------------------------------------------------------------------------

#[test]
fn a_hook_notice_prints_a_codex_hook_cell_and_quiet_ones_print_nothing() {
    let mut h = started(7);
    h.app.on_backend(Event::Notice {
        level: agent_core::NoticeLevel::Info,
        text: "hook pre_tool_use: blocked \u{2014} run tests first (/opt/policy.sh)".into(),
    });
    h.app.on_backend(Event::Notice {
        level: agent_core::NoticeLevel::Info,
        text: "hook post_tool_use: appended context (fmt.sh)".into(),
    });
    h.draw();
    let t = h.screen.text();
    assert!(
        t.contains("• PreToolUse hook (blocked)\n  feedback: run tests first"),
        "{t}"
    );
    assert!(!t.contains("appended context"), "{t}");
    // an ordinary notice is untouched
    h.app.on_backend(Event::Notice {
        level: agent_core::NoticeLevel::Info,
        text: "hooks are fun".into(),
    });
    h.draw();
    assert!(h.screen.text().contains("• hooks are fun"));
}

// ---- approval prompt with the diff above it ----------------------------------------------------

#[test]
fn an_edit_approval_draws_the_edited_cell_above_the_prompt_and_only_once() {
    use agent_core::{FileDiff, PermissionRequest, ToolCall, ToolKind, ToolStatus};
    let diff = FileDiff {
        path: "/tmp/cxw-home/proj/src/lib.rs".into(),
        old: Some("pub fn add(a: i32, b: i32) -> i32 {\n    a + b\n}".into()),
        new: "pub fn add(a: i32, b: i32) -> i32 {\n    a.wrapping_add(b)\n}".into(),
    };
    let mut h = started(7);
    h.app.on_backend(Event::TurnStart);
    h.app.on_backend(Event::Permission(PermissionRequest {
        id: "p1".into(),
        tool: "edit_file".into(),
        kind: ToolKind::Edit,
        title: "src/lib.rs".into(),
        input: serde_json::json!({}),
        diff: Some(diff.clone()),
        rule: "src/**".into(),
    }));
    h.draw();
    let t = h.screen.text();
    let edited = t
        .find("• Edited src/lib.rs (+1 -1)")
        .unwrap_or_else(|| panic!("{t}"));
    let ask = t
        .find("Would you like to make the following edits?")
        .expect("prompt");
    assert!(edited < ask, "the diff is above the prompt\n{t}");
    assert!(t.contains("2 +    a.wrapping_add(b)"), "{t}");
    // Yes, then wizard finishes the call: no second cell
    h.key(KeyCode::Enter);
    h.app.on_backend(Event::Tool(ToolCall {
        id: "c1".into(),
        name: "edit_file".into(),
        kind: ToolKind::Edit,
        title: "src/lib.rs".into(),
        input: serde_json::json!({"path": "src/lib.rs"}),
        status: ToolStatus::Completed,
        output: Some("ok".into()),
        diff: Some(diff),
        parent_id: None,
    }));
    h.draw();
    assert_eq!(
        h.screen.full_text().matches("• Edited src/lib.rs").count(),
        1,
        "{}",
        h.screen.full_text()
    );
}

// ---- slash commands mapped to wizard or answered locally ---------------------------------------

#[test]
fn goal_side_memories_agents_and_compact_run_as_wizard_commands_without_an_echo() {
    let mut h = started(7);
    for (typed, sent) in [
        ("/goal ship the fix", "/goal ship the fix"),
        ("/goal", "/goal"),
        ("/side what is a monad", "/btw what is a monad"),
        ("/btw a quick one", "/btw a quick one"),
        ("/memories read notes", "/memory read notes"),
        ("/memories", "/memory"),
        ("/agent", "/agents"),
        ("/subagents", "/agents"),
        ("/compact", "/compact"),
        ("/usage", "/usage"),
    ] {
        slash(&mut h, typed);
        assert_eq!(h.sent(), vec![Request::Prompt(sent.into())], "{typed}");
    }
    assert!(
        !h.screen.full_text().contains("› /goal"),
        "no user cell for a command"
    );
}

#[test]
fn what_wizard_cannot_do_is_said_plainly_and_nothing_is_sent() {
    let mut h = started(7);
    slash(&mut h, "/goal clear");
    assert!(
        h.screen
            .text()
            .contains("■ '/goal clear' is not supported by wizard."),
        "{}",
        h.screen.text()
    );
    slash(&mut h, "/side");
    assert!(
        h.screen
            .text()
            .contains("■ '/side' needs a question in wizard")
    );
    slash(&mut h, "/fork");
    assert!(
        h.screen
            .text()
            .contains("■ '/fork' is not supported by wizard.")
    );
    assert!(h.sent().is_empty());
}

#[test]
fn ps_echoes_the_command_and_asks_wizard_for_its_background_tasks() {
    let mut h = started(7);
    slash(&mut h, "/ps");
    assert_eq!(h.sent(), vec![Request::Prompt("/bashes".into())]);
    let t = h.screen.text();
    assert!(t.contains("/ps\n\nBackground terminals"), "{t}");
    let st = h.screen.style_of("/ps").unwrap();
    assert_eq!(st.fg, Some(ratatui::style::Color::Indexed(5)));
}

#[test]
fn mcp_lists_the_servers_from_wizards_config() {
    let home = std::env::temp_dir().join(format!("cxw-mcp-{}", std::process::id()));
    std::fs::create_dir_all(home.join(".wizard")).unwrap();
    std::fs::write(
        home.join(".wizard/mcp.toml"),
        "[[server]]\nname = \"playwright\"\ntransport = \"stdio\"\ncommand = \"npx\"\n",
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
    h.draw();
    slash(&mut h, "/mcp");
    let t = h.screen.text();
    let _ = std::fs::remove_dir_all(&home);
    assert!(
        t.contains("/mcp\n\n🔌  MCP Tools\n\n  • playwright\n    • Transport: stdio\n"),
        "{t}"
    );
    slash(&mut h, "/mcp nope");
    assert!(h.screen.text().contains("■ Usage: /mcp [verbose]"));
}

#[test]
#[cfg_attr(
    not(reference_captures),
    ignore = "needs the reference/ captures, not in this repository; see build.rs"
)]
fn hooks_opens_the_events_screen_like_the_capture_and_escape_closes_it() {
    let mut h = started(7);
    slash(&mut h, "/hooks");
    let rows = h.screen.rows();
    let top = rows
        .iter()
        .position(|r| r.trim() == "Hooks")
        .expect("title");
    let want = reference("popups-07-hooks");
    let wtop = want.iter().position(|r| r.trim() == "Hooks").unwrap();
    for i in 0..16 {
        assert_eq!(rows[top + i], want[wtop + i], "row {i}");
    }
    h.key(KeyCode::Esc);
    h.draw();
    assert!(!h.screen.text().contains("Lifecycle hooks"));
}

#[test]
fn debug_config_prints_the_layers() {
    let mut h = started(7);
    slash(&mut h, "/debug-config");
    let t = h.screen.text();
    assert!(
        t.contains("/debug-config\n\nConfig layer stack (lowest precedence first):"),
        "{t}"
    );
    assert!(t.contains("Requirements:\n  <none>"), "{t}");
}

#[test]
fn copy_without_an_answer_is_codexs_error() {
    let mut h = started(7);
    slash(&mut h, "/copy");
    assert!(h.screen.text().contains("■ No agent response to copy"));
    h.ctrl('o');
    h.draw();
    assert_eq!(
        h.screen
            .full_text()
            .matches("No agent response to copy")
            .count(),
        2
    );
}

// ---- /statusline and /title ---------------------------------------------------------------------

/// The rows of the screen from the row reading `title` down, against the capture's.
fn assert_from(h: &Harness, name: &str, title: &str) {
    let want_all = reference(name);
    let wtop = want_all
        .iter()
        .position(|r| r.trim() == title)
        .unwrap_or_else(|| panic!("{name} has no {title} row"));
    let want = &want_all[wtop..];
    let got_all = h.screen.rows();
    let gtop = got_all
        .iter()
        .position(|r| r.trim() == title)
        .unwrap_or_else(|| panic!("no {title} row\n{}", h.screen.text()));
    let got = &got_all[gtop..];
    let mut diffs = Vec::new();
    for (i, w) in want.iter().enumerate() {
        let g = got.get(i).map(String::as_str).unwrap_or("");
        if w != g {
            diffs.push(format!("row {i}\n  want |{w}\n  got  |{g}"));
        }
    }
    assert!(diffs.is_empty(), "{name}:\n{}", diffs.join("\n"));
}

fn keys(h: &mut Harness, codes: &[KeyCode]) {
    for c in codes {
        h.key(*c);
    }
    h.draw();
}

#[test]
#[cfg_attr(
    not(reference_captures),
    ignore = "needs the reference/ captures, not in this repository; see build.rs"
)]
fn statusline_picker_follows_the_capture_step_by_step() {
    let mut h = started(7);
    slash(&mut h, "/statusline");
    assert_from(&h, "xh-08-statusline-0", "Configure Status Line");
    keys(&mut h, &[KeyCode::Char(' ')]);
    assert_from(&h, "xh-08-statusline-theme-toggle", "Configure Status Line");
    keys(&mut h, &[KeyCode::Down, KeyCode::Down, KeyCode::Char(' ')]);
    assert_from(&h, "xh-08-statusline-toggle2", "Configure Status Line");
    keys(&mut h, &[KeyCode::Right, KeyCode::Down, KeyCode::Char(' ')]);
    assert_from(&h, "xh-08-statusline-3", "Configure Status Line");
    h.type_str("git");
    h.draw();
    assert_from(&h, "xh-08-statusline-filter", "Configure Status Line");
}

#[test]
fn confirming_the_statusline_changes_the_footer_and_is_saved() {
    let home = std::env::temp_dir().join(format!("cxw-sl-flow-{}", std::process::id()));
    std::fs::create_dir_all(&home).unwrap();
    let mut h = harness_with(
        120,
        36,
        AppOpts {
            placeholder: Some(7),
            home: Some(home.display().to_string()),
            ..Default::default()
        },
    );
    h.app.on_backend(Event::Ready {
        session_id: "s1".into(),
        config: Config {
            model: "gpt-5.5".into(),
            effort: "default".into(),
            ..Default::default()
        },
    });
    h.draw();
    slash(&mut h, "/statusline");
    keys(
        &mut h,
        &[
            KeyCode::Down,
            KeyCode::Down,
            KeyCode::Char(' '),
            KeyCode::Right,
            KeyCode::Down,
            KeyCode::Char(' '),
        ],
    );
    keys(&mut h, &[KeyCode::Enter]);
    let t = h.screen.text();
    assert!(t.contains("  gpt-5.5 default · default"), "{t}");
    assert!(!t.contains("Configure Status Line"));
    let saved = std::fs::read_to_string(home.join(".config/codexw/config.toml")).unwrap();
    assert_eq!(
        saved,
        "status_line = \"model-with-reasoning,reasoning\"\nstatus_line_use_colors = \"true\"\n"
    );
    // a new app starts with it
    let mut h2 = harness_with(
        120,
        36,
        AppOpts {
            placeholder: Some(7),
            home: Some(home.display().to_string()),
            ..Default::default()
        },
    );
    h2.app.on_backend(Event::Ready {
        session_id: "s1".into(),
        config: Config {
            model: "gpt-5.5".into(),
            effort: "default".into(),
            ..Default::default()
        },
    });
    h2.draw();
    let t2 = h2.screen.text();
    let _ = std::fs::remove_dir_all(&home);
    assert!(t2.contains("  gpt-5.5 default · default"), "{t2}");
}

#[test]
fn an_empty_status_line_brings_the_hint_row_back() {
    let (mut h, home) = started_in_private_home(7);
    slash(&mut h, "/statusline");
    // uncheck both defaults
    keys(
        &mut h,
        &[
            KeyCode::Down,
            KeyCode::Char(' '),
            KeyCode::Down,
            KeyCode::Char(' '),
            KeyCode::Enter,
        ],
    );
    let t = h.screen.text();
    let _ = std::fs::remove_dir_all(&home);
    assert!(t.contains("  ? for shortcuts"), "{t}");
}

#[test]
#[cfg_attr(
    not(reference_captures),
    ignore = "needs the reference/ captures, not in this repository; see build.rs"
)]
fn title_picker_matches_the_capture_and_the_toggle() {
    let mut h = started(7);
    slash(&mut h, "/title");
    assert_from(&h, "xh-09-title-0", "Configure Terminal Title");
    keys(&mut h, &[KeyCode::Char(' ')]);
    assert_from(&h, "xh-09-title-toggle", "Configure Terminal Title");
}

#[test]
fn confirming_the_title_changes_what_the_terminal_shows() {
    let (mut h, home) = started_in_private_home(7);
    slash(&mut h, "/title");
    // uncheck activity; app-name is the third row
    keys(
        &mut h,
        &[
            KeyCode::Char(' '),
            KeyCode::Down,
            KeyCode::Down,
            KeyCode::Char(' '),
            KeyCode::Enter,
        ],
    );
    h.draw();
    let _ = std::fs::remove_dir_all(&home);
    assert_eq!(h.screen.title, "proj | codexw");
}

// ---- /keymap and the key table ------------------------------------------------------------------

#[test]
#[cfg_attr(
    not(reference_captures),
    ignore = "needs the reference/ captures, not in this repository; see build.rs"
)]
fn keymap_lists_the_shortcuts_like_the_capture_apart_from_fast_mode() {
    let mut h = started(7);
    slash(&mut h, "/keymap");
    let rows = h.screen.rows();
    let top = rows
        .iter()
        .position(|r| r.trim() == "Keymap")
        .expect("title");
    let want_all = reference("popups-03-keymap");
    let wtop = want_all.iter().position(|r| r.trim() == "Keymap").unwrap();
    // wizard has no Fast mode: that row is gone from the Global group, and the counts follow
    for (i, w) in want_all[wtop..wtop + 22].iter().enumerate() {
        let g = &rows[top + i];
        if w.contains("Toggle Fast Mode") || w.contains("110 actions") || w.contains("Unbound (3)")
        {
            continue;
        }
        if (11..=17).contains(&i) {
            continue; // the rows shift by one without the Fast mode row
        }
        assert_eq!(g, w, "row {i}");
    }
    assert!(
        rows[top + 2].contains("0 customized, 2 unbound."),
        "{}",
        rows[top + 2]
    );
    assert!(
        h.screen
            .text()
            .contains("Global       - Toggle Vim Mode            unbound")
    );
    assert!(!h.screen.text().contains("Fast"));
}

#[test]
fn keymap_tabs_search_and_the_remap_refusal() {
    let mut h = started(7);
    slash(&mut h, "/keymap");
    keys(&mut h, &[KeyCode::Right]);
    let t = h.screen.text();
    assert!(
        t.contains("[Common]") && t.contains("Frequently customized shortcuts."),
        "{t}"
    );
    assert!(t.contains("Chat           Interrupt Turn"), "{t}");
    keys(&mut h, &[KeyCode::Right]);
    assert!(h.screen.text().contains("› No customized shortcuts"));
    keys(&mut h, &[KeyCode::Left, KeyCode::Left]);
    h.type_str("transcript");
    h.draw();
    let t = h.screen.text();
    assert!(t.contains("Open Transcript"), "{t}");
    assert!(!t.contains("Copy "), "{t}");
    keys(&mut h, &[KeyCode::Enter]);
    assert!(
        h.screen
            .text()
            .contains("■ Shortcuts cannot be remapped in codexw"),
        "{}",
        h.screen.text()
    );
}

#[test]
#[cfg_attr(
    not(reference_captures),
    ignore = "needs the reference/ captures, not in this repository; see build.rs"
)]
fn keypress_inspector_names_a_key_and_its_actions() {
    let mut h = started(7);
    slash(&mut h, "/keymap debug");
    assert!(h.screen.text().contains("Waiting for a keypress..."));
    ctrl(&mut h, 'o');
    h.draw();
    let rows = h.screen.rows();
    let top = rows
        .iter()
        .position(|r| r.trim() == "Keypress Inspector")
        .unwrap();
    let want_all = reference("xh-12-keymap-debug-key");
    let wtop = want_all
        .iter()
        .position(|r| r.trim() == "Keypress Inspector")
        .unwrap();
    for i in 0..10 {
        assert_eq!(rows[top + i], want_all[wtop + i], "row {i}");
    }
    h.app
        .on_key(KeyEvent::new(KeyCode::Down, KeyModifiers::SHIFT));
    h.draw();
    assert!(h.screen.text().contains("Detected: shift + down"));
    assert!(h.screen.text().contains("chat.decrease_reasoning_effort"));
    ctrl(&mut h, 'c');
    h.draw();
    assert!(!h.screen.text().contains("Keypress Inspector"));
}

#[test]
fn shift_up_and_down_step_the_reasoning_effort_like_alt_period_and_comma() {
    let mut h = harness(120, 36, 7);
    h.app.on_backend(Event::Ready {
        session_id: "s1".into(),
        config: Config {
            model: "gpt-5.5".into(),
            effort: "medium".into(),
            efforts: ["default", "low", "medium", "high", "xhigh"]
                .map(String::from)
                .to_vec(),
            ..Default::default()
        },
    });
    h.draw();
    h.app
        .on_key(KeyEvent::new(KeyCode::Up, KeyModifiers::SHIFT));
    assert_eq!(h.sent(), vec![Request::SetEffort("high".into())]);
    h.app
        .on_key(KeyEvent::new(KeyCode::Down, KeyModifiers::SHIFT));
    assert_eq!(h.sent(), vec![Request::SetEffort("low".into())]);
}

#[test]
fn ctrl_slash_asks_for_a_question_the_way_side_does() {
    let mut h = started(7);
    ctrl(&mut h, '/');
    h.draw();
    assert!(
        h.screen
            .text()
            .contains("■ '/side' needs a question in wizard")
    );
}
