// OWNER: renderer
//! Terminal layer: modes, startup probe, terminal capabilities, the screen choice, the inline
//! viewport and its history inserter, and the alt-screen pager guard.

pub mod alt_screen;
pub mod capabilities;
pub mod history_insert;
pub mod inline;
pub mod modes;
pub mod pager;
pub mod probe;

pub use inline::{Frame, InlineTerminal};
