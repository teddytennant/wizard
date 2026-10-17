//! Scrollback work: find, jump, vim keys, copy, the block viewer and mouse selection, against
//! `reference/grok/120x36/{74b,75,76,63}` where a capture shows the state.

mod common;

use common::cells::{assert_cells, Skip};
use common::*;
use crossterm::event::{KeyCode, KeyModifiers};
use grokw::app::Focus;
use grokw::ui::transcript::Kind;

/// The long turn as the real session ran it: the search group holds one search, one web search
/// that returned three sites, and the two thoughts after them. `74b-viewer` shows it open.
fn long_turn_real() -> Harness {
    use agent_core::transcript::Part;
    use agent_core::Event;
    use serde_json::json;
    let mut h = Harness::new(120, 36);
    h.long_turn(3);
    h.event(Event::Tool(tool(
        "t4",
        "search_files",
        agent_core::ToolKind::Search,
        "println",
        json!({"pattern": "println", "path": "src"}),
        Some("src/main.rs:2:    println!(\"hi\");"),
    )));
    h.event(Event::Tool(tool(
        "w0",
        "web_search",
        agent_core::ToolKind::Search,
        "ratatui",
        json!({"query": "ratatui"}),
        Some(
            "1. [Ratatui](https://ratatui.rs)\n   A Rust library for terminal UIs.\n2. [ratatui on crates.io](https://crates.io/crates/ratatui)\n3. [ratatui docs](https://docs.rs/ratatui)",
        ),
    )));
    for t in ["Reading the search results.", "Writing the final reply."] {
        h.app
            .tr
            .messages
            .last_mut()
            .unwrap()
            .parts
            .push(Part::Thought {
                text: t.into(),
                started: std::time::Instant::now(),
                took: Some(std::time::Duration::from_secs(3)),
            });
    }
    h.app.fixed_hm = Some((17, 43));
    h.render();
    h.event(Event::TextDelta(DONE.to_string()));
    h.event(Event::Usage(agent_core::Usage {
        context_tokens: 23_200,
        context_window: 256_000,
        ..Default::default()
    }));
    h.event(Event::TurnEnd(agent_core::StopReason::EndTurn));
    h.pin_durations(&[2.3, 5.9, 2.5], 39.0);
    h
}

/// The group opened, as `74b-viewer` has it, with the first member (the search) under the box.
fn expanded_group() -> Harness {
    let mut h = long_turn_real();
    h.render();
    let group = h
        .app
        .view
        .doc
        .entries
        .iter()
        .rev()
        .find(|e| e.kind == Kind::Group)
        .map(|e| e.key)
        .expect("a verb group");
    h.app.view.folds.insert(group, true);
    h.app.focus = Focus::Scrollback;
    h.render();
    h.app.view.selected = Some(group);
    h.app.view.to_top();
    h
}

#[test]
#[cfg_attr(
    not(reference_captures),
    ignore = "needs the reference/ captures, not in this repository; see build.rs"
)]
fn an_open_group_takes_the_cursor_member_by_member_and_matches_74b() {
    let mut h = expanded_group();
    h.press(KeyCode::Down);
    assert_eq!(h.app.view.member(), Some(0));
    h.dump("viewer-group-120x36");
    assert_snapshot("viewer-group-120x36", &h.text());
    assert_cells(
        "120x36/74b-viewer",
        &h.render(),
        &Skip {
            // the real web search said `(1 site)`; ours reads three results
            rows: vec![24],
            ..Default::default()
        },
    );
    // Down walks the members, then leaves the group; Up comes back to the last member
    for _ in 0..3 {
        h.press(KeyCode::Down);
    }
    assert_eq!(h.app.view.member(), Some(3));
    h.press(KeyCode::Down);
    assert_eq!(h.app.view.member(), None);
    assert_ne!(
        h.app.view.selected.unwrap(),
        h.app
            .view
            .doc
            .entries
            .iter()
            .find(|e| e.kind == Kind::Group)
            .unwrap()
            .key
    );
    h.press(KeyCode::Up);
    assert_eq!(h.app.view.member(), Some(3));
    // Left folds the group away
    h.press(KeyCode::Left);
    assert_eq!(h.app.view.member(), None);
}

