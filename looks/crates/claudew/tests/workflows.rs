//! `/workflows`: the panel, compared cell by cell with Claude Code 2.1.314 driven against a fake
//! Messages server (`tools/fake_anthropic.py` plus a `Workflow` call whose script has three
//! agents in two phases). The captures are `tests/fixtures/ref-2.1.314/workflows-*-140x45.ansi`.
//!
//! The run is written to disk the way wizard writes it (`run.json` and `agents.json` under
//! `$WIZARD_HOME/workflows/runs/<id>/`), because that is all the look reads.

mod common;

use claudew::ui;
use common::{mismatches, reference, Harness};
use crossterm::event::KeyCode;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use serde_json::json;

/// Rows above the panel's edge: the transcript, which differs between the two programs.
const SKIP: u16 = 4;

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

fn write_run(home: &std::path::Path) {
    let dir = home.join("workflows/runs/wf_00000001-aaa");
    std::fs::create_dir_all(&dir).unwrap();
    let start = now_ms() - 18_000;
    std::fs::write(
        dir.join("run.json"),
        json!({
            "run_id": "wf_00000001-aaa", "name": "demo-flow",
            "description": "Three agents in two phases", "status": "running",
            "started_ms": start, "ended_ms": null, "pid": std::process::id(),
            "phases": ["Scan", "Fix"]
        })
        .to_string(),
    )
    .unwrap();
    std::fs::write(
        dir.join("agents.json"),
        json!({
            "phases": ["Scan", "Fix"],
            "agents": [
                {"i": 1, "label": "scan:quick", "phase": "Scan", "state": "done", "agent": "Haiku 5.5",
                 "started_ms": start, "ended_ms": start, "prompt": "quick scan task", "outcome": "ok"},
                {"i": 2, "label": "scan:slow", "phase": "Scan", "state": "running", "agent": "Haiku 5.5",
                 "started_ms": start, "prompt": "slow scan task"}
            ]
        })
        .to_string(),
    )
    .unwrap();
}

/// The rows from `SKIP` down, as a buffer of their own.
fn below(buf: &Buffer) -> Buffer {
    let area = buf.area;
    let mut out = Buffer::empty(Rect::new(0, 0, area.width, area.height - SKIP));
    for y in SKIP..area.height {
        for x in 0..area.width {
            out[(x, y - SKIP)] = buf[(x, y)].clone();
        }
    }
    out
}

/// What differs for a stated reason: the model and token count column (wizard names the
/// subagent), the clocks, and the hint line, which lists only the keys wizard has.
const MASKS: &[&str] = &[
    r"Haiku 5\.5(?: · \d+ tok)?",
    r"\d+/\d+ agents · \d+s",
    r"^   ↑↓ .*$",
    r"^   [│╭╰].*│ +\d+s +│$",
    r"\b\d+s\b",
    r"^   │.{16}│ (?:✔|●) (?:Completed|Running) · .*│$",
    r"140 tok · 0s",
    r"^.{4}[ ❯] ?[✔●] scan:\w+ +",
];

fn check(hn: &mut Harness, name: &str) {
    let want = below(&reference(name));
    let t = hn.render();
    let got = below(t.buffer());
    let bad = mismatches(&got, &want, MASKS);
    assert!(
        bad.is_empty(),
        "{name}: {} cells differ; first:\n{}",
        bad.len(),
        bad.iter().take(4).cloned().collect::<Vec<_>>().join("\n")
    );
}

#[test]
fn the_panel_matches_claude_codes_at_every_level() {
    let home = std::env::temp_dir().join(format!("claudew-wf-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&home);
    write_run(&home);
    std::env::set_var("WIZARD_HOME", &home);

    let mut hn = Harness::new("static-120x40.jsonl", 140, 45);
    hn.type_str("/workflows");
    hn.press(KeyCode::Enter);
    assert!(
        matches!(hn.app.dialog, Some(claudew::app::Dialog::Workflows(_))),
        "/workflows opens the panel"
    );
    check(&mut hn, "workflows-phases-140x45");

    hn.press(KeyCode::Down);
    check(&mut hn, "workflows-empty-phase-140x45");

    hn.press(KeyCode::Up);
    hn.press(KeyCode::Enter);
    check(&mut hn, "workflows-agents-140x45");

    hn.press(KeyCode::Down);
    hn.press(KeyCode::Enter);
    check(&mut hn, "workflows-detail-140x45");

    hn.press(KeyCode::Up);
    check(&mut hn, "workflows-detail-done-140x45");

    // x asks the process that owns the run to stop it, by writing a file it checks every second
    hn.press(KeyCode::Char('x'));
    assert!(home.join("workflows/runs/wf_00000001-aaa/stop").exists());

    hn.press(KeyCode::Esc);
    hn.press(KeyCode::Esc);
    hn.press(KeyCode::Esc);
    assert!(hn.app.dialog.is_none(), "esc backs out to the prompt");
    let _ = ui::draw;
    let _ = std::fs::remove_dir_all(&home);
}
