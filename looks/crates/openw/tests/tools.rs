//! Tool renderers. Parity tests rebuild states captured from real opencode 1.18.34 (the files
//! in `reference/tools/`, Big Pickle driving the tools) and compare rows; snapshot tests cover
//! what wizard sends and the states opencode only reaches for an instant. Update snapshots
//! with `UPDATE_SNAPSHOTS=1 cargo test -p openw --test tools` and read the diff.

mod common;

use agent_core::mock::gallery_calls;
use agent_core::{Event, FileDiff, StopReason, ToolCall, ToolKind, ToolStatus};
use common::*;
use serde_json::json;

const CAPTURE_CWD: &str =
    "/tmp/claude-1000/-home-nixos/46dc526d-4fc6-4f47-b09e-820c7c4f2577/scratchpad/ocproj-a/proj";

fn ref_rows(name: &str) -> Vec<String> {
    let p = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../reference/tools")
        .join(format!("{name}.txt"));
    std::fs::read_to_string(p)
        .expect("reference")
        .lines()
        .map(|l| l.trim_end().to_string())
        .collect()
}

/// A turn that holds exactly `calls`, on a screen tall enough to show all of it.
fn session(w: u16, h: u16, cwd: &str, calls: Vec<ToolCall>) -> Harness {
    let mut hn = Harness::new(w, h);
    hn.app.cwd = cwd.into();
    hn.app.send_prompt("go".into());
    hn.event(Event::TurnStart);
    for c in calls {
        hn.event(Event::Tool(c));
    }
    hn.event(Event::TurnEnd(StopReason::EndTurn));
    hn.fix_durations();
    hn
}

fn rows(h: &mut Harness, cols: usize) -> Vec<String> {
    h.text()
        .lines()
        .map(|l| {
            l.chars()
                .take(cols)
                .collect::<String>()
                .trim_end()
                .to_string()
        })
        .collect()
}

/// Rows from the first one containing `from` up to (not including) the first after it that
/// contains `to`.
fn between(all: &[String], from: &str, to: &str) -> Vec<String> {
    let a = all
        .iter()
        .position(|l| l.contains(from))
        .unwrap_or_else(|| panic!("no row with {from:?}"));
    let b = a + all[a..]
        .iter()
        .position(|l| l.contains(to))
        .unwrap_or_else(|| panic!("no row with {to:?}"));
    all[a..b].to_vec()
}

fn call(
    id: &str,
    name: &str,
    kind: ToolKind,
    title: &str,
    input: serde_json::Value,
    status: ToolStatus,
    output: Option<&str>,
) -> ToolCall {
    ToolCall {
        status,
        ..tool(id, name, kind, title, input, output)
    }
}

fn abs(p: &str) -> String {
    format!("{CAPTURE_CWD}/{p}")
}

const MAIN_RS: &str =
    "fn main() {\n    println!(\"hi\");\n    let x = 1;\n    println!(\"{}\", x);\n}\n";

