//! Rough cost of a frame. Not a benchmark: it fails only if a frame takes absurdly long, which
//! would mean the per-message cache stopped working.

mod common;

use std::time::Instant;

use agent_core::Event;
use common::*;

#[test]
fn a_long_transcript_and_a_streaming_tail_stay_cheap() {
    let mut h = Harness::new(150, 42);
    let body = "Some **bold** text and `code`, a [link](https://example.com) and more words to wrap around the line. ".repeat(6);
    for i in 0..100 {
        h.turn(&format!("question {i}"), &format!("# Heading {i}\n\n{body}\n\n- item one\n- item two\n\n```rust\nfn main() {{ println!(\"{i}\"); }}\n```"));
    }
    let t = Instant::now();
    h.text();
    let first = t.elapsed();

    // stream a long answer and draw after every delta
    h.app.send_prompt("stream".into());
    h.event(Event::TurnStart);
    let t = Instant::now();
    let n = 300;
    for i in 0..n {
        h.event(Event::TextDelta(format!(
            "word{i} lorem ipsum dolor sit amet, consectetur adipiscing elit. "
        )));
        h.text();
    }
    let per = t.elapsed() / n;
    eprintln!("first draw of 100 turns: {first:?}; streaming draw: {per:?} each");
    assert!(per.as_millis() < 100, "streaming frame took {per:?}");
}