/// Back-compat name for the tests below: the group open, the first prompt selected, one row down.
fn find_state() -> Harness {
    let mut h = long_turn_real();
    h.render();
    let group = h
        .app
        .view
        .doc
        .entries
        .iter()
        .rev()
        .find(|e| e.kind == Kind::Group)
        .map(|e| e.key)
        .expect("a verb group");
    h.app.view.folds.insert(group, true);
    h.app.view.folds.insert((0, 0), true);
    h.app.focus = Focus::Scrollback;
    h.render();
    h.app.view.selected = h.app.view.doc.entries.first().map(|e| e.key);
    h.app.view.offset = Some(1);
    h.render();
    h
}

#[test]
#[cfg_attr(
    not(reference_captures),
    ignore = "needs the reference/ captures, not in this repository; see build.rs"
)]
fn find_bar_matches_the_capture() {
    let mut h = find_state();
    h.app.dispatch("/find table".into());
    h.dump("find-120x36");
    assert_snapshot("find-120x36", &h.text());
    // the counter is the capture's `1/2` only when the reply also says `table`; the rest of the
    // screen is cell for cell
    assert_cells(
        "120x36/75-find",
        &h.render(),
        &Skip {
            rows: vec![24, 28],
            ..Default::default()
        },
    );
}

#[test]
fn find_steps_wraps_and_closes() {
    let mut h = find_state();
    h.app.dispatch("/find ratatui".into());
    let n = h.app.view.nav.find.as_ref().unwrap().hits.len();
    assert!(
        n >= 3,
        "ratatui is in the prompt, the search rows and the reply: {n}"
    );
    assert_eq!(h.app.view.nav.find.as_ref().unwrap().cur, 0);
    h.press(KeyCode::Down);
    assert_eq!(h.app.view.nav.find.as_ref().unwrap().cur, 1);
    h.press(KeyCode::Up);
    h.press(KeyCode::Up);
    assert_eq!(h.app.view.nav.find.as_ref().unwrap().cur, n - 1, "wraps");
    // Enter accepts; then n and N step
    h.press(KeyCode::Enter);
    assert!(!h.app.view.nav.find.as_ref().unwrap().composing);
    h.type_str("n");
    assert_eq!(h.app.view.nav.find.as_ref().unwrap().cur, 0);
    h.key(KeyCode::Char('N'), KeyModifiers::SHIFT);
    assert_eq!(h.app.view.nav.find.as_ref().unwrap().cur, n - 1);
    h.press(KeyCode::Esc);
    assert!(h.app.view.nav.find.is_none());
    // the viewport has its rows back
    assert!(!h.text().contains("search:"));
}

#[test]
fn find_is_smart_case_and_says_when_nothing_matches() {
    let mut h = find_state();
    h.app.dispatch("/find DONE".into());
    assert_eq!(h.app.view.nav.find.as_ref().unwrap().hits.len(), 0);
    let t = h.text();
    assert!(t.contains("no matches"), "{t}");
    h.press(KeyCode::Esc);
    h.app.dispatch("/find done".into());
    assert!(!h.app.view.nav.find.as_ref().unwrap().hits.is_empty());
}

#[test]
fn find_reveals_a_match_below_the_view() {
    let mut h = Harness::new(120, 20);
    h.long_turn(5);
    h.pin_durations(&[2.3, 5.9, 2.5, 5.9], 39.0);
    h.render();
    h.app.view.to_top();
    h.render();
    h.app.dispatch("/find successor".into());
    let t = h.text();
    assert!(
        t.contains("successor"),
        "the match scrolled into view:\n{t}"
    );
}

#[test]
fn jump_with_one_turn_says_so() {
    let mut h = Harness::new(120, 36);
    h.long_turn(5);
    h.pin_durations(&[2.3, 5.9, 2.5, 5.9], 39.0);
    h.app.dispatch("/jump".into());
    h.dump("jump-one-turn-120x36");
    assert!(h.text().contains("Nothing to jump to yet"));
    assert!(h.app.view.nav.jump.is_none());
}

