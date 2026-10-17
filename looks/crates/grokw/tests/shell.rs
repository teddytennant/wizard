//! `!cmd`: the `Run (user)` block against `reference/grok/120x36/11-bang-run` (spec 5.8).

mod common;

use common::cells::{assert_cells, Skip};
use common::*;
use crossterm::event::KeyCode;
use grokw::app::Msg;

fn bang(w: u16, h: u16) -> Harness {
    let mut hn = Harness::new(w, h);
    hn.app.tr.usage.context_tokens = 1_500;
    hn.app.tr.usage.context_window = 256_000;
    hn.app.note("No background tasks, workflows, or subagents.");
    hn.type_str("!ls");
    hn.press(KeyCode::Enter);
    hn
}

fn finish(hn: &mut Harness, output: &str, ok: bool) {
    hn.app.update(Msg::Shell {
        id: "bang-1".into(),
        cmd: "ls".into(),
        output: output.into(),
        ok,
    });
}

#[test]
#[cfg_attr(
    not(reference_captures),
    ignore = "needs the reference/ captures, not in this repository; see build.rs"
)]
fn a_finished_bang_is_a_green_run_user_block() {
    for (w, h) in [(120u16, 36u16), (150, 42), (80, 24)] {
        let mut hn = bang(w, h);
        finish(&mut hn, "README.md\nsrc\n", true);
        hn.dump(&format!("bang-run-{w}x{h}"));
        assert_snapshot(&format!("bang-run-{w}x{h}"), &hn.text());
        if (w, h) == (120, 36) {
            assert_cells("120x36/11-bang-run", &hn.render(), &Skip::default());
        }
    }
}

#[test]
fn a_running_bang_shows_its_command_and_no_output_yet() {
    let mut hn = bang(120, 36);
    let t = hn.text();
    assert!(t.contains("Run (user) ls") && t.contains("$ ls"), "{t}");
    assert!(!t.contains("README.md"), "{t}");
    // nothing went to the model and no turn is open
    assert!(hn.sent().is_empty());
    assert!(!hn.app.busy());
}

#[test]
fn a_failed_bang_has_the_red_accent_and_no_exit_row() {
    use ratatui::style::Color;
    let mut hn = bang(120, 36);
    finish(&mut hn, "ls: nope: No such file or directory\n", false);
    let t = hn.render();
    // the rail of the block is the error colour
    let y = (0..36)
        .find(|&y| t.row(y).contains("Run (user) ls"))
        .expect("header row");
    assert_eq!(
        t.cell(2, y).unwrap().fg,
        Color::Rgb(0xf7, 0x76, 0x8e),
        "{}",
        t.plain()
    );
    assert!(!t.plain().contains("exit"));
}
