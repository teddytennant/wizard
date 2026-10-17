//! Regression tests for the round 1 interaction and safety findings (`docs/critics/piw-interaction-r1.md`).

mod common;

use agent_core::{Event, ToolKind};
use common::*;
use serde_json::json;

/// Every byte of `out` that is not an SGR sequence, a CR or an LF is a plain printable one.
fn assert_no_controls(out: &str) {
    let mut rest = out;
    while let Some(i) = rest.find('\x1b') {
        let tail = &rest[i + 1..];
        let ok = tail.strip_prefix('[').and_then(|t| {
            let n = t.find(|c: char| !(c.is_ascii_digit() || c == ';'))?;
            (t.as_bytes()[n] == b'm').then_some(i + 1 + 1 + n + 1)
        });
        match ok {
            Some(skip) => rest = &rest[skip..],
            None => panic!("a stray escape in {:?}", &rest[i..(i + 40).min(rest.len())]),
        }
    }
    assert!(
        !out.chars()
            .any(|c| c.is_control() && !matches!(c, '\x1b' | '\r' | '\n')),
        "{out:?}"
    );
}

#[test]
fn the_exit_epilogue_carries_no_control_bytes() {
    let mut h = Harness::new(100, 30);
    let evil = "\x1b]0;P_x\x07\x1b]52;c;cHduZWQ=\x07\x1b[2J\x1bP1;2|q\x1b\\\u{9d}x\u{9c}";
    h.app.send_prompt(format!("hello {evil}"));
    h.event(Event::TurnStart);
    h.event(Event::TextDelta(format!(
        "- li{evil}\n\n```{evil}\ncode{evil}\n```\n\n[lt{evil}](http://x/{evil})\n\n| a |\n|---|\n| td{evil} |\n"
    )));
    h.event(Event::Tool(tool(
        "t1",
        "bash",
        ToolKind::Execute,
        "",
        json!({"command": format!("echo {evil}")}),
        Some(&format!("out{evil}")),
    )));
    h.event(Event::Tool(tool(
        "t2",
        "read",
        ToolKind::Read,
        "",
        json!({"path": format!("/tmp/{evil}")}),
        None,
    )));
    h.event(Event::Tool(tool(
        "t3",
        "grep",
        ToolKind::Search,
        "",
        json!({"pattern": evil, "path": evil}),
        None,
    )));
    h.event(Event::TurnEnd(agent_core::StopReason::EndTurn));
    h.event(Event::Ready {
        session_id: "ses_'; rm -rf ~; echo \x1b]0;t\x07".into(),
        config: piw::mock::config(),
    });
    let out = h.app.epilogue(100);
    assert_no_controls(&out);
    assert!(out.contains("P_x"), "the text itself is still shown");
    assert!(
        out.contains("wizard-ui-pi --session '"),
        "an odd id is quoted: {out:?}"
    );
}

#[test]
fn the_window_title_carries_no_control_bytes() {
    let mut h = Harness::new(100, 30);
    h.app.cwd = "/tmp/dir \x1b]52;c;cHduZWQ=\x07\x1b[2J end".into();
    h.app.session_name = Some("n\x07\x1b]0;x\x07\nz".into());
    let t = h.app.window_title();
    assert!(!t.chars().any(char::is_control), "{t:?}");
    assert!(t.starts_with("π - n"), "{t:?}");
    assert!(tuikit::width::display_width(&t) <= 100);
}

// ---- 9: a stale Tab popup --------------------------------------------------------------

use crossterm::event::{KeyCode, KeyModifiers};
use std::path::PathBuf;

/// A scratch directory that is removed when it goes out of scope.
struct Scratch(PathBuf);

