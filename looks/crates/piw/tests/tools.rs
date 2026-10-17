//! Tool blocks against what the real Pi 1.0.3 drew (`reference/pi/120x36/39-tool-*`, `42`..`58`).
//! Colours are the `dark` theme's final hex from the captures.

use std::time::{Duration, Instant};

use agent_core::{FileDiff, ToolCall, ToolKind, ToolStatus};
use piw::theme::PiTheme;
use piw::ui::tools::{render, render_user_bash, ToolCx, ToolTime, UserBash};
use piw::ui::{Cx, Lines};
use ratatui::style::{Color, Modifier};
use serde_json::json;

fn cx(width: u16, expanded: bool) -> Cx {
    Cx {
        theme: PiTheme::dark(),
        width,
        expanded,
        hide_thinking: false,
        out_pad: 1,
        cwd: "/nonexistent-piw-cwd".into(),
        home: "/home/me".into(),
        clock: Duration::ZERO,
        version: "1.0.3".into(),
    }
}

fn call(
    name: &str,
    kind: ToolKind,
    input: serde_json::Value,
    status: ToolStatus,
    out: Option<&str>,
) -> ToolCall {
    ToolCall {
        id: format!("t-{name}-{status:?}"),
        name: name.into(),
        kind,
        title: String::new(),
        input,
        status,
        output: out.map(Into::into),
        diff: None,
        parent_id: None,
    }
}

fn rows(c: &ToolCall, cx: &Cx) -> Lines {
    render(c, &ToolCx::new(cx, ToolTime::default()))
}

fn text(l: &ratatui::text::Line) -> String {
    l.spans.iter().map(|s| s.content.as_ref()).collect()
}

fn plain(l: &Lines) -> Vec<String> {
    l.iter().map(|r| text(r).trim_end().to_string()).collect()
}

fn rgb(h: u32) -> Color {
    Color::Rgb((h >> 16) as u8, (h >> 8) as u8, h as u8)
}

const SUCCESS: u32 = 0x254131;
const ERROR_BG: u32 = 0x5b282a;
const PENDING: u32 = 0x34383a;

fn all_bg(l: &Lines, bg: u32) -> bool {
    l.iter()
        .skip(1)
        .all(|r| r.spans.iter().all(|s| s.style.bg == Some(rgb(bg))))
}

#[test]
fn bash_block_matches_39_tool_bash() {
    let cx = cx(120, false);
    let c = call(
        "execute",
        ToolKind::Execute,
        json!({"command": "echo hello; ls src", "timeout": 20}),
        ToolStatus::Completed,
        Some("hello\nmain.rs"),
    );
    let r = rows(&c, &cx);
    assert_eq!(
        plain(&r),
        [
            "",
            "",
            " $ echo hello; ls src (timeout 20s)",
            "",
            " hello",
            " main.rs",
            "",
            " Took 0.0s",
            ""
        ]
    );
    assert!(all_bg(&r, SUCCESS));
    assert!(r.iter().skip(1).all(|l| line_w(l) == 120));
    let title = &r[2].spans[1];
    assert!(title.style.add_modifier.contains(Modifier::BOLD));
    assert_eq!(title.style.fg, Some(rgb(0xdee0e1)));
    assert_eq!(r[2].spans[2].style.fg, Some(rgb(0x9da5a9))); // ` (timeout 20s)` muted
    assert_eq!(r[4].spans[1].style.fg, Some(rgb(0x9da5a9)));
}

fn line_w(l: &ratatui::text::Line) -> usize {
    tuikit::width::spans_width(&l.spans)
}

#[test]
fn failed_bash_is_the_error_background() {
    let cx = cx(120, false);
    let c = call(
        "execute",
        ToolKind::Execute,
        json!({"command": "echo partial; echo oops >&2; exit 3"}),
        ToolStatus::Failed,
        Some("partial\noops\n\n\nCommand exited with code 3"),
    );
    let r = rows(&c, &cx);
    assert!(all_bg(&r, ERROR_BG));
    let p = plain(&r);
    assert_eq!(p[4], " partial");
    assert_eq!(p[7], "");
    assert_eq!(p[8], " Command exited with code 3");
    assert_eq!(p[10], " Took 0.0s");
}

#[test]
fn running_bash_is_pending_with_elapsed() {
    let cx = cx(120, false);
    let c = call(
        "execute",
        ToolKind::Execute,
        json!({"command": "sleep 25"}),
        ToolStatus::Running,
        Some("start"),
    );
    let r = render(
        &c,
        &ToolCx::new(
            &cx,
            ToolTime {
                elapsed: Duration::from_secs(3),
                done: false,
            },
        ),
    );
    assert!(all_bg(&r, PENDING));
    assert!(plain(&r).contains(&" Elapsed 3.0s".to_string()));
}

