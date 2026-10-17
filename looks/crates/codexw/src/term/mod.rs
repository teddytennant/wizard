// OWNER: renderer
//! Terminal layer: modes, startup probe, the inline viewport and its history inserter, and the
//! alt-screen pager guard.

pub mod history_insert;
pub mod inline;
pub mod modes;
pub mod pager;
pub mod probe;

pub use inline::{Frame, InlineTerminal};
