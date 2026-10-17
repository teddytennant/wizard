//! Info, warning and error rows, separators (spec B.12, B.15).

use ratatui::style::{Color, Style, Stylize};
use ratatui::text::{Line, Span};

use crate::ui::HistoryCell;
use crate::ui::status_indicator::fmt_elapsed_compact;
use crate::wrap::{WrapOpts, adaptive_wrap_line, word_wrap_line};

/// `• text` info row with an optional dark-gray hint.
#[derive(Clone, Debug)]
pub struct InfoCell {
    pub text: String,
    pub hint: Option<String>,
}

impl HistoryCell for InfoCell {
    fn display_lines(&self, _width: u16) -> Vec<Line<'static>> {
        // One row per line of text: backend notices such as `/cost` come with line breaks. The
        // terminal wraps long rows itself.
        let mut out: Vec<Line<'static>> = Vec::new();
        let rows: Vec<&str> = self.text.trim_end_matches('\n').split('\n').collect();
        for (i, row) in rows.iter().enumerate() {
            let lead = if i == 0 { "• " } else { "  " };
            let mut spans = vec![Span::from(lead).dim(), Span::from((*row).to_string())];
            if i + 1 == rows.len()
                && let Some(h) = &self.hint
            {
                spans.push(Span::from(" "));
                spans.push(Span::styled(
                    h.clone(),
                    Style::default().fg(Color::DarkGray),
                ));
            }
            out.push(Line::from(spans));
        }
        out
    }
}

/// `■ message` in red, word-wrapped at the terminal width with no hanging indent.
#[derive(Clone, Debug)]
pub struct ErrorCell {
    pub text: String,
}

impl HistoryCell for ErrorCell {
    fn display_lines(&self, width: u16) -> Vec<Line<'static>> {
        let line = Line::from(Span::styled(
            format!("■ {}", self.text),
            Style::default().fg(Color::Red),
        ));
        adaptive_wrap_line(&line, &WrapOpts::new(width as usize))
    }
}

/// `⚠ message` in yellow with a two-space continuation.
#[derive(Clone, Debug)]
pub struct WarningCell {
    pub text: String,
}

impl HistoryCell for WarningCell {
    fn display_lines(&self, width: u16) -> Vec<Line<'static>> {
        let y = Style::default().fg(Color::Yellow);
        let line = Line::from(Span::styled(self.text.clone(), y));
        let opts = WrapOpts::new((width as usize).saturating_sub(2).max(1));
        word_wrap_line(&line, &opts)
            .into_iter()
            .enumerate()
            .map(|(i, mut l)| {
                l.spans
                    .insert(0, Span::styled(if i == 0 { "⚠ " } else { "  " }, y));
                l
            })
            .collect()
    }
}

pub const INTERRUPTED: &str = "Conversation interrupted - tell the model what to do differently. Something went wrong? Hit `/feedback` to report the issue.";

/// The rule after a turn that did work, with `Worked for 1m 08s` when it took over a minute.
#[derive(Clone, Debug)]
pub struct FinalMessageSeparator {
    pub elapsed_seconds: Option<u64>,
}

impl HistoryCell for FinalMessageSeparator {
    fn raw_lines(&self) -> Vec<Line<'static>> {
        match self.elapsed_seconds.filter(|s| *s > 60) {
            Some(s) => vec![Line::from(format!("Worked for {}", fmt_elapsed_compact(s)))],
            None => Vec::new(),
        }
    }

    fn display_lines(&self, width: u16) -> Vec<Line<'static>> {
        let w = width as usize;
        let label = self
            .elapsed_seconds
            .filter(|s| *s > 60)
            .map(|s| format!("─ Worked for {} ─", fmt_elapsed_compact(s)));
        let Some(label) = label else {
            return vec![Line::from("─".repeat(w).dim())];
        };
        let mut shown = String::new();
        let mut used = 0;
        for ch in label.chars() {
            let cw = crate::width::char_width(ch);
            if used + cw > w {
                break;
            }
            used += cw;
            shown.push(ch);
        }
        shown.push_str(&"─".repeat(w.saturating_sub(used)));
        vec![Line::from(shown).dim()]
    }
}

/// A dim sentence under a cell, for what wizard cannot give the way Codex's backend does (spec
/// Part E rows 6 and 11). Shown once per session.
#[derive(Clone, Debug)]
pub struct NoteCell {
    pub text: String,
}

impl HistoryCell for NoteCell {
    fn display_lines(&self, width: u16) -> Vec<Line<'static>> {
        let line = Line::from(Span::from(self.text.clone()).dim());
        adaptive_wrap_line(&line, &WrapOpts::new(width as usize))
    }
}

pub const EXEC_OUTPUT_NOTE: &str =
    "Output appears when the command finishes; wizard does not stream it.";
pub const PATCH_SNIPPET_NOTE: &str = "Changes are shown as the edited snippet; wizard does not send full diffs. Run /diff for the whole tree.";