#[test]
#[cfg_attr(
    not(reference_captures),
    ignore = "needs the reference/ captures, not in this repository; see build.rs"
)]
fn matches_opencode_mixed_run_at_170() {
    let seq: String = (1..=30).map(|i| format!("{i}\n")).collect();
    let step1 =
        "fn main() {\n    println!(\"hello\");\n    let x = 1;\n    println!(\"{}\", x);\n}\n";
    let step2 =
        "fn main() {\n    println!(\"hello\");\n    let x = 2;\n    println!(\"{}\", x);\n}\n";
    let mut e1 = tool(
        "5",
        "edit",
        ToolKind::Edit,
        "src/main.rs",
        json!({"filePath": abs("src/main.rs"), "oldString": "\"hi\"", "newString": "\"hello\""}),
        Some("Edit applied successfully."),
    );
    e1.diff = Some(FileDiff {
        path: abs("src/main.rs"),
        old: Some(MAIN_RS.into()),
        new: step1.into(),
    });
    let mut e2 = tool(
        "6",
        "edit",
        ToolKind::Edit,
        "src/main.rs",
        json!({"filePath": abs("src/main.rs"), "oldString": "let x = 1;", "newString": "let x = 2;"}),
        Some("Edit applied successfully."),
    );
    e2.diff = Some(FileDiff {
        path: abs("src/main.rs"),
        old: Some(step1.into()),
        new: step2.into(),
    });
    let mut w = tool(
        "7",
        "write",
        ToolKind::Edit,
        "notes.txt",
        json!({"filePath": abs("notes.txt"), "content": "Line one\nLine two\nLine three"}),
        Some("Wrote file successfully."),
    );
    w.diff = Some(FileDiff {
        path: abs("notes.txt"),
        old: None,
        new: "Line one\nLine two\nLine three".into(),
    });
    let calls = vec![
        tool(
            "1",
            "bash",
            ToolKind::Execute,
            "seq 1 30",
            json!({"command": "seq 1 30"}),
            Some(&seq),
        ),
        tool(
            "2",
            "read",
            ToolKind::Read,
            "README.md",
            json!({"filePath": "README.md", "offset": 2, "limit": 2}),
            Some("<content>"),
        ),
        tool(
            "3",
            "glob",
            ToolKind::Search,
            "",
            json!({"pattern": "**/*.rs"}),
            Some("/p/src/main.rs\n/p/src/lib.rs"),
        ),
        tool(
            "4",
            "grep",
            ToolKind::Search,
            "println",
            json!({"pattern": "println", "path": "src"}),
            Some("/p/src/main.rs:2: x\n/p/src/main.rs:4: x"),
        ),
        e1,
        e2,
        w,
        tool(
            "8",
            "webfetch",
            ToolKind::Fetch,
            "https://example.com",
            json!({"url": "https://example.com"}),
            Some("Example Domain"),
        ),
        tool(
            "9",
            "bash",
            ToolKind::Execute,
            "ls /nonexistent_dir",
            json!({"command": "ls /nonexistent_dir"}),
            Some("ls: cannot access '/nonexistent_dir': No such file or directory\n"),
        ),
    ];
    let mut h = session(170, 70, CAPTURE_CWD, calls);
    h.dump("tools-170x70-mixed");
    let got = rows(&mut h, 128);
    let want: Vec<String> = ref_rows("tools-170x70-mixed")
        .into_iter()
        .map(|l| {
            l.chars()
                .take(128)
                .collect::<String>()
                .trim_end()
                .to_string()
        })
        .collect();
    // The glob row in the capture says `(2 matches)` from metadata; ours counts the output lines,
    // and the read and grep rows are inline like it. Compare from the first inline row down to
    // the shell that failed, minus the model's "Thought" row.
    let mut want = between(&want, "→ Read README.md", "% WebFetch");
    want.retain(|l| !l.contains("Thought"));
    want.dedup_by(|a, b| a.is_empty() && b.is_empty());
    let mut got_s = between(&got, "→ Read README.md", "% WebFetch");
    got_s.retain(|l| !l.contains("Thought"));
    got_s.dedup_by(|a, b| a.is_empty() && b.is_empty());
    // "→ Read src/main.rs" is a second read the model made; drop it from the reference.
    want.retain(|l| !l.contains("→ Read src/main.rs"));
    want.dedup_by(|a, b| a.is_empty() && b.is_empty());
    assert_eq!(got_s.join("\n"), want.join("\n"));
}

#[test]
#[cfg_attr(
    not(reference_captures),
    ignore = "needs the reference/ captures, not in this repository; see build.rs"
)]
fn failed_rows_are_red_and_wrap_like_opencode() {
    let mut read = call(
        "1",
        "read",
        ToolKind::Read,
        "nonexistent.txt",
        json!({"filePath": "nonexistent.txt"}),
        ToolStatus::Failed,
        Some("File not found: /p/nonexistent.txt"),
    );
    read.input = json!({"filePath": "nonexistent.txt"});
    let long = "https://example.com/some-long-path-name.with.dots/and-dashes-here/and_underscores_here/more-words-more-words-more-words-more-words-more-words/x?query=1&other=2";
    let fetch = tool(
        "2",
        "webfetch",
        ToolKind::Fetch,
        long,
        json!({"url": long}),
        Some("x"),
    );
    let mut h = session(120, 40, "/p", vec![read, fetch]);
    h.dump("failed");
    let got = rows(&mut h, 120);
    let want = ref_rows("tools-120x60-parallel");
    assert!(got.contains(&"     → Read nonexistent.txt".to_string()));
    let a = between(&want, "% WebFetch", "→ Asked");
    let b = between(&got, "% WebFetch", "▣");
    let b: Vec<String> = b.into_iter().filter(|l| !l.is_empty()).collect();
    let a: Vec<String> = a.into_iter().filter(|l| !l.is_empty()).collect();
    assert_eq!(b, a);
    // Colour: the whole failed row is `error`.
    let ansi = h.render().ansi();
    let row = ansi
        .lines()
        .find(|l| l.contains("nonexistent.txt"))
        .unwrap();
    assert!(
        row.contains("38;2;224;108;117m→ Read nonexistent.txt"),
        "{row:?}"
    );
}

