//! A backlog of backend events. Finding 4: appending text was quadratic, so a long answer
//! applied with no drawing took tens of seconds.
mod common;

use agent_core::Event;
use common::Harness;
use std::time::Instant;

fn apply(mb: usize) -> std::time::Duration {
    let mut h = Harness::new(120, 40);
    h.ready();
    h.event(Event::TurnStart, 1);
    let para = ("A paragraph about the parser, with `code` and **bold**.\n\n".repeat(40))[..2000]
        .to_string();
    let t = Instant::now();
    for _ in 0..(mb * 500) {
        h.event(Event::TextDelta(para.clone()), 0);
    }
    t.elapsed()
}

#[test]
fn applying_text_deltas_is_linear_in_the_text() {
    // Warm up so the first run does not pay for page faults.
    let _ = apply(1);
    let small = apply(2);
    let big = apply(8);
    // 4x the text: linear is about 4x, the old recount was about 16x.
    let ratio = big.as_secs_f64() / small.as_secs_f64().max(1e-6);
    assert!(
        ratio < 9.0,
        "8 MB took {big:?}, 2 MB took {small:?} (ratio {ratio:.1}); appending is not linear"
    );
    assert!(big.as_secs() < 5, "8 MB of deltas took {big:?}");
}
