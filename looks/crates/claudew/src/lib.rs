//! claudew: Claude Code 2.1.296's fullscreen terminal UI on the wizard backend. The binary in
//! `main.rs` is a thin shell around this library so tests can build an [`app::App`] and render
//! it. Captures of the real product are under `tests/fixtures/`; `tools/compare.sh` diffs a
//! live run of both cell by cell.

pub mod app;
#[path = "../../../../src/keybindings.rs"]
pub mod keybindings;
pub mod replay;
pub mod term;
pub mod text;
pub mod theme;
pub mod truecolor;
pub mod ui;
#[path = "../../../../src/vim.rs"]
pub mod vim;