#[test]
#[cfg_attr(
    not(reference_captures),
    ignore = "needs the reference/ captures, not in this repository; see build.rs"
)]
fn pending_rows_start_at_column_5() {
    let mut pending = call(
        "1",
        "write_file",
        ToolKind::Edit,
        "",
        json!({}),
        ToolStatus::Pending,
        None,
    );
    pending.input = serde_json::Value::Null;
    let mut h = session(120, 40, "/p", vec![pending]);
    h.dump("pending");
    let got = rows(&mut h, 120);
    let want = ref_rows("tools-120x40-pending-write");
    let a = want.iter().find(|l| l.contains("Preparing write")).unwrap();
    assert!(got.contains(a), "{a:?}");
}

#[test]
#[cfg_attr(
    not(reference_captures),
    ignore = "needs the reference/ captures, not in this repository; see build.rs"
)]
fn write_gutter_widens_with_the_line_count() {
    let nums: String = (1..=105).map(|i| format!("{i}\n")).collect();
    let mut w = tool(
        "1",
        "write",
        ToolKind::Edit,
        "nums.txt",
        json!({"filePath": "nums.txt", "content": nums}),
        Some("Wrote file successfully."),
    );
    w.diff = Some(FileDiff {
        path: "nums.txt".into(),
        old: None,
        new: nums.clone(),
    });
    let mut h = session(120, 130, "/p", vec![w]);
    h.dump("write");
    let got = rows(&mut h, 120);
    let want = ref_rows("tools-120x110-diff-long");
    for needle in ["┃    82 82", "┃   105 105"] {
        let g = got
            .iter()
            .find(|l| l.contains(needle))
            .unwrap_or_else(|| panic!("{needle}"));
        let r = want.iter().find(|l| l.contains(needle)).unwrap();
        assert_eq!(g, r);
    }
    let first = got
        .iter()
        .find(|l| l.contains("┃     1 1"))
        .expect("1-digit numbers use the 3-digit gutter too");
    assert!(first.ends_with("1 1"));
}

#[test]
#[cfg_attr(
    not(reference_captures),
    ignore = "needs the reference/ captures, not in this repository; see build.rs"
)]
fn edit_hunks_use_four_lines_of_context() {
    let old: String = (1..=60).map(|i| format!("{i}\n")).collect();
    let new: String = (1..=60)
        .map(|i| {
            if i % 10 == 7 {
                format!(
                    "{}seven\n",
                    if i == 7 {
                        String::new()
                    } else {
                        (i / 10).to_string()
                    }
                )
            } else {
                format!("{i}\n")
            }
        })
        .collect();
    let mut e = tool(
        "1",
        "edit",
        ToolKind::Edit,
        "f.txt",
        json!({"filePath": "f.txt", "replaceAll": true}),
        Some("ok"),
    );
    e.diff = Some(FileDiff {
        path: "f.txt".into(),
        old: Some(old),
        new,
    });
    let mut h = session(120, 120, "/p", vec![e]);
    h.dump("edit");
    let got = rows(&mut h, 120);
    let want = ref_rows("tools-120x110-diff-long");
    let a = between(&want, "┃     3   3", "✱ Grep");
    let b = between(&got, "┃     3   3", "▣");
    // The capture is cut at the top of the screen and has no hunk after line 57; compare the
    // lines both have.
    let n = a.len().min(b.len()) - 3;
    assert_eq!(&a[..n], &b[..n]);
}

