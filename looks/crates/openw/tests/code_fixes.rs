//! Regression tests for docs/critics/openw-code-r1.md, one per finding, named after it.

mod common;

use std::path::PathBuf;
use std::time::Duration;

use agent_core::{Event, Request, ToolStatus};
use crossterm::event::{KeyCode, KeyModifiers};
use openw::app::{App, AppOpts, Msg};
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver};

struct Live {
    app: App,
    requests: UnboundedReceiver<Request>,
    msgs: UnboundedReceiver<Msg>,
}

fn live() -> Live {
    let (tx, requests) = unbounded_channel();
    let (mtx, msgs) = unbounded_channel();
    let mut app = App::new(
        AppOpts {
            cwd: std::env::temp_dir(),
            mock: true,
            ..Default::default()
        },
        tx,
        mtx,
    );
    app.size = (100, 30);
    app.frozen = Some(Duration::ZERO);
    let mut cfg = agent_core::mock::config();
    cfg.cwd = std::env::temp_dir().to_string_lossy().into_owned();
    app.on_event(Event::Ready {
        session_id: "ses_1".into(),
        config: cfg,
    });
    Live {
        app,
        requests,
        msgs,
    }
}

impl Live {
    fn key(&mut self, c: KeyCode) {
        self.app
            .on_key(crossterm::event::KeyEvent::new(c, KeyModifiers::NONE));
    }
    fn type_str(&mut self, s: &str) {
        for c in s.chars() {
            self.key(KeyCode::Char(c));
        }
    }
}

/// Finding 4: Esc on a running `!command` killed nothing. The runner is the interaction fixer's;
/// this pins the finding's own reproduction (two Esc on `sleep`, a result within seconds).
#[tokio::test]
async fn escape_twice_kills_a_running_shell_command() {
    let mut l = live();
    l.type_str("!sleep 30");
    l.key(KeyCode::Enter);
    tokio::time::sleep(Duration::from_millis(150)).await;
    l.key(KeyCode::Esc);
    l.key(KeyCode::Esc);
    let end = std::time::Instant::now() + Duration::from_secs(8);
    loop {
        let left = end.saturating_duration_since(std::time::Instant::now());
        let msg = tokio::time::timeout(left, l.msgs.recv())
            .await
            .expect("sleep 30 was still running 8s after two Esc")
            .unwrap();
        let done = matches!(msg, Msg::Shell { .. });
        l.app.update(msg);
        if done {
            break;
        }
    }
    let call = l
        .app
        .transcript
        .messages
        .iter()
        .flat_map(|m| m.parts.iter())
        .find_map(|p| match p {
            agent_core::transcript::Part::Tool(c) => Some(c.clone()),
            _ => None,
        })
        .expect("a tool call");
    assert_eq!(call.status, ToolStatus::Failed);
    assert!(call.output.unwrap_or_default().ends_with("interrupted"));
    // the Esc was for the command, not a cancel for a turn that does not exist
    assert!(l.requests.try_recv().map_or(true, |r| r != Request::Cancel));
}

fn edit_call(i: usize) -> agent_core::ToolCall {
    use agent_core::{FileDiff, ToolCall, ToolKind};
    ToolCall {
        id: format!("e{i}"),
        name: "edit_file".into(),
        kind: ToolKind::Edit,
        title: format!("f{i}.rs"),
        input: serde_json::json!({"path": format!("f{i}.rs"), "old_string": "a", "new_string": "b"}),
        status: ToolStatus::Completed,
        diff: Some(FileDiff {
            path: format!("f{i}.rs"),
            old: Some("a".into()),
            new: "b".into(),
        }),
        ..Default::default()
    }
}

/// Finding 7: `iw - 2` on a usize wrapped at 5 and 6 columns, and a zero-row viewport with a
/// scrolled offset underflowed in a debug build.
#[test]
fn sidebar_overlay_and_tiny_viewports_do_not_panic() {
    use common::Harness;
    for w in 1..=12u16 {
        for h in 1..=8u16 {
            let mut hn = Harness::new(w, h);
            hn.app.send_prompt("hi".into());
            hn.event(Event::TurnStart);
            for i in 0..3 {
                hn.event(Event::Tool(edit_call(i)));
            }
            hn.event(Event::TurnEnd(agent_core::StopReason::EndTurn));
            hn.app.sidebar_pref = openw::app::SidebarPref::Show;
            hn.render();
            hn.app.scroll.to_bottom();
            hn.key(KeyCode::PageUp, KeyModifiers::NONE);
            hn.render();
        }
    }
}

