//! The welcome screen and the empty agent screen at the three sizes the captures use. Set
//! `GROKW_DUMP_DIR` to also write `.ansi` files for `tools/grokw-cmp.py`; update the text
//! snapshots with `UPDATE_SNAPSHOTS=1`.

mod common;

use common::*;

const SIZES: [(u16, u16); 3] = [(120, 36), (150, 42), (80, 24)];

fn each(name: &str, home: bool, f: impl Fn(&mut Harness)) {
    for (w, h) in SIZES {
        let mut hn = if home {
            Harness::home(w, h)
        } else {
            Harness::new(w, h)
        };
        f(&mut hn);
        hn.dump(&format!("{name}-{w}x{h}"));
        assert_snapshot(&format!("{name}-{w}x{h}"), &hn.text());
    }
}

#[test]
fn home() {
    each("home", true, |_| {});
}

#[test]
fn typed_leaves_home() {
    each("typed", true, |h| h.type_str("hello"));
}

#[test]
fn slash_popup() {
    each("slash", false, |h| h.type_str("/"));
    each("slash-filtered", false, |h| h.type_str("/mo"));
    each("model-picker", false, |h| {
        h.type_str("/model");
        h.press(crossterm::event::KeyCode::Enter);
    });
}

#[test]
fn effort_popup() {
    each("effort-popup", false, |h| {
        h.type_str("/effort");
        h.press(crossterm::event::KeyCode::Enter);
    });
}

#[test]
fn modes_and_chords() {
    use crossterm::event::{KeyCode, KeyModifiers};
    each("mode-plan", false, |h| {
        h.key(KeyCode::BackTab, KeyModifiers::SHIFT)
    });
    each("bang-mode", false, |h| h.type_str("!"));
    each("quit-confirm", false, |h| h.ctrl('q'));
    each("stash", false, |h| {
        h.type_str("draft");
        h.ctrl('s');
    });
}

#[test]
fn resume_picker_on_home() {
    use agent_core::Event;
    each("resume-picker", true, |h| {
        h.ctrl('r');
        let now = 1_790_003_600;
        h.event(Event::Sessions(vec![
            session("a", "say hi", now),
            session("b", "Rust demo edits, checks, and verbose plan", now - 70),
            session("c", "Codebase Directory Listing with ls Command", now - 380),
        ]));
    });
}

#[test]
fn palette() {
    each("palette", false, |h| h.ctrl('p'));
}

/// The welcome screen at the sizes `reference/grok/layouts` holds, with the announcement and tip
/// each capture showed (they come from a server and rotate, so they are pinned here).
#[test]
fn home_layouts() {
    const HERE: &str = "Grok 4.7 is here!";
    const SELECT: &str = "Select 'Grok 4.7' under /model.";
    const LEARN: &str = "New /learn skill!";
    const TUNE: &str =
        "Ask Grok to /learn from your past traces and tune Grok Build to how you work.";
    const AT: &str = "Use @ to attach files like @src/main.rs.";
    const COMPACT: &str = "Run /compact [context] when chat gets long.";
    const WORKTREE: &str = "Start Grok in a fresh worktree with `-w`; add `-r <session-id>` to resume an existing session there.";
    const DASH: &str =
        "Run /dashboard (or Ctrl+\\) to see and manage all your agents in one place.";
    const AUTO: &str = "Press Ctrl+O to toggle auto-approve mode.";
    type Case = (u16, u16, Option<(&'static str, &'static str)>, &'static str);
    let cases: [Case; 11] = [
        (100, 28, Some((LEARN, TUNE)), COMPACT),
        (120, 22, None, WORKTREE),
        (120, 24, Some((HERE, SELECT)), AT),
        (150, 30, Some((LEARN, TUNE)), AUTO),
        (200, 50, Some((HERE, SELECT)), COMPACT),
        (60, 20, Some((HERE, SELECT)), WORKTREE),
        (80, 26, Some((HERE, SELECT)), WORKTREE),
        (80, 30, Some((LEARN, TUNE)), DASH),
        (89, 30, Some((HERE, SELECT)), AT),
        (90, 30, Some((LEARN, TUNE)), AUTO),
        (40, 12, Some((HERE, SELECT)), DASH),
    ];
    for (w, h, ann, tip) in cases {
        let mut hn = Harness::home(w, h);
        hn.app.config.effort = "xhigh".into();
        hn.app.home.tip = tip.to_string();
        // no announcement at all means the card shows the subtitle row
        hn.app.home.announcement = Some(ann.map_or((String::new(), String::new()), |(a, b)| {
            (a.to_string(), b.to_string())
        }));
        hn.dump(&format!("layout-{w}x{h}"));
        assert_snapshot(&format!("layout-{w}x{h}"), &hn.text());
    }
}

#[test]
fn at_picker() {
    each("at-picker", false, |h| {
        h.app.files = vec!["README.md".into(), "src/".into()];
        h.type_str("@");
    });
}

#[test]
fn home_focus_and_hover() {
    use crossterm::event::{KeyCode, MouseEvent, MouseEventKind};
    each("home-unfocused", true, |h| h.press(KeyCode::Esc));
    each("home-menu-down", true, |h| {
        h.press(KeyCode::Esc);
        h.press(KeyCode::Down);
    });
    // the pointer over `Resume session` (row 14 at 120x36)
    let mut h = Harness::home(120, 36);
    h.app.on_mouse(MouseEvent {
        kind: MouseEventKind::Moved,
        column: 40,
        row: 14,
        modifiers: crossterm::event::KeyModifiers::NONE,
    });
    h.dump("home-hover-120x36");
    assert_snapshot("home-hover-120x36", &h.text());
}

#[test]
fn resume_modal_in_session_while_loading() {
    let mut h = Harness::new(120, 36);
    h.type_str("x");
    h.app.ed.clear();
    h.ctrl('r');
    // the dot spinner shows `:` from 136 ms on
    h.at(150);
    h.dump("resume-modal-120x36");
    assert_snapshot("resume-modal-120x36", &h.text());
}

#[test]
fn resume_modal_in_session_with_sessions() {
    use agent_core::Event;
    let mut h = Harness::new(120, 36);
    h.type_str("x");
    h.app.ed.clear();
    h.ctrl('r');
    let now = 1_790_003_600;
    let mut v = vec![
        session(
            "1",
            "Create build/out demo.txt and list contents",
            now - 130,
        ),
        session("2", "Create build/out demo.txt and list files", now - 560),
        session("3", "Exact markdown echo between BEGIN END", now - 1210),
        session("4", "Exact markdown echo between BEGIN END", now - 1510),
    ];
    for i in 0..20 {
        v.push(session(
            &format!("f{i}"),
            "Edit README.md hello",
            now - 3600 - i * 60,
        ));
    }
    h.event(Event::Sessions(v));
    h.dump("resume-modal-loaded-120x36");
    assert_snapshot("resume-modal-loaded-120x36", &h.text());
}
