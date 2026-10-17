//! Input behaviour: every binding in the keymap pressed once and checked for its effect,
//! plus images, history, the external editor, drafts and the resume dialog. The coverage
//! test fails when a binding is added to `keys::BINDINGS` without an entry here.

mod common;

use std::collections::HashMap;

use agent_core::{Event, Request, StopReason, ToolCall, ToolKind, ToolStatus};
use common::Harness;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use openc::app::{Focus, RailMode};
use openc::keys::{event_for, Scope, BINDINGS};
use openc::ui::dialogs::{Overlay, SelKind};

fn started() -> Harness {
    let mut h = Harness::new(120, 30);
    h.ready();
    h
}

fn press(h: &mut Harness, spec: &str) {
    let ev = event_for(spec).unwrap_or_else(|| panic!("no event for {spec}"));
    h.t += 40;
    let now = h.now();
    h.app.on_key(ev, now);
}

fn text(h: &Harness) -> String {
    h.app.composer.text().to_string()
}

fn cursor(h: &Harness) -> usize {
    h.app.composer.ed.cursor()
}

/// A finished turn with a tool call, so there is something to navigate.
fn with_turn(h: &mut Harness) {
    for i in 0..3 {
        h.type_str(&format!("question {i}"));
        h.key(KeyCode::Enter);
        h.event(Event::TurnStart, 10);
        h.event(
            Event::Tool(ToolCall {
                id: format!("t{i}"),
                name: "Bash".into(),
                kind: ToolKind::Execute,
                title: format!("ls {i}"),
                output: Some("a\nb".into()),
                status: ToolStatus::Completed,
                ..Default::default()
            }),
            10,
        );
        h.event(Event::TextDelta(format!("answer {i} ").repeat(120)), 10);
        h.event(Event::TurnEnd(StopReason::EndTurn), 10);
    }
    h.draw();
    let _ = h.sent();
}

fn busy(h: &mut Harness) {
    h.type_str("work");
    h.key(KeyCode::Enter);
    h.event(Event::TurnStart, 10);
    let _ = h.sent();
}

fn nav(h: &mut Harness) {
    with_turn(h);
    h.key(KeyCode::Esc);
    assert_eq!(h.app.focus, Focus::Nav);
}

fn has(h: &mut Harness, f: impl Fn(&Request) -> bool) -> bool {
    h.sent().iter().any(f)
}

type Check = fn(&mut Harness);

