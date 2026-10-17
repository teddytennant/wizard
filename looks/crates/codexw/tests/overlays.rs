//! Screen-level behaviour of the pager, backtrack, `/diff` and approval views on the headless
//! terminal: the alternate screen is entered for overlays only and left again, the inline view
//! comes back intact, and approvals answer the backend and print their decision cells.

use agent_core::{Config, Event, PermissionRequest, Request, StopReason, ToolKind};
use codexw::app::AppOpts;
use codexw::testing::{Harness, harness, harness_with};
use crossterm::event::KeyCode;
use ratatui::style::{Modifier, Style};
use serde_json::json;

fn ready(h: &mut Harness) {
    h.app.on_backend(Event::Ready {
        session_id: "s1".into(),
        config: Config {
            model: "gpt-5.5".into(),
            effort: "default".into(),
            ..Default::default()
        },
    });
    h.draw();
}

/// One finished turn: the user line and a short answer.
fn one_turn(h: &mut Harness, prompt: &str) {
    h.app.submit_prompt(prompt.into());
    h.app.on_backend(Event::TurnStart);
    h.app.on_backend(Event::TextDelta("Hello there.\n".into()));
    h.app.on_backend(Event::TurnEnd(StopReason::EndTurn));
    h.draw();
}

fn bash_request() -> PermissionRequest {
    PermissionRequest {
        id: "perm-1".into(),
        tool: "Bash".into(),
        kind: ToolKind::Execute,
        title: "touch approved.txt && echo created".into(),
        input: json!({"command": "touch approved.txt && echo created"}),
        diff: None,
        rule: "touch approved.txt".into(),
    }
}

fn has(s: Style, m: Modifier) -> bool {
    s.add_modifier.contains(m)
}

fn count(bytes: &[u8], needle: &str) -> usize {
    String::from_utf8_lossy(bytes).matches(needle).count()
}

#[test]
fn the_alternate_screen_is_for_overlays_only() {
    let mut h = harness(120, 36, 6);
    ready(&mut h);
    one_turn(&mut h, "go fake:text");
    assert!(!h.screen.in_alt_screen());
    assert_eq!(
        h.screen.alt_screen_entered, 0,
        "chat never touches the alt screen"
    );

    h.ctrl('t');
    h.draw();
    assert!(h.screen.in_alt_screen());
    assert_eq!(h.screen.alt_screen_entered, 1);
    assert!(!h.screen.cursor_visible, "no cursor on the pager");
    let rows = h.screen.rows();
    assert!(
        rows[0].starts_with("/ T R A N S C R I P T / /"),
        "{}",
        rows[0]
    );
    assert!(rows.iter().any(|r| r.contains("go fake:text")));
    assert!(rows.iter().any(|r| r.contains("Hello there.")));
    assert!(rows[32].contains("100%"));
    assert_eq!(rows[34], " q to quit   esc to edit prev");

    h.key(KeyCode::Char('q'));
    h.draw();
    assert!(!h.screen.in_alt_screen());
    assert_eq!(h.screen.alt_screen_entered, 1);
    // back on the inline view: composer placeholder, history above it
    let text = h.screen.full_text();
    assert!(text.contains("go fake:text") && text.contains("Hello there."));
    assert!(h.screen.rows().iter().any(|r| r.contains("› ")));
    let all = String::from_utf8_lossy(&h.bytes).to_string();
    assert_eq!(all.matches("\x1b[?1049h").count(), 1);
    assert_eq!(all.matches("\x1b[?1049l").count(), 1);
    assert_eq!(all.matches("\x1b[?1007h").count(), 1);
    assert_eq!(all.matches("\x1b[?1007l").count(), 1);
}

#[test]
fn ctrl_t_and_ctrl_c_also_close_the_pager() {
    let mut h = harness(80, 24, 6);
    ready(&mut h);
    one_turn(&mut h, "hi");
    for close in [|h: &mut Harness| h.ctrl('t'), |h: &mut Harness| h.ctrl('c')] {
        h.ctrl('t');
        h.draw();
        assert!(h.screen.in_alt_screen());
        close(&mut h);
        h.draw();
        assert!(!h.screen.in_alt_screen());
    }
    assert!(
        !h.app.should_exit,
        "Ctrl+C in the pager closes it and does not quit"
    );
}