#[test]
#[cfg_attr(
    not(reference_captures),
    ignore = "needs the reference/ captures, not in this repository; see build.rs"
)]
fn task_rows_and_the_subagent_hint() {
    let parent = call(
        "t",
        "task",
        ToolKind::Other,
        "",
        json!({"description": "Count README lines", "subagent_type": "general"}),
        ToolStatus::Completed,
        Some("5"),
    );
    let mut kid = call(
        "k",
        "read",
        ToolKind::Read,
        "README.md",
        json!({"filePath": "README.md"}),
        ToolStatus::Completed,
        Some("x"),
    );
    kid.parent_id = Some("t".into());
    let mut h = session(120, 40, "/p", vec![parent, kid]);
    h.dump("task");
    let got = rows(&mut h, 120);
    let want = ref_rows("tools-120x100-failed-task-todo");
    let a = between(&want, "✓ General Task", "┃  # Todos");
    let b = between(&got, "✓ General Task", "▣");
    // The capture had a 2.8s duration; ours has none because nothing timed the call.
    assert_eq!(a[0], b[0]);
    assert_eq!(a[1], "       ↳ 1 toolcall · 2.8s");
    assert_eq!(b[1], "       ↳ 1 toolcall");
    assert_eq!(a[3], b[3]);
    assert_eq!(a[2], b[2]);
}

#[test]
#[cfg_attr(
    not(reference_captures),
    ignore = "needs the reference/ captures, not in this repository; see build.rs"
)]
fn loaded_instruction_files_are_listed_under_a_read() {
    let out = "<content>\n1: hi\n</content>\n\n<system-reminder>\nInstructions from: /p/sub/AGENTS.md\nBe brief.\n</system-reminder>";
    let r = tool(
        "1",
        "read",
        ToolKind::Read,
        "sub/a.txt",
        json!({"filePath": "sub/a.txt"}),
        Some(out),
    );
    let mut h = session(120, 40, "/p", vec![r]);
    h.dump("loaded");
    let got = rows(&mut h, 120);
    let want = ref_rows("tools-120x40-loaded");
    assert_eq!(
        between(&got, "→ Read", "▣"),
        between(&want, "→ Read", "Done.")
    );
}

// ---- wizard --------------------------------------------------------------------------------

#[test]
fn wizard_tools_120() {
    let mut h = session(120, 150, "/home/me/proj", gallery_calls());
    assert_snapshot("tools-gallery-120", &h.text());
}

#[test]
fn wizard_tools_170_split_diff() {
    let mut h = session(170, 150, "/home/me/proj", gallery_calls());
    assert_snapshot("tools-gallery-170", &h.text());
}

#[test]
fn in_flight_states() {
    let calls = vec![
        call(
            "a",
            "execute",
            ToolKind::Execute,
            "cargo test",
            json!({"command": "cargo test"}),
            ToolStatus::Pending,
            None,
        ),
        call(
            "b",
            "execute",
            ToolKind::Execute,
            "cargo test",
            json!({"command": "cargo test"}),
            ToolStatus::Running,
            None,
        ),
        call(
            "c",
            "execute",
            ToolKind::Execute,
            "make",
            json!({"command": "make"}),
            ToolStatus::Running,
            Some("cc a.c\ncc b.c"),
        ),
        call(
            "d",
            "read_file",
            ToolKind::Read,
            "src/lib.rs",
            json!({"path": "src/lib.rs"}),
            ToolStatus::Running,
            None,
        ),
        call(
            "e",
            "edit_file",
            ToolKind::Edit,
            "a.rs",
            json!({"path": "a.rs", "old_string": "x", "new_string": "y", "replace_all": true}),
            ToolStatus::Running,
            None,
        ),
        call(
            "f",
            "search_files",
            ToolKind::Search,
            "foo",
            json!({"pattern": "foo"}),
            ToolStatus::Running,
            None,
        ),
        call(
            "g",
            "web_fetch",
            ToolKind::Fetch,
            "",
            json!({}),
            ToolStatus::Pending,
            None,
        ),
        call(
            "h",
            "web_search",
            ToolKind::Search,
            "rust",
            json!({"query": "rust"}),
            ToolStatus::Running,
            None,
        ),
        call(
            "i",
            "todo",
            ToolKind::Other,
            "todo",
            json!({"action": "write"}),
            ToolStatus::Running,
            None,
        ),
        call(
            "j",
            "spawn_subagent",
            ToolKind::Other,
            "spawn_subagent",
            json!({"subagent": "explorer", "task": "Find the parser", "background": true}),
            ToolStatus::Running,
            None,
        ),
        call(
            "k",
            "memory",
            ToolKind::Other,
            "memory",
            json!({"action": "list"}),
            ToolStatus::Running,
            None,
        ),
    ];
    let mut h = session(120, 80, "/p", calls);
    assert_snapshot("tools-in-flight-120", &h.text());
}

