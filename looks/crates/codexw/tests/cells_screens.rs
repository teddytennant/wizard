//! Screen-level checks of the history cells: a scripted turn goes through the whole app on a
//! headless terminal and the rows are compared with what Codex 0.147.0 drew for the same turn
//! (`reference/codex/<size>/turns-*.txt`). Rows that hold scripted text which differs between
//! the fake model and the backend that fed the capture are skipped by name.

use std::path::PathBuf;

use agent_core::{Config, Event};
use codexw::fake::scenario_events;
use codexw::testing::{Harness, harness};

fn reference(size: &str, name: &str) -> Vec<String> {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../reference/codex")
        .join(size)
        .join(name);
    std::fs::read_to_string(&p)
        .unwrap_or_else(|e| panic!("{}: {e}", p.display()))
        .lines()
        .map(|l| l.trim_end().to_string())
        .collect()
}

fn ready() -> Event {
    Event::Ready {
        session_id: "s1".into(),
        config: Config {
            model: "gpt-5.5".into(),
            effort: "default".into(),
            ..Default::default()
        },
    }
}

/// An app that has shown its header, then sent `go fake:<scenario>` and received the turn.
pub fn run_turn(cols: u16, rows: u16, scenario: &str) -> Harness {
    run_turn_as(cols, rows, scenario, scenario)
}

/// Like `run_turn`, with the prompt typed as `go fake:<typed>` (the capture's name for it).
pub fn run_turn_as(cols: u16, rows: u16, scenario: &str, typed: &str) -> Harness {
    let mut h = harness(cols, rows, 7);
    // The capture has no notes about what wizard cannot do.
    h.app.log.notes = false;
    h.app.on_backend(ready());
    h.draw();
    h.type_str(&format!("go fake:{typed}"));
    h.key(crossterm::event::KeyCode::Enter);
    h.draw();
    for ev in scenario_events(scenario, "/tmp/cxw-home/proj").expect("scenario") {
        h.app.on_backend(ev);
        h.draw();
    }
    h.draw();
    h
}

/// The composer placeholder is a random pick in Codex: blank it, found as the row two above
/// the footer.
fn without_placeholder(rows: &[String]) -> Vec<String> {
    let mut rows = rows.to_vec();
    if let Some(f) = rows.iter().rposition(|r| r.starts_with("  gpt-5.5 "))
        && f >= 2
        && rows[f - 2].starts_with("› ")
    {
        rows[f - 2] = "›".into();
    }
    rows
}

/// Rows of `got` against `want`, ignoring the composer placeholder and any row whose number is
/// in `skip` (1-based). Panics with the first few differences.
pub fn assert_rows(got: &[String], want: &[String], skip: &[usize]) {
    let (got, want) = (without_placeholder(got), without_placeholder(want));
    let mut diffs = Vec::new();
    for i in 0..want.len().max(got.len()) {
        if skip.contains(&(i + 1)) {
            continue;
        }
        let g = got.get(i).map(String::as_str).unwrap_or("");
        let w = want.get(i).map(String::as_str).unwrap_or("");
        if g != w {
            diffs.push(format!("row {}:\n  want {w:?}\n  got  {g:?}", i + 1));
        }
    }
    assert!(
        diffs.is_empty(),
        "{} rows differ:\n{}",
        diffs.len(),
        diffs.iter().take(8).cloned().collect::<Vec<_>>().join("\n")
    );
}

const SIZES: [(&str, u16, u16); 7] = [
    ("120x36", 120, 36),
    ("150x42", 150, 42),
    ("200x50", 200, 50),
    ("80x24", 80, 24),
    ("60x24", 60, 24),
    ("40x24", 40, 24),
    ("30x24", 30, 24),
];

/// The git hash in `git log --oneline` differs per fixture repo.
fn normalize(rows: &[String]) -> Vec<String> {
    rows.iter()
        .map(|r| {
            let t = r.trim_start();
            match t.split_once(' ') {
                Some((h, rest))
                    if h.len() == 7
                        && h.chars().all(|c| c.is_ascii_hexdigit())
                        && rest == "init" =>
                {
                    format!("{}HASH init", &r[..r.len() - t.len()])
                }
                _ => r.clone(),
            }
        })
        .collect()
}

/// Every size the capture was taken at. `typed` is the scenario name the capture's prompt used.
fn check_all(scenario: &str, typed: &str, refname: &str, sizes: &[&str]) {
    for (size, c, r) in SIZES {
        if !sizes.is_empty() && !sizes.contains(&size) {
            continue;
        }
        let p = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../reference/codex")
            .join(size)
            .join(format!("{refname}.txt"));
        if !p.exists() {
            continue;
        }
        let h = run_turn_as(c, r, scenario, typed);
        let got = normalize(&h.screen.rows());
        let want = normalize(&reference(size, &format!("{refname}.txt")));
        println!("{size} {scenario}");
        assert_rows(&got, &want, &[]);
    }
}

