//! openw: opencode's terminal UI on the wizard backend. The binary in `main.rs` is a thin shell
//! around this library so tests can build an [`app::App`] and render it.

pub mod app;
pub mod clipboard;
pub mod commands;
pub mod keys;
pub mod private;
pub mod pump;
pub mod timefmt;
pub mod ui;
