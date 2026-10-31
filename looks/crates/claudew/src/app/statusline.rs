//! Claude Code's `statusLine` in the look: the command, its payload and its colours are the ones
//! the wizard TUI uses (`src/statusline_run.rs`); this file feeds it the look's session and
//! draws what it prints under the prompt.

use std::path::PathBuf;
use std::time::Instant;

use ratatui::style::Style;
use unicode_segmentation::UnicodeSegmentation;

use super::App;
use crate::statusline_run::{ansi_spans, RunIn, Session};
use crate::text::{width, Row};

/// What the last tick saw, to run the command again when it changes.
#[derive(Default, Debug, PartialEq)]
pub struct Seen {
    vim: Option<&'static str>,
    mode: String,
    plan: bool,
    busy: bool,
    model: String,
    effort: String,
}

fn home() -> PathBuf {
    std::env::var_os("HOME").map(Into::into).unwrap_or_default()
}

impl App {
    /// Read the `statusLine` setting again and ask for a run.
    pub fn load_status_line(&mut self) {
        self.status_line.reload(&home(), &self.opts.cwd);
    }

    fn statusline_seen(&self) -> Seen {
        Seen {
            vim: self.vim.label(),
            mode: self.cfg.mode.clone(),
            plan: self.plan,
            busy: self.busy.is_some(),
            model: self.cfg.model.clone(),
            effort: self.cfg.effort.clone(),
        }
    }

    /// The session as the command reads it.
    pub fn statusline_payload(&self) -> serde_json::Value {
        let cwd = self.opts.cwd.to_string_lossy().into_owned();
        let effort = match self.cfg.effort.as_str() {
            "default" => "",
            e => e,
        };
        let version = self.wizard_version();
        Session {
            session_id: &self.session_id,
            transcript_path: home()
                .join(".wizard/sessions")
                .join(format!("{}.jsonl", self.session_id))
                .display()
                .to_string(),
            cwd: &cwd,
            model_id: &self.cfg.model,
            model_name: &self.model_name(),
            effort,
            version: if version.is_empty() {
                super::VERSION
            } else {
                &version
            },
            input_tokens: self.usage.input_tokens,
            output_tokens: self.usage.output_tokens,
            cached_tokens: self.usage.cached_tokens,
            context_tokens: self.usage.context_tokens,
            window: self.usage.context_window,
            cost_usd: self.usage.cost_usd.unwrap_or(0.0),
            duration_ms: self.clock.elapsed().as_millis() as u64,
            vim_mode: self.vim.label(),
        }
        .payload()
    }

    /// Called on every tick: run the command again when the session changed in a way it may
    /// show (a turn ended, a mode, the vim mode, the model), take a finished run's rows.
    pub fn statusline_tick(&mut self) {
        let seen = self.statusline_seen();
        if seen != self.sl_seen {
            // a turn ending may have been `/statusline` itself: look at the settings again
            if self.sl_seen.busy && !seen.busy {
                self.load_status_line();
            }
            self.sl_seen = seen;
            self.status_line.trigger();
        }
        if !self.status_line.on() {
            return;
        }
        let run_in = RunIn {
            cwd: self.opts.cwd.clone(),
            cols: self.size.0,
            rows: self.size.1,
            session_id: self.session_id.clone(),
        };
        let payload = if self.status_line.run_due() {
            self.statusline_payload().to_string()
        } else {
            String::new()
        };
        if self.status_line.tick(|| payload, &run_in, usize::MAX) {
            self.dirty = true;
        }
    }

    /// The soonest moment the command is due, for the event loop to wake at.
    pub fn statusline_due(&self, now: Instant) -> Option<Instant> {
        self.status_line.next_due().map(|at| at.max(now))
    }

    /// The rows under the prompt: what the command printed, each indented by two and its
    /// `padding`, cut with an ellipsis where the line passes the right margin. A command that
    /// printed nothing still holds one blank row.
    pub fn statusline_rows(&self) -> Vec<Row> {
        let Some(cfg) = &self.status_line.cfg else {
            return Vec::new();
        };
        let th = self.theme;
        let pad = cfg.padding;
        let room = (self.size.0 as usize).saturating_sub(4 + 2 * pad);
        let lead = " ".repeat(2 + pad);
        if self.status_line.lines.is_empty() {
            return vec![Row::new()];
        }
        self.status_line
            .lines
            .iter()
            .map(|line| {
                let mut row = Row::of(lead.clone(), Style::new());
                let cells = fit(&ansi_spans(line), room, th.dim());
                row.extend(cells);
                row
            })
            .collect()
    }
}

/// The spans as a row no wider than `max`, an ellipsis in the last cell when something went.
fn fit(spans: &[ratatui::text::Span<'static>], max: usize, base: Style) -> Row {
    let total: usize = spans.iter().map(|s| width(&s.content)).sum();
    let keep = if total <= max {
        max
    } else {
        max.saturating_sub(1)
    };
    let mut row = Row::new();
    let mut used = 0;
    let mut cut = false;
    'spans: for span in spans {
        let style = base.patch(span.style);
        let mut text = String::new();
        for g in span.content.graphemes(true) {
            let w = width(g);
            if used + w > keep {
                cut = true;
                row.push(std::mem::take(&mut text), style);
                break 'spans;
            }
            text.push_str(g);
            used += w;
        }
        row.push(text, style);
    }
    if cut {
        let style = row.0.last().map_or(base, |(_, s)| *s);
        row.push("…", style);
    }
    row
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::style::Color;
    use ratatui::text::Span;

    fn text(row: &Row) -> String {
        row.text()
    }

    #[test]
    fn a_long_line_is_cut_with_an_ellipsis_inside_the_margin() {
        let spans = vec![Span::raw("abcdefghij".repeat(8))];
        let row = fit(&spans, 56, Style::new());
        assert_eq!(width(&text(&row)), 56);
        assert!(text(&row).ends_with('…'));
        let short = fit(&[Span::raw("ok")], 56, Style::new());
        assert_eq!(text(&short), "ok");
    }

    #[test]
    fn a_wide_character_is_not_split_and_colours_ride_on_the_base() {
        let spans = vec![
            Span::styled("red", Style::new().fg(Color::Indexed(1))),
            Span::raw("世界世界"),
        ];
        let base = Style::new().fg(Color::Indexed(246));
        let row = fit(&spans, 8, base);
        assert_eq!(text(&row), "red世界…");
        assert_eq!(row.0[0].1.fg, Some(Color::Indexed(1)));
        assert_eq!(row.0[1].1.fg, Some(Color::Indexed(246)));
    }
}
