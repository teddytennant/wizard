//! The owned alternate screen, Codex 0.174's default: the chat enters it on its first frame, draws
//! a scrollable transcript above the composer, keeps it across overlays, hands it back for the
//! editor and on exit, and never writes the conversation into the terminal's scrollback. The
//! inline renderer stays the `--no-alt-screen` path; the other test files cover it.

use agent_core::{Config, Event, StopReason};
use codexw::testing::{Harness, harness, harness_owned};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEventKind};

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

fn turn(h: &mut Harness, prompt: &str, answer: &str) {
    h.app.submit_prompt(prompt.into());
    h.app.on_backend(Event::TurnStart);
    h.app.on_backend(Event::TextDelta(format!("{answer}\n")));
    h.app.on_backend(Event::TurnEnd(StopReason::EndTurn));
    h.draw();
}

/// Enough turns to overflow a 36-row screen several times.
fn long_chat(h: &mut Harness) {
    for i in 0..15 {
        turn(h, &format!("question {i}"), &format!("answer {i}"));
    }
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).to_string()
}

#[test]
fn the_owned_chat_enters_the_alternate_screen_on_its_first_frame() {
    let mut h = harness_owned(120, 36, 6);
    assert!(
        !h.screen.in_alt_screen(),
        "nothing is written before a frame"
    );
    ready(&mut h);
    assert!(h.screen.in_alt_screen());
    assert_eq!(h.screen.alt_screen_entered, 1);
    assert!(
        h.screen.mouse_reporting,
        "the owned transcript takes the pointer"
    );
    let rows = h.screen.rows();
    assert!(
        rows.iter().take(6).any(|r| r.contains("Wizard")),
        "the header starts at the top:\n{}",
        h.screen.text()
    );
    // The composer sits on the bottom rows, not under the header.
    let prompt = rows.iter().rposition(|r| r.starts_with('›')).unwrap();
    assert!(
        prompt > 30,
        "composer at row {prompt}:\n{}",
        h.screen.text()
    );
    turn(&mut h, "hello there", "Hello back.");
    assert!(h.screen.text().contains("› hello there"));
    assert!(h.screen.text().contains("Hello back."));
    assert!(h.screen.scrollback.is_empty(), "nothing goes to scrollback");
    assert_eq!(h.screen.alt_screen_entered, 1);
}

#[test]
fn the_inline_chat_never_enters_it() {
    let mut h = harness(120, 36, 6);
    ready(&mut h);
    turn(&mut h, "hello there", "Hello back.");
    assert!(!h.screen.in_alt_screen());
    assert_eq!(h.screen.alt_screen_entered, 0);
    assert!(!h.screen.mouse_reporting);
}

#[test]
fn the_wheel_scrolls_back_and_the_control_returns_to_the_bottom() {
    let mut h = harness_owned(100, 36, 6);
    ready(&mut h);
    long_chat(&mut h);
    let bottom = h.screen.text();
    assert!(bottom.contains("answer 14"), "{bottom}");
    assert!(!bottom.contains("Back to bottom"));

    for _ in 0..20 {
        h.mouse(MouseEventKind::ScrollUp, 10, 5);
    }
    h.draw();
    let up = h.screen.text();
    assert_ne!(up, bottom);
    assert!(!up.contains("answer 14"), "{up}");
    let rows = h.screen.rows();
    let gap = rows
        .iter()
        .position(|r| r.contains("↓ Back to bottom · esc"))
        .unwrap_or_else(|| panic!("no return control:\n{up}"));
    // It sits in the gap row right above the composer.
    assert!(rows[gap + 1].is_empty() || rows[gap + 1].starts_with('›') || gap > 25);

    // New activity while reading keeps the place and says so. (Sending a prompt would return
    // to the bottom; this is the agent's output arriving.)
    h.app.on_backend(Event::TurnStart);
    h.app.on_backend(Event::TextDelta("answer late\n".into()));
    h.app.on_backend(Event::TurnEnd(StopReason::EndTurn));
    h.draw();
    let reading = h.screen.text();
    assert!(!reading.contains("answer late"), "{reading}");
    assert!(reading.contains("New activity"), "{reading}");

    // A click on the control follows the latest rows again.
    let row = h
        .screen
        .rows()
        .iter()
        .position(|r| r.contains("New activity"))
        .unwrap() as u16;
    let col = h.screen.rows()[row as usize].find('N').unwrap() as u16 + 2;
    h.mouse(MouseEventKind::Down(MouseButton::Left), col, row);
    h.mouse(MouseEventKind::Up(MouseButton::Left), col, row);
    h.draw();
    let back = h.screen.text();
    assert!(back.contains("answer late"), "{back}");
    assert!(!back.contains("Back to bottom"), "{back}");
}

