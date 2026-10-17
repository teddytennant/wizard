//! How the main loop takes events off its queue without letting a backlog starve the screen.
//!
//! A fast model, or a tool that prints a lot, can queue thousands of backend events between two
//! frames. Applying all of them before drawing meant nothing was drawn and no key was handled
//! until the queue ran dry (a 100 MB stream delayed the first key by 0.3 to 3 seconds). So the
//! loop applies events for a slice of time, goes back around to draw, and merges the runs of
//! text deltas it finds so a thousand of them cost one append.

use agent_core::Event;
use tuikit::pump::BackendMsg;
pub use tuikit::pump::{drain_slice, merge_deltas, weight, FRAME_BYTES, MERGE_CAP, SLICE};

use crate::app::Msg;

impl BackendMsg for Msg {
    fn backend(&self) -> Option<&Event> {
        match self {
            Msg::Backend(e) => Some(e),
            _ => None,
        }
    }

    fn from_backend(e: Event) -> Self {
        Msg::Backend(e)
    }

    fn into_backend(self) -> Result<Event, Self> {
        match self {
            Msg::Backend(e) => Ok(e),
            m => Err(m),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::time::{Duration, Instant};
    use tuikit::term::Event as TermEvent;

    fn text(s: &str) -> TermEvent<Msg> {
        TermEvent::Msg(Msg::Backend(Event::TextDelta(s.into())))
    }

    fn body(e: &TermEvent<Msg>) -> Option<&str> {
        match e {
            TermEvent::Msg(Msg::Backend(Event::TextDelta(t) | Event::ThoughtDelta(t))) => Some(t),
            _ => None,
        }
    }

    #[test]
    fn an_endless_queue_cannot_hold_the_loop() {
        let t0 = Instant::now();
        let mut applied = 0u64;
        let n = drain_slice(
            || Some(1u8),
            |_| {
                applied += 1;
                0
            },
            Duration::from_millis(5),
            usize::MAX,
        );
        assert!(n > 0 && n as u64 == applied);
        assert!(
            t0.elapsed() < Duration::from_millis(500),
            "{:?}",
            t0.elapsed()
        );
    }

    #[test]
    fn a_zero_budget_still_applies_one_event() {
        let mut q: VecDeque<u8> = (0..10).collect();
        assert_eq!(
            drain_slice(|| q.pop_front(), |_| 0, Duration::ZERO, usize::MAX),
            1
        );
        assert_eq!(q.len(), 9);
    }

    #[test]
    fn the_drain_stops_once_enough_text_is_applied() {
        let mut q: VecDeque<usize> = std::iter::repeat_n(10_000, 100).collect();
        let n = drain_slice(
            || q.pop_front(),
            |w| w,
            Duration::from_secs(10),
            FRAME_BYTES,
        );
        // 7 deltas of 10,000 bytes reach 64 KB, so the rest wait for the next frame
        assert_eq!(n, 7);
        assert_eq!(q.len(), 93);
        assert_eq!(weight(&text("abcd")), 4);
        assert_eq!(weight(&TermEvent::<Msg>::Tick), 0);
    }

    #[test]
    fn a_run_of_deltas_becomes_one_event_and_stops_at_the_first_other_event() {
        let mut q = VecDeque::from(vec![text("b"), text("c"), TermEvent::Tick, text("d")]);
        let (e, held) = merge_deltas(text("a"), || q.pop_front());
        assert_eq!(body(&e), Some("abc"));
        assert!(matches!(held, Some(TermEvent::Tick)));
        assert_eq!(q.len(), 1);
        // a thought delta does not swallow a text delta
        let mut q = VecDeque::from(vec![text("t")]);
        let first = TermEvent::Msg(Msg::Backend(Event::ThoughtDelta("h".into())));
        let (e, held) = merge_deltas(first, || q.pop_front());
        assert_eq!(body(&e), Some("h"));
        assert_eq!(held.as_ref().and_then(body), Some("t"));
    }

    #[test]
    fn merging_is_capped() {
        let big = "x".repeat(MERGE_CAP);
        let mut q = VecDeque::from(vec![text("y")]);
        let (e, held) = merge_deltas(text(&big), || q.pop_front());
        assert_eq!(body(&e).map(str::len), Some(MERGE_CAP));
        assert!(held.is_none());
        assert_eq!(q.len(), 1);
    }
}
