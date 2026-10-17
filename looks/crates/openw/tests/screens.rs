//! Screen snapshots. Each test builds an app state, renders it, and compares with the checked-in
//! text in `tests/snapshots/`. Colors and positions that matter are asserted separately.

mod common;

use common::*;

use agent_core::{Event, StopReason, ToolKind};

/// Config that names the model like the reference captures do.
fn opencode_like(h: &mut Harness) {
    let mut cfg = agent_core::mock::config();
    cfg.model = "opencode/big-pickle".into();
    cfg.models = vec![agent_core::ModelOption {
        id: "opencode/big-pickle".into(),
        name: "Big Pickle".into(),
        provider: "OpenCode Zen".into(),
    }];
    h.event(Event::ConfigChanged(cfg));
}

/// The scripted run from `reference/120x36/16-session-done`.
fn reference_run(h: &mut Harness) {
    h.app.cwd =
        "/tmp/claude-1000/-home-nixos/46dc526d-4fc6-4f47-b09e-820c7c4f2577/scratchpad/ocref/proj"
            .into();
    h.app.send_prompt("List the files in this dir, read README.md, then write notes.txt containing the word hi. Keep the answer short.".into());
    h.event(Event::TurnStart);
    h.event(Event::TextDelta(
        "I'll list the files and read the README.".into(),
    ));
    let ls = "total 24\ndrwxr-xr-x 4 nixos users 4096 Oct  5 13:17 .\ndrwxr-xr-x 4 nixos users 4096 Oct  5 13:16 ..\ndrwxr-xr-x 7 nixos users 4096 Oct  5 13:16 .git\n-rw-r--r-- 1 nixos users    2 Oct  5 13:16 notes.txt\n-rw-r--r-- 1 nixos users   21 Oct  5 13:16 README.md\ndrwxr-xr-x 2 nixos users 4096 Oct  5 13:16 src";
    h.event(Event::Tool(tool(
        "1",
        "bash",
        ToolKind::Execute,
        "ls -la",
        serde_json::json!({"command": "ls -la"}),
        Some(ls),
    )));
    h.event(Event::Tool(tool(
        "2",
        "glob",
        ToolKind::Search,
        "README.md",
        serde_json::json!({"pattern": "README.md"}),
        Some("README.md"),
    )));
    h.event(Event::ThoughtDelta("**Checking**\nthe files".into()));
    h.event(Event::Tool(tool("3", "read", ToolKind::Read, "README.md", serde_json::json!({"filePath": "/tmp/claude-1000/-home-nixos/46dc526d-4fc6-4f47-b09e-820c7c4f2577/scratchpad/ocref/proj/README.md"}), Some("x"))));
    h.event(Event::Tool(tool("4", "read", ToolKind::Read, "notes.txt", serde_json::json!({"filePath": "/tmp/claude-1000/-home-nixos/46dc526d-4fc6-4f47-b09e-820c7c4f2577/scratchpad/ocref/proj/notes.txt"}), Some("hi"))));
    h.event(Event::ThoughtDelta("more thinking".into()));
    h.event(Event::TextDelta("`notes.txt` already contains exactly `hi` \u{2014} no change needed.\n\n- Dir contents: `README.md`, `notes.txt`, `src/`, `.git/`\n- `README.md`: title \"demo project\", body \"hello\"".into()));
    h.event(Event::Usage(agent_core::Usage {
        input_tokens: 11_000,
        output_tokens: 700,
        context_tokens: 11_700,
        context_window: 200_000,
        ..Default::default()
    }));
    h.event(Event::TurnEnd(StopReason::EndTurn));
    h.fix_durations();
    for m in &mut h.app.transcript.messages {
        if m.took.is_some() {
            m.took = Some(std::time::Duration::from_millis(6500));
        }
        for p in &mut m.parts {
            if let agent_core::transcript::Part::Thought {
                text,
                took: Some(t),
                ..
            } = p
            {
                *t = std::time::Duration::from_millis(if text.contains("Checking") {
                    237
                } else {
                    986
                });
            }
        }
    }
}

/// Reference rows with the lines that depend on the machine (cwd, model) left out.
fn ref_rows(path: &str) -> Vec<String> {
    let p = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../reference")
        .join(path);
    std::fs::read_to_string(p)
        .expect("reference file")
        .lines()
        .map(|l| l.trim_end().to_string())
        .collect()
}

#[test]
#[cfg_attr(
    not(reference_captures),
    ignore = "needs the reference/ captures, not in this repository; see build.rs"
)]
fn session_matches_reference_16_layout() {
    let mut h = Harness::new(120, 36);
    opencode_like(&mut h);
    reference_run(&mut h);
    h.dump("16-session-done");
    let got: Vec<String> = h.text().lines().map(|l| l.trim_end().to_string()).collect();
    let want = ref_rows("120x36/16-session-done.txt");
    // Thought rows carry a title in ours ("Checking"), which the reference run did not have.
    for (i, w) in want.iter().enumerate().take(35) {
        let g = got.get(i).cloned().unwrap_or_default();
        if w.contains("Thought") {
            assert!(g.contains("Thought"), "row {i}: want {w:?} got {g:?}");
            continue;
        }
        assert_eq!(&g, w, "row {i}");
    }
}