#[test]
fn text_turn_matches_the_capture() {
    check_all("text", "text", "turns-01-text-done", &[]);
}

#[test]
fn markdown_turn_matches_the_capture_at_every_size() {
    check_all("markdown", "markdown", "turns-02-markdown-done", &[]);
}

#[test]
fn long_answer_matches_the_capture() {
    check_all("long", "long", "turns-03-long-done", &[]);
}

#[test]
fn exec_turn_matches_the_capture() {
    check_all("exec", "exec", "turns-05-exec-done", &[]);
}

#[test]
fn patch_turn_matches_the_capture() {
    check_all("patch", "patch", "turns-06-patch-done", &[]);
}

#[test]
fn web_search_turn_matches_the_capture() {
    check_all("search", "search", "turns-09-search-done", &[]);
}

#[test]
fn tour_matches_the_capture() {
    check_all("tour", "tour", "turns-11-tour-done", &[]);
}

/// Wizard has no explanation field on its plan tool, so the first plan cell (which only the big
/// screens still show) has no `Start with the library` row.
#[test]
fn plan_turn_matches_the_capture() {
    check_all(
        "plan",
        "plan",
        "turns-08-plan-done",
        &["80x24", "60x24", "40x24", "30x24"],
    );
}

/// Codex lists the explored calls in the order they finished, which varies between runs; the
/// scripted calls here arrive as list, read, search.
#[test]
#[cfg_attr(
    not(reference_captures),
    ignore = "needs the reference/ captures, not in this repository; see build.rs"
)]
fn explore_turn_matches_the_capture_up_to_row_order() {
    for (size, c, r) in SIZES {
        let h = run_turn_as(c, r, "explore-sh", "explore");
        let sorted = |rows: Vec<String>| {
            let mut rows = rows;
            if let Some(i) = rows.iter().position(|l| l == "• Explored") {
                let mut j = i + 1;
                while j < rows.len() && !rows[j].is_empty() {
                    j += 1;
                }
                let mut body: Vec<String> = rows[i + 1..j]
                    .iter()
                    .map(|l| l.replacen("  └ ", "    ", 1))
                    .collect();
                body.sort();
                for (k, l) in body.into_iter().enumerate() {
                    rows[i + 1 + k] = l;
                }
            }
            rows
        };
        let want = sorted(reference(size, "turns-04-explore-done.txt"));
        assert_rows(&sorted(h.screen.rows()), &want, &[]);
    }
}

#[test]
#[ignore]
fn dump_markdown() {
    let sc = std::env::var("SC").unwrap_or("markdown".into());
    let (c, r) = std::env::var("SZ")
        .ok()
        .and_then(|s| {
            s.split_once('x')
                .map(|(a, b)| (a.parse().unwrap(), b.parse().unwrap()))
        })
        .unwrap_or((120, 36));
    let h = run_turn(c, r, &sc);
    for (i, r) in h.screen.rows().iter().enumerate() {
        println!("{:2} {r}", i + 1);
    }
}

#[test]
#[ignore]
fn dump_viewport_heights() {
    let sc = std::env::var("SC").unwrap_or("markdown".into());
    let mut h = harness(120, 36, 7);
    h.app.on_backend(ready());
    h.draw();
    h.type_str(&format!("go fake:{sc}"));
    h.key(crossterm::event::KeyCode::Enter);
    h.draw();
    for ev in scenario_events(&sc, "/tmp/cxw-home/proj").unwrap() {
        let name = format!("{ev:?}");
        println!("active before: {}", h.app.log.active_lines(120).len());
        h.app.on_backend(ev);
        h.draw();
        if name.contains("|---") || name.contains("1 |") {
            for (i, r) in h.screen.rows().iter().enumerate().skip(22) {
                println!("  {:2} {:.60}", i + 1, r);
            }
        }
        println!(
            "{:.40} vp={:?} want={}",
            name.replace('\n', " "),
            h.app.term.viewport_area,
            h.app.desired_height()
        );
    }
}

fn survey_one(size: &str, cols: u16, rows: u16, scenario: &str, refname: &str) -> String {
    let h = run_turn(cols, rows, scenario);
    let got = without_placeholder(&h.screen.rows());
    let want = without_placeholder(&reference(size, &format!("{refname}.txt")));
    let mut out = String::new();
    let mut n = 0;
    for i in 0..want.len().max(got.len()) {
        let g = got.get(i).map(String::as_str).unwrap_or("");
        let w = want.get(i).map(String::as_str).unwrap_or("");
        if g != w {
            n += 1;
            if n <= 6 {
                out.push_str(&format!(
                    "   row {}:\n     want {w:?}\n     got  {g:?}\n",
                    i + 1
                ));
            }
        }
    }
    format!("{size} {scenario}: {n} rows differ\n{out}")
}