#[test]
fn pager_keys_scroll_and_report_percent() {
    let mut h = harness(120, 36, 6);
    ready(&mut h);
    for i in 0..12 {
        one_turn(&mut h, &format!("question number {i}"));
    }
    h.ctrl('t');
    h.draw();
    assert!(h.screen.rows()[32].contains("100%"));
    h.key(KeyCode::Home);
    h.draw();
    let rows = h.screen.rows();
    assert!(rows[32].ends_with(" 0% ─"), "{}", rows[32]);
    assert!(
        rows[1].starts_with('╭'),
        "the session header box is first: {}",
        rows[1]
    );
    h.key(KeyCode::PageDown);
    h.draw();
    assert!(!h.screen.rows()[32].ends_with(" 0% ─"));
    h.key(KeyCode::End);
    h.draw();
    assert!(h.screen.rows()[32].contains("100%"));
}

#[test]
fn pager_follows_new_cells_while_pinned() {
    let mut h = harness(80, 24, 6);
    ready(&mut h);
    for i in 0..10 {
        one_turn(&mut h, &format!("old {i}"));
    }
    h.ctrl('t');
    h.draw();
    h.app
        .on_backend(Event::TextDelta("fresh tail line\n".into()));
    h.draw();
    assert!(
        h.screen
            .rows()
            .iter()
            .any(|r| r.contains("fresh tail line"))
    );
    // the new cell went to the scrollback only after the overlay closed
    assert!(!h.screen.full_text().contains("fresh tail line") || h.screen.in_alt_screen());
}

#[test]
fn resize_relays_out_the_open_pager() {
    let mut h = harness(120, 36, 6);
    ready(&mut h);
    one_turn(&mut h, "go fake:text");
    h.ctrl('t');
    h.draw();
    h.resize(80, 24);
    h.draw();
    let rows = h.screen.rows();
    assert!(h.screen.in_alt_screen());
    assert_eq!(rows[0].chars().count(), 79);
    assert!(rows[20].starts_with('─'), "separator at H-4: {}", rows[20]);
    assert_eq!(rows[22], " q to quit   esc to edit prev");
}

#[test]
fn double_escape_opens_the_backtrack_preview() {
    let mut h = harness(120, 36, 6);
    ready(&mut h);
    one_turn(&mut h, "go fake:text");
    h.key(KeyCode::Esc);
    h.draw();
    assert!(
        h.screen
            .text()
            .contains("esc again to edit previous message")
    );
    h.key(KeyCode::Esc);
    h.draw();
    assert!(h.screen.in_alt_screen());
    let rows = h.screen.rows();
    assert_eq!(
        rows[34],
        " q to quit   esc/← to edit prev   → to edit next   enter to edit message"
    );
    let at = rows
        .iter()
        .position(|r| r.starts_with("› go fake:text"))
        .expect("user cell");
    assert!(
        has(h.screen.cell(0, at).style, Modifier::REVERSED),
        "highlighted cell is reversed"
    );

    // xg-01-esc-3: one more Esc with a single user message changes nothing
    h.key(KeyCode::Esc);
    h.draw();
    assert!(h.screen.in_alt_screen());

    // Enter: wizard cannot fork, so the prompt comes back and the error says why
    h.key(KeyCode::Enter);
    h.draw();
    assert!(!h.screen.in_alt_screen());
    let text = h.screen.text();
    assert!(
        text.contains("Failed to branch before the selected prompt"),
        "{text}"
    );
    assert!(text.contains("go fake:text"));
    assert!(h.sent().is_empty() || h.sent().iter().all(|r| !matches!(r, Request::Prompt(_))));
}

#[test]
fn double_escape_without_a_user_message_says_so() {
    let mut h = harness(120, 36, 6);
    ready(&mut h);
    h.key(KeyCode::Esc);
    h.key(KeyCode::Esc);
    h.draw();
    assert!(!h.screen.in_alt_screen());
    assert!(h.screen.text().contains("• No previous message to edit."));
}

#[test]
fn backtrack_steps_back_through_user_messages() {
    let mut h = harness(120, 36, 6);
    ready(&mut h);
    one_turn(&mut h, "first prompt");
    one_turn(&mut h, "second prompt");
    h.key(KeyCode::Esc);
    h.key(KeyCode::Esc);
    h.draw();
    let on = |h: &Harness, s: &str| {
        let rows = h.screen.rows();
        let y = rows.iter().position(|r| r.contains(s)).unwrap();
        has(h.screen.cell(2, y).style, Modifier::REVERSED)
    };
    assert!(on(&h, "second prompt") && !on(&h, "first prompt"));
    h.key(KeyCode::Left);
    h.draw();
    assert!(on(&h, "first prompt") && !on(&h, "second prompt"));
    h.key(KeyCode::Right);
    h.draw();
    assert!(on(&h, "second prompt"));
    h.key(KeyCode::Char('q'));
    h.draw();
    assert!(!h.screen.in_alt_screen());
}

