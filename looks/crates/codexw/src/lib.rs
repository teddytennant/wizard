// OWNER: shared
//! `codexw`: the OpenAI Codex CLI 0.147.0 terminal UI on the wizard backend.

// Edition 2024 turns on let chains; code written before that nests `if let` on purpose.
#![allow(clippy::collapsible_if)]

pub mod app;
pub mod clipboard;
pub mod commands;
pub mod config;
pub mod editor;
pub mod fake;
pub mod highlight;
pub mod hyperlink;
pub mod images;
pub mod keymap;
pub mod line_utils;
pub mod markdown;
pub mod outputs;
pub mod rewind;
pub mod skills;
pub mod slash;
pub mod statusline;
pub mod style;
pub mod term;
pub mod testing;
pub mod ui;
pub mod width;
pub mod wrap;
