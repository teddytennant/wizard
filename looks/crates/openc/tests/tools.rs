//! Tool cards, the permission panels for each kind of call, the question and plan flows, the
//! full diff viewer, the subagent tree and rewind, driven by the scripted mock backend on a
//! fake clock.

mod common;

use agent_core::{
    DecideScope, Event, PermissionRequest, Request, RewindPreview, ToolCall, ToolKind, ToolStatus,
};
use common::{assert_snapshot, hello, Harness, Mock};
use crossterm::event::{KeyCode, KeyModifiers};

fn started(w: u16, h: u16) -> Harness {
    let mut hx = Harness::new(w, h);
    hx.ready();
    hx
}

fn send(h: &mut Harness, text: &str) {
    h.type_str(text);
    h.key(KeyCode::Enter);
}

/// Everything the mock says for `prompt`, up to the end of the turn or a permission request.
async fn events_for(prompt: &str) -> Vec<Event> {
    let mut m = Mock::start();
    hello(&mut m).await;
    m.run(prompt).await
}

/// A drawn harness that has been through `prompt`'s events.
async fn after(prompt: &str, w: u16, h: u16) -> Harness {
    let events = events_for(prompt).await;
    let mut hx = started(w, h);
    send(&mut hx, prompt);
    hx.events(events, 60);
    hx
}

fn lines_with<'a>(s: &'a str, needle: &str) -> Vec<&'a str> {
    s.lines().filter(|l| l.contains(needle)).collect()
}

// ---- cards ---------------------------------------------------------------------------------

#[tokio::test]
async fn output_variety_reads_right_in_one_row_each() {
    let mut hx = after("rich output", 110, 40).await;
    let s = hx.screen();
    assert!(
        lines_with(&s, "make all")[0].contains("✗ exit 2"),
        "a failing command names its exit code\n{s}"
    );
    assert!(
        lines_with(&s, "mkdir -p build/out")[0].contains("✓ no output"),
        "{s}"
    );
    assert!(
        lines_with(&s, "cat logo.png")[0].contains("binary"),
        "binary output is named, not drawn\n{s}"
    );
    assert!(
        lines_with(&s, "src/render.rs")[0].contains("✓ 140 lines"),
        "{s}"
    );
    assert!(lines_with(&s, "wrap(")[0].contains("12 results"), "{s}");
    assert!(
        lines_with(&s, "brave:browser_click")[0]
            .trim_start()
            .starts_with("▸ Tool"),
        "{s}"
    );
    assert!(
        lines_with(&s, "src/main.rs")
            .iter()
            .any(|l| l.contains("✗ denied")),
        "{s}"
    );
    assert!(s.contains("you said: use a flag instead"), "{s}");
    assert_snapshot("rich-110x40", &s);
}

#[tokio::test]
async fn detail_mode_keeps_colour_and_cuts_long_output() {
    let mut hx = after("rich output", 110, 110).await;
    hx.ctrl('o');
    let s = hx.screen();
    // The 140 line read is cut with a count of what is left.
    assert!(
        s.lines()
            .any(|l| l.contains("more lines  e opens in pager")),
        "{s}"
    );
    // Colour from `ls --color` arrives as colour, not as escape bytes.
    assert!(!s.contains('\u{1b}'));
    let ansi = hx.ansi();
    assert!(
        ansi.contains("34m") || ansi.contains("38;2"),
        "colours kept"
    );
    // The 5000-char-style line takes a few rows and says how much it cut.
    assert!(s.contains("more chars"), "{s}");
}

#[tokio::test]
async fn parallel_calls_each_keep_their_own_row_and_spinner() {
    let events = events_for("parallel calls").await;
    let mut hx = started(100, 30);
    send(&mut hx, "parallel calls");
    // Only the first three events: three calls, none finished.
    hx.events(events.into_iter().take(5).collect(), 100);
    let s = hx.screen();
    assert_eq!(lines_with(&s, "cargo build --release").len(), 1, "{s}");
    assert_eq!(lines_with(&s, "pytest -x tests/").len(), 1, "{s}");
    assert!(
        lines_with(&s, "doc.rust-lang.org")[0].contains("Fetch"),
        "{s}"
    );
}

// ---- subagents -----------------------------------------------------------------------------