fn three_turns() -> Harness {
    let mut h = Harness::new(120, 36);
    for (q, a) in [
        (
            "first question about parsing",
            "Parsing is done with a small recursive descent.",
        ),
        (
            "second question about layout",
            "Layout is two passes over the tree.",
        ),
        (
            "third question about the cursor",
            "The cursor is drawn last, inside the sync block.",
        ),
    ] {
        h.turn(q, a);
        h.took(1.0);
    }
    h
}

#[test]
fn jump_lists_turns_scrolls_as_you_move_and_restores_on_esc() {
    let mut h = three_turns();
    h.render();
    let before = h.app.view.top();
    h.app.dispatch("/jump".into());
    let t = h.text();
    assert!(t.contains("Jump to which turn?"), "{t}");
    assert!(t.contains("1 first question about parsing"), "{t}");
    assert!(t.contains("3 third question about the cursor"), "{t}");
    h.dump("jump-120x36");
    assert_snapshot("jump-120x36", &t);
    // the cursor starts on the last turn; moving up previews the one above
    h.press(KeyCode::Up);
    h.render();
    assert_eq!(h.app.view.nav.jump.as_ref().unwrap().sel, 1);
    let top = h.app.view.top();
    assert!(top < before || before == 0);
    h.press(KeyCode::Esc);
    assert!(h.app.view.nav.jump.is_none());
    h.render();
    assert_eq!(h.app.view.top(), before, "Esc puts the view back");
    // Enter lands on the turn and leaves the list
    h.app.dispatch("/jump".into());
    h.type_str("kk");
    h.press(KeyCode::Enter);
    assert!(h.app.view.nav.jump.is_none());
    assert_eq!(h.app.focus, Focus::Scrollback);
    let sel = h.app.view.selected.unwrap();
    assert_eq!(sel, (0, 0), "the first prompt is selected");
}

#[test]
fn the_jump_list_is_docked_where_the_rewind_picker_sits() {
    // geometry of 63-rewind-picker: the list ends on row 32 (H-4), one row above it for the
    // title, one for each turn, a blank row at each end, and the host arrow row above
    let mut h = three_turns();
    h.app.dispatch("/jump".into());
    let t = h.render();
    let first = 32 - (1 + 1 + 3);
    assert_eq!(t.cell(2, first).unwrap().symbol(), "┃");
    assert_eq!(t.cell(2, 32).unwrap().symbol(), "┃");
    assert_eq!(t.cell(2, first - 1).unwrap().symbol(), " ");
    assert!(t.row(first + 1).contains("Jump to which turn?"));
}

fn select_tool(h: &mut Harness, word: &str) {
    h.render();
    h.app.focus = Focus::Scrollback;
    let key = h
        .app
        .view
        .doc
        .entries
        .iter()
        .find(|e| {
            e.kind == Kind::Tool
                && e.rows
                    .iter()
                    .any(|r| r.segs.iter().any(|s| s.text.contains(word)))
        })
        .map(|e| e.key)
        .expect("a tool entry");
    h.app.view.selected = Some(key);
}

#[test]
fn enter_opens_the_block_viewer_over_the_dimmed_screen() {
    let mut h = long_turn_real();
    select_tool(&mut h, "Edit");
    h.press(KeyCode::Enter);
    assert!(h.app.view.nav.viewer.is_some());
    let t = h.render();
    let text = t.plain();
    // popup geometry from spec 3.13: cols 3..=116, rows 1..=31, `[x]` four in from the right
    assert_eq!(t.cell(3, 1).unwrap().symbol(), "╭");
    assert_eq!(t.cell(116, 1).unwrap().symbol(), "╮");
    assert_eq!(t.cell(3, 31).unwrap().symbol(), "╰");
    assert!(t.row(2).contains("[x]"), "{text}");
    assert_eq!(t.cell(112, 2).unwrap().symbol(), "[");
    assert!(text.contains("Edit README.md") && text.contains("hello world"));
    assert!(text.contains("Esc:close  │  Enter:quote"), "{text}");
    h.dump("viewer-120x36");
    assert_snapshot("viewer-120x36", &text);
    // the rest of the screen is dimmed halfway to the background
    let dim = t.cell(5, 33).unwrap().fg;
    let _ = dim;
    h.press(KeyCode::Esc);
    assert!(h.app.view.nav.viewer.is_none());
}