#[test]
#[ignore]
fn survey() {
    let sizes = [
        ("120x36", 120, 36),
        ("80x24", 80, 24),
        ("60x24", 60, 24),
        ("40x24", 40, 24),
        ("30x24", 30, 24),
        ("150x42", 150, 42),
        ("200x50", 200, 50),
    ];
    let scenarios = [
        ("text", "turns-01-text-done"),
        ("markdown", "turns-02-markdown-done"),
        ("long", "turns-03-long-done"),
        ("explore-sh", "turns-04-explore-done"),
        ("exec", "turns-05-exec-done"),
        ("patch", "turns-06-patch-done"),
        ("patch-multi", "turns-07-patch-multi-done"),
        ("plan", "turns-08-plan-done"),
        ("search", "turns-09-search-done"),
        ("mcp", "turns-10-mcp-done"),
        ("tour", "turns-11-tour-done"),
    ];
    let only = std::env::var("ONLY").unwrap_or_default();
    for (size, c, r) in sizes {
        for (sc, rf) in scenarios {
            if !only.is_empty() && !only.split(',').any(|o| o == sc) {
                continue;
            }
            let p = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("../../reference/codex")
                .join(size)
                .join(format!("{rf}.txt"));
            if !p.exists() {
                continue;
            }
            println!("{}", survey_one(size, c, r, sc, rf));
        }
    }
}

/// `(3s • esc to interrupt)` depends on the clock.
fn normalize_clock(rows: &[String]) -> Vec<String> {
    rows.iter()
        .map(|r| match (r.find(" ("), r.find("s • esc to interrupt")) {
            (Some(a), Some(b)) if a < b && r[a + 2..b].chars().all(|c| c.is_ascii_digit()) => {
                format!("{}(Ns{}", &r[..a + 1], &r[b + 1..])
            }
            _ => r.clone(),
        })
        .collect()
}

/// A turn stopped after `take` events (`TurnStart` counts as the first).
fn run_prefix(cols: u16, rows: u16, scenario: &str, typed: &str, take: usize) -> Harness {
    let mut h = harness(cols, rows, 7);
    // The capture has no notes about what wizard cannot do.
    h.app.log.notes = false;
    h.app.on_backend(ready());
    h.draw();
    h.type_str(&format!("go fake:{typed}"));
    h.key(crossterm::event::KeyCode::Enter);
    h.draw();
    for ev in scenario_events(scenario, "/tmp/cxw-home/proj")
        .expect("scenario")
        .into_iter()
        .take(take)
    {
        h.app.on_backend(ev);
        h.draw();
    }
    h
}

#[test]
#[cfg_attr(
    not(reference_captures),
    ignore = "needs the reference/ captures, not in this repository; see build.rs"
)]
fn reasoning_title_is_the_status_header() {
    let h = run_prefix(120, 36, "text", "text", 2);
    let want = reference("120x36", "turns-01-text-mid.txt");
    assert_rows(
        &normalize_clock(&h.screen.rows()),
        &normalize_clock(&want),
        &[],
    );
}

#[test]
#[cfg_attr(
    not(reference_captures),
    ignore = "needs the reference/ captures, not in this repository; see build.rs"
)]
fn explored_cell_then_rule_then_working_while_the_answer_has_no_newline() {
    // thought, 3 calls (running + done each), then the first words of the answer
    let h = run_prefix(120, 36, "explore-sh", "explore", 1 + 1 + 6 + 1);
    let want = reference("120x36", "turns-04-explore-mid.txt");
    assert_rows(
        &normalize_clock(&h.screen.rows()),
        &normalize_clock(&want),
        &[],
    );
}

#[test]
#[cfg_attr(
    not(reference_captures),
    ignore = "needs the reference/ captures, not in this repository; see build.rs"
)]
fn finished_commands_stay_above_the_working_row() {
    let h = run_prefix(120, 36, "exec", "exec", 1 + 3 * 2);
    let want = reference("120x36", "turns-05-exec-mid.txt");
    assert_rows(
        &normalize_clock(&h.screen.rows()),
        &normalize_clock(&want),
        &[],
    );
}

#[test]
fn a_running_command_is_the_in_flight_cell() {
    // The call is announced, the answer is not in yet: `Running` shows inside the viewport.
    let h = run_prefix(120, 36, "exec", "exec", 2);
    let rows = h.screen.rows();
    let i = rows
        .iter()
        .position(|r| r.starts_with("• Running echo hello from the shell"))
        .expect("running row");
    assert!(
        rows[i + 1].is_empty(),
        "blank row between the cell and the status row: {rows:?}"
    );
    assert!(rows[i + 2].starts_with("• Working ("), "{rows:?}");
}