#[tokio::test]
async fn subagent_rows_show_live_counts_then_a_summary_and_the_focus_key_opens_one() {
    let events = events_for("agents please").await;
    let mut hx = started(110, 36);
    send(&mut hx, "agents please");
    let mid: Vec<Event> = events.iter().take(10).cloned().collect();
    hx.events(mid, 300);
    let s = hx.screen();
    let running = lines_with(&s, "survey callers of wrap()")[0];
    assert!(running.contains("tools"), "live tool count: {running}");
    assert!(
        lines_with(&s, "find the test layout")[0].contains("tools"),
        "{s}"
    );
    // Finish everything.
    hx.events(events.iter().skip(10).cloned().collect(), 300);
    let s = hx.screen();
    let done = lines_with(&s, "survey callers of wrap()")[0];
    assert!(done.contains("✓") && done.contains("4 tools"), "{done}");
    assert!(!s.contains("├─"), "folded subagents show no tree\n{s}");
    // `a` in nav mode jumps to a subagent and opens it with every call.
    hx.key(KeyCode::Esc);
    hx.key(KeyCode::Char('a'));
    let s = hx.screen();
    assert!(s.contains("├─") && s.contains("└─"), "{s}");
    assert!(s.contains("src/layout.rs"), "{s}");
    assert!(s.contains("NAV"), "{s}");
}

// ---- permission panels ---------------------------------------------------------------------

async fn panel(prompt: &str, w: u16, h: u16) -> Harness {
    let mut hx = after(prompt, w, h).await;
    hx.advance(400);
    hx
}

#[tokio::test]
async fn each_kind_of_call_gets_its_own_panel() {
    let mut f = panel("perm fetch", 100, 30).await;
    let s = f.screen();
    assert!(s.contains("─ Fetch https://docs.rs/ratatui"), "{s}");
    assert!(
        s.contains("url") && s.contains("List the widgets") && s.contains("Fetch this page?"),
        "{s}"
    );
    assert!(s.contains("a yes, for docs.rs"), "{s}");

    let mut m = panel("perm mcp", 100, 30).await;
    let s = m.screen();
    assert!(s.contains("─ Tool brave:browser_click"), "{s}");
    assert!(s.contains("selector") && s.contains("button#submit"), "{s}");
    assert!(s.contains("Call this tool?"), "{s}");

    let mut a = panel("perm agent", 100, 30).await;
    let s = a.screen();
    assert!(s.contains("─ Task survey callers of wrap()"), "{s}");
    assert!(
        s.contains("type") && s.contains("Explore") && s.contains("prompt"),
        "{s}"
    );
    assert!(!s.contains("a yes"), "no rule, no always key\n{s}");

    let mut r = panel("perm read", 100, 30).await;
    let s = r.screen();
    assert!(s.contains("─ Read /etc/hosts") && s.contains("path"), "{s}");
    assert!(s.contains("outside /work/proj"), "{s}");
}

#[tokio::test]
async fn a_call_waiting_on_you_is_named_once_by_the_panel_not_twice() {
    // Finding 7: the card row and the panel rule printed the same title on adjacent rows.
    let mut hx = panel("perm bash", 100, 30).await;
    let s = hx.screen();
    assert!(
        lines_with(&s, "cargo test --workspace").len() == 1,
        "the command shows once, in the panel body\n{s}"
    );
    assert!(s.contains("─ Bash"), "{s}");
    assert!(!s.contains("waiting for you"), "{s}");
    hx.key(KeyCode::Char('y'));
    hx.advance(2500);
    let s = hx.screen();
    assert!(!s.contains("waiting for you"), "{s}");
    // The clock started when it was allowed to run, so it reads about 2.5 s, not the wait.
    let row = lines_with(&s, "cargo test --workspace")[0];
    assert!(row.contains("2.5s") || row.contains("2.6s"), "{row}");
}

#[tokio::test]
async fn a_new_file_panel_is_cut_and_points_at_the_full_diff() {
    let mut hx = panel("perm write", 100, 30).await;
    let s = hx.screen();
    assert!(s.contains("Create this file?"), "{s}");
    assert!(
        s.lines().any(|l| l.contains("more rows  d full diff")),
        "{s}"
    );
    assert!(s.contains("a yes, edits this session"), "{s}");
    assert_snapshot("perm-write-100x30", &s);
}

