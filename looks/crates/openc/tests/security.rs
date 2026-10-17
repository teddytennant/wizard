//! Text that reaches the user's terminal outside ratatui, and text a reader cannot see.
mod common;

use agent_core::{Event, StopReason};
use common::Harness;

const EVIL: &str =
    "x\x1b]52;c;cHduZWQ=\x07y\x1b[2Jz\x1b[?1000h\x1b[6n\x1bc\x1b]0;t\x1b\\\u{9b}2J\u{202e}end";

#[test]
fn exit_summary_holds_no_control_characters() {
    let mut h = Harness::new(100, 30);
    h.ready();
    h.event(Event::TurnStart, 1);
    h.event(
        Event::TextDelta(format!("answer {EVIL}\n```\n{EVIL}\n```")),
        1,
    );
    h.event(Event::TurnEnd(StopReason::EndTurn), 1);
    let s = h.app.exit_summary();
    assert!(s.contains("answer xyz"), "{s:?}");
    assert!(
        !s.chars()
            .any(|c| (c.is_control() && c != '\n') || tuikit::width::is_format_spoof(c)),
        "exit_summary carries control characters: {s:?}"
    );
    assert!(!s.contains("52;c"), "an OSC body survived: {s:?}");
}

#[test]
fn a_mouse_report_at_the_edge_of_the_u16_range_does_not_overflow() {
    // `CSI < 32 ; 65536 ; 65536 M` decodes to row 65535, and `row + 1` overflowed in a debug build.
    use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
    let mut h = Harness::new(100, 30);
    h.ready();
    h.draw();
    let now = h.now();
    for kind in [
        MouseEventKind::Drag(MouseButton::Left),
        MouseEventKind::Down(MouseButton::Left),
        MouseEventKind::Up(MouseButton::Left),
        MouseEventKind::Moved,
        MouseEventKind::ScrollDown,
    ] {
        h.app.on_mouse(
            MouseEvent {
                kind,
                column: u16::MAX,
                row: u16::MAX,
                modifiers: KeyModifiers::NONE,
            },
            now,
        );
    }
}

#[test]
fn tool_output_cannot_hide_text_from_the_reader() {
    use agent_core::{ToolCall, ToolKind, ToolStatus};
    use ratatui::style::Modifier;
    let mut h = Harness::new(100, 30);
    h.ready();
    h.event(Event::TurnStart, 1);
    h.event(
        Event::Tool(ToolCall {
            id: "t".into(),
            name: "Bash".into(),
            kind: ToolKind::Execute,
            title: "ls".into(),
            status: ToolStatus::Completed,
            output: Some(
                "visible\n\x1b[8mSECRETINSTRUCTION\x1b[0m\n\x1b[30;40mblack on black\x1b[0m".into(),
            ),
            ..Default::default()
        }),
        1,
    );
    h.event(Event::TurnEnd(StopReason::EndTurn), 1);
    h.key(crossterm::event::KeyCode::Esc);
    h.ctrl('o');
    h.draw();
    let buf = h.term.backend().buffer().clone();
    let shown = tuikit::testing::plain_text(&buf);
    assert!(shown.contains("SECRETINSTRUCTION"), "{shown}");
    assert!(shown.contains("black on black"), "{shown}");
    let mut hidden = 0;
    let mut same = 0;
    for y in 0..buf.area.height {
        for x in 0..buf.area.width {
            let c = &buf[(x, y)];
            if c.symbol() == " " {
                continue;
            }
            if c.modifier.contains(Modifier::HIDDEN) {
                hidden += 1;
            }
            if c.fg == c.bg {
                same += 1;
            }
        }
    }
    assert_eq!(hidden, 0, "{hidden} cells are concealed");
    assert_eq!(
        same, 0,
        "{same} cells have the same colour as their background"
    );
}

#[test]
fn export_is_private_and_never_overwrites() {
    use std::os::unix::fs::PermissionsExt;
    let dir = std::env::temp_dir().join(format!("openc-export-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let mut h = Harness::new(100, 30);
    h.ready();
    h.app.opts.cwd = dir.clone();
    h.event(Event::TurnStart, 1);
    h.event(Event::TextDelta("an answer with a secret".into()), 1);
    h.event(Event::TurnEnd(StopReason::EndTurn), 1);
    std::fs::write(dir.join("taken.md"), "mine").unwrap();
    for (name, want) in [("out.md", "wrote out.md"), ("taken.md", "exists")] {
        h.type_str(&format!("/export {name}"));
        h.key(crossterm::event::KeyCode::Enter);
        let s = h.screen();
        assert!(s.contains(want), "{name}: {s}");
    }
    let mode = std::fs::metadata(dir.join("out.md"))
        .unwrap()
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o600, "the export is readable by others");
    assert!(std::fs::read_to_string(dir.join("out.md"))
        .unwrap()
        .contains("a secret"));
    assert_eq!(
        std::fs::read_to_string(dir.join("taken.md")).unwrap(),
        "mine"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
