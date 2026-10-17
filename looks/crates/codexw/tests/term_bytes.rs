//! Byte-level proof that the inline renderer writes what Codex 0.147.0 writes.
//!
//! The fixture is the first ten synchronized frames of a real `codex` session in a 120x36 pane
//! (`reference/codex/raw/modes-120x36.typescript`): the placeholder frame, three unchanged
//! redraws, the frame that inserts the session header and tip into the scrollback, three idle
//! redraws, the typed draft, and the Enter that inserts the user message. The terminal-title writes are dropped from both sides. Documented differences:
//! the model name and version are whatever the scripted config says (`gpt-5.5` and `0.147.0`
//! here, as in the capture; the loading header gets the same version, which wizard would only
//! report once it answers; the tip is the capture's, not codexw's own), and
//! the title spinner that Codex runs during MCP startup does not exist in codexw.

mod common;

use agent_core::{Config, Event};
use common::*;

const FIXTURE: &[u8] = include_bytes!("fixtures/codex-first-frames-120x36.bin");
/// The tip the capture drew. codexw's own names no model.
const CODEX_TIP: &str = "Our most capable model yet. GPT-5.6 Sol can tackle complex code changes, dig into research, produce polished documents, and take on your most ambitious work. Sol is highly capable at lower reasoning efforts\u{2014}try starting lower, then turn it up for harder jobs.";

fn ready() -> Event {
    Event::Ready {
        session_id: "s1".into(),
        config: Config {
            model: "gpt-5.5".into(),
            effort: "default".into(),
            backend: "wizard 0.147.0".into(),
            ..Default::default()
        },
    }
}

#[test]
fn first_frames_match_the_codex_byte_stream() {
    // Placeholder 7 is `Run /review on my current changes`, the one the capture drew.
    let mut h = harness(120, 36, 6);
    h.app.tip = CODEX_TIP;
    if let Some(header) = h.app.placeholder_header.as_mut() {
        header.version = "0.147.0".into();
    }
    let mut ours = Vec::new();
    for _ in 0..4 {
        h.app.draw().unwrap();
        ours.extend(h.out.take());
    }
    h.app.on_backend(ready());
    h.app.draw().unwrap();
    ours.extend(h.out.take());
    // Three idle redraws, the typed draft (one frame: it arrived as a burst), then Enter, which
    // writes the user message into the scrollback and moves the viewport down four rows.
    for _ in 0..3 {
        h.app.draw().unwrap();
        ours.extend(h.out.take());
    }
    h.app.on_paste("go fake:tour");
    h.app.draw().unwrap();
    ours.extend(h.out.take());
    h.key(crossterm::event::KeyCode::Enter);
    h.app.draw().unwrap();
    ours.extend(h.out.take());

    let want = strip_osc(FIXTURE);
    let got = strip_osc(&ours);
    if want != got {
        let at = want
            .iter()
            .zip(&got)
            .position(|(a, b)| a != b)
            .unwrap_or(want.len().min(got.len()));
        let lo = at.saturating_sub(120);
        panic!(
            "streams differ at byte {at} (want {} bytes, got {})\nwant: ...{}\ngot:  ...{}",
            want.len(),
            got.len(),
            show(&want[lo..(at + 160).min(want.len())]),
            show(&got[lo..(at + 160).min(got.len())]),
        );
    }
}

/// The same frames, replayed into the emulator: the screen must show the header, the tip and the
/// composer exactly where the capture has them (header row 0, composer text row 12).
#[test]
fn emulated_screen_matches_start_capture() {
    let mut h = harness(120, 36, 7);
    h.app.tip = CODEX_TIP;
    h.draw();
    h.app.on_backend(ready());
    h.draw();
    let rows = h.screen.rows();
    assert!(rows[0].starts_with("╭──"), "{rows:?}");
    assert_eq!(rows[3], "│ model:     gpt-5.5   /model to change │");
    assert!(
        rows[7].starts_with("  Tip: Our most capable model yet."),
        "{rows:?}"
    );
    assert_eq!(rows[12], "› Use /skills to list available skills");
    assert_eq!(rows[14], "  gpt-5.5 default · ~/proj");
    assert_eq!(
        h.screen.alt_screen_entered, 0,
        "the chat must never use the alternate screen"
    );
}