#[test]
#[cfg_attr(
    not(reference_captures),
    ignore = "needs the reference/ captures, not in this repository; see build.rs"
)]
fn working_state_matches_reference_15() {
    let mut h = Harness::new(120, 36);
    opencode_like(&mut h);
    h.app.cwd = "/tmp/x".into();
    h.app.send_prompt("List the files in this dir, read README.md, then write notes.txt containing the word hi. Keep the answer short.".into());
    h.event(Event::TurnStart);
    h.event(Event::TextDelta(
        "I'll list the files and read the README.".into(),
    ));
    let ls = "total 24\ndrwxr-xr-x 4 nixos users 4096 Oct  5 13:17 .\ndrwxr-xr-x 4 nixos users 4096 Oct  5 13:16 ..\ndrwxr-xr-x 7 nixos users 4096 Oct  5 13:16 .git\n-rw-r--r-- 1 nixos users    2 Oct  5 13:16 notes.txt\n-rw-r--r-- 1 nixos users   21 Oct  5 13:16 README.md\ndrwxr-xr-x 2 nixos users 4096 Oct  5 13:16 src";
    let mut run = tool(
        "1",
        "bash",
        ToolKind::Execute,
        "ls -la",
        serde_json::json!({"command": "ls -la"}),
        Some(ls),
    );
    run.status = agent_core::ToolStatus::Running;
    h.event(Event::Tool(run));
    h.event(Event::Tool(tool(
        "2",
        "glob",
        ToolKind::Search,
        "README.md",
        serde_json::json!({"pattern": "README.md"}),
        Some("README.md"),
    )));
    h.app.frozen = Some(std::time::Duration::ZERO);
    h.dump("15-working");
    let got: Vec<String> = h.text().lines().map(|l| l.trim_end().to_string()).collect();
    let want = ref_rows("120x36/15-working.txt");
    for (i, w) in want.iter().enumerate().take(35) {
        if i == 34 {
            // the right side carries usage in ours, which the reference did not have yet
            assert!(
                got[i].starts_with("   ■⬝⬝⬝⬝⬝⬝⬝  esc interrupt"),
                "row 34: {:?}",
                got[i]
            );
            continue;
        }
        assert_eq!(&got[i], w, "row {i}");
    }
}

#[test]
#[cfg_attr(
    not(reference_captures),
    ignore = "needs the reference/ captures, not in this repository; see build.rs"
)]
fn home_80x24_matches_reference() {
    let mut h = Harness::new(80, 24);
    h.app.cwd =
        "/tmp/claude-1000/-home-nixos/46dc526d-4fc6-4f47-b09e-820c7c4f2577/scratchpad/spec/proj"
            .into();
    h.app.config.models.clear();
    h.app.config.backend = "wizard 1.18.34".into();
    let got: Vec<String> = h.text().lines().map(|l| l.trim_end().to_string()).collect();
    let want = ref_rows("extra/home-80x24.txt");
    // logo glyphs, placeholder example and the model line differ by design
    for i in [
        0, 1, 2, 3, 8, 9, 10, 12, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23,
    ] {
        let (g, w) = (
            got.get(i).cloned().unwrap_or_default(),
            want.get(i).cloned().unwrap_or_default(),
        );
        assert_eq!(g, w, "row {i}");
    }
}

#[test]
#[cfg_attr(
    not(reference_captures),
    ignore = "needs the reference/ captures, not in this repository; see build.rs"
)]
fn narrow_column_wraps_the_hint_row_like_the_150x42_reference() {
    let mut h = Harness::new(150, 42);
    opencode_like(&mut h);
    h.app.cwd =
        "/tmp/claude-1000/-home-nixos/46dc526d-4fc6-4f47-b09e-820c7c4f2577/scratchpad/ocref2/proj"
            .into();
    h.app.send_prompt("hi".into());
    h.event(Event::TurnStart);
    h.event(Event::TextDelta("ok".into()));
    h.event(Event::Usage(agent_core::Usage {
        input_tokens: 11_000,
        output_tokens: 261,
        context_tokens: 11_261,
        context_window: 200_000,
        ..Default::default()
    }));
    h.event(Event::TurnEnd(StopReason::EndTurn));
    h.dump("hint-150x42");
    let got: Vec<String> = h.text().lines().map(|l| l.trim_end().to_string()).collect();
    let want = ref_rows("150x42/16-session-done.txt");
    // prompt box rows and the two hint rows; the sidebar column is not drawn yet, so compare
    // only the main column (first 108 cells)
    let cut = |s: &str| {
        s.chars()
            .take(108)
            .collect::<String>()
            .trim_end()
            .to_string()
    };
    for i in 34..=40 {
        assert_eq!(cut(&got[i]), cut(&want[i]), "row {i}");
    }
}
