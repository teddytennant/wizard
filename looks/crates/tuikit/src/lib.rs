//! Shared full-screen rendering toolkit for the `openw` and `openc` frontends.
//!
//! Everything draws through ratatui `Buffer`s and bounds-checks its rects, so zero-size areas,
//! wide characters and stray control bytes never panic. Colours come from [`Theme`], which
//! reads opencode's theme JSON.
//!
//! - [`theme`]: opencode theme files resolved to a token struct, plus alpha blending
//! - [`markdown`], [`syntax`]: Markdown to styled lines with highlighted code, streaming cache
//! - [`diff`]: unified and split diffs from texts or a patch
//! - [`editor`]: multi-line input model with readline keys, undo and history
//! - [`select`], [`dialog`], [`fuzzy`]: filterable lists and the modal frame
//! - [`toast`], [`spinner`], [`scroll`], [`border`]: small widgets
//! - [`width`], [`ansi`], [`paint`]: text measurement, SGR parsing, safe drawing
//! - [`pump`]: bounded event drain and delta merge for a main loop
//! - [`term`]: terminal guard and event pump; [`testing`]: render to text/ANSI for snapshots

pub mod ansi;
pub mod border;
pub mod depth;
pub mod dialog;
pub mod diff;
pub mod editor;
pub mod fsread;
pub mod fuzzy;
pub mod fuzzysort;
pub mod git;
pub mod input;
pub mod markdown;
pub mod paint;
pub mod private;
pub mod pump;
pub mod scroll;
pub mod select;
pub mod spinner;
pub mod syntax;
pub mod term;
pub mod testing;
pub mod theme;
pub mod toast;
pub mod ts;
mod ts_rules;
pub mod width;

pub use theme::{Mode, Theme, Variant};
