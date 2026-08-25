//! Cross-block text selection, which stock iced does not have.
//!
//! The one question `internal/iced-migration-plan.md` named as able to turn a
//! ten-week project into a thirty-week one was whether a user could select text
//! across a prose paragraph, a code block and a tool row, and copy it. A
//! transcript you cannot select out of is a demo, not a replacement for the
//! browser GUI, and nothing above this layer can be designed until it is
//! answered — every widget in the transcript is shaped by whether it owns its
//! own text runs or hands them here.
//!
//! The answer, from the spike recorded in `internal/v2-decisions.md` §6: iced
//! 0.14 has no such thing (`Selection` lives inside `text_editor` and
//! `text_input`, both single-buffer), but everything needed to build one is real
//! and implemented over cosmic-text rather than stubbed. So this is that layer,
//! in four parts:
//!
//! | module | what it owns |
//! |---|---|
//! | [`block`] | what gets selected: a run of styled text and its plain form |
//! | [`geometry`] | the cosmic-text bridge: point → offset, range → rectangles |
//! | [`cache`] | keeping shaped paragraphs across layout passes |
//! | [`widget`] | the one widget that owns every run, and the gesture |
//!
//! Each has its own header explaining the part of the problem it answers, and
//! the two that would otherwise be mysterious — why the geometry reaches past
//! iced's own `Paragraph` trait, and why the cache is keyed by content rather
//! than by index — are worth reading before changing anything here.

pub mod block;
pub mod cache;
pub mod geometry;
pub mod widget;

pub use block::Block;
pub use widget::{Anchor, Selectable};