#[tokio::test]
async fn the_plan_is_read_in_the_panel_and_a_note_says_what_to_change() {
    let mut hx = panel("exit plan now", 100, 36).await;
    let s = hx.screen();
    assert!(s.contains("─ Plan"), "{s}");
    assert!(
        s.contains("Add a --width flag") && s.contains("Start on this plan?"),
        "{s}"
    );
    assert!(s.contains("n no, keep planning"), "{s}");
    hx.sent();
    hx.key(KeyCode::Char('e'));
    hx.type_str("keep it to two steps");
    hx.key(KeyCode::Enter);
    assert_eq!(
        hx.sent(),
        vec![Request::Decide {
            id: "plan-1".into(),
            allow: false,
            scope: DecideScope::Once,
            note: "keep it to two steps".into()
        }]
    );
}

#[tokio::test]
async fn approving_a_plan_leaves_one_plan_row() {
    let mut hx = panel("exit plan now", 100, 36).await;
    hx.sent();
    hx.key(KeyCode::Char('y'));
    assert!(matches!(
        hx.sent().as_slice(),
        [Request::Decide { allow: true, .. }]
    ));
    // The backend's side of it: the call completes.
    let mut call = ToolCall {
        id: "plan1".into(),
        name: "ExitPlanMode".into(),
        kind: ToolKind::Other,
        title: "Add a --width flag".into(),
        input: serde_json::json!({"plan": "## Add a --width flag\n- step"}),
        status: ToolStatus::Completed,
        output: Some("User has approved your plan.".into()),
        ..Default::default()
    };
    hx.event(Event::Tool(call.clone()), 50);
    let s = hx.screen();
    let row = lines_with(&s, "Add a --width flag")[0];
    assert!(row.contains("Plan") && row.contains("✓ approved"), "{row}");
    // Refused with a note: the card says so and keeps what you said under it.
    call.status = ToolStatus::Failed;
    call.output = Some("The user denied this tool call and said: shorter".into());
    hx.event(Event::Tool(call), 50);
    let s = hx.screen();
    assert!(
        s.contains("✗ denied") && s.contains("you said: shorter"),
        "{s}"
    );
}

#[tokio::test]
async fn denied_edit_reads_as_denied_with_the_note_and_no_diff_until_opened() {
    let mut hx = panel("perm edit", 100, 30).await;
    hx.sent();
    hx.key(KeyCode::Char('e'));
    hx.type_str("use a flag");
    hx.key(KeyCode::Enter);
    assert!(matches!(
        hx.sent().as_slice(),
        [Request::Decide { allow: false, note, .. }] if note == "use a flag"
    ));
    let mut call = ToolCall {
        id: "pe".into(),
        name: "Edit".into(),
        kind: ToolKind::Edit,
        title: "src/main.rs".into(),
        status: ToolStatus::Failed,
        output: Some("The user denied this tool call and said: use a flag".into()),
        diff: Some(agent_core::FileDiff {
            path: "/nonexistent/src/main.rs".into(),
            old: Some("a\n".into()),
            new: "b\n".into(),
        }),
        ..Default::default()
    };
    call.input = serde_json::json!({"file_path": "src/main.rs"});
    hx.event(Event::Tool(call), 50);
    let s = hx.screen();
    let head = lines_with(&s, "Edit    src/main.rs")
        .into_iter()
        .find(|l| l.contains("denied"))
        .unwrap_or_default();
    assert!(head.contains("✗ denied · +"), "{s}");
    assert!(s.contains("you said: use a flag"), "{s}");
    assert!(
        !s.lines().any(|l| l.contains(" + b")),
        "the diff stays folded\n{s}"
    );
}

#[tokio::test]
async fn typed_ahead_enter_does_not_answer_a_question() {
    let mut hx = after("ask me something", 100, 30).await;
    hx.sent();
    hx.advance(40);
    hx.key(KeyCode::Enter);
    assert!(hx.sent().is_empty(), "inside the grace window");
    hx.advance(400);
    hx.key(KeyCode::Enter);
    assert!(
        hx.sent().is_empty(),
        "the first answer only moves to question 2"
    );
}

