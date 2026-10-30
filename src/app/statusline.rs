//! Claude Code's `statusLine` setting: a command that is handed the session as JSON on stdin and
//! prints rows that sit under the composer, above the status bar.
//!
//! ```json
//! { "statusLine": { "type": "command", "command": "~/.claude/statusline.sh", "padding": 0 } }
//! ```
//!
//! The settings are read from `~/.claude/settings.json`, then the project's `settings.json` and
//! `settings.local.json`; the last one to name `statusLine` wins. The command runs in a thread
//! so the interface never waits for it: on start, after each turn, on a mode change and on the
//! `refreshInterval` timer (seconds), debounced to 300 ms, and a newer run replaces one in
//! flight. Each line it prints is a row; ANSI colours are drawn.

use std::path::Path;

use ratatui::text::{Line, Span};

use super::App;
pub use crate::statusline_run::{Cfg, State, ansi_spans};
use crate::statusline_run::{RunIn, Session};

/// Rows a status line may take.
pub const MAX_ROWS: usize = 4;

impl App {
    /// Read the `statusLine` setting and ask for a first run.
    pub fn load_status_line(&mut self, home: &Path, project: &Path) {
        self.status_line.reload(home, project);
    }

    /// Something the script may want to show has changed.
    pub fn statusline_trigger(&mut self) {
        self.status_line.trigger();
    }

    /// The session as the script reads it.
    pub fn statusline_payload(&self) -> serde_json::Value {
        let cwd = self.project_root.display().to_string();
        let transcript_path = crate::config::Config::sessions_dir()
            .map(|d| {
                d.join(format!("{}.jsonl", self.session_id))
                    .display()
                    .to_string()
            })
            .unwrap_or_default();
        Session {
            session_id: &self.session_id,
            transcript_path,
            cwd: &cwd,
            model_id: &self.status.model,
            model_name: &self.status.model,
            effort: "",
            version: env!("CARGO_PKG_VERSION"),
            input_tokens: self.status.prompt_tokens,
            output_tokens: self.status.completion_tokens,
            cached_tokens: 0,
            context_tokens: self.status.context_tokens,
            window: self.config.max_context_tokens as u64,
            cost_usd: 0.0,
            duration_ms: 0,
            vim_mode: self.vim.label(),
        }
        .payload()
    }

    /// Called on every tick: take a finished run's rows, start the run a trigger asked for.
    /// True when the rows changed.
    pub fn statusline_tick(&mut self) -> bool {
        let (cols, rows) = crossterm::terminal::size().unwrap_or((80, 24));
        let run_in = RunIn {
            cwd: self.project_root.clone(),
            cols,
            rows,
            session_id: self.session_id.clone(),
        };
        let payload = if self.status_line.run_due() {
            self.statusline_payload().to_string()
        } else {
            String::new()
        };
        self.status_line.tick(|| payload, &run_in, MAX_ROWS)
    }

    /// Rows the status line takes under the composer right now.
    pub fn statusline_rows(&self) -> u16 {
        if !self.status_line.on() || self.picker.is_some() || self.plan_review.is_some() {
            return 0;
        }
        self.status_line.lines.len().min(MAX_ROWS) as u16
    }

    /// The rows, ready to draw: padded, colours applied.
    pub fn statusline_spans(&self) -> Vec<Line<'static>> {
        let pad = " ".repeat(1 + self.status_line.cfg.as_ref().map_or(0, |c| c.padding));
        self.status_line
            .lines
            .iter()
            .take(MAX_ROWS)
            .map(|l| {
                let mut spans = vec![Span::raw(pad.clone())];
                spans.extend(ansi_spans(l));
                Line::from(spans)
            })
            .collect()
    }

    /// `/statusline [command | off]`. With no argument it says what is set.
    pub fn statusline_command(&mut self, arg: &str) {
        match std::env::var_os("HOME") {
            Some(home) => self.statusline_command_in(Path::new(&home), arg),
            None => self.notice("no home directory to keep the setting in"),
        }
    }

    pub(super) fn statusline_command_in(&mut self, home: &Path, arg: &str) {
        let arg = arg.trim();
        match arg {
            "" => {
                let said = match &self.status_line.cfg {
                    Some(c) => format!("status line: {}", c.command),
                    None => {
                        "no status line. `/statusline <command>` sets one: the command gets the \
                             session as JSON on stdin and prints rows for the footer"
                            .to_string()
                    }
                };
                self.notice(said);
            }
            "off" | "remove" | "clear" => {
                crate::statusline_run::save(home, None);
                self.status_line.cfg = None;
                self.status_line.lines.clear();
                self.notice("status line removed");
            }
            command => {
                crate::statusline_run::save(home, Some(command));
                self.status_line.cfg =
                    crate::statusline_run::parse(&serde_json::json!({ "command": command }));
                self.statusline_trigger();
                self.notice(format!("status line set to: {command}"));
            }
        }
    }
}