/// Finding 8: block heights were `u16`, so 70,000 rows laid out as 4,484 and the blocks after
/// it drew over the output.
#[test]
fn a_tall_block_keeps_its_height() {
    use agent_core::{ToolCall, ToolKind};
    let mut h = common::Harness::new(150, 42);
    h.app.send_prompt("go".into());
    h.event(Event::TurnStart);
    h.event(Event::Tool(ToolCall {
        id: "b".into(),
        name: "bash".into(),
        kind: ToolKind::Execute,
        title: "x".into(),
        input: serde_json::json!({"command": "x"}),
        status: ToolStatus::Completed,
        output: Some("y\n".repeat(70_000)),
        ..Default::default()
    }));
    h.event(Event::TurnEnd(agent_core::StopReason::EndTurn));
    h.app.view.toggle("b");
    h.render();
    let rows = h.app.scroll.max_offset() + 42;
    assert!(rows >= 70_000, "70000 output lines laid out as {rows} rows");
}

/// Finding 19: a submodule's `gitdir: ../.git/modules/sub` was resolved against the process cwd.
#[test]
fn git_branch_follows_a_relative_gitdir_file() {
    let d = std::env::temp_dir().join(format!("openw-gb-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(d.join(".git/modules/sub")).unwrap();
    std::fs::create_dir_all(d.join("sub")).unwrap();
    std::fs::write(
        d.join(".git/modules/sub/HEAD"),
        "ref: refs/heads/feature-x\n",
    )
    .unwrap();
    std::fs::write(d.join("sub/.git"), "gitdir: ../.git/modules/sub\n").unwrap();
    let got = openw::app::git_branch(&d.join("sub"));
    let _ = std::fs::remove_dir_all(&d);
    assert_eq!(got.as_deref(), Some("feature-x"));
}

/// Finding 13: history was created with the default umask and kept every pasted body.
#[test]
fn history_is_owner_only_and_leaves_huge_pastes_out() {
    use std::os::unix::fs::PermissionsExt;
    let d = std::env::temp_dir().join(format!("openw-hist-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    let (tx, _requests) = unbounded_channel();
    let (mtx, _msgs) = unbounded_channel();
    let mut app = App::new(
        AppOpts {
            cwd: std::env::temp_dir(),
            mock: true,
            state_dir: Some(d.clone()),
            ..Default::default()
        },
        tx,
        mtx,
    );
    app.size = (100, 30);
    let big = format!(
        "export OPENAI_API_KEY=sk-secret\n{}",
        "x".repeat(1000) + "\n"
    )
    .repeat(100);
    app.on_paste(&big);
    for c in " and more".chars() {
        app.on_key(crossterm::event::KeyEvent::new(
            KeyCode::Char(c),
            KeyModifiers::NONE,
        ));
    }
    app.on_key(crossterm::event::KeyEvent::new(
        KeyCode::Enter,
        KeyModifiers::NONE,
    ));
    let f = d.join("prompt-history.jsonl");
    let body = std::fs::read_to_string(&f).expect("history file");
    let mode = std::fs::metadata(&f).unwrap().permissions().mode() & 0o777;
    let dir_mode = std::fs::metadata(&d).unwrap().permissions().mode() & 0o777;
    let _ = std::fs::remove_dir_all(&d);
    assert_eq!(mode, 0o600, "history mode");
    assert_eq!(dir_mode, 0o700, "state dir mode");
    assert!(
        body.len() < 4096,
        "history holds {} bytes; the pasted body was persisted",
        body.len()
    );
}

fn titled(title: &str) -> common::Harness {
    let mut h = common::Harness::new(100, 30);
    h.app.send_prompt("hello".into());
    let id = h.app.transcript.session_id.clone();
    h.event(Event::Sessions(vec![agent_core::SessionInfo {
        id,
        title: title.into(),
        cwd: "/work/proj".into(),
        updated: 1,
    }]));
    h
}

/// Finding 1: a title with BEL ended the OSC early and the rest ran as terminal commands, in
/// the window title and again in the exit summary.
#[test]
fn the_title_and_the_exit_summary_carry_no_control_bytes() {
    let h = titled("evil\x1b]0;PWNED\x07\x1b[2Jred\u{202e}");
    let t = h.app.window_title();
    assert!(
        !t.chars().any(|c| c.is_control() || c == '\u{202e}'),
        "window title {t:?}"
    );
    assert!(t.contains("evil") && t.contains("red"), "{t:?}");
    let e = h.app.epilogue().unwrap();
    assert!(
        !e.contains("PWNED")
            && !e.contains('\x07')
            && !e.contains("\x1b]")
            && !e.contains("\x1b[2J"),
        "{e:?}"
    );
    // what is left is our own colours and the logo
    assert!(e.contains("Session"), "{e:?}");
}

/// The id comes from the backend and ends up in a line a user may paste into a shell.
#[test]
fn the_resume_hint_quotes_an_odd_session_id() {
    let mut h = common::Harness::new(100, 30);
    h.app.send_prompt("hello".into());
    h.app.transcript.session_id = "a b;rm -rf ~'\x1b]0;x\x07".into();
    let e = h.app.epilogue().unwrap();
    assert!(
        e.contains("wizard-ui-opencode -s 'a b;rm -rf ~'\\''"),
        "{e:?}"
    );
    assert!(!e.contains('\x07'));
    h.app.transcript.session_id = "2026-07-13T11-30-22".into();
    assert!(h
        .app
        .epilogue()
        .unwrap()
        .contains("wizard-ui-opencode -s 2026-07-13T11-30-22\x1b"));
}

fn git_repo(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("openw-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    let ok = std::process::Command::new("git")
        .args(["init", "-q", "."])
        .current_dir(&d)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .status()
        .unwrap()
        .success();
    assert!(ok);
    d
}

/// Finding 2: `git ls-files --others` ran the repository's own `core.fsmonitor` program.
#[test]
fn listing_files_never_runs_the_repositorys_fsmonitor() {
    let d = git_repo("fsm");
    let marker = d.join("RAN");
    let hook = d.join("hook.sh");
    std::fs::write(&hook, format!("#!/bin/sh\ntouch {}\n", marker.display())).unwrap();
    std::fs::set_permissions(&hook, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    for args in [
        ["config", "core.fsmonitor"].as_slice(),
        &["config", "core.hooksPath", "/nonexistent"],
    ] {
        let mut c = std::process::Command::new("git");
        c.args(args);
        if args[1] == "core.fsmonitor" {
            c.arg(&hook);
        }
        assert!(c.current_dir(&d).status().unwrap().success());
    }
    std::fs::write(d.join("a.txt"), "x").unwrap();
    let files = openw::app::list_files(&d);
    let ran = marker.exists();
    let _ = std::fs::remove_dir_all(&d);
    assert!(!ran, "the repo's fsmonitor command ran");
    assert!(files.iter().any(|f| f == "a.txt"), "{files:?}");
}

/// Finding 18b: without -z git quotes non-ASCII names, so `@caf` found nothing.
#[test]
fn the_file_list_keeps_non_ascii_names() {
    let d = git_repo("qp");
    std::fs::write(d.join("café.txt"), "x").unwrap();
    std::fs::write(d.join("日本語.md"), "x").unwrap();
    let got = openw::app::list_files(&d);
    let _ = std::fs::remove_dir_all(&d);
    assert!(
        got.iter().any(|f| f == "café.txt") && got.iter().any(|f| f == "日本語.md"),
        "{got:?}"
    );
}

/// Finding 11: one unbroken token in the prompt (an editor result, a stash pop) made every
/// frame quadratic: 160 KB cost 336 ms, 5 MB never returned.
#[test]
fn a_long_unbroken_prompt_draws_in_linear_time() {
    let frame = |kb: usize| {
        let mut h = common::Harness::new(150, 42);
        h.app.prompt.set_text(&"x".repeat(kb * 1024));
        h.render();
        let t = std::time::Instant::now();
        h.render();
        t.elapsed().as_secs_f64()
    };
    let small = frame(80);
    let big = frame(640);
    // 8x the text costs about 8x; quadratic costs about 64x
    assert!(
        big < small * 24.0 + 0.2,
        "80 KB {small:.3}s, 640 KB {big:.3}s"
    );
}

/// Finding 18a: the `@` index was rebuilt only when the file count changed, so a branch switch
/// to as many files kept completing the old names.
#[test]
fn the_file_popup_follows_a_changed_file_list_of_the_same_length() {
    let mut h = common::Harness::new(100, 30);
    let at = |h: &mut common::Harness, q: &str| {
        h.app.prompt.clear();
        for c in q.chars() {
            h.key(KeyCode::Char(c), KeyModifiers::NONE);
        }
        h.app.prompt.ac.as_ref().map_or(0, |a| a.items.len())
    };
    h.app.files = (0..50).map(|i| format!("old/file_{i}.rs")).collect();
    assert!(at(&mut h, "@old") > 0);
    h.app.files = (0..50).map(|i| format!("new/file_{i}.rs")).collect();
    assert!(
        at(&mut h, "@new") > 0,
        "popup for @new is empty after the list changed"
    );
}

/// Finding 18c: outside a repository the walk skipped every dot directory and stopped at 5,000.
#[test]
fn the_walk_outside_git_reaches_dot_directories() {
    let d = std::env::temp_dir().join(format!("openw-walk-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(d.join(".github/workflows")).unwrap();
    std::fs::create_dir_all(d.join(".git/objects")).unwrap();
    std::fs::create_dir_all(d.join("node_modules/x")).unwrap();
    std::fs::write(d.join(".github/workflows/ci.yml"), "x").unwrap();
    std::fs::write(d.join(".env.example"), "x").unwrap();
    std::fs::write(d.join(".git/objects/aa"), "x").unwrap();
    std::fs::write(d.join("node_modules/x/i.js"), "x").unwrap();
    std::fs::write(d.join("main.rs"), "x").unwrap();
    // a link back up the tree is not followed
    std::os::unix::fs::symlink(&d, d.join("loop")).unwrap();
    let mut got = openw::app::list_files(&d);
    got.sort();
    let _ = std::fs::remove_dir_all(&d);
    assert!(
        got.contains(&".github/workflows/ci.yml".to_string()),
        "{got:?}"
    );
    assert!(
        got.contains(&".env.example".to_string()) && got.contains(&"main.rs".to_string()),
        "{got:?}"
    );
    assert!(
        !got.iter()
            .any(|f| f.starts_with(".git/") || f.starts_with("node_modules/")),
        "{got:?}"
    );
    assert!(!got.iter().any(|f| f.starts_with("loop/")), "{got:?}");
}

/// Finding 17: ctrl+v asks a helper, which may be slow, so the answer arrives as a message.
#[test]
fn a_clipboard_answer_that_arrives_later_is_pasted_into_the_prompt() {
    use openw::clipboard::Clip;
    let mut h = common::Harness::new(100, 30);
    h.app
        .update(Msg::Clip(Some(Clip::Text("from the clipboard".into()))));
    assert_eq!(h.app.prompt.text(), "from the clipboard");
    h.app.update(Msg::Clip(None));
    assert!(h.text().contains("nothing to paste"));
}

/// Finding 6b: a streaming answer cost a frame proportional to its whole size (51 ms at 1 MB,
/// 194 ms at 4 MB), because every row was copied and indented again on every frame.
#[test]
fn a_streaming_answer_costs_the_same_frame_at_any_length() {
    let mut h = common::Harness::new(150, 42);
    h.app.send_prompt("stream".into());
    h.event(Event::TurnStart);
    let para = "Some **bold** text and `code`, a [link](https://example.com) and more words \
                to wrap around the line, then a few more so the paragraph runs to a few rows.\n\n";
    let mut sent = 0usize;
    let mut frame = |h: &mut common::Harness, upto: usize| -> f64 {
        while sent < upto {
            h.event(Event::TextDelta(para.into()));
            sent += para.len();
        }
        h.render();
        let t = std::time::Instant::now();
        for i in 0..5 {
            // one more delta per frame, as a stream does
            h.event(Event::TextDelta(format!("tail {i} words words words ")));
            h.render();
        }
        t.elapsed().as_secs_f64() / 5.0
    };
    let small = frame(&mut h, 200 * 1024);
    let big = frame(&mut h, 1600 * 1024);
    // 8x the text: copying every row costs about 8x a frame, caching the finished ones about 1x
    assert!(
        big < small * 3.0 + 0.01,
        "frame at 200 KB {small:.4}s, at 1.6 MB {big:.4}s"
    );
}

/// Finding 21 (the part that is not viewport virtualisation): expanding one tool row rebuilt
/// every message of the session, because the expanded set was part of every message's key.
#[test]
fn expanding_a_tool_row_rebuilds_only_its_own_message() {
    use agent_core::{ToolCall, ToolKind};
    let mut h = common::Harness::new(120, 40);
    for i in 0..30 {
        h.app.send_prompt(format!("q{i}"));
        h.event(Event::TurnStart);
        h.event(Event::Tool(ToolCall {
            id: format!("t{i}"),
            name: "bash".into(),
            kind: ToolKind::Execute,
            title: "ls".into(),
            input: serde_json::json!({"command": "ls"}),
            status: ToolStatus::Completed,
            output: Some("a\nb\nc\nd\ne\nf\n".repeat(5)),
            ..Default::default()
        }));
        h.event(Event::TextDelta(format!("answer {i}")));
        h.event(Event::TurnEnd(agent_core::StopReason::EndTurn));
    }
    h.render();
    let before = h.app.view.builds;
    h.render();
    assert_eq!(h.app.view.builds, before, "an idle frame rebuilt messages");
    h.app.view.toggle("t3");
    h.render();
    let rebuilt = h.app.view.builds - before;
    assert!(
        rebuilt <= 2,
        "{rebuilt} messages rebuilt for one expanded tool"
    );
    assert!(rebuilt >= 1, "the expanded tool's message was not rebuilt");
}