#[tokio::test]
async fn questions_take_single_multi_and_typed_answers() {
    let mut hx = panel("ask me something", 110, 34).await;
    let s = hx.screen();
    assert!(s.contains("─ Language · 1 of 2"), "{s}");
    assert!(
        s.contains("Which language should the tool be written in?"),
        "{s}"
    );
    assert!(
        s.contains("Rust") && s.contains("Go") && s.contains("Python") && s.contains("Other…"),
        "{s}"
    );
    assert_snapshot("ask-q1-110x34", &s);
    hx.sent();
    // Single choice: move to Go and take it.
    hx.key(KeyCode::Down);
    hx.key(KeyCode::Enter);
    let s = hx.screen();
    assert!(
        s.contains("─ Extras · 2 of 2") && s.contains("[ ] Tests"),
        "{s}"
    );
    // Multiple choice: pick Tests and CI.
    hx.key(KeyCode::Char(' '));
    hx.key(KeyCode::Down);
    hx.key(KeyCode::Down);
    hx.key(KeyCode::Char(' '));
    let s = hx.screen();
    assert!(s.contains("[x] Tests") && s.contains("[x] CI"), "{s}");
    hx.key(KeyCode::Enter);
    let sent = hx.sent();
    assert_eq!(
        sent,
        vec![Request::Answer {
            id: "ask-1".into(),
            answers: vec![
                (
                    "Which language should the tool be written in?".into(),
                    "Go".into()
                ),
                ("Which extras do you want?".into(), "Tests, CI".into()),
            ]
        }]
    );
    assert!(hx.app.perm.is_none());
}

#[tokio::test]
async fn a_typed_answer_and_esc_to_skip() {
    let mut hx = panel("ask me something", 110, 34).await;
    hx.sent();
    hx.key(KeyCode::Up); // wraps to the typed-answer row
    hx.key(KeyCode::Enter);
    hx.type_str("Zig");
    let s = hx.screen();
    assert!(s.contains("Zig"), "{s}");
    hx.key(KeyCode::Enter);
    hx.key(KeyCode::Char('1'));
    hx.key(KeyCode::Enter);
    let sent = hx.sent();
    assert!(
        matches!(&sent[..], [Request::Answer { answers, .. }] if answers[0].1 == "Zig" && answers[1].1 == "Tests"),
        "{sent:?}"
    );
    // Skipping tells the model so.
    let mut hx = panel("ask me something", 110, 34).await;
    hx.sent();
    hx.key(KeyCode::Esc);
    assert!(matches!(
        hx.sent().as_slice(),
        [Request::Decide { allow: false, note, .. }] if note.contains("not to answer")
    ));
}

#[tokio::test]
async fn an_answered_question_reads_as_one_row() {
    let mut hx = panel("ask me something", 110, 34).await;
    hx.key(KeyCode::Enter);
    hx.key(KeyCode::Enter);
    let reqs = hx.sent();
    let Some(Request::Answer { answers, .. }) =
        reqs.iter().find(|r| matches!(r, Request::Answer { .. }))
    else {
        panic!("{reqs:?}")
    };
    // The mock answers the way the CLI does.
    let text = answers
        .iter()
        .map(|(q, a)| format!("\"{q}\"=\"{a}\""))
        .collect::<Vec<_>>()
        .join(", ");
    let input = serde_json::json!({"questions": [
        {"header": "Language", "question": "Which language should the tool be written in?"},
        {"header": "Extras", "question": "Which extras do you want?"}]});
    hx.event(
        Event::Tool(ToolCall {
            id: "ask1".into(),
            name: "AskUserQuestion".into(),
            title: "Which language should the tool be written in? (+1 more)".into(),
            input,
            status: ToolStatus::Completed,
            output: Some(format!(
                "Your questions have been answered: {text}. You can now continue."
            )),
            ..Default::default()
        }),
        50,
    );
    let s = hx.screen();
    let row = lines_with(&s, "Which language should")[0];
    assert!(row.contains("Ask") && row.contains("✓ Rust"), "{row}");
}