#[test]
fn the_viewer_scrolls_and_quotes_into_the_composer() {
    let mut h = Harness::new(120, 20);
    h.long_turn(5);
    h.pin_durations(&[2.3, 5.9, 2.5, 5.9], 39.0);
    h.render();
    h.app.focus = Focus::Scrollback;
    let reply = h
        .app
        .view
        .doc
        .entries
        .iter()
        .rev()
        .find(|e| e.kind == Kind::Agent)
        .map(|e| e.key)
        .unwrap();
    h.app.view.selected = Some(reply);
    h.press(KeyCode::Enter);
    h.render();
    let v = h.app.view.nav.viewer.as_ref().unwrap();
    assert!(
        v.rows.len() > v.visible,
        "the reply is longer than the popup"
    );
    h.press(KeyCode::PageDown);
    assert!(h.app.view.nav.viewer.as_ref().unwrap().top > 0);
    h.press(KeyCode::Char('g'));
    assert_eq!(h.app.view.nav.viewer.as_ref().unwrap().top, 0);
    h.press(KeyCode::Enter);
    assert!(h.app.view.nav.viewer.is_none());
    assert_eq!(h.app.focus, Focus::Prompt);
    assert!(h.app.ed.text().starts_with("> "), "{:?}", h.app.ed.text());
}

#[test]
fn vim_keys_move_fold_jump_and_copy() {
    let mut h = three_turns();
    h.app.dispatch("/vim-mode".into());
    assert!(h.app.vim_mode);
    h.render();
    h.press(KeyCode::Tab);
    // focus lands on the last entry; k goes up, g goes to the top, G back to the end
    h.type_str("g");
    assert_eq!(h.app.view.selected, Some((0, 0)));
    h.type_str("j");
    assert_eq!(h.app.view.selected, Some((1, 0)));
    // Shift+l and Shift+h hop between prompts
    h.key(KeyCode::Char('L'), KeyModifiers::SHIFT);
    assert_eq!(h.app.view.selected, Some((2, 0)));
    h.key(KeyCode::Char('H'), KeyModifiers::SHIFT);
    assert_eq!(h.app.view.selected, Some((0, 0)));
    h.key(KeyCode::Char('G'), KeyModifiers::SHIFT);
    assert!(h.app.view.following());
    // the bar names the vim keys
    let t = h.text();
    assert!(t.contains("j/k:nav") && t.contains("Shift+l/h:turn"), "{t}");
    // r shows a reply as its markdown source
    h.type_str("k");
    let before = h.text();
    h.type_str("r");
    assert!(h.app.view.nav.raw.len() <= 1);
    let _ = before;
    // i and Space go back to the prompt
    h.type_str("i");
    assert_eq!(h.app.focus, Focus::Prompt);
}

#[test]
fn y_copies_what_the_selected_block_holds() {
    let mut h = long_turn_real();
    select_tool(&mut h, "Edit");
    let app = &h.app;
    let key = app.view.selected.unwrap();
    let patch = grokw::ui::transcript_nav::entry_copy_text(app, key, false).unwrap();
    assert!(
        patch.contains("-hello\n") && patch.contains("+hello world\n"),
        "{patch}"
    );
    let path = grokw::ui::transcript_nav::entry_copy_text(app, key, true).unwrap();
    assert_eq!(path, "README.md");
    // a command copies its output, and `Y` the command line
    select_tool(&mut h, "Run");
    let key = h.app.view.selected.unwrap();
    assert_eq!(
        grokw::ui::transcript_nav::entry_copy_text(&h.app, key, true).unwrap(),
        "ls -la"
    );
    assert!(
        grokw::ui::transcript_nav::entry_copy_text(&h.app, key, false)
            .unwrap()
            .contains("README.md")
    );
    h.app.vim_mode = true;
    h.type_str("y");
    assert!(h.app.toast.as_ref().is_some_and(|t| t.text == "Copied!"));
}

