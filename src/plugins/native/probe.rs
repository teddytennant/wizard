//! Test-only selectors, for the assertions `iced_test` does not ship.
//!
//! `iced_selector`'s `&str` selector matches a text widget's content
//! **exactly**. That is the right default — a test that passes because the word
//! it wanted was a substring of something else is a test that will pass forever
//! — but three kinds of assertion in this window cannot use it: a truncated
//! path, a sentence with a count in it, and any label with a number that moves.
//!
//! So [`contains`] exists, and it is deliberately the only loosening: it still
//! walks the real widget tree, it still finds only text, and it still fails
//! when nothing matches.

use iced::advanced::widget::Id;
use iced_test::selector::{Candidate, Selector, Text};

/// Matches the first text widget whose content contains `needle`.
pub fn contains(needle: &str) -> Contains<'_> {
    Contains(needle)
}

pub struct Contains<'a>(&'a str);

impl Selector for Contains<'_> {
    type Output = Text;

    fn select(&mut self, candidate: Candidate<'_>) -> Option<Self::Output> {
        let (id, bounds, visible_bounds, content): (Option<&Id>, _, _, String) = match candidate {
            Candidate::Text {
                id,
                bounds,
                visible_bounds,
                content,
            } => (id, bounds, visible_bounds, content.to_string()),
            Candidate::TextInput {
                id,
                bounds,
                visible_bounds,
                state,
            } => (id, bounds, visible_bounds, state.text().to_string()),
            _ => return None,
        };
        content.contains(self.0).then(|| Text::Raw {
            id: id.cloned(),
            bounds,
            visible_bounds,
        })
    }

    fn description(&self) -> String {
        format!("text contains {:?}", self.0)
    }
}
