//! User, assistant and reasoning cells (spec B.1, B.2, B.4.2).

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use ratatui::style::{Style, Stylize};
use ratatui::text::{Line, Span};

use crate::line_utils::{line_text, prefix_lines};
use crate::markdown::agent::render_markdown_agent;
use crate::style::palette;
use crate::ui::HistoryCell;
use crate::width::usable_content_width;
use crate::wrap::{WrapOpts, adaptive_wrap_line};

/// `› text` on the tint, one tinted pad row above and below (spec B.1).
#[derive(Clone, Debug)]
pub struct UserCell {
    pub text: String,
}

/// Control characters and CSI sequences have no business in a user row (spec B.1.1).
pub fn sanitize_user_text(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            if chars.peek() == Some(&'[') {
                chars.next();
                for n in chars.by_ref() {
                    if ('@'..='~').contains(&n) {
                        break;
                    }
                }
            }
            continue;
        }
        if c == '\n' || c == '\t' || !c.is_control() {
            out.push(c);
        }
    }
    out
}

impl HistoryCell for UserCell {
    fn is_user_message(&self) -> bool {
        true
    }

    fn raw_lines(&self) -> Vec<Line<'static>> {
        let message = sanitize_user_text(&self.text);
        crate::ui::raw_lines_from_source(message.trim_end_matches(['\r', '\n']))
    }

    fn display_lines(&self, width: u16) -> Vec<Line<'static>> {
        let style = palette().user_message_style();
        let Some(wrap_w) = usable_content_width(width as usize, 3) else {
            return Vec::new();
        };
        let message = sanitize_user_text(&self.text);
        let message = message.trim_end_matches(['\r', '\n']);
        if message.is_empty() {
            return Vec::new();
        }
        let opts = WrapOpts::new(wrap_w);
        let mut rows: Vec<Line<'static>> = Vec::new();
        for src in message.split('\n') {
            rows.extend(adaptive_wrap_line(&Line::from(src.to_string()), &opts));
        }
        while rows.len() > 1
            && rows
                .last()
                .is_some_and(|l| l.spans.iter().all(|s| s.content.is_empty()))
        {
            rows.pop();
        }
        let mut out = vec![Line::default().style(style)];
        out.extend(
            prefix_lines(
                rows,
                Span::styled("› ", Style::default().bold().dim()),
                Span::from("  "),
            )
            .into_iter()
            .map(|l| l.style(style)),
        );
        out.push(Line::default().style(style));
        out
    }
}

/// What a finished stream leaves behind: the source, so a resize can render it again.
pub type StreamSource = Arc<Mutex<Option<String>>>;

/// Gutter for an assistant message: `• ` on the first row, two spaces after.
fn gutter_lines(lines: Vec<Line<'static>>) -> Vec<Line<'static>> {
    let lines = prefix_lines(lines, "• ".dim(), "  ".into());
    lines
        .into_iter()
        .map(|mut l| {
            if l.spans.iter().skip(1).all(|s| s.content.trim().is_empty()) {
                let first = l.spans.first().cloned();
                l.spans.clear();
                // A bare bullet row keeps its bullet; any other blank row is empty.
                if let Some(f) = first.filter(|f| f.content.starts_with('•')) {
                    l.spans.push(f);
                }
            }
            l
        })
        .collect()
}

/// The finished message rendered from source at `width` (Codex's `AgentMarkdownCell`).
pub fn agent_markdown_lines(
    source: &str,
    width: u16,
    cwd: Option<&std::path::Path>,
) -> Vec<Line<'static>> {
    let Some(wrap_width) = usable_content_width(width as usize, 2) else {
        return vec![Line::from("• ".dim())];
    };
    gutter_lines(render_markdown_agent(source, Some(wrap_width), cwd))
}