#[test]
fn failures_and_denials() {
    let calls = vec![
        call("a", "execute", ToolKind::Execute, "x", json!({"command": "/nonexistentcmd hi"}), ToolStatus::Failed, Some("unknown command '/nonexistentcmd' — try /help")),
        call("b", "execute", ToolKind::Execute, "x", json!({"command": "rm -rf /"}), ToolStatus::Failed, Some("The user denied this tool call.")),
        call("c", "edit_file", ToolKind::Edit, "a.rs", json!({"path": "a.rs", "old_string": "x", "new_string": "y"}), ToolStatus::Failed, Some("old_string not found in a.rs")),
        call("d", "write_file", ToolKind::Edit, "b.rs", json!({"path": "b.rs", "content": "x"}), ToolStatus::Failed, Some("The user denied this tool call.")),
        call("e", "web_fetch", ToolKind::Fetch, "u", json!({"url": "https://example.com/404"}), ToolStatus::Failed, Some("HTTP 404 fetching https://example.com/404, a long message that has to wrap onto a second row when it is opened")),
        call("f", "memory", ToolKind::Other, "memory", json!({"action": "save"}), ToolStatus::Failed, Some("missing name")),
        call("g", "spawn_subagent", ToolKind::Other, "s", json!({"subagent": "worker", "task": "do it"}), ToolStatus::Failed, Some("subagent failed: boom")),
        call("h", "todo", ToolKind::Other, "todo", json!({"action": "write"}), ToolStatus::Failed, Some("bad")),
    ];
    let mut h = session(120, 80, "/p", calls);
    // Click the failed fetch open.
    h.app.view.expanded.insert("e".into());
    assert_snapshot("tools-failures-120", &h.text());
}

#[test]
fn output_edge_cases() {
    let many: String = (1..=40)
        .map(|i| format!("line {i}"))
        .collect::<Vec<_>>()
        .join("\n");
    let wide = "x".repeat(500);
    let calls = vec![
        call(
            "a",
            "execute",
            ToolKind::Execute,
            "c",
            json!({"command": "cat big"}),
            ToolStatus::Completed,
            Some(&many),
        ),
        call(
            "b",
            "execute",
            ToolKind::Execute,
            "c",
            json!({"command": "cat wide"}),
            ToolStatus::Completed,
            Some(&wide),
        ),
        call(
            "c",
            "execute",
            ToolKind::Execute,
            "c",
            json!({"command": "ls --color"}),
            ToolStatus::Completed,
            Some("\x1b[1;34msrc\x1b[0m  \x1b[32mrun.sh\x1b[0m\r\n"),
        ),
        call(
            "d",
            "execute",
            ToolKind::Execute,
            "c",
            json!({"command": "true"}),
            ToolStatus::Completed,
            Some(""),
        ),
        call(
            "e",
            "execute",
            ToolKind::Execute,
            "c",
            json!({"command": "cat bin"}),
            ToolStatus::Completed,
            Some("PK\u{3}\u{4}\0\0\u{8}\0binary\u{7f}\u{1b}[2J tail"),
        ),
        call(
            "f",
            "execute",
            ToolKind::Execute,
            "c",
            json!({"command": "echo $LONG", "workdir": "/p/sub"}),
            ToolStatus::Completed,
            Some("ok"),
        ),
    ];
    let mut h = session(120, 140, "/p", calls);
    h.app.view.expanded.insert("b".into());
    assert_snapshot("tools-output-120", &h.text());
}

#[test]
fn expanding_a_collapsed_shell_shows_everything() {
    let many: String = (1..=40)
        .map(|i| format!("line {i}"))
        .collect::<Vec<_>>()
        .join("\n");
    let c = call(
        "a",
        "execute",
        ToolKind::Execute,
        "c",
        json!({"command": "cat big"}),
        ToolStatus::Completed,
        Some(&many),
    );
    let mut h = session(120, 70, "/p", vec![c]);
    let before = h.text();
    assert!(before.contains("Click to expand") && !before.contains("line 11"));
    h.app.view.toggle("a");
    let after = h.text();
    assert!(after.contains("Click to collapse") && after.contains("line 40"));
}