#[test]
fn page_keys_jump_keys_and_esc_move_the_transcript() {
    let mut h = harness_owned(100, 36, 6);
    ready(&mut h);
    long_chat(&mut h);
    h.key(KeyCode::PageUp);
    h.draw();
    assert!(h.screen.text().contains("Back to bottom"));
    h.app
        .on_key(KeyEvent::new(KeyCode::Home, KeyModifiers::CONTROL));
    h.draw();
    assert!(
        h.screen.text().contains("Wizard"),
        "the top shows the header:\n{}",
        h.screen.text()
    );
    // Esc goes back to the bottom before it does anything else.
    h.key(KeyCode::Esc);
    h.draw();
    assert!(h.screen.text().contains("answer 14"));
    assert!(
        !h.app.esc_primed,
        "the first Esc only returned to the bottom"
    );
    // Typing keeps working while reading, and sending returns to the bottom.
    h.key(KeyCode::PageUp);
    h.type_str("next");
    h.draw();
    assert!(h.screen.text().contains("› next"));
}

#[test]
fn overlays_open_and_close_on_the_owned_screen() {
    let mut h = harness_owned(120, 36, 6);
    ready(&mut h);
    turn(&mut h, "go", "Hello there.");
    h.ctrl('t');
    h.draw();
    assert!(h.screen.text().contains("T R A N S C R I P T"));
    h.key(KeyCode::Char('q'));
    h.draw();
    assert!(
        h.screen.in_alt_screen(),
        "closing the pager keeps the screen"
    );
    assert_eq!(h.screen.alt_screen_entered, 1);
    assert!(!text(&h.bytes).contains("\x1b[?1049l"));
    // Nothing of the pager is left behind: the screen is what it would be had it never opened.
    let mut fresh = harness_owned(120, 36, 6);
    ready(&mut fresh);
    turn(&mut fresh, "go", "Hello there.");
    assert_eq!(h.screen.text(), fresh.screen.text());
}

#[test]
fn resize_and_clear_never_purge_the_shell_scrollback() {
    let mut h = harness_owned(120, 36, 6);
    ready(&mut h);
    long_chat(&mut h);
    h.pump();
    h.bytes.clear();
    h.resize(70, 24);
    assert!(h.screen.in_alt_screen());
    assert!(h.screen.text().contains("answer 14"), "{}", h.screen.text());
    assert!(
        h.screen.rows().iter().all(|r| r.chars().count() <= 70),
        "{}",
        h.screen.text()
    );
    h.ctrl('l');
    h.draw();
    assert!(
        !text(&h.bytes).contains("\x1b[3J"),
        "a purge would take the shell's history"
    );
    assert!(!h.screen.text().contains("answer 14"));
}

#[test]
fn exit_gives_the_main_screen_back_with_the_pointer_released() {
    let mut h = harness_owned(120, 36, 6);
    ready(&mut h);
    turn(&mut h, "hello there", "Hello back.");
    h.app.finish_screen();
    h.pump();
    assert!(!h.screen.in_alt_screen());
    assert!(!h.screen.mouse_reporting);
    let all = text(&h.bytes);
    assert_eq!(all.matches("\x1b[?1049h").count(), 1);
    assert_eq!(all.matches("\x1b[?1049l").count(), 1);
    // The pointer is released before the screen.
    assert!(all.find("\x1b[?1000l").unwrap() < all.find("\x1b[?1049l").unwrap());
    // The conversation stays on the alternate screen; the shell's screen is untouched.
    assert!(!h.screen.full_text().contains("hello there"));
}

#[cfg(unix)]
#[test]
fn the_editor_gets_the_main_screen_and_the_chat_takes_it_back() {
    use std::os::unix::fs::PermissionsExt;
    let dir = std::env::temp_dir().join(format!("cxw-alt-ed-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let sh = dir.join("ed.sh");
    std::fs::write(&sh, "#!/bin/sh\nprintf 'from the editor\\n' > \"$1\"\n").unwrap();
    std::fs::set_permissions(&sh, std::fs::Permissions::from_mode(0o755)).unwrap();
    // SAFETY: the only test in this binary that touches the environment.
    unsafe {
        std::env::set_var("VISUAL", &sh);
        std::env::remove_var("EDITOR");
    }
    let mut h = harness_owned(120, 36, 6);
    ready(&mut h);
    h.pump();
    h.bytes.clear();
    h.ctrl('g');
    h.app.launch_editor().unwrap();
    h.draw();
    let _ = std::fs::remove_dir_all(&dir);
    let all = text(&h.bytes);
    let left = all.find("\x1b[?1049l").expect("left for the editor");
    let back = all.rfind("\x1b[?1049h").expect("came back");
    assert!(left < back);
    assert!(
        all[..left].contains("\x1b[?1000l"),
        "pointer released first"
    );
    assert!(all[back..].contains("\x1b[?1003h"), "pointer taken again");
    assert!(h.screen.in_alt_screen());
    assert!(h.screen.mouse_reporting);
    assert!(
        h.screen.text().contains("› from the editor"),
        "{}",
        h.screen.text()
    );
}