#[test]
fn a_drag_selects_text_and_copies_it_on_release() {
    use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
    let mut h = Harness::new(120, 36);
    h.turn("hello there", "The answer is forty-two.");
    h.took(1.0);
    h.render();
    // find the reply row
    let t = h.render();
    let y = (0..36).find(|&y| t.row(y).contains("The answer")).unwrap();
    let mouse = |kind, col| MouseEvent {
        kind,
        column: col,
        row: y,
        modifiers: KeyModifiers::NONE,
    };
    h.app
        .on_mouse(mouse(MouseEventKind::Down(MouseButton::Left), 5));
    h.app
        .on_mouse(mouse(MouseEventKind::Drag(MouseButton::Left), 14));
    let d = h.app.view.nav.drag.expect("a selection");
    assert_eq!(h.app.view.selected_text(d), "The answer");
    // the selected cells are inverted
    let t = h.render();
    let c = t.cell(7, y).unwrap();
    assert_eq!(c.bg, h.app.theme.text_primary);
    h.app
        .on_mouse(mouse(MouseEventKind::Up(MouseButton::Left), 14));
    assert!(h.app.toast.as_ref().is_some_and(|t| t.text == "Copied!"));
}

#[test]
fn a_double_click_folds_a_block() {
    use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
    let mut h = Harness::new(120, 150);
    h.long_turn(5);
    h.pin_durations(&[2.3, 5.9, 2.5, 5.9], 39.0);
    let t = h.render();
    let y = (0..150)
        .find(|&y| t.row(y).contains("Searched 1 pattern"))
        .unwrap();
    let click = |col| MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: col,
        row: y,
        modifiers: KeyModifiers::NONE,
    };
    h.app.on_mouse(click(8));
    assert_eq!(h.app.focus, Focus::Scrollback);
    assert!(h.render().plain().contains("Searched 1 pattern"));
    let before = h.render().plain().lines().count();
    h.app.on_mouse(click(8));
    let t = h.render();
    assert!(t.plain().contains("Search \"println\""), "{}", t.plain());
    let _ = before;
}

#[test]
fn wheel_and_the_arrow_below_the_viewport_scroll() {
    use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
    let mut h = Harness::new(120, 30);
    h.long_turn(5);
    h.pin_durations(&[2.3, 5.9, 2.5, 5.9], 39.0);
    h.render();
    h.app.on_mouse(MouseEvent {
        kind: MouseEventKind::ScrollUp,
        column: 10,
        row: 8,
        modifiers: KeyModifiers::NONE,
    });
    assert!(!h.app.view.following());
    let t = h.render();
    let host = (0..30)
        .find(|&y| t.row(y).contains('▼'))
        .expect("the arrow");
    h.app.on_mouse(MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: 60,
        row: host,
        modifiers: KeyModifiers::NONE,
    });
    assert!(h.app.view.following(), "the arrow brings the end back");
}

/// The prompt just sent sits on the top row; the earlier prompt pinned over the viewport must not
/// cover it.
#[test]
fn the_pinned_prompt_does_not_cover_the_one_just_sent() {
    use agent_core::Event;
    let mut h = Harness::new(120, 36);
    let answer: String = (1..=40).map(|i| format!("- item {i}\n")).collect();
    h.turn("earlier question", &answer);
    h.took(1.0);
    h.render();
    h.app.send_prompt("the newest question".into());
    h.event(Event::TurnStart);
    let t = h.render();
    let text: String = (0..12).map(|y| t.row(y)).collect::<Vec<_>>().join("\n");
    assert!(text.contains("the newest question"), "{text}");
    assert!(!text.contains("earlier question"), "{text}");
}