#[tokio::test]
async fn parallel_requests_queue_behind_each_other_and_none_is_dropped() {
    let mut hx = started(100, 30);
    send(&mut hx, "two at once");
    hx.event(Event::TurnStart, 10);
    let req = |id: &str, cmd: &str| {
        Event::Permission(PermissionRequest {
            id: id.into(),
            tool: "Bash".into(),
            kind: ToolKind::Execute,
            title: cmd.into(),
            input: serde_json::json!({"command": cmd}),
            diff: None,
            rule: String::new(),
        })
    };
    hx.event(req("p1", "sleep 2"), 10);
    hx.event(req("p2", "sleep 3"), 10);
    hx.sent();
    hx.advance(400);
    let s = hx.screen();
    assert!(
        s.contains("$ sleep 2") && s.contains("1 more waiting"),
        "{s}"
    );
    hx.key(KeyCode::Char('y'));
    assert!(matches!(
        hx.sent().as_slice(),
        [Request::Decide { id, allow: true, .. }] if id == "p1"
    ));
    hx.advance(400);
    let s = hx.screen();
    assert!(
        s.contains("$ sleep 3") && !s.contains("more waiting"),
        "{s}"
    );
    hx.key(KeyCode::Char('n'));
    assert!(matches!(
        hx.sent().as_slice(),
        [Request::Decide { id, allow: false, .. }] if id == "p2"
    ));
    assert!(hx.app.perm.is_none());
}

// ---- the diff viewer -----------------------------------------------------------------------

#[tokio::test]
async fn d_opens_the_viewer_on_the_edit_and_esc_goes_back() {
    let mut hx = after("edits please", 150, 36).await;
    hx.key(KeyCode::Esc);
    hx.key(KeyCode::Char('d'));
    let s = hx.screen();
    assert!(s.lines().next().unwrap().contains("diff"), "{s}");
    assert!(s.contains("4 files") || s.contains("3 files"), "{s}");
    assert!(s.contains("src/main.rs") && s.contains("src/lib.rs"), "{s}");
    assert!(s.contains("[/] file") && s.contains("esc back"), "{s}");
    // 150 columns: split view, a replaced line on one row.
    assert!(
        s.lines()
            .any(|l| l.contains("hi {name}") && l.contains("hello, {name}")),
        "{s}"
    );
    assert_snapshot("diffview-split-150x36", &s);
    hx.key(KeyCode::Char('s'));
    let s = hx.screen();
    assert!(
        !s.lines()
            .any(|l| l.contains("hi {name}") && l.contains("hello, {name}")),
        "{s}"
    );
    assert!(s.contains("split"), "the key now offers split\n{s}");
    // Next file, previous file, hunk keys.
    hx.key(KeyCode::Char(']'));
    let s = hx.screen();
    assert!(s.contains("greet(name: &str) -> String"), "{s}");
    hx.key(KeyCode::Char('['));
    hx.key(KeyCode::Char('n'));
    hx.key(KeyCode::Char('N'));
    // Back where we were, with the transcript intact.
    hx.key(KeyCode::Esc);
    let s = hx.screen();
    assert!(s.contains("edits please") && s.contains("NAV"), "{s}");
}

#[tokio::test]
async fn narrow_viewer_is_unified_and_the_slash_command_opens_it_too() {
    let mut hx = after("edits please", 70, 24).await;
    send(&mut hx, "/diff");
    let s = hx.screen();
    assert!(s.contains("diff") && s.contains("1/"), "file strip: {s}");
    assert!(s.contains("unified") || s.contains("split"), "{s}");
    for l in s.lines() {
        assert!(tuikit::width::display_width(l) <= 70, "{l:?}");
    }
    hx.key(KeyCode::Char('q'));
    assert!(hx.app.diffview.is_none());
}

#[tokio::test]
async fn the_wheel_scrolls_the_viewer_and_a_click_picks_a_file() {
    use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
    let mut hx = after("edits please", 150, 8).await;
    send(&mut hx, "/diff");
    assert!(hx.screen().contains("modified"));
    let mouse = |kind, column, row| MouseEvent {
        kind,
        column,
        row,
        modifiers: KeyModifiers::NONE,
    };
    let now = hx.now();
    hx.app
        .on_mouse(mouse(MouseEventKind::ScrollDown, 80, 6), now);
    let s = hx.screen();
    assert!(!s.contains("modified"), "scrolled past the file title\n{s}");
    // The second file in the list.
    hx.app
        .on_mouse(mouse(MouseEventKind::Down(MouseButton::Left), 5, 2), now);
    let s = hx.screen();
    assert!(s.contains("greet(name: &str) -> String"), "{s}");
}

