//! piw: Pi coding agent 1.0.3's terminal UI on the wizard backend. The binary in `main.rs` is a
//! thin shell around this library so tests can build an [`app::App`] and render it. The contract is
//! `docs/piw-spec.md`; screenshots of the real Pi are under `reference/pi/`.

pub mod app;
pub mod commands;
pub mod keys;
pub mod mock;
pub mod pump;
pub mod settings;
pub mod shell;
pub mod system_theme;
pub mod theme;
pub mod ui;
