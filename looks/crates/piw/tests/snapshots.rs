//! Regression snapshots of piw's screens at the three sizes the reference captures use. Update
//! with `UPDATE_SNAPSHOTS=1 cargo test -p piw --test snapshots` and read the diff.
//! Set `PIW_DUMP_DIR` to also write `.ansi` files for `tools/cmp-ansi.py`.

mod common;

use agent_core::{Event, StopReason, ToolKind};
use common::*;
use crossterm::event::{KeyCode, KeyModifiers};

const SIZES: [(u16, u16); 3] = [(120, 36), (150, 42), (80, 24)];

fn each(name: &str, f: impl Fn(&mut Harness)) {
    for (w, h) in SIZES {
        let mut hn = Harness::new(w, h);
        f(&mut hn);
        hn.dump(&format!("{name}-{w}x{h}"));
        assert_snapshot(&format!("{name}-{w}x{h}"), &hn.text());
    }
}

#[test]
fn startup() {
    each("startup", |_| {});
}

#[test]
fn startup_expanded() {
    each("startup-expanded", |h| h.ctrl('o'));
}

#[test]
fn typed_and_multiline() {
    each("typed", |h| h.type_str("hello world"));
    each("multiline", |h| {
        h.type_str("first");
        h.key(KeyCode::Enter, KeyModifiers::SHIFT);
        h.type_str("second");
        h.key(KeyCode::Enter, KeyModifiers::SHIFT);
        h.type_str("third");
    });
}

#[test]
fn long_line_wraps_at_the_width_minus_one() {
    each("wrap", |h| h.type_str(&"word ".repeat(60)));
}

#[test]
fn editor_grows_then_scrolls() {
    each("grown", |h| {
        for i in 1..=13 {
            h.type_str(&format!("line {i}"));
            h.key(KeyCode::Enter, KeyModifiers::SHIFT);
        }
        h.type_str("end");
    });
}

#[test]
fn paste_markers() {
    each("paste-lines", |h| {
        let big: String = (1..=30).map(|i| format!("pasted line {i}\n")).collect();
        h.paste(&big);
    });
    each("paste-chars", |h| h.paste(&"x".repeat(1500)));
}

#[test]
fn bash_mode_rules() {
    each("bash-mode", |h| h.type_str("!ls"));
}

#[test]
fn slash_menu() {
    each("slash", |h| h.type_str("/"));
    each("slash-filtered", |h| h.type_str("/se"));
    each("slash-model-arg", |h| h.type_str("/model "));
}

#[test]
fn at_menu() {
    each("at-files", |h| {
        h.app.update(piw::app::Msg::Files(
            ["src/main.rs", "src/lib.rs", "README.md", "data/long.txt"]
                .map(String::from)
                .to_vec(),
        ));
        h.type_str("look at @");
    });
}

#[test]
fn thinking_level_colours() {
    each("thinking-cycle", |h| {
        h.press(KeyCode::BackTab);
        h.press(KeyCode::BackTab);
    });
}

#[test]
fn answer_and_footer() {
    each("answer", |h| {
        h.turn(
            "hello",
            "Hello! I can read, write and edit files in this project.",
        );
    });
}

#[test]
fn working_indicator_and_queue() {
    each("working", |h| {
        h.app.send_prompt("slow please".into());
        h.event(Event::TurnStart);
        h.event(Event::TextDelta("This answer streams slowly.".into()));
    });
    each("queued", |h| {
        h.app.send_prompt("slow please".into());
        h.event(Event::TurnStart);
        h.event(Event::TextDelta("This answer streams slowly.".into()));
        h.type_str("steer this");
        h.press(KeyCode::Enter);
        h.type_str("follow up");
        h.key(KeyCode::Enter, KeyModifiers::ALT);
    });
}

#[test]
fn escape_aborts_and_restores_the_queue() {
    each("aborted", |h| {
        h.app.send_prompt("slow please".into());
        h.event(Event::TurnStart);
        h.event(Event::TextDelta("This answer streams slowly.".into()));
        h.type_str("steer this");
        h.press(KeyCode::Enter);
        h.press(KeyCode::Esc);
        h.event(Event::TurnEnd(StopReason::Cancelled));
    });
}

#[test]
fn thinking_blocks() {
    each("thinking", |h| {
        h.app.send_prompt("think".into());
        h.event(Event::TurnStart);
        h.event(Event::ThoughtDelta(
            "The user wants a short answer.\n\nSecond paragraph of thinking.".into(),
        ));
        h.event(Event::TextDelta("Done. The answer is **42**.".into()));
        h.event(Event::TurnEnd(StopReason::EndTurn));
    });
    each("thinking-hidden", |h| {
        h.app.send_prompt("think".into());
        h.event(Event::TurnStart);
        h.event(Event::ThoughtDelta("The user wants a short answer.".into()));
        h.event(Event::TextDelta("Done.".into()));
        h.event(Event::TurnEnd(StopReason::EndTurn));
        h.ctrl('t');
    });
}

#[test]
fn error_turn() {
    each("error", |h| {
        h.app.send_prompt("fail".into());
        h.event(Event::TurnStart);
        h.event(Event::Notice {
            level: agent_core::NoticeLevel::Error,
            text: "400: {\"message\":\"scripted bad request\"}".into(),
        });
        h.event(Event::TurnEnd(StopReason::Error));
    });
}

#[test]
fn model_selector() {
    each("model-selector", |h| h.ctrl('l'));
}

#[test]
fn tool_blocks() {
    each("tools", |h| {
        h.app.send_prompt("tools please".into());
        h.event(Event::TurnStart);
        h.event(Event::TextDelta("I'll look around first.".into()));
        h.event(Event::Tool(tool(
            "1",
            "list_files",
            ToolKind::Read,
            ".",
            serde_json::json!({"path": "."}),
            Some(".git/\nAGENTS.md\ndata/\nsrc/"),
        )));
        h.event(Event::Tool(tool(
            "2",
            "execute",
            ToolKind::Execute,
            "git status --short",
            serde_json::json!({"command": "git status --short"}),
            Some("?? AGENTS.md\n?? pic.png"),
        )));
        h.event(Event::TextDelta("All done.".into()));
        h.event(Event::TurnEnd(StopReason::EndTurn));
    });
}

#[test]
fn exit_transcript_has_the_resume_hint() {
    let mut h = Harness::new(100, 30);
    h.turn("hello", "Hello!");
    let out = h.app.epilogue(100);
    let plain = strip_ansi(&out);
    assert!(plain.contains("hello"), "{plain}");
    assert!(
        plain.contains("~/proj") || plain.contains("/home/me/proj"),
        "{plain}"
    );
    assert!(
        plain
            .trim_end()
            .ends_with("To resume this session: wizard-ui-pi --session ses_1234567890"),
        "{plain}"
    );
    assert!(out.contains("\r\n"));
    assert!(!out.contains("\x1b[H"), "no cursor addressing");
}

fn strip_ansi(s: &str) -> String {
    let mut out = String::new();
    let mut it = s.chars().peekable();
    while let Some(c) = it.next() {
        if c == '\x1b' {
            for d in it.by_ref() {
                if d == 'm' {
                    break;
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}
