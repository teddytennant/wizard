//! How the main loop takes events off its queue without letting a backlog starve the screen.
//!
//! A fast model, or a tool that prints a lot, can queue thousands of backend events between two
//! frames. Applying all of them before drawing means nothing is drawn and no key is read until
//! the queue is empty; `ctrl+c` included. So the loop applies events for a slice of time
//! ([`SLICE`]), then goes back around to draw and to see what the keyboard did.

use std::time::{Duration, Instant};

use agent_core::Event;

/// How long one pass of the loop may spend applying queued events before it must draw.
pub const SLICE: Duration = Duration::from_millis(8);

/// Largest text a merged delta may reach, so one merge cannot hold the loop either.
const MERGE_CAP: usize = 256 * 1024;

/// Apply events from `next` until it runs dry or `budget` is used up. At least one event is
/// applied when there is one, so a budget of zero still makes progress. Returns how many.
pub fn drain_slice<E>(
    mut next: impl FnMut() -> Option<E>,
    mut apply: impl FnMut(E),
    budget: Duration,
) -> usize {
    let end = Instant::now() + budget;
    let mut n = 0;
    while let Some(ev) = next() {
        apply(ev);
        n += 1;
        if Instant::now() >= end {
            break;
        }
    }
    n
}

/// Join the run of text (or thought) deltas that starts with `first` into one event. Returns the
/// merged event and the first event that was not part of the run, which the caller must send
/// next. A thousand queued deltas become one message, so a flood costs one allocation and one
/// layout pass instead of a thousand.
pub fn merge_deltas(
    first: Event,
    mut next: impl FnMut() -> Option<Event>,
) -> (Event, Option<Event>) {
    match first {
        Event::TextDelta(mut t) => {
            while t.len() < MERGE_CAP {
                match next() {
                    Some(Event::TextDelta(more)) => t.push_str(&more),
                    other => return (Event::TextDelta(t), other),
                }
            }
            (Event::TextDelta(t), None)
        }
        Event::ThoughtDelta(mut t) => {
            while t.len() < MERGE_CAP {
                match next() {
                    Some(Event::ThoughtDelta(more)) => t.push_str(&more),
                    other => return (Event::ThoughtDelta(t), other),
                }
            }
            (Event::ThoughtDelta(t), None)
        }
        e => (e, None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    #[test]
    fn an_endless_queue_cannot_hold_the_loop() {
        let t0 = Instant::now();
        let mut applied = 0u64;
        let n = drain_slice(
            || Some(Event::TextDelta("x".into())),
            |_| applied += 1,
            Duration::from_millis(5),
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
        let n = drain_slice(|| q.pop_front(), |_| {}, Duration::ZERO);
        assert_eq!(n, 1);
        assert_eq!(q.len(), 9);
    }

    #[test]
    fn a_short_queue_is_drained_completely() {
        let mut q: VecDeque<u8> = (0..10).collect();
        assert_eq!(
            drain_slice(|| q.pop_front(), |_| {}, Duration::from_secs(1)),
            10
        );
    }

    #[test]
    fn a_run_of_deltas_becomes_one_event_and_stops_at_the_first_other_event() {
        let mut q: VecDeque<Event> = VecDeque::from(vec![
            Event::TextDelta("b".into()),
            Event::TextDelta("c".into()),
            Event::TurnStart,
            Event::TextDelta("d".into()),
        ]);
        let (e, held) = merge_deltas(Event::TextDelta("a".into()), || q.pop_front());
        assert_eq!(e, Event::TextDelta("abc".into()));
        assert_eq!(held, Some(Event::TurnStart));
        assert_eq!(q.len(), 1);
        // a thought delta does not swallow a text delta
        let mut q = VecDeque::from(vec![Event::TextDelta("t".into())]);
        let (e, held) = merge_deltas(Event::ThoughtDelta("h".into()), || q.pop_front());
        assert_eq!(e, Event::ThoughtDelta("h".into()));
        assert_eq!(held, Some(Event::TextDelta("t".into())));
    }

    #[test]
    fn merging_is_capped() {
        let big = "x".repeat(MERGE_CAP);
        let mut q = VecDeque::from(vec![Event::TextDelta("y".into())]);
        let (e, held) = merge_deltas(Event::TextDelta(big), || q.pop_front());
        assert!(matches!(e, Event::TextDelta(t) if t.len() == MERGE_CAP));
        assert!(held.is_none());
        assert_eq!(q.len(), 1);
    }
}