#[test]
#[cfg_attr(
    not(reference_captures),
    ignore = "needs the reference/ captures, not in this repository; see build.rs"
)]
fn proposed_plan_is_one_tinted_cell_that_appears_whole() {
    let h = run_turn(120, 36, "proposed-plan");
    let want = reference("120x36", "xb-05-proposed-plan-done.txt");
    let got = h.screen.rows();
    // The capture is in Plan mode with the "Implement this plan?" prompt in the bottom pane;
    // compare the cell only, up to and including its closing tinted row.
    let i = got
        .iter()
        .position(|r| r == "• Proposed Plan")
        .expect("header");
    let j = want
        .iter()
        .position(|r| r == "• Proposed Plan")
        .expect("header");
    assert_eq!(&got[i..i + 8], &want[j..j + 8], "{got:?}");
}

#[test]
#[cfg_attr(
    not(reference_captures),
    ignore = "needs the reference/ captures, not in this repository; see build.rs"
)]
fn provider_errors_are_one_red_row_after_the_prompt() {
    for sc in ["500", "429", "failed", "quota", "400"] {
        let h = run_turn(120, 36, &format!("error-{sc}"));
        let want = reference("120x36", &format!("errors-{sc}-final.txt"));
        println!("error-{sc}");
        assert_rows(&h.screen.rows(), &want, &[]);
    }
}

#[test]
#[ignore]
fn survey_extra() {
    for (sc, rf) in [
        ("reason-body", "xb-03-reason-body-done"),
        ("reason-two", "xb-04-reason-two"),
        ("reason-variants", "xm-02-reason-variants"),
        ("patch-add", "xd-patch-add"),
        ("patch-delete", "xd-patch-delete"),
        ("patch-hunks", "xd-patch-hunks"),
        ("patch-wrap", "xd-patch-wrap"),
        ("patch-rename", "xd-patch-rename"),
        ("patch-fail", "xd-patch-fail"),
        ("mcp-long", "xe-01-mcp-long"),
        ("mcp-long2", "xm-01-mcp-long2"),
        ("exec-edge", "xe-04-exec-edge-done"),
        ("search-slow", "xe-03-search-done"),
        ("view-image", "xd-view-image"),
    ] {
        let p = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../reference/codex/120x36")
            .join(format!("{rf}.txt"));
        if !p.exists() || scenario_events(sc, "/tmp/x").is_none() {
            println!("120x36 {sc}: no reference or scenario");
            continue;
        }
        println!("{}", survey_one("120x36", 120, 36, sc, rf));
    }
}

/// What the Ctrl+T pager shows: every cell's transcript lines, one blank row between cells that
/// do not continue the one before.
fn transcript_rows(h: &Harness, width: u16) -> Vec<String> {
    let mut rows: Vec<String> = Vec::new();
    for c in &h.app.cells {
        let lines = c.transcript_lines(width);
        if lines.is_empty() {
            continue;
        }
        if !rows.is_empty() && !c.is_stream_continuation() {
            rows.push(String::new());
        }
        rows.extend(lines.iter().map(|l| {
            l.spans
                .iter()
                .map(|s| s.content.as_ref())
                .collect::<String>()
                .trim_end()
                .to_string()
        }));
    }
    rows
}

#[test]
#[cfg_attr(
    not(reference_captures),
    ignore = "needs the reference/ captures, not in this repository; see build.rs"
)]
fn the_pager_shows_reasoning_and_the_exec_transcript_form() {
    let h = run_turn(120, 36, "tour");
    let got = transcript_rows(&h, 120);
    let want = reference("120x36", "pager-04-pager-home.txt");
    // reasoning is visible in the pager, plan cell as in the viewport
    let g = got
        .iter()
        .position(|r| r.starts_with("• Planning the change"))
        .expect("reasoning row");
    let w = want
        .iter()
        .position(|r| r.starts_with("• Planning the change"))
        .expect("reasoning row");
    assert_eq!(&got[g..g + 6], &want[w..w + 6]);
    // exec cells read `$ cmd`, the output and `✓ • dur`
    let i = got
        .iter()
        .position(|r| r == "$ cat src/lib.rs")
        .expect("cat");
    assert_eq!(got[i + 1], "pub fn add(a: i32, b: i32) -> i32 {");
    assert!(
        got[i + 4].starts_with("✓ • ") && got[i + 4].ends_with("ms"),
        "{:?}",
        got[i + 4]
    );
    let j = got
        .iter()
        .position(|r| r.starts_with("$ sh -c"))
        .expect("sh");
    assert!(got[j + 4].starts_with("✗ (101) • "), "{:?}", got[j + 4]);
}