#[tokio::test]
async fn no_edits_means_an_honest_empty_viewer() {
    let mut hx = started(100, 24);
    send(&mut hx, "/diff");
    let s = hx.screen();
    assert!(s.contains("No files changed"), "{s}");
    hx.key(KeyCode::Esc);
    assert!(hx.app.diffview.is_none());
}

#[tokio::test]
async fn d_on_a_pending_edit_opens_the_viewer_and_esc_returns_to_the_question() {
    let mut hx = panel("perm edit", 120, 30).await;
    hx.sent();
    hx.key(KeyCode::Char('d'));
    let s = hx.screen();
    assert!(s.contains("waiting for your answer"), "{s}");
    assert!(s.contains("esc back to the question"), "{s}");
    assert!(hx.sent().is_empty(), "looking is not deciding");
    hx.key(KeyCode::Esc);
    let s = hx.screen();
    assert!(s.contains("Allow this edit?"), "{s}");
    assert!(hx.sent().is_empty(), "esc left the viewer, it did not deny");
}

// ---- rewind --------------------------------------------------------------------------------

fn preview(turn: usize) -> RewindPreview {
    RewindPreview {
        turn,
        can_rewind_files: true,
        files: vec![
            "/work/proj/src/main.rs".into(),
            "/work/proj/notes.txt".into(),
        ],
        insertions: 4,
        deletions: 1,
        can_rewind_conversation: true,
        note: String::new(),
    }
}

async fn two_turns() -> Harness {
    let mut hx = started(110, 34);
    let tools = events_for("tools please").await;
    for p in ["first thing", "tools please"] {
        send(&mut hx, p);
        hx.event(Event::TurnStart, 10);
        if p == "tools please" {
            hx.events(tools.iter().skip(1).cloned().collect(), 30);
        } else {
            hx.event(Event::TextDelta("Done.".into()), 10);
            hx.event(Event::TurnEnd(agent_core::StopReason::EndTurn), 10);
        }
    }
    hx.sent();
    hx
}

#[tokio::test]
async fn r_asks_the_backend_what_a_rewind_would_undo_and_shows_the_files() {
    let mut hx = two_turns().await;
    hx.key(KeyCode::Esc);
    hx.key(KeyCode::Char('['));
    hx.key(KeyCode::Char('r'));
    assert_eq!(hx.sent(), vec![Request::RewindPreview { turn: 1 }]);
    let s = hx.screen();
    assert!(s.contains("─ Rewind") && s.contains("tools please"), "{s}");
    assert!(s.contains("asking claude"), "{s}");
    hx.event(Event::RewindPreview(preview(1)), 20);
    let s = hx.screen();
    assert!(
        s.contains("put back 2 files (src/main.rs, notes.txt), +4 -1"),
        "{s}"
    );
    assert_snapshot("rewind-110x34", &s);
    // Conversation and files: the request goes out and the message comes back to edit.
    hx.key(KeyCode::Char('1'));
    assert_eq!(
        hx.sent(),
        vec![Request::Rewind {
            turn: 1,
            conversation: true,
            files: true
        }]
    );
    assert_eq!(hx.app.composer.text(), "tools please");
    assert!(hx.app.rewind.is_none());
}

#[tokio::test]
async fn a_backend_with_no_rewind_leaves_one_honest_choice() {
    let mut hx = two_turns().await;
    hx.key(KeyCode::Esc);
    hx.key(KeyCode::Char('['));
    hx.key(KeyCode::Char('r'));
    hx.sent();
    // No preview ever comes. After the wait the panel says so and offers the fallback.
    hx.advance(2000);
    let s = hx.screen();
    assert!(s.contains("just edit this message"), "{s}");
    assert!(!s.contains("conversation and files"), "{s}");
    hx.key(KeyCode::Enter);
    assert!(
        hx.sent().is_empty(),
        "nothing is sent to a backend that cannot rewind"
    );
    assert_eq!(hx.app.composer.text(), "tools please");
    assert!(hx.screen().contains("nothing undone"));
}

