//! How the main loop takes events off its queue without letting a backlog starve the screen.
//! The logic is `tuikit::pump`; this file says which of the app's messages carry backend events.

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
    use tuikit::term::Event as TermEvent;

    fn text(s: &str) -> TermEvent<Msg> {
        TermEvent::Msg(Msg::Backend(Event::TextDelta(s.into())))
    }

    #[test]
    fn deltas_in_the_app_message_merge_and_weigh() {
        let mut q = std::collections::VecDeque::from(vec![text("b"), TermEvent::Tick]);
        let (e, held) = merge_deltas(text("a"), || q.pop_front());
        assert_eq!(weight(&e), 2);
        assert!(matches!(held, Some(TermEvent::Tick)));
        // a shell line is not a delta
        let shell = TermEvent::Msg(Msg::ShellOut { idx: 0 });
        assert_eq!(weight(&shell), 0);
    }
}