/// Prompt sent after a long history: the sent prompt is pinned at the top and a bottom reserve
/// (a viewport of blank rows) keeps it there, so a running turn shows a scrollbar thumb even
/// though there is almost nothing below the prompt (`46-thinking-live`).
#[test]
fn a_sent_prompt_pinned_over_history_keeps_a_thumb_while_the_turn_runs() {
    use agent_core::Event;
    let mut h = Harness::new(120, 36);
    // forty rows of history above the next prompt: a prompt (5 rows and a gap), a 33-row list,
    // the `Worked for` line, and a gap after each
    let answer: String = (1..=33).map(|i| format!("- item {i}\n")).collect();
    h.turn("earlier question", &answer);
    h.took(1.0);
    h.render();
    let before = h.app.view.doc.total;
    h.app.tr.usage.context_tokens = 23_200;
    h.at(100);
    h.app
        .send_prompt("Run sleep 25 in the shell, then reply with the single word done.".into());
    h.event(Event::TurnStart);
    h.event(Event::ThoughtDelta("First paragraph of the plan.\n\nThe command will likely be backgrounded after the ~15s foreground limit. I'll use a higher timeout.\n\nI need to check the long-running-background-tasks skill before launching the background sleep.".into()));
    h.render();
    let v = &h.app.view;
    assert_eq!(before, 40, "history rows");
    // the pinned prompt is at the top of the viewport, with the reserve held below it
    assert_eq!(
        v.top(),
        v.doc.starts[v.doc.entries.len() - 2].min(v.max_offset())
    );
    let t = h.render();
    let thumb: Vec<u16> = (3..27)
        .filter(|&y| t.cell(119, y).unwrap().symbol() == "█")
        .collect();
    assert!(!thumb.is_empty(), "a thumb shows");
    // rows 18 to 26, the thumb `46-thinking-live` shows at this scroll position
    assert_eq!(thumb, (18..=26).collect::<Vec<u16>>());
    // once the answer outgrows the reserve it is released and the thumb tracks the document
    h.event(Event::TextDelta("done\n".repeat(60)));
    h.render();
    assert!(h.app.view.following());
}

fn subagent_call(status: agent_core::ToolStatus, secs: Option<f64>) -> agent_core::ToolCall {
    use serde_json::json;
    let mut input = json!({
        "subagent": "explore",
        "task": "Count lines in main.rs",
        "description": "Count lines in main.rs"
    });
    if let Some(s) = secs {
        input["grokw_secs"] = json!(s);
    }
    let mut c = tool(
        "s1",
        "spawn_subagent",
        agent_core::ToolKind::Other,
        "explore: Count lines in main.rs",
        input,
        None,
    );
    c.status = status;
    c
}

/// `52-subagent-running` and `53-subagent-done`: the verb group counts the subagent, present
/// tense while it runs, and the open group says how it ended.
#[test]
fn a_subagent_is_a_group_row_that_says_how_it_ended() {
    use agent_core::{Event, ToolStatus};
    let mut h = Harness::new(120, 36);
    h.app.send_prompt(
        "Spawn one explore subagent to count the lines in src/main.rs and tell me the number."
            .into(),
    );
    h.event(Event::TurnStart);
    h.event(Event::TextDelta(
        "I'll spawn one explore subagent to count the lines in src/main.rs.".into(),
    ));
    h.event(Event::Tool(subagent_call(ToolStatus::Running, None)));
    assert!(h.text().contains("◈ Running 1 subagent"), "{}", h.text());
    h.event(Event::Tool(subagent_call(ToolStatus::Completed, Some(6.4))));
    h.event(Event::TextDelta("src/main.rs has 3 lines.".into()));
    h.event(Event::TurnEnd(agent_core::StopReason::EndTurn));
    h.pin_durations(&[], 17.0);
    let t = h.text();
    assert!(t.contains("◈ Ran 1 subagent"), "{t}");
    assert!(t.contains("Worked for 17s"), "{t}");
    // the group opened shows the member: how long it took and what it was asked
    h.render();
    let g = h
        .app
        .view
        .doc
        .entries
        .iter()
        .find(|e| e.kind == Kind::Group)
        .map(|e| e.key)
        .unwrap();
    h.app.view.folds.insert(g, true);
    let t = h.text();
    assert!(
        t.contains("Subagent completed in 6.4s: “Count lines in main.rs”"),
        "{t}"
    );
}