#[tokio::test]
async fn files_only_leaves_the_composer_alone_and_esc_cancels() {
    let mut hx = two_turns().await;
    hx.key(KeyCode::Esc);
    hx.key(KeyCode::Char('['));
    hx.key(KeyCode::Char('r'));
    hx.sent();
    hx.event(Event::RewindPreview(preview(1)), 20);
    hx.key(KeyCode::Char('3'));
    assert_eq!(
        hx.sent(),
        vec![Request::Rewind {
            turn: 1,
            conversation: false,
            files: true
        }]
    );
    assert!(hx.app.composer.is_empty());
    hx.key(KeyCode::Char('r'));
    hx.sent();
    hx.key(KeyCode::Esc);
    assert!(hx.app.rewind.is_none() && hx.sent().is_empty());
}

#[tokio::test]
async fn rewinding_under_a_running_turn_only_copies_the_message() {
    let mut hx = two_turns().await;
    send(&mut hx, "something slow");
    hx.event(Event::TurnStart, 10);
    hx.sent();
    hx.key(KeyCode::PageUp);
    hx.app.act(openc::keys::Action::NavUp, hx.now());
    hx.key(KeyCode::Char('r'));
    assert!(hx
        .sent()
        .iter()
        .all(|r| !matches!(r, Request::Rewind { .. } | Request::RewindPreview { .. })));
}

// ---- checklist items 15 and 21 -------------------------------------------------------------

#[tokio::test]
async fn item_15_diffs_are_drawn_on_the_palette_tints_and_nothing_else() {
    use openc::palette::{Depth, Kind, Palette};
    use ratatui::style::Color;
    let p = Palette::new(Kind::Hearth, Depth::True);
    let allowed = [
        p.bg,
        p.surface,
        p.raised,
        p.add_bg,
        p.del_bg,
        p.add_word,
        p.del_word,
        Color::Reset,
    ];
    let mut screens: Vec<Harness> = vec![
        after("edits please", 120, 50).await,
        panel("perm edit", 120, 30).await,
        panel("perm write", 120, 30).await,
    ];
    let mut v = after("edits please", 150, 36).await;
    v.key(KeyCode::Esc);
    v.key(KeyCode::Char('d'));
    screens.push(v);
    for hx in &mut screens {
        hx.draw();
        let buf = hx.term.backend().buffer().clone();
        let mut seen_add = false;
        for c in buf.content() {
            assert!(
                allowed.contains(&c.bg),
                "{:?} on {:?} is not a palette surface",
                c.bg,
                c.symbol()
            );
            assert_ne!(c.bg, Color::Rgb(255, 0, 0));
            assert_ne!(c.bg, Color::Rgb(0, 255, 0));
            seen_add |= c.bg == p.add_bg;
        }
        assert!(seen_add, "the screen shows a diff");
    }
}

#[tokio::test]
async fn item_21_a_tool_card_is_one_row_at_every_width() {
    for w in [40u16, 60, 80, 120, 160] {
        let mut hx = after("rich output", w, 60).await;
        let s = hx.screen();
        let heads = s
            .lines()
            .filter(|l| {
                let t = l.trim_start();
                let rest: String = t.chars().skip(2).collect();
                ["▸ ", "▾ "].iter().any(|g| t.starts_with(g))
                    && ["Bash", "Read", "Search", "Fetch", "Tool", "Edit"]
                        .iter()
                        .any(|v| rest.starts_with(v))
            })
            .count();
        // Ten calls, ten rows, whatever the width; the denied edit and the failed make
        // all kinds of outcomes.
        assert_eq!(heads, 10, "at {w}:\n{s}");
        for l in s.lines() {
            assert!(tuikit::width::display_width(l) <= w as usize, "{l:?}");
        }
    }
}

