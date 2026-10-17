//! jcodew: jcode's terminal UI on the wizard backend. The binary in `main.rs` is a thin shell
//! around this library so tests can build an [`app::App`] and render it. Captures of the real
//! jcode are under `tests/reference/`; `tools/compare.sh` reproduces the comparison.

pub mod app;
pub mod commands;
pub mod fake;
pub mod hotkeys;
pub mod keys;
pub mod theme;
pub mod ui;