#[test]
fn long_bash_output_keeps_the_last_five_visual_lines() {
    let out: String = (1..=30).map(|i| format!("line {i} of output\n")).collect();
    let c = call(
        "execute",
        ToolKind::Execute,
        json!({"command": "loop"}),
        ToolStatus::Completed,
        Some(out.trim_end()),
    );
    let p = plain(&rows(&c, &cx(120, false)));
    assert_eq!(p[4], " ... (25 earlier lines, ctrl+o to expand)");
    assert_eq!(p[5], " line 26 of output");
    assert_eq!(p[9], " line 30 of output");
    let r = rows(&c, &cx(120, false));
    // the key is dim, the rest of the hint muted
    let hint = &r[4];
    assert_eq!(hint.spans[3].style.fg, Some(rgb(0x7e888e)));
    assert_eq!(hint.spans[1].style.fg, Some(rgb(0x9da5a9)));
    let p = plain(&rows(&c, &cx(120, true)));
    assert_eq!(p.iter().filter(|l| l.starts_with(" line ")).count(), 30);
    assert!(!p.iter().any(|l| l.contains("earlier lines")));
}

#[test]
fn wrapped_lines_count_as_visual_lines() {
    let long = "word ".repeat(40);
    let out = format!("{}\n{}\n{}", long, long, long);
    let c = call(
        "execute",
        ToolKind::Execute,
        json!({"command": "x"}),
        ToolStatus::Completed,
        Some(&out),
    );
    let p = plain(&rows(&c, &cx(80, false)));
    assert!(p.iter().any(|l| l.contains("earlier lines")), "{p:#?}");
}

#[test]
fn read_collapsed_is_the_title_only() {
    let cx = cx(120, false);
    let c = call(
        "read_file",
        ToolKind::Read,
        json!({"path": "data/long.txt", "offset": 10, "limit": 5}),
        ToolStatus::Completed,
        Some("a\nb"),
    );
    let r = rows(&c, &cx);
    assert_eq!(plain(&r), ["", "", " read data/long.txt:10-14", ""]);
    let title = &r[2];
    assert!(title.spans[1].style.add_modifier.contains(Modifier::BOLD));
    assert_eq!(title.spans[3].style.fg, Some(rgb(0xa798d7)));
    assert_eq!(title.spans[4].style.fg, Some(rgb(0xcd9a22)));
    assert!(all_bg(&r, SUCCESS));
}

#[test]
fn read_expanded_shows_the_file_and_trims_trailing_blank_lines() {
    let c = call(
        "read_file",
        ToolKind::Read,
        json!({"path": "src/main.rs"}),
        ToolStatus::Completed,
        Some("fn main() {\n}\n\n\n"),
    );
    let p = plain(&rows(&c, &cx(120, true)));
    assert_eq!(
        p,
        ["", "", " read src/main.rs", "", " fn main() {", " }", ""]
    );
}

#[test]
fn read_error_shows_even_collapsed() {
    let c = call(
        "read_file",
        ToolKind::Read,
        json!({"path": "nope/missing.txt"}),
        ToolStatus::Failed,
        Some("ENOENT: no such file or directory"),
    );
    let r = rows(&c, &cx(120, false));
    assert!(all_bg(&r, ERROR_BG));
    assert_eq!(plain(&r)[4], " ENOENT: no such file or directory");
}

#[test]
fn write_shows_ten_lines_then_the_count() {
    let content: String = (1..=23).map(|i| format!("row {i}\n")).collect();
    let c = call(
        "write_file",
        ToolKind::Edit,
        json!({"path": "data/out.py", "content": content}),
        ToolStatus::Completed,
        Some("ok"),
    );
    let r = rows(&c, &cx(120, false));
    let p = plain(&r);
    assert_eq!(p[2], " write data/out.py");
    assert_eq!(p[4], " row 1");
    assert_eq!(p[13], " row 10");
    assert_eq!(p[14], " ... (13 more lines, 23 total, ctrl+o to expand)");
    assert_eq!(r[14].spans[3].style.fg, Some(rgb(0x7e888e)));
    let p = plain(&rows(&c, &cx(120, true)));
    assert_eq!(p.iter().filter(|l| l.starts_with(" row ")).count(), 23);
}

fn temp_project(file: &str, body: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "piw-tools-{}-{}",
        std::process::id(),
        file.replace('/', "_")
    ));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join(file), body).unwrap();
    dir
}