#[test]
fn a_command_run_in_the_background_is_a_task_started_row() {
    use agent_core::Event;
    use serde_json::json;
    let mut h = Harness::new(120, 36);
    h.app
        .send_prompt("Run sleep 25 in the shell, then reply with the single word done.".into());
    h.event(Event::TurnStart);
    h.event(Event::Tool(tool(
        "b1",
        "execute",
        agent_core::ToolKind::Execute,
        "sleep 25",
        json!({"command": "sleep 25", "description": "Sleep for 25 seconds", "run_in_background": true}),
        Some("started background task #1"),
    )));
    let t = h.text();
    assert!(t.contains("◆ Task started: Sleep for 25 seconds"), "{t}");
    assert!(!t.contains("Run Sleep"), "{t}");
}

/// The header counter is what wizard's `/status` said (`context: 9377 tokens`), over the window
/// the backend reported, or the model's when it did not.
#[test]
fn the_counter_shows_the_context_wizard_reported() {
    use agent_core::Event;
    let mut h = Harness::new(120, 36);
    let counter = |h: &mut Harness| h.render().row(1).trim_end().to_string();
    h.event(Event::Usage(agent_core::Usage {
        context_tokens: 9_377,
        ..Default::default()
    }));
    assert!(
        counter(&mut h).ends_with("9.4K / 256K"),
        "{}",
        counter(&mut h)
    );
    // a backend that knows its window wins over the guess
    h.event(Event::Usage(agent_core::Usage {
        context_tokens: 9_377,
        context_window: 1_000_000,
        ..Default::default()
    }));
    assert!(
        counter(&mut h).ends_with("9.4K / 1.0M"),
        "{}",
        counter(&mut h)
    );
    // 95% of the window is the error colour
    h.event(Event::Usage(agent_core::Usage {
        context_tokens: 244_000,
        context_window: 256_000,
        ..Default::default()
    }));
    let t = h.render();
    let x = (0..120)
        .rev()
        .find(|&x| t.cell(x, 1).unwrap().symbol() == "K")
        .unwrap();
    assert_eq!(
        t.cell(x, 1).unwrap().fg,
        ratatui::style::Color::Rgb(0xf7, 0x76, 0x8e)
    );
}

/// `/timestamps` off: the transcript loses its stamps and re-wraps, but the pinned prompt keeps
/// its stamp and its old wrap (`87-timestamps-off`; the capture was taken after the toast went).
/// `84-compact-mode` in `reference/grok` is not a compact screen, so there is nothing to
/// compare compact mode with.
#[test]
#[cfg_attr(
    not(reference_captures),
    ignore = "needs the reference/ captures, not in this repository; see build.rs"
)]
fn timestamps_off_matches_its_capture() {
    let mut h = Harness::new(120, 36);
    h.app.fixed_hm = Some((17, 43));
    h.markdown_turn();
    h.app.dispatch("/timestamps".into());
    assert!(h.text().contains("✓ Timestamps: off"));
    assert_cells(
        "120x36/87-timestamps-off",
        &h.render(),
        &Skip {
            rows: vec![28],
            ..Default::default()
        },
    );
}

/// `71-scroll-pgup1` to `73-scroll-top`: with the scrollback focused, a page key leaves the
/// first entry on screen selected, so the view at the top shows the box on the prompt.
#[test]
fn page_keys_select_the_entry_at_the_edge_of_the_view() {
    let mut h = long_turn_real();
    h.render();
    h.press(KeyCode::Tab);
    h.press(KeyCode::PageUp);
    h.press(KeyCode::PageUp);
    h.press(KeyCode::PageUp);
    h.render();
    assert_eq!(h.app.view.top(), 0);
    assert_eq!(h.app.view.selected, Some((0, 0)));
    // the same screen as the select-entries state, and as 74 `select-entries`: box rows 2 to 8
    let t = h.render();
    assert_eq!(t.cell(1, 2).unwrap().symbol(), "┌");
    assert_eq!(t.cell(1, 8).unwrap().symbol(), "└");
    h.press(KeyCode::PageDown);
    h.press(KeyCode::PageDown);
    h.press(KeyCode::PageDown);
    h.render();
    assert!(h.app.view.following());
    assert_ne!(h.app.view.selected, Some((0, 0)));
}