/// One committed chunk of a streamed answer. The first chunk of a message has the `• `
/// bullet, the rest a two-space gutter and no blank row before them.
///
/// The app asks a cell for its lines once, when it appends it, and again on every reflow. The
/// first call returns the rows as they were committed; once the stream has finished, later calls
/// render the whole message from its source in the first chunk and nothing in the others, which
/// is Codex's consolidation into one cell without the app having to replace anything.
#[derive(Debug)]
pub struct AgentMessageCell {
    lines: Vec<Line<'static>>,
    first: bool,
    source: StreamSource,
    cwd: Option<PathBuf>,
    shown: AtomicBool,
}

impl AgentMessageCell {
    pub fn new(
        lines: Vec<Line<'static>>,
        first: bool,
        source: StreamSource,
        cwd: Option<PathBuf>,
    ) -> Self {
        Self {
            lines,
            first,
            source,
            cwd,
            shown: AtomicBool::new(false),
        }
    }

    fn committed_lines(&self, width: u16) -> Vec<Line<'static>> {
        let mut out = Vec::new();
        for (i, line) in self.lines.iter().enumerate() {
            let initial: Line<'static> = if i == 0 && self.first {
                Line::from("• ".dim())
            } else {
                Line::from("  ")
            };
            let mut hang = Line::from("  ");
            let text = line_text(line);
            let lead = text.len() - text.trim_start().len();
            hang.spans.push(Span::from(" ".repeat(lead)));
            let opts = WrapOpts::new(width as usize)
                .initial_indent(initial)
                .subsequent_indent(hang);
            out.extend(adaptive_wrap_line(line, &opts));
        }
        normalize_blank_rows(out)
    }
}

fn normalize_blank_rows(lines: Vec<Line<'static>>) -> Vec<Line<'static>> {
    lines
        .into_iter()
        .map(|mut l| {
            if l.spans
                .iter()
                .all(|s| s.content.chars().all(char::is_whitespace))
            {
                l.spans.clear();
            }
            l
        })
        .collect()
}

impl HistoryCell for AgentMessageCell {
    fn is_stream_continuation(&self) -> bool {
        !self.first
    }

    fn display_lines(&self, width: u16) -> Vec<Line<'static>> {
        let first_call = !self.shown.swap(true, Ordering::Relaxed);
        if !first_call && let Some(src) = self.source.lock().ok().and_then(|g| g.clone()) {
            return if self.first {
                agent_markdown_lines(&src, width, self.cwd.as_deref())
            } else {
                Vec::new()
            };
        }
        self.committed_lines(width)
    }

    fn raw_lines(&self) -> Vec<Line<'static>> {
        match self.source.lock().ok().and_then(|g| g.clone()) {
            Some(src) if self.first => crate::ui::raw_lines_from_source(&src),
            Some(_) => Vec::new(),
            // Still streaming: what was committed so far, without the bullet and the indent.
            None => crate::ui::plain_lines(self.lines.clone()),
        }
    }
}

/// A whole answer known up front (a replayed session): the same cell a finished stream becomes.
#[derive(Clone, Debug)]
pub struct AgentMarkdownCell {
    pub source: String,
    pub cwd: Option<PathBuf>,
}

impl HistoryCell for AgentMarkdownCell {
    fn display_lines(&self, width: u16) -> Vec<Line<'static>> {
        agent_markdown_lines(&self.source, width, self.cwd.as_deref())
    }

    fn raw_lines(&self) -> Vec<Line<'static>> {
        crate::ui::raw_lines_from_source(&self.source)
    }
}

