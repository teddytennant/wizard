//! OSC 8 links: the markdown label and the printed URL both carry the destination, bare URLs in
//! prose are linked, code is not, and the escapes never change what is on screen.

use agent_core::{Config, Event};
use codexw::fake::scenario_events;
use codexw::testing::harness;

fn ready() -> Event {
    Event::Ready {
        session_id: "s1".into(),
        config: Config {
            model: "gpt-5.5".into(),
            effort: "default".into(),
            ..Default::default()
        },
    }
}

fn answer(cols: u16, rows: u16, text: &str) -> codexw::testing::Harness {
    let mut h = harness(cols, rows, 7);
    h.app.log.notes = false;
    h.app.on_backend(ready());
    h.draw();
    h.app.on_backend(Event::TurnStart);
    h.app.on_backend(Event::TextDelta(text.into()));
    h.app
        .on_backend(Event::TurnEnd(agent_core::StopReason::EndTurn));
    h.draw();
    h.draw();
    h
}

#[test]
fn markdown_link_label_and_suffix_url_both_link() {
    let h = answer(
        100,
        30,
        "See [the docs](https://doc.rust-lang.org/std/primitive.i32.html) now.\n",
    );
    let url = "https://doc.rust-lang.org/std/primitive.i32.html";
    let links = h.screen.links();
    assert_eq!(
        links,
        vec![
            ("the docs".to_string(), url.to_string()),
            (url.to_string(), url.to_string())
        ],
        "{}",
        h.screen.full_text()
    );
    // Same visible text as without the escapes.
    assert!(
        h.screen
            .full_text()
            .contains(&format!("the docs ({url}) now."))
    );
}

#[test]
fn bare_url_links_and_trailing_punctuation_stays_out() {
    let h = answer(
        100,
        30,
        "Try https://example.com/a?b=1, or (https://example.org/x).\n",
    );
    let got: Vec<String> = h.screen.links().into_iter().map(|l| l.1).collect();
    assert_eq!(got, ["https://example.com/a?b=1", "https://example.org/x"]);
}

#[test]
fn code_spans_and_blocks_are_not_linked() {
    let h = answer(
        100,
        30,
        "`https://example.com/inline`\n\n```\nhttps://example.com/block\n```\n",
    );
    assert!(h.screen.links().is_empty(), "{:?}", h.screen.links());
}

#[test]
fn non_web_destinations_are_not_linked() {
    let h = answer(
        100,
        30,
        "[a](file:///etc/passwd) and [b](javascript:alert(1))\n",
    );
    assert!(h.screen.links().is_empty());
}

#[test]
fn a_wrapped_url_is_linked_on_every_row() {
    let h = answer(
        40,
        30,
        "go to https://example.com/a/very/long/path/that/cannot/fit/on/one/row/of/forty now\n",
    );
    let links = h.screen.links();
    assert!(!links.is_empty());
    let joined: String = links.iter().map(|l| l.0.as_str()).collect();
    assert_eq!(
        joined,
        "https://example.com/a/very/long/path/that/cannot/fit/on/one/row/of/forty"
    );
    assert!(links.iter().all(|l| l.1 == links[0].1));
}

#[test]
fn the_scripted_markdown_turn_links_its_docs_url() {
    let mut h = harness(120, 60, 7);
    h.app.log.notes = false;
    h.app.on_backend(ready());
    h.draw();
    h.type_str("go fake:markdown");
    h.key(crossterm::event::KeyCode::Enter);
    h.draw();
    for ev in scenario_events("markdown", "/tmp/cxw-home/proj").unwrap() {
        h.app.on_backend(ev);
        h.draw();
    }
    h.draw();
    let urls: Vec<String> = h.screen.links().into_iter().map(|l| l.1).collect();
    assert!(
        urls.iter().any(|u| u.contains("doc.rust-lang.org")),
        "{urls:?}"
    );
}