/// `(scope, spec)` -> what to set up, press and assert.
fn table() -> HashMap<(Scope, &'static str), Check> {
    let mut m: HashMap<(Scope, &'static str), Check> = HashMap::new();
    macro_rules! t {
        ($scope:expr, $spec:expr, $body:expr) => {
            m.insert(($scope, $spec), $body as Check);
        };
    }
    use Scope::*;
    // ---- everywhere
    t!(Global, "ctrl+c", |h| {
        busy(h);
        press(h, "ctrl+c");
        assert!(has(h, |r| *r == Request::Cancel));
    });
    t!(Global, "ctrl+d", |h| {
        press(h, "ctrl+d");
        assert!(h.app.should_quit);
    });
    t!(Global, "esc", |h| {
        busy(h);
        press(h, "esc");
        assert!(h.app.esc_armed.is_some());
        press(h, "esc");
        assert!(has(h, |r| *r == Request::Cancel));
    });
    t!(Global, "ctrl+p", |h| {
        press(h, "ctrl+p");
        assert!(matches!(
            h.app.overlay,
            Some(Overlay::Select {
                kind: SelKind::Palette,
                ..
            })
        ));
    });
    t!(Global, "ctrl+t", |h| {
        press(h, "ctrl+t");
        assert!(matches!(h.app.overlay, Some(Overlay::Settings { .. })));
    });
    t!(Global, "shift+tab", |h| {
        press(h, "shift+tab");
        assert!(has(h, |r| *r == Request::SetMode("acceptEdits".into())));
    });
    t!(Global, "ctrl+o", |h| {
        press(h, "ctrl+o");
        assert!(h.app.detail);
    });
    t!(Global, "ctrl+\\", |h| {
        h.draw();
        press(h, "ctrl+\\");
        assert_eq!(h.app.rail, RailMode::On);
    });
    t!(Global, "ctrl+l", |h| {
        press(h, "ctrl+l");
        assert!(h.app.repaint);
    });
    t!(Global, "ctrl+z", |h| {
        press(h, "ctrl+z");
        assert!(h.app.suspend);
    });
    t!(Global, "f1", |h| {
        press(h, "f1");
        assert!(matches!(h.app.overlay, Some(Overlay::Help { .. })));
    });
    t!(Global, "pgup", |h| {
        with_turn(h);
        press(h, "pgup");
        assert_eq!(h.app.focus, Focus::Nav);
        assert!(!h.app.view.sticky);
    });
    t!(Global, "pgdn", |h| {
        with_turn(h);
        press(h, "pgup");
        let top = h.app.view.top;
        h.draw();
        press(h, "pgdn");
        h.draw();
        assert!(h.app.view.top != top || h.app.view.sticky);
    });
    // ---- composer
    t!(Composer, "enter", |h| {
        h.type_str("hello");
        press(h, "enter");
        assert!(has(h, |r| *r == Request::Prompt("hello".into())));
    });
    t!(Composer, "ctrl+j", |h| {
        h.type_str("a");
        press(h, "ctrl+j");
        h.type_str("b");
        assert_eq!(text(h), "a\nb");
    });
    t!(Composer, "shift+enter", |h| {
        h.type_str("a");
        press(h, "shift+enter");
        h.type_str("b");
        assert_eq!(text(h), "a\nb");
    });
    t!(Composer, "alt+enter", |h| {
        h.type_str("a");
        press(h, "alt+enter");
        h.type_str("b");
        assert_eq!(text(h), "a\nb");
    });
    t!(Composer, "ctrl+s", |h| {
        busy(h);
        h.type_str("change course");
        press(h, "ctrl+s");
        assert!(has(h, |r| *r == Request::Steer("change course".into())));
    });
    t!(Composer, "ctrl+r", |h| {
        press(h, "ctrl+r");
        assert!(matches!(
            h.app.overlay,
            Some(Overlay::Select {
                kind: SelKind::History,
                ..
            })
        ));
    });
    t!(Composer, "ctrl+g", |h| {
        h.type_str("draft");
        press(h, "ctrl+g");
        assert!(matches!(
            h.app.external,
            Some(openc::app::External::Editor(ref t)) if t == "draft"
        ));
    });
    t!(Composer, "ctrl+v", |h| {
        // No clipboard tool answers in the test environment: the key says so instead of
        // doing nothing.
        std::env::remove_var("WAYLAND_DISPLAY");
        std::env::remove_var("DISPLAY");
        press(h, "ctrl+v");
        assert!(h.app.flash.is_some() || h.app.composer.text().starts_with("[image"));
    });
    t!(Composer, "tab", |h| {
        h.type_str("/mo");
        let want = format!(
            "{} ",
            h.app.composer.popup.as_ref().unwrap().items[0].insert
        );
        press(h, "tab");
        assert_eq!(text(h), want);
    });
    t!(Composer, "up", |h| {
        busy(h);
        h.type_str("later");
        h.key(KeyCode::Enter);
        assert_eq!(h.app.queue.len(), 1);
        press(h, "up");
        assert_eq!(text(h), "later");
        assert!(h.app.queue.is_empty());
    });
    t!(Composer, "home", |h| {
        with_turn(h);
        press(h, "home");
        assert_eq!(h.app.focus, Focus::Nav);
    });
    t!(Composer, "end", |h| {
        with_turn(h);
        press(h, "pgup");
        press(h, "end");
        assert_eq!(h.app.focus, Focus::Composer);
        assert!(h.app.view.sticky || h.app.view.cursor.is_none());
    });
    // ---- editing
    t!(Edit, "up down", |h| {
        h.type_str("old one");
        h.key(KeyCode::Enter);
        h.event(Event::TurnStart, 5);
        h.event(Event::TurnEnd(StopReason::EndTurn), 5);
        press(h, "up");
        assert_eq!(text(h), "old one");
        press(h, "down");
        assert_eq!(text(h), "");
    });
    t!(Edit, "ctrl+a ctrl+e", |h| {
        h.type_str("hello");
        press(h, "ctrl+a");
        assert_eq!(cursor(h), 0);
        press(h, "ctrl+e");
        assert_eq!(cursor(h), 5);
    });
    t!(Edit, "alt+b alt+f", |h| {
        h.type_str("one two");
        press(h, "alt+b");
        assert_eq!(cursor(h), 4);
        press(h, "alt+f");
        assert_eq!(cursor(h), 7);
    });
    t!(Edit, "ctrl+left ctrl+right", |h| {
        h.type_str("one two");
        press(h, "ctrl+left");
        assert_eq!(cursor(h), 4);
        press(h, "ctrl+right");
        assert_eq!(cursor(h), 7);
    });
    t!(Edit, "ctrl+w alt+backspace", |h| {
        h.type_str("one two");
        press(h, "ctrl+w");
        assert_eq!(text(h), "one ");
        h.type_str("three");
        press(h, "alt+backspace");
        assert_eq!(text(h), "one ");
    });
    t!(Edit, "alt+d", |h| {
        h.type_str("one two");
        press(h, "ctrl+a");
        press(h, "alt+d");
        assert_eq!(text(h), " two");
    });
    t!(Edit, "ctrl+u ctrl+k", |h| {
        h.type_str("one two");
        press(h, "alt+b");
        press(h, "ctrl+k");
        assert_eq!(text(h), "one ");
        press(h, "ctrl+u");
        assert_eq!(text(h), "");
    });
    t!(Edit, "ctrl+y", |h| {
        h.type_str("one two");
        press(h, "alt+b");
        press(h, "ctrl+k");
        press(h, "ctrl+y");
        assert_eq!(text(h), "one two");
    });
    t!(Edit, "ctrl+_", |h| {
        h.type_str("one");
        let big: String = (0..12).map(|i| format!("l{i}\n")).collect();
        h.app.on_paste(&big);
        assert!(text(h).contains("[paste 12 lines]"));
        press(h, "ctrl+_");
        assert_eq!(text(h), "one", "undo takes the whole paste back out");
    });
    t!(Edit, "backspace", |h| {
        let big: String = (0..12).map(|i| format!("l{i}\n")).collect();
        h.app.on_paste(&big);
        press(h, "backspace");
        assert_eq!(text(h), "", "a paste chip goes in one press");
        h.type_str("ab");
        press(h, "backspace");
        assert_eq!(text(h), "a");
    });
    // ---- the completion popup
    t!(Popup, "up down ctrl+p ctrl+n", |h| {
        h.type_str("/");
        let sel = |h: &Harness| h.app.composer.popup.as_ref().unwrap().sel;
        press(h, "down");
        assert_eq!(sel(h), 1);
        press(h, "ctrl+n");
        assert_eq!(sel(h), 2);
        press(h, "ctrl+p");
        assert_eq!(sel(h), 1);
        press(h, "up");
        assert_eq!(sel(h), 0);
        assert!(
            h.app.overlay.is_none(),
            "ctrl+p moves the popup, no palette"
        );
    });
    t!(Popup, "tab enter", |h| {
        h.type_str("/rai");
        press(h, "enter");
        assert_eq!(text(h), "/rail ", "enter completes the name first");
        press(h, "enter");
        assert!(text(h).is_empty(), "the second enter runs it");
    });
    t!(Popup, "esc", |h| {
        h.type_str("/mo");
        press(h, "esc");
        assert!(h.app.composer.popup.is_none());
        assert_eq!(text(h), "/mo");
    });
    // ---- nav mode
    t!(Nav, "j down", |h| {
        nav(h);
        let c = h.app.view.cursor;
        press(h, "k");
        let up = h.app.view.cursor;
        press(h, "j");
        assert_eq!(h.app.view.cursor, c);
        assert!(up != c);
        press(h, "k");
        press(h, "down");
        assert_eq!(h.app.view.cursor, c);
    });
    t!(Nav, "k up", |h| {
        nav(h);
        let c = h.app.view.cursor;
        press(h, "up");
        assert!(h.app.view.cursor != c);
    });
    t!(Nav, "ctrl+d", |h| {
        nav(h);
        press(h, "g");
        let top = h.app.view.top;
        press(h, "ctrl+d");
        assert!(h.app.view.top != top);
    });
    t!(Nav, "ctrl+u", |h| {
        nav(h);
        press(h, "ctrl+u");
        assert!(!h.app.view.sticky);
    });
    t!(Nav, "g home", |h| {
        nav(h);
        press(h, "g");
        let a = h.app.view.cursor;
        press(h, "G");
        press(h, "esc");
        press(h, "home");
        assert_eq!(h.app.view.cursor, a);
    });
    t!(Nav, "G end", |h| {
        nav(h);
        press(h, "g");
        press(h, "G");
        assert_eq!(h.app.focus, Focus::Composer);
        press(h, "esc");
        press(h, "g");
        press(h, "end");
        assert_eq!(h.app.focus, Focus::Composer);
    });
    t!(Nav, "[", |h| {
        nav(h);
        press(h, "[");
        let c = h.app.view.cursor.unwrap();
        assert!(h.app.tr.blocks[c].is_user());
    });
    t!(Nav, "]", |h| {
        nav(h);
        press(h, "g");
        press(h, "]");
        let c = h.app.view.cursor.unwrap();
        assert!(h.app.tr.blocks[c].is_user());
    });
    t!(Nav, "{", |h| {
        nav(h);
        press(h, "{");
        let c = h.app.view.cursor.unwrap();
        assert!(h.app.tr.blocks[c].is_tool());
    });
    t!(Nav, "}", |h| {
        nav(h);
        press(h, "g");
        press(h, "}");
        let c = h.app.view.cursor.unwrap();
        assert!(h.app.tr.blocks[c].is_tool());
    });
    t!(Nav, "o enter", |h| {
        nav(h);
        press(h, "{");
        let before = h.screen();
        press(h, "o");
        let after = h.screen();
        assert!(before != after, "o folds or unfolds");
        press(h, "enter");
        assert_eq!(h.screen(), before, "enter toggles it back");
    });
    t!(Nav, "O", |h| {
        nav(h);
        press(h, "{");
        let before = h.screen();
        press(h, "O");
        assert!(h.screen() != before);
    });
    t!(Nav, "y", |h| {
        nav(h);
        press(h, "{");
        press(h, "y");
        assert!(!openc::clipboard::captured().is_empty());
    });
    t!(Nav, "Y", |h| {
        nav(h);
        press(h, "{");
        let n = openc::clipboard::captured().len();
        press(h, "Y");
        assert!(openc::clipboard::captured().len() > n);
    });
    t!(Nav, "e", |h| {
        nav(h);
        press(h, "{");
        press(h, "e");
        assert!(matches!(
            h.app.external,
            Some(openc::app::External::Pager(_))
        ));
    });
    t!(Nav, "d", |h| {
        nav(h);
        press(h, "d");
        assert!(h.app.diffview.is_some());
        assert!(h.screen().contains("No files changed"));
        press(h, "esc");
        assert!(h.app.diffview.is_none());
    });
    t!(Nav, "r", |h| {
        nav(h);
        press(h, "[");
        press(h, "r");
        assert!(has(h, |r| matches!(r, Request::RewindPreview { .. })));
        // This backend never answers, so the only choice is to take the message back.
        press(h, "4");
        assert!(text(h).starts_with("question"));
        assert_eq!(h.app.focus, Focus::Composer);
    });
    t!(Nav, "a", |h| {
        nav(h);
        h.event(
            Event::Tool(ToolCall {
                id: "ag".into(),
                name: "Agent".into(),
                kind: ToolKind::Think,
                title: "survey".into(),
                status: ToolStatus::Completed,
                ..Default::default()
            }),
            10,
        );
        press(h, "a");
        let c = h.app.view.cursor.expect("the cursor moved to the subagent");
        assert!(matches!(
            &h.app.tr.blocks[c].kind,
            openc::ui::transcript::Kind::Tools(e) if e[0].focus
        ));
    });
    t!(Nav, "/", |h| {
        nav(h);
        press(h, "/");
        assert!(h.app.search_ed.is_some());
    });
    t!(Nav, "n", |h| {
        nav(h);
        press(h, "/");
        h.type_str("answer");
        h.key(KeyCode::Enter);
        let a = h.app.tr.search_counts();
        press(h, "n");
        assert!(h.app.tr.search_counts() != a, "the current match moved");
    });
    t!(Nav, "N", |h| {
        nav(h);
        press(h, "/");
        h.type_str("answer");
        h.key(KeyCode::Enter);
        let a = h.app.tr.search_counts();
        press(h, "N");
        assert!(h.app.tr.search_counts() != a, "the current match moved");
    });
    t!(Nav, "i esc", |h| {
        nav(h);
        press(h, "i");
        assert_eq!(h.app.focus, Focus::Composer);
        press(h, "esc");
        assert_eq!(h.app.focus, Focus::Nav);
        press(h, "esc");
        assert_eq!(h.app.focus, Focus::Composer);
    });
    t!(Nav, "?", |h| {
        nav(h);
        press(h, "?");
        assert!(matches!(h.app.overlay, Some(Overlay::Help { .. })));
    });
    // ---- list dialogs
    t!(Dialog, "up down ctrl+p ctrl+n", |h| {
        press(h, "ctrl+p");
        let sel = |h: &Harness| match &h.app.overlay {
            Some(Overlay::Select { dlg, .. }) => dlg.state.selected_index(),
            _ => None,
        };
        let a = sel(h);
        press(h, "down");
        let b = sel(h);
        assert!(a != b);
        press(h, "ctrl+n");
        let c = sel(h);
        assert!(b != c);
        press(h, "ctrl+p");
        assert_eq!(sel(h), b);
        press(h, "up");
        assert_eq!(sel(h), a);
    });
    t!(Dialog, "pgup pgdn", |h| {
        press(h, "ctrl+p");
        let sel = |h: &Harness| match &h.app.overlay {
            Some(Overlay::Select { dlg, .. }) => dlg.state.selected_index().unwrap_or(0),
            _ => 0,
        };
        press(h, "pgdn");
        assert!(sel(h) > 1);
        press(h, "pgup");
        assert_eq!(sel(h), 0);
    });
    t!(Dialog, "enter", |h| {
        press(h, "ctrl+p");
        h.type_str("repaint");
        press(h, "enter");
        assert!(h.app.overlay.is_none());
        assert!(h.app.repaint, "the palette ran the action");
    });
    t!(Dialog, "tab", |h| {
        h.type_str("remember me");
        press(h, "enter");
        h.event(Event::TurnStart, 5);
        h.event(Event::TurnEnd(StopReason::EndTurn), 5);
        press(h, "ctrl+r");
        press(h, "tab");
        assert_eq!(text(h), "remember me");
        assert!(h.app.overlay.is_none());
    });
    t!(Dialog, "esc", |h| {
        press(h, "ctrl+p");
        press(h, "esc");
        assert!(h.app.overlay.is_none());
    });
    // ---- resume dialog
    t!(Resume, "space", |h| resume_dialog_keys(h, "space"));
    t!(Resume, "ctrl+r", |h| resume_dialog_keys(h, "ctrl+r"));
    t!(Resume, "ctrl+d", |h| resume_dialog_keys(h, "ctrl+d"));
    t!(Resume, "ctrl+a", |h| resume_dialog_keys(h, "ctrl+a"));
    t!(Resume, "ctrl+b", |h| resume_dialog_keys(h, "ctrl+b"));
    // ---- settings
    t!(Settings, "left right", |h| {
        press(h, "ctrl+t");
        press(h, "right");
        assert!(has(h, |r| matches!(r, Request::SetModel(_))));
        press(h, "left");
        assert!(has(h, |r| matches!(r, Request::SetModel(_))));
    });
    t!(Settings, "up down tab", |h| {
        press(h, "ctrl+t");
        press(h, "down");
        press(h, "right");
        assert!(
            has(h, |r| matches!(r, Request::SetEffort(_))),
            "down moved to effort"
        );
        press(h, "tab");
        press(h, "right");
        assert!(
            has(h, |r| matches!(r, Request::SetMode(_))),
            "tab moved to mode"
        );
        press(h, "up");
        press(h, "right");
        assert!(has(h, |r| matches!(r, Request::SetEffort(_))));
    });
    t!(Settings, "enter esc", |h| {
        press(h, "ctrl+t");
        press(h, "enter");
        assert!(h.app.overlay.is_none());
        press(h, "ctrl+t");
        press(h, "esc");
        assert!(h.app.overlay.is_none());
    });
    m
}

fn resume_rows(dir: &std::path::Path) -> Vec<openc::ui::resume::Row> {
    use openc::ui::resume::Row;
    let mk = |id: &str, title: &str, branch: &str| {
        let p = dir.join(format!("{id}.jsonl"));
        std::fs::write(
            &p,
            r#"{"type":"user","message":{"role":"user","content":"hi"}}"#,
        )
        .unwrap();
        Row {
            id: id.into(),
            title: title.into(),
            first_prompt: "hi".into(),
            cwd: "/work/proj".into(),
            branch: branch.into(),
            updated: 100,
            bytes: 10,
            messages: 2,
            approx: false,
            path: Some(p),
        }
    };
    vec![mk("r1", "First", "main"), mk("r2", "Second", "feat")]
}

fn resume_dialog_keys(h: &mut Harness, spec: &str) {
    let dir =
        std::env::temp_dir().join(format!("openc-input-resume-{}-{spec}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let rows = resume_rows(&dir);
    let mut d = openc::ui::resume::Resume::new("/work/proj", "other", "main", 1000);
    d.set_rows(rows.clone(), false);
    d.set_rows(rows, true);
    h.app.overlay = Some(Overlay::Resume(Box::new(d)));
    match spec {
        "space" => {
            press(h, "space");
            assert!(h.screen().contains("you"), "the preview shows the session");
        }
        "ctrl+r" => {
            press(h, "ctrl+r");
            for _ in 0..8 {
                press(h, "backspace");
            }
            h.type_str("Renamed");
            press(h, "enter");
            assert!(h.screen().contains("Renamed"));
            let body = std::fs::read_to_string(dir.join("r1.jsonl")).unwrap();
            assert!(body.contains("custom-title") && body.contains("Renamed"));
        }
        "ctrl+d" => {
            press(h, "ctrl+d");
            press(h, "y");
            assert!(!dir.join("r1.jsonl").exists());
            assert!(dir.join("r2.jsonl").exists());
        }
        "ctrl+a" => {
            press(h, "ctrl+a");
            assert!(h.screen().contains("all projects"));
            press(h, "ctrl+a");
            assert!(h.screen().contains("this directory"));
        }
        "ctrl+b" => {
            press(h, "ctrl+b");
            let s = h.screen();
            assert!(s.contains("First") && !s.contains("Second"), "{s}");
        }
        _ => unreachable!(),
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn every_binding_works() {
    let t = table();
    // A row's checks: one for the whole row, or one per key on it.
    let checks = |scope: Scope, keys: &'static str| -> Vec<Check> {
        if let Some(c) = t.get(&(scope, keys)) {
            return vec![*c];
        }
        keys.split(' ')
            .filter_map(|k| t.get(&(scope, k)).copied())
            .collect()
    };
    let mut missing = Vec::new();
    for bd in BINDINGS {
        let want = if t.contains_key(&(bd.scope, bd.keys)) {
            1
        } else {
            bd.keys.split(' ').count()
        };
        if checks(bd.scope, bd.keys).len() != want {
            missing.push(format!("{:?} `{}`", bd.scope, bd.keys));
        }
    }
    assert!(missing.is_empty(), "no check for: {missing:#?}");
    let mut ran = 0;
    for bd in BINDINGS {
        for check in checks(bd.scope, bd.keys) {
            let mut h = started();
            check(&mut h);
            ran += 1;
        }
    }
    assert!(ran >= BINDINGS.len());
}

// ---- images, history, drafts

fn tiny_png() -> Vec<u8> {
    let mut v = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR".to_vec();
    v.extend(64u32.to_be_bytes());
    v.extend(48u32.to_be_bytes());
    v.extend([8, 2, 0, 0, 0, 0, 0, 0, 0]);
    v
}

#[test]
fn pasting_an_image_path_makes_a_chip_and_the_prompt_carries_the_image() {
    let mut h = started();
    let f = std::env::temp_dir().join(format!("openc-input-{}.png", std::process::id()));
    std::fs::write(&f, tiny_png()).unwrap();
    h.type_str("what is in ");
    h.app.on_paste(&f.to_string_lossy());
    assert_eq!(text(&h), "what is in [image 64x48]");
    assert!(h.screen().contains("[image 64x48]"));
    h.key(KeyCode::Enter);
    let sent = h.sent();
    let Some(Request::PromptWith { text, images }) = sent.into_iter().next() else {
        panic!("no PromptWith");
    };
    assert_eq!(text, "what is in [image 64x48]");
    assert_eq!(images.len(), 1);
    assert_eq!(images[0].media_type, "image/png");
    assert!(
        images[0].base64.starts_with("iVBORw0KGgo"),
        "{}",
        images[0].base64
    );
    let _ = std::fs::remove_file(f);
}

#[test]
fn an_image_message_waits_in_the_queue_with_its_image() {
    let mut h = started();
    let f = std::env::temp_dir().join(format!("openc-input-q-{}.png", std::process::id()));
    std::fs::write(&f, tiny_png()).unwrap();
    h.type_str("busy");
    h.key(KeyCode::Enter);
    h.event(Event::TurnStart, 5);
    let _ = h.sent();
    h.app.on_paste(&f.to_string_lossy());
    h.key(KeyCode::Enter);
    assert!(h.sent().is_empty(), "queued while busy");
    assert_eq!(h.app.queue.len(), 1);
    h.event(Event::TurnEnd(StopReason::EndTurn), 5);
    let sent = h.sent();
    assert!(
        matches!(sent.first(), Some(Request::PromptWith { images, .. }) if images.len() == 1),
        "{sent:?}"
    );
    let _ = std::fs::remove_file(f);
}

#[test]
fn steering_with_an_image_queues_instead_of_dropping_it() {
    let mut h = started();
    let f = std::env::temp_dir().join(format!("openc-input-s-{}.png", std::process::id()));
    std::fs::write(&f, tiny_png()).unwrap();
    h.type_str("busy");
    h.key(KeyCode::Enter);
    h.event(Event::TurnStart, 5);
    let _ = h.sent();
    h.app.on_paste(&f.to_string_lossy());
    press(&mut h, "ctrl+s");
    assert!(!has(&mut h, |r| matches!(r, Request::Steer(_))));
    assert_eq!(h.app.queue.len(), 1);
    let _ = std::fs::remove_file(f);
}

#[test]
fn a_recalled_paste_still_sends_its_full_text() {
    let mut h = started();
    let blob: String = (0..20).map(|i| format!("row {i}\n")).collect();
    h.app.on_paste(&blob);
    h.key(KeyCode::Enter);
    let first = h.sent();
    assert!(matches!(&first[0], Request::Prompt(t) if t.lines().count() == 20));
    h.event(Event::TurnStart, 5);
    h.event(Event::TurnEnd(StopReason::EndTurn), 5);
    h.key(KeyCode::Up);
    assert_eq!(text(&h), "[paste 20 lines]");
    h.key(KeyCode::Enter);
    let again = h.sent();
    assert!(
        matches!(&again[0], Request::Prompt(t) if t.lines().count() == 20),
        "{again:?}"
    );
}

#[test]
fn history_up_walks_by_prefix_and_ctrl_r_searches_fuzzily() {
    let mut h = started();
    for m in ["cargo build", "git status", "cargo test"] {
        h.type_str(m);
        h.key(KeyCode::Enter);
        h.event(Event::TurnStart, 5);
        h.event(Event::TurnEnd(StopReason::EndTurn), 5);
        let _ = h.sent();
    }
    h.type_str("git");
    h.key(KeyCode::Up);
    assert_eq!(
        text(&h),
        "git status",
        "up keeps to entries with this prefix"
    );
    h.key(KeyCode::Down);
    assert_eq!(text(&h), "git", "down returns to what was typed");
    // A draft nothing starts with still walks the whole history, and down returns the draft.
    h.app.composer.clear();
    h.type_str("abc");
    h.key(KeyCode::Up);
    assert_eq!(
        text(&h),
        "cargo test",
        "up walks even with a draft no entry starts with"
    );
    h.key(KeyCode::Up);
    assert_eq!(text(&h), "git status");
    h.key(KeyCode::Down);
    h.key(KeyCode::Down);
    assert_eq!(text(&h), "abc", "the draft comes back");
    h.app.composer.clear();
    press(&mut h, "ctrl+r");
    h.type_str("cbld");
    let shown = match &h.app.overlay {
        Some(Overlay::Select { dlg, .. }) => dlg.state.visible_indices().len(),
        _ => panic!("no history dialog"),
    };
    assert_eq!(shown, 1, "only `cargo build` matches `cbld`");
    press(&mut h, "enter");
    assert_eq!(text(&h), "cargo build");
    assert!(h.sent().is_empty(), "picking an entry does not send it");
}

#[test]
fn the_draft_is_written_half_a_second_after_the_last_key_and_a_send_clears_it() {
    let mut h = started();
    h.type_str("unsent thought");
    let sid = h.app.session_id.clone();
    assert!(openc::store::take_draft(&sid).is_none(), "not yet");
    let due = h.app.next_wake(h.now()).expect("a wake is scheduled");
    h.t += 600;
    assert!(due <= h.now());
    let now = h.now();
    h.app.on_wake(now);
    // take_draft deletes what it reads; put it back to check the send path next.
    let d = openc::store::take_draft(&sid).expect("saved");
    assert_eq!(d, "unsent thought");
    openc::store::save_draft(&sid, &d);
    h.key(KeyCode::Enter);
    h.t += 600;
    let now = h.now();
    h.app.on_wake(now);
    assert!(
        openc::store::take_draft(&sid).is_none(),
        "sent text is not a draft"
    );
}

#[test]
fn the_external_editor_keeps_chips_when_nothing_changed_and_takes_edits() {
    let mut h = started();
    let blob: String = (0..12).map(|i| format!("row {i}\n")).collect();
    h.type_str("see ");
    h.app.on_paste(&blob);
    let original = h.app.composer.expanded();
    h.app.editor_returned(&original, Some(original.clone()));
    assert_eq!(
        text(&h),
        "see [paste 12 lines]",
        "unchanged: the chip stays a chip"
    );
    h.app
        .editor_returned(&original, Some(format!("{original}\nmore\n")));
    assert!(text(&h).starts_with("see row 0") && text(&h).ends_with("more"));
    h.app.composer.set_text("kept");
    h.app.editor_returned("kept", None);
    assert_eq!(text(&h), "kept", "a failed editor leaves the draft alone");
}

#[test]
fn slash_popup_rows_have_descriptions_and_hints_and_files_honour_the_list() {
    let mut h = started();
    h.event(
        Event::Commands(vec![
            agent_core::SlashCommand {
                name: "zzfoo".into(),
                description: "run the loop".into(),
                input_hint: "<bar>".into(),
            },
            agent_core::SlashCommand {
                name: "compact".into(),
                description: "summarise the conversation".into(),
                input_hint: "[focus]".into(),
            },
        ]),
        5,
    );
    h.type_str("/comp");
    let s = h.screen();
    assert!(
        s.contains("/compact") && s.contains("[focus] summarise the conversation"),
        "{s}"
    );
    h.app.composer.clear();
    h.type_str("/zzfo");
    let s = h.screen();
    assert!(
        s.contains("<bar> run the loop") && s.contains("skill"),
        "a backend skill carries its source tag:\n{s}"
    );
    h.app.composer.clear();
    h.app.files = vec!["src/lib.rs".into(), "src/main.rs".into(), "src/".into()];
    h.type_str("look at @lib");
    let s = h.screen();
    assert!(s.contains("src/lib.rs"), "{s}");
    press(&mut h, "tab");
    assert_eq!(text(&h), "look at @src/lib.rs ");
}

#[test]
fn the_busy_composer_says_what_enter_does_and_shows_the_queue() {
    let mut h = started();
    busy(&mut h);
    let s = h.screen();
    assert!(
        s.contains("enter queues") && s.contains("ctrl+s steers"),
        "{s}"
    );
    h.type_str("next");
    h.key(KeyCode::Enter);
    let s = h.screen();
    assert!(
        s.contains("○ queued: next") && s.contains("up takes the last back"),
        "{s}"
    );
    h.type_str("typing");
    let s = h.screen();
    assert!(
        s.contains("typing") && s.contains("enter queues"),
        "the hint stays beside text:\n{s}"
    );
}

#[test]
fn keys_do_nothing_surprising_to_the_editor_text() {
    // ctrl+d with text deletes forward instead of quitting.
    let mut h = started();
    h.type_str("ab");
    press(&mut h, "ctrl+a");
    press(&mut h, "ctrl+d");
    assert_eq!(text(&h), "b");
    assert!(!h.app.should_quit);
    let _ = KeyEvent::new(KeyCode::Null, KeyModifiers::NONE);
}