#[test]
fn tool_details_toggle_hides_only_completed_calls() {
    let calls = vec![
        call(
            "a",
            "read_file",
            ToolKind::Read,
            "a.rs",
            json!({"path": "a.rs"}),
            ToolStatus::Completed,
            Some("x"),
        ),
        call(
            "b",
            "read_file",
            ToolKind::Read,
            "b.rs",
            json!({"path": "b.rs"}),
            ToolStatus::Running,
            None,
        ),
        call(
            "c",
            "read_file",
            ToolKind::Read,
            "c.rs",
            json!({"path": "c.rs"}),
            ToolStatus::Failed,
            Some("nope"),
        ),
    ];
    let mut h = session(120, 40, "/p", calls);
    h.app.flags.tool_details = false;
    let t = h.text();
    assert!(!t.contains("Read a.rs") && t.contains("Read b.rs") && t.contains("Read c.rs"));
}

#[test]
fn generic_output_block_when_asked_for() {
    let c = call(
        "a",
        "github__create_issue",
        ToolKind::Other,
        "x",
        json!({"title": "t"}),
        ToolStatus::Completed,
        Some("one\ntwo\nthree\nfour\nfive"),
    );
    let mut h = session(120, 40, "/p", vec![c]);
    h.app.flags.generic_output = true;
    assert_snapshot("tools-generic-output-120", &h.text());
}

#[test]
fn edit_expands_to_the_whole_file_from_disk() {
    let dir = std::env::temp_dir().join(format!("openw-tools-{}", std::process::id()));
    std::fs::create_dir_all(dir.join("src")).unwrap();
    let body: String = (1..=20)
        .map(|i| {
            if i == 10 {
                "    println!(\"hello\");\n".to_string()
            } else {
                format!("line {i}\n")
            }
        })
        .collect();
    std::fs::write(dir.join("src/a.rs"), &body).unwrap();
    let mut e = tool(
        "1",
        "edit_file",
        ToolKind::Edit,
        "src/a.rs",
        json!({"path": "src/a.rs", "old_string": "    println!(\"hi\");", "new_string": "    println!(\"hello\");"}),
        Some("Edited src/a.rs: replaced 1 occurrence (line 10)"),
    );
    e.diff = Some(FileDiff {
        path: "src/a.rs".into(),
        old: Some("    println!(\"hi\");".into()),
        new: "    println!(\"hello\");".into(),
    });
    let mut h = session(120, 40, dir.to_str().unwrap(), vec![e]);
    let t = h.text();
    let _ = std::fs::remove_dir_all(&dir);
    for want in [
        "  6   line 6",
        " 10 -     println!(\"hi\");",
        " 10 +     println!(\"hello\");",
        " 14   line 14",
    ] {
        assert!(t.contains(want), "{want:?} in\n{t}");
    }
    assert!(!t.contains("line 5") && !t.contains("line 15"));
}

#[test]
fn apply_patch_blocks_per_file() {
    let patch = "*** Begin Patch\n*** Add File: a.txt\n+one\n*** Update File: b.rs\n@@\n ctx\n-old\n+new\n*** Delete File: c.txt\n*** End Patch";
    let c = tool(
        "1",
        "apply_patch",
        ToolKind::Edit,
        "",
        json!({"patchText": patch}),
        Some("Success"),
    );
    let mut h = session(120, 60, "/p", vec![c]);
    assert_snapshot("tools-apply-patch-120", &h.text());
}

#[test]
fn question_asked_and_answered() {
    let q = json!({"questions": [{"question": "Which color?", "options": []}, {"question": "How many?"}]});
    let pending = call(
        "a",
        "question",
        ToolKind::Other,
        "",
        q.clone(),
        ToolStatus::Running,
        None,
    );
    let done = tool("b", "question", ToolKind::Other, "", q, Some("User has answered your questions: \"Which color?\"=\"red\", \"How many?\"=\"\". You can now continue with the user's answers in mind."));
    let mut h = session(120, 40, "/p", vec![pending, done]);
    assert_snapshot("tools-question-120", &h.text());
}