#[test]
fn edit_numbers_the_hunk_from_the_file_and_marks_changed_words() {
    let dir = temp_project("main.rs", "fn main() {\n    println!(\"hi\");\n}\n");
    let mut cx = cx(120, false);
    cx.cwd = dir.display().to_string();
    let mut c = call(
        "edit_file",
        ToolKind::Edit,
        json!({"path": "main.rs"}),
        ToolStatus::Completed,
        Some("ok"),
    );
    c.id = "edit-1".into();
    c.diff = Some(FileDiff {
        path: "main.rs".into(),
        old: Some("println!(\"hi\");".into()),
        new: "println!(\"hello, world\");".into(),
    });
    let r = rows(&c, &cx);
    let p = plain(&r);
    assert_eq!(
        p[2..=7],
        [
            " edit main.rs",
            "",
            "  1 fn main() {",
            " -2     println!(\"hi\");",
            " +2     println!(\"hello, world\");",
            "  3 }"
        ]
    );
    assert!(all_bg(&r, SUCCESS));
    // removed line: red, the word `hi` inverse
    let rem = &r[5];
    assert_eq!(rem.spans[1].style.fg, Some(rgb(0xea7f81)));
    let inv: Vec<&str> = rem
        .spans
        .iter()
        .filter(|s| s.style.add_modifier.contains(Modifier::REVERSED))
        .map(|s| s.content.as_ref())
        .collect();
    assert_eq!(inv, ["hi"]);
    let add = &r[6];
    assert_eq!(add.spans[1].style.fg, Some(rgb(0x68b78d)));
    assert_eq!(r[4].spans[1].style.fg, Some(rgb(0x9da5a9)));
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn edit_without_a_file_keeps_a_blank_gutter() {
    let mut c = call(
        "edit_file",
        ToolKind::Edit,
        json!({"path": "gone.rs"}),
        ToolStatus::Completed,
        None,
    );
    c.id = "edit-2".into();
    c.diff = Some(FileDiff {
        path: "gone.rs".into(),
        old: Some("a".into()),
        new: "b".into(),
    });
    let p = plain(&rows(&c, &cx(120, false)));
    assert_eq!(p[4], " -  a");
    assert_eq!(p[5], " +  b");
}

#[test]
fn failed_edit_shows_the_error_text() {
    let c = call(
        "edit_file",
        ToolKind::Edit,
        json!({"path": "src/main.rs"}),
        ToolStatus::Failed,
        Some("Could not find the exact text in src/main.rs."),
    );
    let r = rows(&c, &cx(120, false));
    assert!(all_bg(&r, ERROR_BG));
    assert_eq!(r[4].spans[1].style.fg, Some(rgb(0xea7f81)));
}

#[test]
fn grep_find_and_ls_titles_and_limit_warning() {
    let c = call(
        "search_files",
        ToolKind::Search,
        json!({"pattern": "line 1", "path": "data", "glob": "*.txt", "limit": 5}),
        ToolStatus::Completed,
        Some("long.txt:1: line 1\n\n[5 matches limit reached. Use limit=10 for more, or refine pattern]"),
    );
    let r = rows(&c, &cx(120, false));
    let p = plain(&r);
    assert_eq!(p[2], " grep /line 1/ in data (*.txt) limit 5");
    assert_eq!(r[2].spans[3].style.fg, Some(rgb(0xa798d7)));
    assert_eq!(p[p.len() - 2], " [Truncated: 5 matches limit]");
    let w = &r[r.len() - 2];
    assert_eq!(w.spans[1].style.fg, Some(rgb(0xcd9a22)));

    let f = call(
        "find",
        ToolKind::Search,
        json!({"pattern": "*.rs", "path": "."}),
        ToolStatus::Completed,
        Some("src/main.rs"),
    );
    assert_eq!(plain(&rows(&f, &cx(120, false)))[2], " find *.rs in .");
    let l = call(
        "list_files",
        ToolKind::Read,
        json!({"path": "data"}),
        ToolStatus::Completed,
        Some("long.txt"),
    );
    assert_eq!(plain(&rows(&l, &cx(120, false)))[2], " ls data");
    let many: String = (1..=25).map(|i| format!("f{i}\n")).collect();
    let l = call(
        "list_files",
        ToolKind::Read,
        json!({}),
        ToolStatus::Completed,
        Some(many.trim_end()),
    );
    let p = plain(&rows(&l, &cx(120, false)));
    assert_eq!(p[2], " ls .");
    assert!(p.contains(&" ... (5 more lines, ctrl+o to expand)".to_string()));
}

#[test]
fn generic_tool_shows_key_value_pairs() {
    let c = call(
        "git_status",
        ToolKind::Read,
        json!({"short": true}),
        ToolStatus::Completed,
        Some("## main"),
    );
    let p = plain(&rows(&c, &cx(120, false)));
    assert_eq!(p[2], " git_status short=true");
    assert_eq!(p[3], " ## main");
    let p = plain(&rows(&c, &cx(120, true)));
    assert_eq!(p[2], " git_status");
    assert_eq!(p[3], "   short: true");
}

#[test]
fn aborted_pending_call_turns_into_an_error_block() {
    let cx = cx(120, false);
    let c = call(
        "execute",
        ToolKind::Execute,
        json!({"command": "sleep 25"}),
        ToolStatus::Running,
        None,
    );
    let mut t = ToolCx::new(&cx, ToolTime::default());
    t.abort_message = Some("Operation aborted".into());
    let r = render(&c, &t);
    assert!(all_bg(&r, ERROR_BG));
    assert!(plain(&r).contains(&" Operation aborted".to_string()));
}

#[test]
fn subagent_calls_are_indented_two_columns() {
    let mut c = call(
        "read_file",
        ToolKind::Read,
        json!({"path": "a.rs"}),
        ToolStatus::Completed,
        None,
    );
    c.parent_id = Some("parent".into());
    let r = rows(&c, &cx(120, false));
    assert_eq!(plain(&r)[2], "   read a.rs");
}

fn ubash(cmd: &str, out: &str, exclude: bool) -> UserBash {
    UserBash {
        cmd: cmd.into(),
        exclude,
        output: out.into(),
        running: false,
        exit: Some(0),
        cancelled: false,
        started: Instant::now(),
        took: Some(Duration::ZERO),
    }
}

#[test]
fn user_bash_block_matches_55_and_56() {
    let cx = cx(120, false);
    let b = ubash("git status --short", "?? AGENTS.md\n", false);
    let r = render_user_bash(&b, &ToolCx::new(&cx, ToolTime::default()));
    let p = plain(&r);
    assert_eq!(p[0], "");
    assert!(p[1].starts_with("────"));
    assert_eq!(p[2], " $ git status --short");
    assert_eq!(p[3], "");
    assert_eq!(p[4], " ?? AGENTS.md");
    assert_eq!(p[5], "");
    assert!(p[6].starts_with("────"));
    assert_eq!(r[1].spans[0].style.fg, Some(rgb(0x5eb286)));
    assert_eq!(r[2].spans[1].style.fg, Some(rgb(0x5eb286)));
    assert!(r[2].spans[1].style.add_modifier.contains(Modifier::BOLD));
    assert_eq!(r[4].spans[1].style.fg, Some(rgb(0x9da5a9)));
    // `!!` draws dim rules but keeps the green command
    let b = ubash("echo secret", "secret\n", true);
    let r = render_user_bash(&b, &ToolCx::new(&cx, ToolTime::default()));
    assert_eq!(r[1].spans[0].style.fg, Some(rgb(0x7e888e)));
    assert_eq!(r[2].spans[1].style.fg, Some(rgb(0x5eb286)));
}

#[test]
fn long_user_bash_output_folds_to_twenty_visual_rows() {
    let out: String = (1..=30).map(|i| format!("l{i}\n")).collect();
    let b = ubash("seq", &out, false);
    let c = cx(120, false);
    let p = plain(&render_user_bash(&b, &ToolCx::new(&c, ToolTime::default())));
    // 31 logical lines (the last is empty), twenty kept, no blank row after the command
    assert_eq!(p[3], " l12");
    assert_eq!(p[p.len() - 5], " l30");
    assert_eq!(p[p.len() - 4], "");
    assert_eq!(p[p.len() - 3], "");
    assert_eq!(p[p.len() - 2], " ... 11 more lines (ctrl+o to expand)");
    let c = cx(120, true);
    let p = plain(&render_user_bash(&b, &ToolCx::new(&c, ToolTime::default())));
    assert_eq!(p[4], " l1");
    assert_eq!(p[p.len() - 2], " (ctrl+o to collapse)");
}

#[test]
fn user_bash_failure_and_cancel_markers() {
    let c = cx(120, false);
    let mut b = ubash("false", "", false);
    b.exit = Some(3);
    let p = plain(&render_user_bash(&b, &ToolCx::new(&c, ToolTime::default())));
    assert_eq!(p[p.len() - 2], " (exit 3)");
    b.exit = None;
    b.cancelled = true;
    let p = plain(&render_user_bash(&b, &ToolCx::new(&c, ToolTime::default())));
    assert_eq!(p[p.len() - 2], " (cancelled)");
}

#[test]
fn blocks_fit_80_columns() {
    let c = call(
        "execute",
        ToolKind::Execute,
        json!({"command": "echo a-very-long-command-line-that-has-to-wrap-around-the-edge-of-the-narrow-terminal-window-once"}),
        ToolStatus::Completed,
        Some("ok"),
    );
    let r = rows(&c, &cx(80, false));
    assert!(r.iter().skip(1).all(|l| line_w(l) == 80));
    assert!(plain(&r)[3].starts_with(' '));
}
