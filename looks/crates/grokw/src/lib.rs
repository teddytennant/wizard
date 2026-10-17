//! grokw: Grok Build 1.0.24's terminal UI on the wizard backend. The binary in `main.rs` is a
//! thin shell around this library so tests can build an [`app::App`] and render it. The contract
//! is `docs/grokw-spec.md`; screenshots of the real Grok Build are under `reference/grok/`.

pub mod app;
pub mod commands;
pub mod keys;
pub mod term;
pub mod theme;
pub mod ui;