#[test]
#[cfg_attr(
    not(reference_captures),
    ignore = "needs the reference/ captures, not in this repository; see build.rs"
)]
fn running_states_match_opencode() {
    let long = "https://example.com/some-long-path-name.with.dots/and-dashes-here/and_underscores_here/more-words-more-words-more-words-more-words-more-words/x?query=1&other=2";
    let calls = vec![
        call(
            "1",
            "bash",
            ToolKind::Execute,
            "",
            json!({"command": "sleep 14 && echo finished-sleep"}),
            ToolStatus::Running,
            None,
        ),
        call(
            "2",
            "task",
            ToolKind::Other,
            "",
            json!({"description": "Sleep and count README lines", "subagent_type": "general"}),
            ToolStatus::Running,
            None,
        ),
        call(
            "3",
            "webfetch",
            ToolKind::Fetch,
            long,
            json!({"url": long}),
            ToolStatus::Running,
            None,
        ),
        call(
            "4",
            "question",
            ToolKind::Other,
            "",
            json!({"questions": [{"question": "Which color do you prefer?"}]}),
            ToolStatus::Running,
            None,
        ),
    ];
    let mut h = session(120, 40, "/p", calls);
    h.dump("running");
    let got: Vec<String> = rows(&mut h, 120)
        .into_iter()
        .map(|l| l.replace('⠋', "⠼"))
        .collect();
    let want = ref_rows("tools-120x60-running");
    let a = between(&want, "┃  ⠼ sleep 14", "▣");
    let b = between(&got, "┃  ⠼ sleep 14", "▣");
    let spin = |l: &String| l.replace(['⠼', '⠴'], "⠋");
    assert_eq!(
        a.iter().map(spin).collect::<Vec<_>>(),
        b.iter().map(spin).collect::<Vec<_>>()
    );
}

#[test]
fn clicking_a_failed_row_opens_its_error_and_a_shell_expands() {
    let many: String = (1..=40)
        .map(|i| format!("line {i}"))
        .collect::<Vec<_>>()
        .join("\n");
    let calls = vec![
        call(
            "f",
            "web_fetch",
            ToolKind::Fetch,
            "u",
            json!({"url": "https://example.com/404"}),
            ToolStatus::Failed,
            Some("HTTP 404 not found"),
        ),
        call(
            "s",
            "execute",
            ToolKind::Execute,
            "c",
            json!({"command": "cat big"}),
            ToolStatus::Completed,
            Some(&many),
        ),
    ];
    let mut h = session(120, 70, "/p", calls);
    let screen = h.text();
    assert!(!screen.contains("HTTP 404"));
    let y = screen.lines().position(|l| l.contains("WebFetch")).unwrap() as u16;
    assert!(h.app.view.click(y));
    assert!(h.text().contains("       HTTP 404 not found"));
    let y = h
        .text()
        .lines()
        .position(|l| l.contains("Click to expand"))
        .unwrap() as u16;
    assert!(h.app.view.click(y));
    assert!(h.text().contains("line 40"));
}

/// What `OPENW_BACKEND=mock` sends for a `tools` prompt: wizard's todo call goes out next to
/// `Event::Todos`, so the transcript gets opencode's `# Todos` block and not only the sidebar
/// list (fidelity round 1, finding 5), and the `write` block ends with its empty last line.
#[tokio::test]
async fn the_mock_tools_turn_draws_the_todos_block_and_the_write_tail() {
    use agent_core::mock::{MockBackend, MockOpts};
    use agent_core::Request;
    let mut be = MockBackend::spawn_with(
        "/home/me/proj".into(),
        None,
        MockOpts {
            todo_tool: true,
            ..Default::default()
        },
    )
    .expect("mock");
    be.tx.send(Request::Prompt("tools".into())).unwrap();
    let mut h = Harness::new(120, 64);
    h.app.send_prompt("tools".into());
    loop {
        let ev = tokio::time::timeout(std::time::Duration::from_secs(20), be.rx.recv())
            .await
            .expect("mock stalled")
            .expect("mock closed");
        let end = matches!(ev, Event::TurnEnd(_));
        h.event(ev);
        if end {
            break;
        }
    }
    h.fix_durations();
    let t = h.text();
    let rows: Vec<&str> = t.lines().map(|l| l.trim_end()).collect();
    let at = |needle: &str| rows.iter().position(|r| r.contains(needle)).expect(needle);
    let todos = at("# Todos");
    assert_eq!(rows[todos + 2], "  ┃  [✓] Read the code");
    assert_eq!(rows[todos + 3], "  ┃  [•] Fix greeting");
    assert_eq!(rows[todos + 4], "  ┃  [ ] Add tests");
    let wrote = at("# Wrote notes.txt");
    assert_eq!(rows[wrote + 2], "  ┃   1 todo: add tests");
    assert_eq!(rows[wrote + 3], "  ┃   2");
    assert!(wrote < todos);
}
