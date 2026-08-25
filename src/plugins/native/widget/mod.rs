//! The window's two surfaces: the transcript that is read, and the composer
//! that is typed into.
//!
//! The split between them is the design decision the whole native GUI turns on,
//! and it is worth naming because the obvious reading of the file list is the
//! wrong one.
//!
//! [`transcript`] does **not** build widgets. It turns a
//! [`TranscriptModel`](crate::transcript::TranscriptModel) into a flat
//! `Vec<Block>` — text runs — which [`super::select::Selectable`] then owns
//! entirely: every glyph in the conversation belongs to one widget, so a drag
//! from a paragraph into a code block into a tool row is one gesture over one
//! buffer set. Splitting the transcript into a column of `text`, `container` and
//! `row` widgets is the thing the spike proved cannot be selected across.
//!
//! [`composer`] *is* stock widgets, and that is not an inconsistency. A composer
//! is typed into: it wants a real `text_input` with its own caret, its own
//! selection, its own IME and its own focus, none of which the selection layer
//! has or should grow. Read-only text is ours; editable text is iced's.
//!
//! [`markdown`] sits under `transcript` and is the third *renderer* over
//! Wizard's one markdown parser, next to the TUI's and the browser's.

pub mod chrome;
pub mod composer;
pub mod markdown;
pub mod transcript;
