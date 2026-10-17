//! Screens against captures of the real jcode (`tests/reference`) and against text snapshots
//! of the look itself (`tests/snapshots`, updated with `UPDATE_SNAPSHOTS=1`).

mod common;

use common::cells::assert_block;
use common::*;
use crossterm::event::KeyCode;

const SIZES: [(u16, u16); 3] = [(80, 24), (120, 40), (200, 50)];

fn each(name: &str, f: impl Fn(&mut Harness)) {
    for (w, h) in SIZES {
        let mut hn = Harness::new(w, h);
        f(&mut hn);
        assert_snapshot(&format!("{name}-{w}x{h}"), &hn.text());
    }
}

#[test]
fn cold_start() {
    each("cold", |_| {});
}

#[test]
fn typing_and_newlines() {
    each("multiline", |h| {
        h.type_str("line one");
        h.key(KeyCode::Enter, crossterm::event::KeyModifiers::ALT);
        h.type_str("line two");
    });
}

#[test]
fn slash_suggestions() {
    each("slash", |h| h.type_str("/"));
    each("slash-filter", |h| h.type_str("/mo"));
}

#[test]
fn model_picker() {
    each("model", |h| h.type_str("/model"));
}

#[test]
fn help_overlay() {
    each("help", |h| h.say("/help"));
}

#[test]
fn session_picker() {
    each("resume", |h| {
        h.say("/resume");
        h.event(agent_core::Event::Sessions(jcodew::fake::sessions(
            std::path::Path::new(CWD),
        )));
        if let Some(jcodew::app::Overlay::Sessions { .. }) = h.app.overlay {
            // ages are relative to now; the snapshot keeps the rest
            for s in &mut h.app.sessions {
                s.updated = 0;
            }
        }
    });
}

#[test]
fn a_plain_reply_matches_the_capture() {
    for (w, h) in SIZES {
        let mut hn = Harness::new(w, h);
        hn.turn("fake:text");
        let t = hn.render();
        // the prompt row is left out: jcode can leave the last frame of its 450 ms entry
        // pulse on screen, and one capture caught it
        assert_block(
            &format!("text-{w}x{h}"),
            &t,
            "Hello from the scripted",
            "Hello from the scripted",
        );
        assert_snapshot(&format!("text-{w}x{h}"), &hn.text());
    }
}

#[test]
fn an_edit_matches_the_capture() {
    for (w, h) in SIZES {
        let mut hn = Harness::new(w, h);
        hn.turn("fake:edit");
        let t = hn.render();
        assert_block(
            &format!("edit-{w}x{h}"),
            &t,
            "Creating the file.",
            "Edited ",
        );
    }
}

#[test]
fn commands_match_the_capture() {
    for (w, h) in SIZES {
        let mut hn = Harness::new(w, h);
        hn.turn("fake:bash");
        let t = hn.render();
        assert_block(
            &format!("bash-{w}x{h}"),
            &t,
            "fake:bash",
            "Three lines were printed.",
        );
        let mut hn = Harness::new(w, h);
        hn.turn("fake:bashfail");
        let t = hn.render();
        assert_block(
            &format!("bashfail-{w}x{h}"),
            &t,
            "fake:bashfail",
            "The command failed.",
        );
    }
}

#[test]
fn markdown_matches_the_capture() {
    // at 80x24 the reply runs past the screen; its tail is on both
    for ((w, h), from) in SIZES
        .into_iter()
        .zip(["name │ value", "Heading one", "Heading one"])
    {
        let mut hn = Harness::new(w, h);
        hn.turn("fake:markdown");
        let t = hn.render();
        assert_block(&format!("markdown-{w}x{h}"), &t, from, "Done.");
    }
}

#[test]
fn the_todo_band_matches_the_capture() {
    for (w, h) in SIZES {
        let mut hn = Harness::new(w, h);
        hn.turn("fake:todo");
        let t = hn.render();
        // jcode then clips the transcript's last rows under the band while the screen is not
        // full; the look scrolls instead, so only the band is compared
        assert_block(&format!("todo-{w}x{h}"), &t, "●●○", "Run the tests");
    }
}

#[test]
fn a_running_turn_and_a_queued_prompt() {
    each("queued", |h| {
        h.say("fake:slow");
        h.begin();
        h.advance(3_000);
        h.say("next one");
    });
}