#[tokio::test]
async fn ascii_mode_draws_every_new_screen_without_a_non_ascii_glyph() {
    let scenarios = [
        ("rich output", None),
        ("edits please", Some('d')),
        ("ask me something", None),
        ("exit plan now", None),
        ("perm write", None),
        ("agents please", None),
    ];
    for (prompt, key) in scenarios {
        let events = events_for(prompt).await;
        let mut hx = started(110, 40);
        hx.app.g = openc::palette::ASCII;
        send(&mut hx, prompt);
        hx.events(events, 60);
        hx.advance(400);
        if let Some(k) = key {
            hx.key(KeyCode::Esc);
            hx.key(KeyCode::Char(k));
        }
        let s = hx.screen();
        let odd: Vec<char> = s.chars().filter(|c| !c.is_ascii() && *c != '\n').collect();
        assert!(odd.is_empty(), "{prompt}: {odd:?}\n{s}");
    }
    // The rewind panel too.
    let mut hx = two_turns().await;
    hx.app.g = openc::palette::ASCII;
    hx.key(KeyCode::Esc);
    hx.key(KeyCode::Char('['));
    hx.key(KeyCode::Char('r'));
    hx.event(Event::RewindPreview(preview(1)), 10);
    let s = hx.screen();
    // The scrollbar thumb and `truncate`'s ellipsis are shared code with their own fallbacks.
    let odd: Vec<char> = s
        .chars()
        .filter(|c| !c.is_ascii() && *c != '┃' && *c != '…')
        .collect();
    assert!(odd.is_empty(), "rewind: {odd:?}\n{s}");
}

// ---- nothing breaks at any size ------------------------------------------------------------

#[tokio::test]
async fn the_new_screens_draw_at_every_small_size() {
    let rich = events_for("rich output").await;
    let edits = events_for("edits please").await;
    let ask = events_for("ask me something").await;
    let plan = events_for("exit plan now").await;
    let wr = events_for("perm write").await;
    for w in [1u16, 8, 39, 40, 41, 80, 100, 160] {
        for h in [1u16, 4, 12, 30] {
            for events in [&rich, &edits, &ask, &plan, &wr] {
                let mut hx = Harness::new(w, h);
                hx.ready();
                send(&mut hx, "go");
                hx.events(events.clone(), 30);
                hx.advance(400);
                hx.screen();
                for k in [KeyCode::Down, KeyCode::Char(' '), KeyCode::Tab] {
                    hx.key(k);
                    hx.screen();
                }
                hx.key(KeyCode::Esc);
                hx.screen();
                hx.key(KeyCode::Char('d'));
                hx.screen();
                for c in "jknN]s[Gg".chars() {
                    hx.key(KeyCode::Char(c));
                    hx.screen();
                }
                hx.key(KeyCode::Esc);
                hx.key(KeyCode::Esc);
                hx.key(KeyCode::Char('r'));
                hx.screen();
                hx.key(KeyCode::Esc);
                hx.key_mod(KeyCode::Char('c'), KeyModifiers::NONE);
                hx.screen();
            }
        }
    }
}

#[test]
fn a_permission_panel_never_exceeds_its_rect_for_any_request() {
    // A request with every field long enough to need cutting.
    let long = "x".repeat(400);
    let reqs = [
        (
            "Bash",
            ToolKind::Execute,
            serde_json::json!({"command": long, "description": long}),
        ),
        (
            "WebFetch",
            ToolKind::Fetch,
            serde_json::json!({"url": long, "prompt": long}),
        ),
        (
            "mcp__a__b",
            ToolKind::Other,
            serde_json::json!({"k": long, "other": [1, 2, 3]}),
        ),
        (
            "Agent",
            ToolKind::Think,
            serde_json::json!({"description": long, "prompt": long}),
        ),
        (
            "Read",
            ToolKind::Read,
            serde_json::json!({"file_path": format!("/etc/{long}")}),
        ),
    ];
    for (tool, kind, input) in reqs {
        for w in [20u16, 40, 80, 120] {
            let mut hx = Harness::new(w, 24);
            hx.ready();
            send(&mut hx, "go");
            hx.event(Event::TurnStart, 10);
            hx.event(
                Event::Permission(PermissionRequest {
                    id: "p".into(),
                    tool: tool.into(),
                    kind,
                    title: long.clone(),
                    input: input.clone(),
                    diff: None,
                    rule: long.clone(),
                }),
                10,
            );
            hx.advance(400);
            let s = hx.screen();
            for l in s.lines() {
                assert!(
                    tuikit::width::display_width(l) <= w as usize,
                    "{tool} at {w}: {l:?}"
                );
            }
        }
    }
}