/// The still-growing end of an answer: rows that are rendered but not yet committed (a table
/// that may yet change its column widths). Drawn in the viewport, never re-wrapped.
pub fn stream_tail_lines(tail: &[Line<'static>], first: bool) -> Vec<Line<'static>> {
    normalize_blank_rows(prefix_lines(
        tail.to_vec(),
        if first { "• ".dim() } else { "  ".into() },
        "  ".into(),
    ))
}

// ---- reasoning --------------------------------------------------------------------------------

/// Split reasoning text into the status header and the body to render (spec B.4.2).
pub fn split_reasoning_summary(text: &str) -> (String, String) {
    let part = text.trim();
    if part.is_empty() {
        return (String::new(), String::new());
    }
    let header_end = part.strip_prefix("**").and_then(|after| {
        after
            .find("**")
            .and_then(|close| (close > 0).then_some(close + 4))
    });
    let body = header_end.map_or(part, |e| &part[e..]);
    if body.trim() == "<!-- -->" {
        return (
            header_end
                .map(|e| part[..e].to_string())
                .unwrap_or_default(),
            String::new(),
        );
    }
    let content = part;
    if let Some(after_open) = content.strip_prefix("**")
        && let Some(close) = after_open.find("**")
    {
        let after_close_idx = 2 + close + 2;
        let after_close = &content[after_close_idx..];
        if after_close.starts_with('\n') || after_close.starts_with('\r') {
            return (
                content[..after_close_idx].to_string(),
                after_close.to_string(),
            );
        }
    }
    (String::new(), content.to_string())
}

/// The first `**bold**` span of a summary that has closed.
pub fn extract_first_bold(s: &str) -> Option<String> {
    let bytes = s.as_bytes();
    let mut i = 0usize;
    while i + 1 < bytes.len() {
        if bytes[i] == b'*' && bytes[i + 1] == b'*' {
            let start = i + 2;
            let mut j = start;
            while j + 1 < bytes.len() {
                if bytes[j] == b'*' && bytes[j + 1] == b'*' {
                    let inner = s[start..j].trim();
                    return (!inner.is_empty()).then(|| inner.to_string());
                }
                j += 1;
            }
            return None;
        }
        i += 1;
    }
    None
}

/// Wrap every line at `width`: the first row of the first line starts with `initial`, every
/// other line starts with `subsequent`, and wrapped rows hang under `subsequent`.
pub fn wrap_with_gutter(
    lines: &[Line<'static>],
    width: u16,
    initial: Line<'static>,
    subsequent: Line<'static>,
) -> Vec<Line<'static>> {
    let mut out = Vec::new();
    for (i, line) in lines.iter().enumerate() {
        let opts = WrapOpts::new(width as usize)
            .initial_indent(if i == 0 {
                initial.clone()
            } else {
                subsequent.clone()
            })
            .subsequent_indent(subsequent.clone());
        out.extend(adaptive_wrap_line(line, &opts));
    }
    out
}

/// Reasoning text, dim italic under a `•`. Without a bold title it only shows in the Ctrl+T
/// pager (spec B.4.2).
#[derive(Clone, Debug)]
pub struct ReasoningCell {
    content: String,
    transcript_only: bool,
}

impl ReasoningCell {
    pub fn new(text: &str) -> Self {
        let (header, content) = split_reasoning_summary(text);
        let title_only = content
            .strip_prefix("**")
            .and_then(|c| c.strip_suffix("**"))
            .is_some_and(|c| !c.is_empty() && !c.contains("**"));
        Self {
            content,
            transcript_only: header.is_empty() && !title_only,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.content.trim().is_empty()
    }

    fn lines(&self, width: u16) -> Vec<Line<'static>> {
        let mut lines = crate::markdown::render_markdown_text_with_width_and_cwd(
            &self.content,
            usable_content_width(width as usize, 2),
            None,
        )
        .lines;
        let summary_style = Style::default().dim().italic();
        for line in &mut lines {
            for span in &mut line.spans {
                span.style = span.style.patch(summary_style);
            }
        }
        wrap_with_gutter(&lines, width, Line::from("• ".dim()), Line::from("  "))
    }
}

impl HistoryCell for ReasoningCell {
    fn display_lines(&self, width: u16) -> Vec<Line<'static>> {
        if self.transcript_only {
            Vec::new()
        } else {
            self.lines(width)
        }
    }

    fn transcript_lines(&self, width: u16) -> Vec<Line<'static>> {
        self.lines(width)
    }

    fn raw_lines(&self) -> Vec<Line<'static>> {
        if self.transcript_only {
            Vec::new()
        } else {
            crate::ui::raw_lines_from_source(self.content.trim())
        }
    }
}