impl Scratch {
    fn new(tag: &str) -> Scratch {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static N: AtomicUsize = AtomicUsize::new(0);
        let d = std::env::temp_dir().join(format!(
            "piw-fix-{tag}-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        Scratch(d)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn harness_in(dir: &std::path::Path) -> Harness {
    let mut h = Harness::new(80, 24);
    h.app.opts.cwd = dir.to_path_buf();
    h
}

#[test]
fn a_path_popup_does_not_survive_an_edit_that_moves_its_range() {
    let d = Scratch::new("stale");
    std::fs::write(d.0.join("a1"), "").unwrap();
    std::fs::write(d.0.join("a2"), "").unwrap();
    let mut h = harness_in(&d.0);
    h.type_str("a");
    h.press(KeyCode::Tab);
    assert!(
        h.app.autocomplete.is_open(),
        "two candidates open the popup"
    );
    h.press(KeyCode::Home);
    h.type_str("日");
    // the report's four keys: this used to slice inside the 3-byte character and panic
    h.press(KeyCode::Tab);
    assert_eq!(h.app.editor.text(), "日a");
}

#[test]
fn a_stale_path_popup_does_not_replace_what_was_typed() {
    let d = Scratch::new("stale-ascii");
    std::fs::write(d.0.join("a1"), "").unwrap();
    std::fs::write(d.0.join("a2"), "").unwrap();
    let mut h = harness_in(&d.0);
    h.type_str("a");
    h.press(KeyCode::Tab);
    h.press(KeyCode::Home);
    h.type_str("x");
    h.press(KeyCode::Tab);
    assert_eq!(h.app.editor.text(), "xa", "was turned into `a1a`");
}

#[test]
fn a_path_popup_follows_the_text_while_it_stays_ambiguous() {
    let d = Scratch::new("follow");
    for n in ["ab1", "ab2", "ac1"] {
        std::fs::write(d.0.join(n), "").unwrap();
    }
    let mut h = harness_in(&d.0);
    h.type_str("a");
    h.press(KeyCode::Tab);
    assert!(h.app.autocomplete.is_open());
    h.type_str("b");
    assert!(h.app.autocomplete.is_open(), "ab1 and ab2 are still two");
    h.press(KeyCode::Down);
    h.press(KeyCode::Tab);
    assert_eq!(h.app.editor.text(), "ab2");
}

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

/// Keys, pastes and events in random order, with a directory of awkward names and an `@` list to
/// complete from. After every step the editor's cursor must be on a character boundary and the
/// popup's range must fit the live text: those are the two invariants a stale popup breaks.
#[test]
fn random_keys_never_leave_a_cursor_or_popup_range_off_the_text() {
    let d = Scratch::new("fuzz");
    for n in ["a1", "a2", "日本語", "é.txt", "sp ace", "ab", "ac"] {
        std::fs::write(d.0.join(n), "").unwrap();
    }
    std::fs::create_dir(d.0.join("sub")).unwrap();
    std::fs::write(d.0.join("sub/x1"), "").unwrap();
    std::fs::write(d.0.join("sub/x2"), "").unwrap();
    let files: Vec<String> = [
        "a1",
        "a2",
        "日本語",
        "é.txt",
        "sp ace",
        "sub/x1",
        "sub/x2",
        "src/main.rs",
    ]
    .map(String::from)
    .to_vec();
    let pieces = [
        "a",
        "1",
        "x",
        "日",
        "é",
        "\u{3000}",
        " ",
        "/",
        "@",
        "@\"",
        "~",
        ".",
        "s",
        "u",
        "b",
        "\n",
        "e\u{301}",
        "👩‍👩‍👧",
        "model ",
        "/mo",
    ];
    let keys = [
        KeyCode::Tab,
        KeyCode::Backspace,
        KeyCode::Delete,
        KeyCode::Left,
        KeyCode::Right,
        KeyCode::Home,
        KeyCode::End,
        KeyCode::Up,
        KeyCode::Down,
        KeyCode::Esc,
        KeyCode::PageUp,
    ];
    let ctrls = ['a', 'e', 'u', 'k', 'w', 'y', 'b', 'f', 'd', 'h', '_', 'z'];
    for seed in 1..=300u64 {
        let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
        let mut h = harness_in(&d.0);
        h.app.update(piw::app::Msg::Files(files.clone()));
        for step in 0..120 {
            match rng.below(10) {
                0..=3 => {
                    let p = pieces[rng.below(pieces.len())];
                    h.type_str(p);
                }
                4..=6 => h.press(keys[rng.below(keys.len())]),
                7 => h.key(
                    KeyCode::Char(ctrls[rng.below(ctrls.len())]),
                    if rng.below(2) == 0 {
                        KeyModifiers::CONTROL
                    } else {
                        KeyModifiers::ALT
                    },
                ),
                8 => {
                    let p = pieces[rng.below(pieces.len())].repeat(1 + rng.below(40));
                    h.paste(&p);
                }
                _ => {
                    // enter would run a slash command or a shell line; send plain text only
                    let t = h.app.editor.text();
                    if t.starts_with(['/', '!']) || h.app.autocomplete.is_open() {
                        h.press(KeyCode::Esc);
                    } else {
                        h.press(KeyCode::Enter);
                    }
                }
            }
            h.app.pending.clear();
            h.sent();
            let t = h.app.editor.text().to_string();
            let c = h.app.editor.ed.cursor();
            assert!(
                c <= t.len() && t.is_char_boundary(c),
                "seed {seed} step {step}: cursor {c} in {t:?}"
            );
            if let Some(ap) = h.app.autocomplete.selected() {
                assert!(
                    ap.fits(&t),
                    "seed {seed} step {step}: range {:?} in {t:?}",
                    ap.range
                );
            }
            // drawing it must not panic either
            if step % 20 == 0 {
                h.render();
            }
        }
    }
}

// ---- 3, 11, 19: the file list ------------------------------------------------------------

fn git(dir: &std::path::Path, args: &[&str]) {
    let out = std::process::Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}: {out:?}");
}

#[test]
fn listing_files_never_runs_the_repositorys_fsmonitor() {
    let d = Scratch::new("fsmon");
    git(&d.0, &["init", "-q"]);
    let marker = d.0.join("RAN");
    let hook = d.0.join("hook.sh");
    std::fs::write(
        &hook,
        format!("#!/bin/sh\ntouch '{}'\nprintf ''\n", marker.display()),
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
    git(&d.0, &["config", "core.fsmonitor", hook.to_str().unwrap()]);
    std::fs::write(d.0.join("a.txt"), "x").unwrap();
    let got = piw::app::list_files(&d.0);
    assert!(got.contains(&"a.txt".to_string()), "{got:?}");
    assert!(!marker.exists(), "the repository's fsmonitor command ran");
}

#[test]
fn the_file_list_keeps_non_ascii_names_and_the_popup_finds_them() {
    let d = Scratch::new("names");
    git(&d.0, &["init", "-q"]);
    for n in ["é-accent.txt", "日本語.md", "q\"uote.txt", "tab\tname.txt"] {
        std::fs::write(d.0.join(n), "x").unwrap();
    }
    let got = piw::app::list_files(&d.0);
    for n in ["é-accent.txt", "日本語.md", "q\"uote.txt", "tab\tname.txt"] {
        assert!(got.iter().any(|g| g == n), "{n:?} missing from {got:?}");
    }
    let mut h = harness_in(&d.0);
    h.app.update(piw::app::Msg::Files(got));
    h.type_str("@é");
    assert!(h.text().contains("é-accent.txt"), "{}", h.text());
    h.press(KeyCode::Esc);
    h.app.editor.clear();
    h.type_str("@日");
    assert!(h.text().contains("日本語.md"), "{}", h.text());
}

#[test]
fn a_name_with_a_space_is_inserted_quoted() {
    let mut h = Harness::new(80, 24);
    h.app.update(piw::app::Msg::Files(vec![
        "my file.txt".into(),
        "plain.txt".into(),
    ]));
    h.type_str("see @my");
    h.press(KeyCode::Tab);
    assert_eq!(h.app.editor.text(), "see @\"my file.txt\" ");
    h.app.editor.clear();
    h.type_str("@\"my f");
    assert!(
        h.app.autocomplete.is_open(),
        "an open quote keeps the token alive across a space"
    );
    h.press(KeyCode::Tab);
    assert_eq!(h.app.editor.text(), "@\"my file.txt\" ");
    h.app.editor.clear();
    h.type_str("@plain");
    h.press(KeyCode::Tab);
    assert_eq!(h.app.editor.text(), "@plain.txt ");
}

#[test]
fn a_cut_file_list_says_so_once() {
    let mut h = Harness::new(80, 24);
    let files: Vec<String> = (0..piw::app::MAX_FILES)
        .map(|i| format!("f{i}.txt"))
        .collect();
    h.app.update(piw::app::Msg::Files(files.clone()));
    h.app.update(piw::app::Msg::Files(files));
    let t = h.text();
    assert_eq!(t.matches("@ lists the first").count(), 1, "{t}");
}

#[tokio::test]
async fn opening_the_popup_rereads_a_stale_file_list() {
    let d = Scratch::new("stale-list");
    std::fs::write(d.0.join("old.txt"), "").unwrap();
    let mut h = harness_in(&d.0);
    h.app
        .update(piw::app::Msg::Files(piw::app::list_files(&d.0)));
    std::fs::write(d.0.join("fresh.txt"), "").unwrap();
    h.app.files_at = std::time::Instant::now() - std::time::Duration::from_secs(10);
    h.type_str("@");
    for _ in 0..50 {
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        if h.deliver() > 0 {
            break;
        }
    }
    assert!(h.text().contains("fresh.txt"), "{}", h.text());
}

#[test]
fn typing_in_a_big_repo_does_not_rebuild_the_directory_list_each_key() {
    let files: Vec<String> = (0..20_000)
        .map(|i| format!("d{}/e{}/f{}/file_{i}.txt", i % 40, i % 7, i % 13))
        .collect();
    let mut h = Harness::new(80, 24);
    h.app.update(piw::app::Msg::Files(files));
    h.type_str("@f");
    let t0 = std::time::Instant::now();
    h.type_str("ile_1");
    let per_key = t0.elapsed() / 5;
    // debug build: 420 ms a key before the index, about 50 now
    assert!(
        per_key < std::time::Duration::from_millis(150),
        "{per_key:?} a key"
    );
}

// ---- 8, 18: frame cost ---------------------------------------------------------------------

fn paragraphs(n: usize, from: usize) -> String {
    (from..from + n)
        .map(|i| format!("Paragraph {i} has a few plain words in it, enough to wrap at eighty columns once or twice.\n\n"))
        .collect()
}

#[test]
fn a_streaming_answer_costs_about_the_same_frame_at_any_length() {
    let mut h = Harness::new(100, 30);
    h.app.send_prompt("go".into());
    h.event(Event::TurnStart);
    let frame = |h: &mut Harness| {
        h.render(); // lay out what is new
        let t0 = std::time::Instant::now();
        for _ in 0..3 {
            h.render();
        }
        t0.elapsed() / 3
    };
    let mut at = 0;
    let mut grow = |h: &mut Harness, to_bytes: usize| {
        while at < to_bytes {
            let chunk = paragraphs(10, at / 90);
            at += chunk.len();
            h.event(Event::TextDelta(chunk));
        }
    };
    grow(&mut h, 100_000);
    let small = frame(&mut h);
    grow(&mut h, 1_200_000);
    let big = frame(&mut h);
    assert!(
        big < small * 4 + std::time::Duration::from_millis(30),
        "100 KB: {small:?} a frame, 1.2 MB: {big:?}"
    );
}

#[test]
fn a_long_transcript_costs_a_frame_by_its_items_not_its_rows() {
    let mut h = Harness::new(100, 30);
    for i in 0..1500 {
        h.turn(&format!("question {i}"), &paragraphs(6, i));
    }
    h.render();
    let cx = h.app.cx(100);
    let time = |f: &mut dyn FnMut()| {
        let t0 = std::time::Instant::now();
        for _ in 0..5 {
            f();
        }
        t0.elapsed() / 5
    };
    let shared = time(&mut || {
        let c = h.app.transcript_chunks(&cx);
        assert!(c.len() > 20_000, "{} rows", c.len());
    });
    let copied = time(&mut || {
        let _ = h.app.transcript_lines(&cx);
    });
    // before, every frame was the second: all the rows copied out of the cache
    assert!(shared * 4 < copied, "shared {shared:?}, copied {copied:?}");
    let draw = time(&mut || {
        h.render();
    });
    assert!(
        draw < std::time::Duration::from_millis(60),
        "{draw:?} an idle frame"
    );
}

// ---- 7, 17, 20: tool blocks read paths the model chose -------------------------------------------

fn tool_rows(c: &agent_core::ToolCall, cwd: &str) -> Vec<String> {
    use piw::ui::tools::{render, ToolCx, ToolTime};
    let cx = piw::ui::Cx {
        theme: piw::theme::PiTheme::dark(),
        width: 100,
        expanded: false,
        hide_thinking: false,
        out_pad: 1,
        cwd: cwd.into(),
        home: "/home/me".into(),
        clock: std::time::Duration::ZERO,
        version: "1.0.3".into(),
    };
    render(c, &ToolCx::new(&cx, ToolTime::default()))
        .iter()
        .map(|l| {
            l.spans
                .iter()
                .map(|s| s.content.as_ref())
                .collect::<String>()
        })
        .collect()
}

fn edit_call(id: &str, path: &str, old: &str, new: &str) -> agent_core::ToolCall {
    let mut c = tool(
        id,
        "edit_file",
        ToolKind::Edit,
        "",
        json!({"path": path, "old_string": old, "new_string": new}),
        Some("ok"),
    );
    c.diff = Some(agent_core::FileDiff {
        path: path.into(),
        old: Some(old.into()),
        new: new.into(),
    });
    c
}

#[test]
fn a_fifo_is_not_read_while_drawing() {
    let d = Scratch::new("fifo");
    let fifo = d.0.join("pipe");
    let c = std::ffi::CString::new(fifo.to_str().unwrap()).unwrap();
    // SAFETY: mkfifo with a valid path.
    assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
    let call = edit_call("t1", fifo.to_str().unwrap(), "a", "b");
    let cwd = d.0.to_str().unwrap().to_string();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(tool_rows(&call, &cwd));
    });
    let rows = rx
        .recv_timeout(std::time::Duration::from_secs(5))
        .expect("drawing an edit of a FIFO hung the thread");
    assert!(rows.iter().any(|r| r.contains("edit")), "{rows:?}");
}

#[test]
fn a_file_outside_the_working_directory_is_not_read_for_context() {
    let d = Scratch::new("outside");
    let cwd = d.0.join("work");
    std::fs::create_dir_all(&cwd).unwrap();
    std::fs::write(
        d.0.join("secret.txt"),
        "line one\nSECRETWORD two\nline three\n",
    )
    .unwrap();
    let call = edit_call(
        "t1",
        d.0.join("secret.txt").to_str().unwrap(),
        "x",
        "SECRETWORD",
    );
    let rows = tool_rows(&call, cwd.to_str().unwrap());
    assert!(
        !rows.iter().any(|r| r.contains("line one")),
        "context of a file outside the cwd was shown: {rows:?}"
    );
}

#[test]
fn an_edit_diff_is_kept_not_the_file() {
    let d = Scratch::new("editcache");
    let file = d.0.join("big.txt");
    let body: String = (0..2000).map(|i| format!("line {i}\n")).collect();
    std::fs::write(&file, body.replace("line 1000", "CHANGED 1000")).unwrap();
    let cwd = d.0.to_str().unwrap();
    let call = edit_call(
        "keep-1",
        file.to_str().unwrap(),
        "line 1000",
        "CHANGED 1000",
    );
    let first = tool_rows(&call, cwd);
    assert!(
        first.iter().any(|r| r.contains("line 998")),
        "context rows: {first:?}"
    );
    // the file is gone, and the block still draws what it drew: nothing is read again
    std::fs::remove_file(&file).unwrap();
    assert_eq!(tool_rows(&call, cwd), first);
    // 64 other edits push it out, so the cache does not grow for good
    for i in 0..70 {
        let other = edit_call(&format!("other-{i}"), "nowhere.txt", "a", "b");
        tool_rows(&other, cwd);
    }
    assert_ne!(
        tool_rows(&call, cwd),
        first,
        "still cached after 70 other edits"
    );
}

#[test]
fn read_ranges_do_not_overflow() {
    for input in [
        json!({"path": "a.rs", "offset": 0, "limit": 0}),
        json!({"path": "a.rs", "offset": 5, "limit": 0}),
        json!({"path": "a.rs", "offset": 7, "limit": u64::MAX}),
        json!({"path": "a.rs", "limit": u64::MAX}),
    ] {
        let c = tool("r", "read_file", ToolKind::Read, "", input, Some("x"));
        let rows = tool_rows(&c, "/home/me/proj");
        assert!(rows.iter().any(|r| r.contains("a.rs")), "{rows:?}");
    }
}

// ---- 10, 13, 14, 15: sessions, a dead backend, Esc, the queue ---------------------------------

use agent_core::{Request, StopReason};

fn ready(id: &str) -> Event {
    Event::Ready {
        session_id: id.into(),
        config: piw::mock::config(),
    }
}

fn start_turn(h: &mut Harness, text: &str) {
    h.type_str(text);
    h.press(KeyCode::Enter);
    h.event(Event::TurnStart);
    h.event(Event::TextDelta("streaming".into()));
}

#[test]
fn new_clears_the_conversation_when_wizard_confirms_with_ready_alone() {
    let mut h = Harness::new(100, 30);
    h.turn("first question", "the old answer");
    assert!(h.text().contains("the old answer"));
    h.type_str("/new");
    h.press(KeyCode::Enter);
    assert!(h.sent().iter().any(|r| matches!(r, Request::NewSession)));
    // the real backend sends no History for an empty session, only Ready
    h.event(ready("ses_second"));
    let t = h.text();
    assert!(
        !t.contains("the old answer") && !t.contains("first question"),
        "{t}"
    );
    assert!(t.contains("✓ New session started"), "{t}");
    assert_eq!(h.app.transcript.session_id, "ses_second");
    // a later reload of that session does not announce another new one
    h.event(Event::History {
        session_id: "ses_second".into(),
        items: vec![],
    });
    assert!(!h.text().contains("New session started"));
}

#[test]
fn new_while_a_turn_runs_is_refused_here_and_announces_nothing_later() {
    let mut h = Harness::new(100, 30);
    start_turn(&mut h, "go");
    h.sent();
    h.type_str("/new");
    h.press(KeyCode::Enter);
    assert!(!h.sent().iter().any(|r| matches!(r, Request::NewSession)));
    assert!(
        h.text().contains("Finish or cancel the running turn"),
        "{}",
        h.text()
    );
    h.event(Event::TurnEnd(StopReason::EndTurn));
    h.event(Event::History {
        session_id: "ses_other".into(),
        items: vec![],
    });
    assert!(!h.text().contains("New session started"));
}

#[test]
fn a_refused_switch_does_not_leave_a_pending_announcement() {
    let mut h = Harness::new(100, 30);
    h.type_str("/new");
    h.press(KeyCode::Enter);
    h.event(Event::Notice {
        level: agent_core::NoticeLevel::Error,
        text: "New session failed: boom".into(),
    });
    h.event(Event::History {
        session_id: "ses_x".into(),
        items: vec![],
    });
    assert!(!h.text().contains("✓ New session started"));
}

#[tokio::test]
async fn a_dead_backend_keeps_the_draft_and_the_queue_and_new_starts_it_again() {
    let mut h = Harness::new(100, 30);
    start_turn(&mut h, "go");
    h.type_str("later");
    h.press(KeyCode::Enter);
    h.event(Event::Fatal("wizard acp exited (exit status: 3)".into()));
    assert!(!h.app.busy());
    assert_eq!(
        h.app.editor.text(),
        "later",
        "the queued message is back in the editor"
    );
    // a message typed now has nobody to answer it: it stays in the editor, no user block
    h.app.editor.clear();
    let blocks = h.app.transcript.messages.len();
    h.type_str("hello");
    h.press(KeyCode::Enter);
    assert_eq!(h.app.editor.text(), "hello");
    assert_eq!(h.app.transcript.messages.len(), blocks);
    assert!(h.text().contains("wizard is not running"), "{}", h.text());
    // /new does what the error line says
    h.app.editor.clear();
    h.type_str("/new");
    h.press(KeyCode::Enter);
    for _ in 0..100 {
        h.deliver();
        if h.app.backend_ready {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert!(h.app.backend_ready, "the backend never answered");
    assert!(h.text().contains("✓ New session started"), "{}", h.text());
    h.type_str("again");
    h.press(KeyCode::Enter);
    assert!(h
        .app
        .transcript
        .messages
        .iter()
        .any(|m| m.role == agent_core::transcript::Role::User));
}

#[test]
fn escape_says_aborting_until_the_turn_ends() {
    let mut h = Harness::new(100, 30);
    start_turn(&mut h, "go");
    assert!(h.text().contains("Working"));
    h.press(KeyCode::Esc);
    assert!(h.text().contains("Aborting..."), "{}", h.text());
    h.event(Event::TurnEnd(StopReason::Cancelled));
    assert!(!h.text().contains("Aborting"), "{}", h.text());
}

#[test]
fn a_pasted_block_survives_restoring_the_queued_messages() {
    let mut h = Harness::new(100, 30);
    start_turn(&mut h, "go");
    h.type_str("A");
    h.press(KeyCode::Enter);
    let lines: Vec<String> = (0..15).map(|i| format!("pasted line {i}")).collect();
    h.paste(&lines.join("\n"));
    assert!(h.app.editor.text().contains("[paste #1"));
    h.key(KeyCode::Up, KeyModifiers::ALT);
    let t = h.app.editor.text().to_string();
    assert!(t.starts_with("A\n\n"), "{t:?}");
    for l in &lines {
        assert!(t.contains(l), "{l} was lost from {t:?}");
    }
}

// ---- 16: state files -------------------------------------------------------------------------------

#[test]
fn history_is_owner_only_and_leaves_huge_pastes_out() {
    use std::os::unix::fs::PermissionsExt;
    let d = Scratch::new("history");
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let (mtx, _mrx) = tokio::sync::mpsc::unbounded_channel();
    let opts = piw::app::AppOpts {
        cwd: d.0.clone(),
        mock: true,
        state_dir: Some(d.0.join("state")),
        ..Default::default()
    };
    let mut app = piw::app::App::new(opts, tx, mtx);
    app.on_event(ready("s"));
    app.editor.set_text("a small prompt");
    app.submit();
    app.on_event(Event::TurnEnd(StopReason::EndTurn));
    app.editor.set_text(&format!(
        "export OPENAI_API_KEY=sk-secret {}",
        "x".repeat(200_000)
    ));
    app.submit();
    let file = d.0.join("state/history.json");
    let mode = std::fs::metadata(&file).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600);
    let body = std::fs::read_to_string(&file).unwrap();
    assert!(
        body.contains("a small prompt") && !body.contains("sk-secret"),
        "{}",
        body.len()
    );
    // one invalid byte does not cost the whole history
    std::fs::write(&file, b"[\"kept\\n\", \"bad \xff byte\"]").unwrap();
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let (mtx, _mrx) = tokio::sync::mpsc::unbounded_channel();
    let opts = piw::app::AppOpts {
        cwd: d.0.clone(),
        mock: true,
        state_dir: Some(d.0.join("state")),
        ..Default::default()
    };
    let app = piw::app::App::new(opts, tx, mtx);
    assert_eq!(app.editor.ed.history_entries().count(), 2);
}

#[test]
fn a_settings_file_that_does_not_parse_is_kept_beside_the_new_one() {
    let d = Scratch::new("settings");
    std::fs::write(d.0.join("settings.json"), "{ \"theme\": \"light\", }").unwrap();
    let s = piw::settings::Settings {
        hide_thinking_block: true,
        ..Default::default()
    };
    s.save(&d.0);
    assert_eq!(
        std::fs::read_to_string(d.0.join("settings.json.bad")).unwrap(),
        "{ \"theme\": \"light\", }"
    );
    let now: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(d.0.join("settings.json")).unwrap()).unwrap();
    assert_eq!(now["hideThinkingBlock"], true);
}

// ---- 22: options the backend does not accept ----------------------------------------------------

fn app_with(model: Option<&str>, thinking: Option<&str>) -> piw::app::App {
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let (mtx, _mrx) = tokio::sync::mpsc::unbounded_channel();
    let opts = piw::app::AppOpts {
        cwd: "/home/me/proj".into(),
        mock: true,
        model: model.map(String::from),
        thinking: thinking.map(String::from),
        ..Default::default()
    };
    piw::app::App::new(opts, tx, mtx)
}

#[test]
fn an_unknown_model_or_thinking_level_is_an_error_not_a_silent_default() {
    let mut app = app_with(Some("nosuchmodel"), None);
    app.on_event(ready("s"));
    assert!(app.quit);
    assert!(app
        .startup_error
        .as_deref()
        .unwrap()
        .contains("model \"nosuchmodel\" not found"));

    let mut app = app_with(None, Some("bogus"));
    app.on_event(ready("s"));
    assert!(app.quit);
    let e = app.startup_error.unwrap();
    assert!(
        e.contains("unknown thinking level \"bogus\"") && e.contains("Available levels"),
        "{e}"
    );

    // a model the backend lists is accepted, by id
    let id = piw::mock::config().models[0].id.clone();
    let mut app = app_with(Some(&id), None);
    app.on_event(ready("s"));
    assert!(!app.quit && app.startup_error.is_none());
}

// ---- 23, 27, 30, 31: messages that did not tell the truth ---------------------------------------------

#[test]
fn reload_and_copy_say_what_happened() {
    let mut h = Harness::new(100, 30);
    h.type_str("/reload");
    h.press(KeyCode::Enter);
    let t = h.text();
    assert!(
        t.contains("Reloaded settings, theme and wizard skills"),
        "{t}"
    );
    assert!(
        !t.contains("keybindings"),
        "keybindings.json is not read: {t}"
    );
    // ctrl+x with nothing to copy says so; with an answer Pi flashes ` Copied! ` at the top right
    // (and prints no status line: that one belongs to /copy)
    h.ctrl('x');
    assert!(
        h.text().contains("No agent messages to copy yet."),
        "{}",
        h.text()
    );
    h.turn("q", "an answer");
    h.ctrl('x');
    let t = h.text();
    assert!(
        t.lines()
            .next()
            .unwrap_or("")
            .trim_end()
            .ends_with("Copied!"),
        "{t}"
    );
    assert!(!t.contains("Copied last agent message to clipboard"), "{t}");
}

#[test]
fn the_session_path_is_the_file_that_exists() {
    let mut h = Harness::new(100, 30);
    h.type_str("/session");
    h.press(KeyCode::Enter);
    let t = h.text();
    assert!(t.contains("~/.wizard/sessions/ses_1234567890.jsonl"), "{t}");
}

#[test]
fn a_session_opened_by_id_says_it_was_resumed() {
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let (mtx, _mrx) = tokio::sync::mpsc::unbounded_channel();
    let opts = piw::app::AppOpts {
        cwd: "/home/me/proj".into(),
        mock: true,
        resume: Some("ses_old".into()),
        ..Default::default()
    };
    let mut app = piw::app::App::new(opts, tx, mtx);
    app.on_event(Event::History {
        session_id: "ses_old".into(),
        items: vec![],
    });
    app.on_event(ready("ses_old"));
    assert!(
        app.items
            .iter()
            .any(|i| matches!(i, piw::app::Item::Status(t) if t == "Resumed session")),
        "{:?}",
        app.items
    );
}

#[test]
fn the_resume_picker_says_loading_until_the_backend_has_listed_sessions() {
    let mut h = Harness::new(110, 30);
    h.type_str("/resume");
    h.press(KeyCode::Enter);
    let t = h.text();
    assert!(t.contains("Loading..."), "{t}");
    assert!(!t.contains("No sessions in current folder"), "{t}");
    h.event(Event::Sessions(vec![agent_core::SessionInfo {
        id: "s1".into(),
        title: "A long title that has to be cut so the age at the end of the row stays whole"
            .repeat(3),
        cwd: "/home/me/proj".into(),
        updated: piw::app::FROZEN_NOW - 120,
    }]));
    let t = h.text();
    assert!(!t.contains("Loading"), "{t}");
    // not reproduced: the age column is cut at 110 columns (finding 31); it ends the row whole
    let row = t.lines().find(|l| l.contains("A long title")).unwrap();
    assert!(row.trim_end().ends_with("2m"), "{row:?}");
}

#[test]
fn an_empty_listing_is_reported_as_empty() {
    let mut h = Harness::new(110, 30);
    h.type_str("/resume");
    h.press(KeyCode::Enter);
    h.event(Event::Sessions(vec![]));
    assert!(
        h.text().contains("No sessions in current folder"),
        "{}",
        h.text()
    );
}

#[test]
fn compact_while_a_turn_runs_is_refused_and_does_not_mark_compaction() {
    let mut h = Harness::new(100, 30);
    start_turn(&mut h, "go");
    h.sent();
    h.type_str("/compact");
    h.press(KeyCode::Enter);
    assert!(h.sent().is_empty(), "nothing is sent mid-turn");
    let t = h.text();
    assert!(
        t.contains("Finish or cancel the running turn before compacting."),
        "{t}"
    );
    assert!(!t.contains("Compacting context"), "{t}");
}