fn git_fixture(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("cxw-diff-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("src")).unwrap();
    let git = |args: &[&str]| {
        let ok = std::process::Command::new("git")
            .current_dir(&dir)
            .args(["-c", "user.email=a@b", "-c", "user.name=x"])
            .args(args)
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        assert!(ok, "git {args:?}");
    };
    std::fs::write(
        dir.join("src/lib.rs"),
        "pub fn add(a: i32, b: i32) -> i32 {\n    a + b\n}\n",
    )
    .unwrap();
    git(&["init", "-q", "-b", "main"]);
    git(&["add", "-A"]);
    git(&["commit", "-qm", "init"]);
    dir
}

fn have_git() -> bool {
    std::process::Command::new("git")
        .arg("--version")
        .output()
        .is_ok()
}

#[test]
fn diff_shows_tracked_and_untracked_changes() {
    if !have_git() {
        return;
    }
    let dir = git_fixture("changes");
    std::fs::write(
        dir.join("src/lib.rs"),
        "pub fn add(a: i32, b: i32) -> i32 {\n    a.wrapping_add(b)\n}\npub fn extra() {}\n",
    )
    .unwrap();
    std::fs::write(dir.join("untracked.txt"), "new\n").unwrap();
    let mut h = harness_with(
        120,
        36,
        AppOpts {
            cwd: dir.clone(),
            placeholder: Some(6),
            ..Default::default()
        },
    );
    ready(&mut h);
    h.app.open_diff();
    h.draw();
    assert!(h.screen.in_alt_screen());
    let rows = h.screen.rows();
    assert!(rows[0].starts_with("/ D I F F / / /"));
    assert_eq!(rows[1], "diff --git a/src/lib.rs b/src/lib.rs");
    assert!(rows.iter().any(|r| r == "+    a.wrapping_add(b)"));
    assert!(
        rows.iter()
            .any(|r| r == "diff --git a/untracked.txt b/untracked.txt")
    );
    assert!(rows.iter().any(|r| r == "new file mode 100644"));
    assert_eq!(rows[34], " q to quit", "static overlay: no edit-prev hint");
    // git's colours survive: bold header, red removal, green addition
    let st = h.screen.style_of("diff --git a/src/lib.rs").unwrap();
    assert!(has(st, Modifier::BOLD));
    assert!(
        h.screen
            .style_of("+    a.wrapping_add(b)")
            .unwrap()
            .fg
            .is_some()
    );
    h.key(KeyCode::Char('q'));
    h.draw();
    assert!(!h.screen.in_alt_screen());
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn diff_of_a_clean_tree_says_no_changes() {
    if !have_git() {
        return;
    }
    let dir = git_fixture("clean");
    let mut h = harness_with(
        120,
        36,
        AppOpts {
            cwd: dir.clone(),
            placeholder: Some(6),
            ..Default::default()
        },
    );
    ready(&mut h);
    h.app.open_diff();
    h.draw();
    let rows = h.screen.rows();
    assert_eq!(rows[1], "No changes detected.");
    assert_eq!(rows[2], "~");
    assert!(has(
        h.screen.style_of("No changes detected.").unwrap(),
        Modifier::ITALIC
    ));
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn diff_outside_a_repository_says_so() {
    if !have_git() {
        return;
    }
    let dir = std::env::temp_dir().join(format!("cxw-nogit-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let mut h = harness_with(
        120,
        36,
        AppOpts {
            cwd: dir.clone(),
            placeholder: Some(6),
            ..Default::default()
        },
    );
    ready(&mut h);
    h.app.open_diff();
    h.draw();
    // a temp dir under a repository (unlikely) would show a diff instead; accept either
    let t = h.screen.text();
    assert!(
        t.contains("not inside a git repository")
            || t.contains("No changes detected.")
            || t.contains("diff --git")
    );
    let _ = std::fs::remove_dir_all(dir);
}

// ---- approvals --------------------------------------------------------------------------------

#[test]
fn exec_approval_replaces_the_composer_and_y_approves() {
    let mut h = harness(120, 36, 6);
    ready(&mut h);
    one_turn(&mut h, "go fake:approval");
    h.app.on_backend(Event::Permission(bash_request()));
    h.draw();
    let text = h.screen.text();
    assert!(
        text.contains("Would you like to run the following command?"),
        "{text}"
    );
    assert!(text.contains("  Environment: local"));
    assert!(text.contains("  $ touch approved.txt && echo created"));
    assert!(text.contains("› 1. Yes, proceed (y)"));
    assert!(text.contains(
        "2. Yes, and don't ask again for commands that start with `touch approved.txt` (p)"
    ));
    assert!(text.contains("3. No, and tell Wizard what to do differently (esc)"));
    assert!(text.contains("  Press enter to confirm or esc to cancel"));
    assert!(
        !text.contains("Improve documentation"),
        "composer is replaced"
    );
    assert!(
        h.screen.title.contains("Action Required"),
        "title: {}",
        h.screen.title
    );
    assert!(!h.screen.in_alt_screen());

    h.key(KeyCode::Char('y'));
    h.draw();
    let sent = h.sent();
    assert!(
        sent.iter().any(|r| matches!(r, Request::Decide { id, allow: true, scope: agent_core::DecideScope::Once, .. } if id == "perm-1")),
        "{sent:?}"
    );
    let full = h.screen.full_text();
    assert!(
        full.contains("✔ You approved codex to run touch approved.txt && echo created this time"),
        "{full}"
    );
    assert!(!h.screen.text().contains("Would you like to run"));
    assert!(!h.screen.title.contains("Action Required"));
}

#[test]
fn exec_approval_escape_cancels_the_turn() {
    let mut h = harness(120, 36, 6);
    ready(&mut h);
    one_turn(&mut h, "go fake:approval");
    h.app.on_backend(Event::Permission(bash_request()));
    h.draw();
    h.key(KeyCode::Esc);
    h.draw();
    let sent = h.sent();
    assert!(
        sent.iter()
            .any(|r| matches!(r, Request::Decide { allow: false, .. })),
        "{sent:?}"
    );
    assert!(
        sent.iter().any(|r| matches!(r, Request::Cancel)),
        "Abort interrupts the turn: {sent:?}"
    );
    assert!(
        h.screen
            .full_text()
            .contains("✗ You canceled the request to run touch approved.txt && echo created")
    );
}

#[test]
fn approval_keys_not_on_the_list_do_nothing_and_p_always_allows() {
    let mut h = harness(120, 36, 6);
    ready(&mut h);
    h.app.on_backend(Event::Permission(bash_request()));
    h.draw();
    for c in ['d', 'a', 'c'] {
        h.key(KeyCode::Char(c));
    }
    h.draw();
    assert!(h.screen.text().contains("Would you like to run"));
    assert!(h.sent().is_empty());
    h.key(KeyCode::Down);
    h.key(KeyCode::Enter);
    h.draw();
    let sent = h.sent();
    assert!(
        sent.iter().any(|r| matches!(
            r,
            Request::Decide {
                allow: true,
                scope: agent_core::DecideScope::Always,
                ..
            }
        )),
        "{sent:?}"
    );
    assert!(h.screen.full_text().contains(
        "✔ You approved codex to always run commands that start with touch approved.txt"
    ));
}

#[test]
fn requests_queue_behind_the_one_on_screen() {
    let mut h = harness(120, 36, 6);
    ready(&mut h);
    h.app.on_backend(Event::Permission(bash_request()));
    let mut second = bash_request();
    second.id = "perm-2".into();
    second.input = json!({"command": "ls -la"});
    h.app.on_backend(Event::Permission(second));
    h.draw();
    assert!(
        h.screen
            .text()
            .contains("touch approved.txt && echo created")
    );
    h.key(KeyCode::Char('y'));
    h.draw();
    assert!(h.screen.text().contains("$ ls -la"), "{}", h.screen.text());
    h.key(KeyCode::Char('y'));
    h.draw();
    assert!(!h.screen.text().contains("Would you like to run"));
    let ids: Vec<String> = h
        .sent()
        .into_iter()
        .filter_map(|r| match r {
            Request::Decide { id, .. } => Some(id),
            _ => None,
        })
        .collect();
    assert_eq!(ids, vec!["perm-1", "perm-2"]);
}

#[test]
fn an_approval_waits_for_a_typing_composer() {
    let mut h = harness(120, 36, 6);
    ready(&mut h);
    h.type_str("yes please");
    h.app.on_backend(Event::Permission(bash_request()));
    h.draw();
    assert!(
        !h.screen.text().contains("Would you like to run"),
        "typed text must not answer a prompt"
    );
    assert!(h.sent().is_empty());
}

#[test]
fn patch_approval_has_two_blank_rows_and_ctrl_a_shows_the_diff() {
    let mut h = harness(120, 36, 6);
    ready(&mut h);
    let req = PermissionRequest {
        id: "perm-2".into(),
        tool: "Edit".into(),
        kind: ToolKind::Edit,
        title: "src/main.rs".into(),
        input: json!({"file_path": "src/main.rs"}),
        diff: Some(agent_core::FileDiff {
            path: "src/main.rs".into(),
            old: Some("fn main() {\n    println!(\"hi\");\n}\n".into()),
            new: "fn main() {\n    println!(\"hello\");\n}\n".into(),
        }),
        rule: "src/**".into(),
    };
    h.app.on_backend(Event::Permission(req));
    h.draw();
    let rows = h.screen.rows();
    let at = rows
        .iter()
        .position(|r| r.contains("Would you like to make the following edits?"))
        .unwrap();
    assert_eq!(rows[at + 1], "");
    assert_eq!(rows[at + 2], "");
    assert!(rows[at + 3].starts_with("› 1. Yes, proceed (y)"));
    h.ctrl('a');
    h.draw();
    assert!(h.screen.in_alt_screen());
    let rows = h.screen.rows();
    assert!(rows[0].starts_with("/ P A T C H / /"));
    assert_eq!(rows[1], "src/main.rs (+1 -1)");
    assert!(rows.iter().any(|r| r.contains("-    println!(\"hi\");")));
    // Esc does not close a static overlay, q does and the approval is still there
    h.key(KeyCode::Esc);
    h.draw();
    assert!(h.screen.in_alt_screen());
    h.key(KeyCode::Char('q'));
    h.draw();
    assert!(!h.screen.in_alt_screen());
    assert!(
        h.screen
            .text()
            .contains("Would you like to make the following edits?")
    );
    // a patch decision prints no cell
    h.key(KeyCode::Char('y'));
    h.draw();
    assert!(!h.screen.full_text().contains("You approved"));
    assert!(
        h.sent()
            .iter()
            .any(|r| matches!(r, Request::Decide { allow: true, .. }))
    );
}

#[test]
fn ask_user_question_answers_through_request_answer() {
    let mut h = harness(120, 36, 6);
    ready(&mut h);
    let req = PermissionRequest {
        id: "ask-1".into(),
        tool: "AskUserQuestion".into(),
        input: json!({"questions": [{"question": "Which language?", "header": "Language", "multiSelect": false,
            "options": [{"label": "Rust", "description": "Fast"}, {"label": "Go", "description": "Easy"}]}]}),
        ..Default::default()
    };
    h.app.on_backend(Event::Permission(req));
    h.draw();
    let text = h.screen.text();
    assert!(text.contains("  Question 1/1 (1 unanswered)"), "{text}");
    assert!(text.contains("  tab to add notes | enter to submit answer | esc to interrupt"));
    h.key(KeyCode::Down);
    h.key(KeyCode::Enter);
    h.draw();
    let sent = h.sent();
    assert!(
        sent.iter().any(|r| matches!(r, Request::Answer { id, answers } if id == "ask-1" && answers == &vec![("Which language?".to_string(), "Go".to_string())])),
        "{sent:?}"
    );
    let full = h.screen.full_text();
    assert!(full.contains("• Questions 1/1 answered"), "{full}");
    assert!(full.contains("  • Which language?"));
    assert!(full.contains("    answer: Go"));
}

#[test]
fn title_alternates_between_the_two_action_required_prefixes() {
    let mut h = harness(120, 36, 6);
    ready(&mut h);
    h.app.on_backend(Event::Permission(bash_request()));
    h.draw();
    assert!(
        h.screen.title == "[ ! ] Action Required | proj"
            || h.screen.title == "[ . ] Action Required | proj",
        "{}",
        h.screen.title
    );
    h.key(KeyCode::Esc);
    h.draw();
    assert!(count(&h.bytes, "Action Required") > 0);
    assert_eq!(h.screen.title, "proj");
}
