//! How a main loop takes events off its queue without letting a backlog starve the screen.
//!
//! A fast model, or a tool that prints a lot, can queue thousands of backend events between two
//! frames. Applying all of them before drawing meant nothing was drawn and no key was handled
//! until the queue ran dry (a 100 MB stream delayed the first key by 0.3 to 3 seconds). So the
//! loop applies events for a slice of time, goes back around to draw, and merges the runs of
//! text deltas it finds so a thousand of them cost one append.
//!
//! The event type is the frontend's own, so the merge takes it apart and puts it back through
//! two closures.

use std::time::{Duration, Instant};

use agent_core::Event;

use crate::term::Event as TermEvent;

/// How long one pass of the loop may spend applying queued events before it must draw.
pub const SLICE: Duration = Duration::from_millis(8);

/// Largest text a merged delta may reach, so one merge cannot hold the loop either.
pub const MERGE_CAP: usize = 256 * 1024;

/// Text one frame may take in before it is drawn. Applying a delta is a push onto a string and
/// costs nothing; what costs is drawing it, once, as markdown. Without a bound on the text a
/// slice can swallow megabytes and then spend a second laying them out.
pub const FRAME_BYTES: usize = 64 * 1024;

/// Apply events from `next` until it runs dry, `budget` is used up, or the events applied
/// weigh `max_weight` (`apply` returns each one's weight). At least one event is applied when
/// there is one, so a budget of zero still makes progress. Returns how many.
pub fn drain_slice<E>(
    mut next: impl FnMut() -> Option<E>,
    mut apply: impl FnMut(E) -> usize,
    budget: Duration,
    max_weight: usize,
) -> usize {
    let end = Instant::now() + budget;
    let (mut n, mut heavy) = (0, 0);
    while let Some(ev) = next() {
        heavy += apply(ev);
        n += 1;
        if heavy >= max_weight || Instant::now() >= end {
            break;
        }
    }
    n
}

/// A streamed piece of the answer: model text or a thought.
pub struct Delta {
    pub thought: bool,
    pub text: String,
}

/// Join the run of deltas of one kind that starts with `first` into one event. `split` takes a
/// delta out of an event (or hands the event back), `join` makes the event again. Returns the
/// merged event and the first event that was not part of the run, which the caller must handle
/// next.
pub fn merge_runs<E>(
    first: E,
    mut next: impl FnMut() -> Option<E>,
    split: impl Fn(E) -> Result<Delta, E>,
    join: impl Fn(Delta) -> E,
) -> (E, Option<E>) {
    let mut cur = match split(first) {
        Ok(d) => d,
        Err(e) => return (e, None),
    };
    while cur.text.len() < MERGE_CAP {
        let Some(e) = next() else { break };
        match split(e) {
            Ok(more) if more.thought == cur.thought => cur.text.push_str(&more.text),
            Ok(other) => return (join(cur), Some(join(other))),
            Err(e) => return (join(cur), Some(e)),
        }
    }
    (join(cur), None)
}

/// What a frontend's own message type must say about the backend events inside it, so the loop
/// can weigh and merge them.
pub trait BackendMsg: Sized {
    fn backend(&self) -> Option<&Event>;
    fn from_backend(e: Event) -> Self;
    fn into_backend(self) -> Result<Event, Self>;
}

/// How much a delta adds to the next frame's work; zero for everything else.
pub fn weight<M: BackendMsg>(e: &TermEvent<M>) -> usize {
    match e {
        TermEvent::Msg(m) => match m.backend() {
            Some(Event::TextDelta(t) | Event::ThoughtDelta(t)) => t.len(),
            _ => 0,
        },
        _ => 0,
    }
}

/// [`merge_runs`] over terminal events carrying a frontend's messages: the run of text (or
/// thought) deltas that starts with `first` becomes one event.
pub fn merge_deltas<M: BackendMsg>(
    first: TermEvent<M>,
    next: impl FnMut() -> Option<TermEvent<M>>,
) -> (TermEvent<M>, Option<TermEvent<M>>) {
    merge_runs(
        first,
        next,
        |e| match e {
            TermEvent::Msg(m) => match m.into_backend() {
                Ok(Event::TextDelta(text)) => Ok(Delta {
                    thought: false,
                    text,
                }),
                Ok(Event::ThoughtDelta(text)) => Ok(Delta {
                    thought: true,
                    text,
                }),
                Ok(other) => Err(TermEvent::Msg(M::from_backend(other))),
                Err(m) => Err(TermEvent::Msg(m)),
            },
            e => Err(e),
        },
        |d| {
            TermEvent::Msg(M::from_backend(if d.thought {
                Event::ThoughtDelta(d.text)
            } else {
                Event::TextDelta(d.text)
            }))
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    #[derive(Debug, PartialEq)]
    enum Ev {
        Text(String),
        Thought(String),
        Tick,
    }

    fn split(e: Ev) -> Result<Delta, Ev> {
        match e {
            Ev::Text(text) => Ok(Delta {
                thought: false,
                text,
            }),
            Ev::Thought(text) => Ok(Delta {
                thought: true,
                text,
            }),
            e => Err(e),
        }
    }

    fn join(d: Delta) -> Ev {
        if d.thought {
            Ev::Thought(d.text)
        } else {
            Ev::Text(d.text)
        }
    }

    fn text(s: &str) -> Ev {
        Ev::Text(s.into())
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
        assert!(t0.elapsed() < Duration::from_millis(500));
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
        assert_eq!(n, 7);
        assert_eq!(q.len(), 93);
    }

    #[test]
    fn a_run_of_deltas_becomes_one_event_and_stops_at_the_first_other_event() {
        let mut q = VecDeque::from(vec![text("b"), text("c"), Ev::Tick, text("d")]);
        let (e, held) = merge_runs(text("a"), || q.pop_front(), split, join);
        assert_eq!(e, text("abc"));
        assert_eq!(held, Some(Ev::Tick));
        assert_eq!(q.len(), 1);
        // a thought delta does not swallow a text delta
        let mut q = VecDeque::from(vec![text("t")]);
        let (e, held) = merge_runs(Ev::Thought("h".into()), || q.pop_front(), split, join);
        assert_eq!(e, Ev::Thought("h".into()));
        assert_eq!(held, Some(text("t")));
        // something that is not a delta passes through
        let (e, held) = merge_runs(Ev::Tick, || None, split, join);
        assert_eq!((e, held), (Ev::Tick, None));
    }

    #[test]
    fn merging_is_capped() {
        let big = "x".repeat(MERGE_CAP);
        let mut q = VecDeque::from(vec![text("y")]);
        let (e, held) = merge_runs(text(&big), || q.pop_front(), split, join);
        assert_eq!(e, text(&big));
        assert!(held.is_none());
        assert_eq!(q.len(), 1);
    }
}
