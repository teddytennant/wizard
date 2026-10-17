//! A long turn: thinking, shell, read, edit with a diff, grouped searches and a markdown reply.

mod common;

use common::*;

#[test]
fn transcript_tall() {
    let mut h = Harness::new(120, 150);
    h.long_turn(5);
    h.pin_durations(&[2.3, 5.9, 2.5, 5.9], 39.0);
    h.dump("transcript-tall-120x150");
    println!("{}", h.text());
}

#[test]
fn turn_done_has_the_prompt_pinned() {
    let mut h = Harness::new(120, 36);
    h.long_turn(5);
    h.pin_durations(&[2.3, 5.9, 2.5, 5.9], 39.0);
    h.dump("turn-done-120x36");
    assert_snapshot("turn-done-120x36", &h.text());
}

#[test]
fn turn_sent_waits_for_the_response() {
    let mut h = Harness::new(120, 36);
    h.app.tr.usage.context_tokens = 1_480;
    h.at(100);
    h.long_turn(0);
    h.at(2_000);
    h.dump("turn-sent-120x36");
    assert_snapshot("turn-sent-120x36", &h.text());
}

/// A clock reading (ms) whose rail wave gives `first` on the thinking block's first row.
fn ms_for_rail(first: u8) -> u64 {
    use grokw::ui::anim::rail;
    let th = grokw::theme::Theme::groknight();
    for tick in 0..64u64 {
        if let ratatui::style::Color::Rgb(r, ..) = rail(th.bg_base, th.gray_dim, tick, 0) {
            if r == first {
                return tick * 34 + 1;
            }
        }
    }
    panic!("no tick gives {first:#x}");
}

#[test]
fn thinking_live() {
    let mut h = Harness::new(120, 36);
    h.app.tr.usage.context_tokens = 1_480;
    h.at(100);
    h.app
        .send_prompt("Run sleep 25 in the shell, then reply with the single word done.".into());
    h.event(agent_core::Event::TurnStart);
    h.event(agent_core::Event::ThoughtDelta(
        "First paragraph of the plan.\n\nThe command will likely be backgrounded after the ~15s foreground limit. I'll use a higher timeout.\n\nI need to check the long-running-background-tasks skill before launching the background sleep.".into(),
    ));
    h.at(ms_for_rail(0x21));
    h.dump("thinking-live-120x36");
    assert_snapshot("thinking-live-120x36", &h.text());
}

#[test]
fn markdown_showcase() {
    let mut h = Harness::new(120, 150);
    h.app.fixed_hm = Some((17, 43));
    h.markdown_turn();
    h.dump("markdown-tall-120x150");
    let mut h = Harness::new(120, 36);
    h.app.fixed_hm = Some((17, 43));
    h.markdown_turn();
    h.dump("markdown-tail-120x36");
    assert_snapshot("markdown-tail-120x36", &h.text());
}

#[test]
fn mini_turn_at_three_sizes() {
    for (w, h) in [(120u16, 36u16), (150, 42), (80, 24)] {
        let mut hn = Harness::new(w, h);
        hn.mini_turn(true);
        hn.dump(&format!("mini-done-{w}x{h}"));
        assert_snapshot(&format!("mini-done-{w}x{h}"), &hn.text());
    }
    // the model is waiting after a search: turn row, small-screen tip and the cancel hint
    let mut hn = Harness::home(80, 24);
    hn.type_str("x");
    hn.app.ed.clear();
    hn.at(100);
    hn.mini_turn(false);
    hn.at(2_000);
    hn.dump("mini-sent-80x24");
    assert_snapshot("mini-sent-80x24", &hn.text());
}

#[test]
fn todo_pane_pushes_the_scrollback_down() {
    use agent_core::{Event, Todo, TodoStatus};
    let mut h = Harness::new(120, 36);
    h.long_turn(5);
    h.pin_durations(&[2.3, 5.9, 2.5, 5.9], 39.0);
    let done = |t: &str| Todo {
        text: t.into(),
        status: TodoStatus::Completed,
    };
    h.event(Event::Todos(vec![
        done("Make a todo list of these steps"),
        done("Run ls -la in the shell"),
        done("Read README.md"),
        done("Edit README.md: change hello to hello world"),
        done("Grep for println in src"),
        done("Web search ratatui and give one sentence"),
        done("Finish with a markdown reply"),
    ]));
    h.ctrl('t');
    h.dump("todo-pane-120x36");
    assert_snapshot("todo-pane-120x36", &h.text());
}

#[test]
fn selected_group_header_has_a_box_and_a_band() {
    use grokw::app::Focus;
    let mut h = Harness::new(120, 36);
    h.long_turn(5);
    h.pin_durations(&[2.3, 5.9, 2.5, 5.9], 39.0);
    h.render();
    // the group row sits on screen row 22, as in the capture
    let (key, start) = {
        let doc = &h.app.view.doc;
        let i = doc
            .entries
            .iter()
            .position(|e| {
                e.kind == grokw::ui::transcript::Kind::Group
                    && e.rows[0].segs.iter().any(|s| s.text.contains("Searched"))
            })
            .expect("group");
        (doc.entries[i].key, doc.starts[i])
    };
    h.app.view.offset = Some(start - 19);
    h.app.view.selected = Some(key);
    h.app.focus = Focus::Scrollback;
    h.dump("select-entries-120x36");
    assert_snapshot("select-entries-120x36", &h.text());
}

#[test]
fn expanded_thinking_with_the_prompt_selected() {
    use grokw::app::Focus;
    let mut h = Harness::new(120, 36);
    h.app.fixed_hm = Some((17, 42));
    h.long_turn(5);
    // the reference run had a body for the first and last thoughts; the middle ones fold away
    h.pin_durations(&[2.3, 5.9, 2.5, 5.9], 39.0);
    h.render();
    h.app.view.think_open = true;
    h.app.view.offset = Some(0);
    h.app.view.selected = Some((0, 0));
    h.app.focus = Focus::Scrollback;
    h.dump("thinking-expanded-120x36");
    assert_snapshot("thinking-expanded-120x36", &h.text());
}

#[test]
fn a_snippet_edit_is_widened_to_the_file_lines_around_it() {
    use serde_json::json;
    let dir = std::env::temp_dir().join(format!("grokw-edit-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("a.txt"),
        "one\ntwo\nthree\nfour hello there five\nsix\nseven\neight\nnine\n",
    )
    .unwrap();
    let mut h = Harness::new(120, 36);
    h.app.opts.cwd = dir.clone();
    h.app.send_prompt("go".into());
    h.event(agent_core::Event::TurnStart);
    let mut edit = tool(
        "e",
        "edit_file",
        agent_core::ToolKind::Edit,
        "a.txt",
        json!({"path": "a.txt"}),
        Some("ok"),
    );
    // the model replaced a word in the middle of line 4
    edit.diff = Some(agent_core::FileDiff {
        path: "a.txt".into(),
        old: Some("hello".into()),
        new: "hello there".into(),
    });
    h.event(agent_core::Event::Tool(edit));
    let t = h.text();
    std::fs::remove_dir_all(&dir).ok();
    assert!(t.contains("1  one"), "{t}");
    assert!(t.contains("4  four hello five"), "{t}");
    assert!(t.contains("4  four hello there five"), "{t}");
    assert!(t.contains("5  six") && t.contains("7  eight"), "{t}");
    assert!(
        !t.contains("nine"),
        "only three lines of context after:\n{t}"
    );
}
