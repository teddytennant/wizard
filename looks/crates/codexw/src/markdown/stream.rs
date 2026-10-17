//! Newline-gated streaming of an agent answer (spec B.4.1).
//!
//! Deltas pile up in a buffer and only the part up to the last newline is rendered. The whole
//! committed source is re-rendered at the current width each time, lines past the ones already
//! handed out are the new stable lines, and a pipe table that is still growing is held back as
//! the live tail because a new row can change every column width.

use std::path::PathBuf;

use ratatui::text::Line;

use super::agent::render_markdown_agent;
use super::holdback::{TableHoldbackScanner, TableHoldbackState};

#[derive(Debug)]
pub struct AgentStream {
    buffer: String,
    committed: usize,
    /// Markdown width: terminal columns minus the two of the `• ` gutter.
    width: Option<usize>,
    cwd: Option<PathBuf>,
    /// Render of the committed source at `width`.
    render: Vec<Line<'static>>,
    /// Rendered lines already handed to the history.
    emitted: usize,
    scanner: TableHoldbackScanner,
    prefix_cache: Option<(usize, Option<usize>, usize)>,
}

impl AgentStream {
    pub fn new(width: Option<usize>, cwd: Option<PathBuf>) -> Self {
        Self {
            buffer: String::new(),
            committed: 0,
            width,
            cwd,
            render: Vec::new(),
            emitted: 0,
            scanner: TableHoldbackScanner::new(),
            prefix_cache: None,
        }
    }

    /// The directory local file links are shown relative to.
    pub fn set_cwd(&mut self, cwd: Option<PathBuf>) {
        if self.cwd != cwd {
            self.cwd = cwd;
            self.prefix_cache = None;
        }
    }

    pub fn is_empty(&self) -> bool {
        self.buffer.is_empty()
    }

    /// True once any line of this answer has reached the history, or a table is being held back.
    /// The status row stays hidden from then on.
    pub fn has_visible_output(&self) -> bool {
        self.emitted > 0 || !self.tail().is_empty()
    }

    /// Lines that belong to the mutable tail: rendered but not yet handed out.
    pub fn tail(&self) -> &[Line<'static>] {
        &self.render[self.emitted.min(self.render.len())..]
    }

    /// Whether nothing of this stream has been written to the history yet.
    pub fn starts_stream(&self) -> bool {
        self.emitted == 0
    }

    fn render_source(&self, source: &str) -> Vec<Line<'static>> {
        render_markdown_agent(source, self.width, self.cwd.as_deref())
    }

    fn tail_budget(&mut self) -> usize {
        let start = match self.scanner.state() {
            TableHoldbackState::Confirmed { table_start } => table_start,
            TableHoldbackState::PendingHeader { header_start } => header_start,
            TableHoldbackState::None => return 0,
        };
        if start == 0 {
            return self.render.len();
        }
        let start = start.min(self.committed);
        let prefix_len = match self.prefix_cache {
            Some((s, w, n)) if s == start && w == self.width => n,
            _ => {
                let n = self.render_source(&self.buffer[..start]).len();
                self.prefix_cache = Some((start, self.width, n));
                n
            }
        };
        self.render.len().saturating_sub(prefix_len)
    }

    /// Append a delta. Returns the newly stable lines to put in the history.
    pub fn push(&mut self, delta: &str) -> Vec<Line<'static>> {
        self.buffer.push_str(delta);
        let Some(end) = self.buffer.rfind('\n').map(|i| i + 1) else {
            return Vec::new();
        };
        if end <= self.committed {
            return Vec::new();
        }
        let chunk = self.buffer[self.committed..end].to_string();
        self.committed = end;
        self.scanner.push_source_chunk(&chunk);
        self.render = self.render_source(&self.buffer[..self.committed]);
        self.take_stable()
    }

    fn take_stable(&mut self) -> Vec<Line<'static>> {
        let budget = self.tail_budget();
        let target = self.render.len().saturating_sub(budget).max(self.emitted);
        let out = self.render[self.emitted..target].to_vec();
        self.emitted = target;
        out
    }

    /// Re-render at a new width, keeping what is already in the history.
    pub fn set_width(&mut self, width: Option<usize>) -> Vec<Line<'static>> {
        if self.width == width {
            return Vec::new();
        }
        self.width = width;
        self.prefix_cache = None;
        if self.committed == 0 {
            return Vec::new();
        }
        self.render = self.render_source(&self.buffer[..self.committed]);
        self.emitted = self.emitted.min(self.render.len());
        Vec::new()
    }

    /// End of stream: the lines not yet handed out, and the whole source (newline terminated)
    /// for the cell that re-renders on resize. The stream is empty afterwards.
    pub fn finalize(&mut self) -> (Vec<Line<'static>>, String) {
        let mut source = std::mem::take(&mut self.buffer);
        if !source.is_empty() && !source.ends_with('\n') {
            source.push('\n');
        }
        let mut rendered = self.render_source(&source);
        let rest = rendered.split_off(self.emitted.min(rendered.len()));
        self.committed = 0;
        self.render.clear();
        self.emitted = 0;
        self.scanner.reset();
        self.prefix_cache = None;
        (rest, source)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::line_utils::line_text;

    fn texts(lines: &[Line<'static>]) -> Vec<String> {
        lines.iter().map(line_text).collect()
    }

    #[test]
    fn nothing_commits_before_a_newline() {
        let mut s = AgentStream::new(Some(78), None);
        assert!(s.push("hello wor").is_empty());
        assert!(!s.has_visible_output());
        let out = s.push("ld\nnext");
        assert_eq!(texts(&out), vec!["hello world"]);
        let (rest, src) = s.finalize();
        assert_eq!(texts(&rest), vec!["next"]);
        assert_eq!(src, "hello world\nnext\n");
    }

    #[test]
    fn a_table_is_held_back_until_the_stream_ends() {
        let mut s = AgentStream::new(Some(78), None);
        let out = s.push("Intro\n\n| a | b |\n|---|---|\n| 1 | 2 |\n");
        assert_eq!(texts(&out), vec!["Intro"]);
        assert!(!s.tail().is_empty());
        let (rest, _) = s.finalize();
        assert!(texts(&rest).iter().any(|l| l.contains('━')));
    }

    #[test]
    fn pipe_prose_without_a_delimiter_row_is_not_held() {
        let mut s = AgentStream::new(Some(78), None);
        let out = s.push("a | b\nplain\n");
        assert_eq!(out.len(), 2);
    }
}
