//! Plan, web search and MCP cells (spec B.7, B.8).

use std::time::Instant;

use agent_core::{Todo, TodoStatus};
use ratatui::style::{Style, Stylize};
use ratatui::text::{Line, Span};
use unicode_segmentation::UnicodeSegmentation;

use crate::line_utils::{line_text, line_to_static, prefix_lines};
use crate::style::shimmer_spans;
use crate::ui::HistoryCell;
use crate::ui::exec_cell::TOOL_CALL_MAX_LINES;
use crate::wrap::{WrapOpts, adaptive_wrap_line};

/// The bullet of a cell that is still running: the shimmering `•`, or a dim one with no motion.
fn running_bullet(animations: bool) -> Span<'static> {
    if animations {
        shimmer_spans("•")
            .into_iter()
            .next()
            .unwrap_or_else(|| "•".dim())
    } else {
        "•".dim()
    }
}

// ---- plan update ------------------------------------------------------------------------------

/// `• Updated Plan` with the steps as a checklist (spec B.7.1).
#[derive(Clone, Debug)]
pub struct PlanUpdateCell {
    pub explanation: Option<String>,
    pub steps: Vec<Todo>,
}

impl HistoryCell for PlanUpdateCell {
    fn display_lines(&self, width: u16) -> Vec<Line<'static>> {
        let inner = (width as usize).saturating_sub(4).max(1);
        let mut body: Vec<Line<'static>> = Vec::new();
        let note = self
            .explanation
            .as_deref()
            .map(str::trim)
            .filter(|t| !t.is_empty());
        if let Some(note) = note {
            let line = Line::from(note.to_string().dim().italic());
            body.extend(adaptive_wrap_line(&line, &WrapOpts::new(inner)));
        }
        if self.steps.is_empty() {
            body.push(Line::from("(no steps provided)".dim().italic()));
        } else {
            for step in &self.steps {
                let (marker, style) = match step.status {
                    TodoStatus::Completed => ("✔ ", Style::default().crossed_out().dim()),
                    TodoStatus::InProgress => ("□ ", Style::default().cyan().bold()),
                    TodoStatus::Pending => ("□ ", Style::default().dim()),
                };
                let opts = WrapOpts::new(inner)
                    .initial_indent(Line::from(marker))
                    .subsequent_indent(Line::from("  "));
                body.extend(adaptive_wrap_line(
                    &Line::from(Span::styled(step.text.clone(), style)),
                    &opts,
                ));
            }
        }
        let mut lines = vec![Line::from(vec!["• ".dim(), "Updated Plan".bold()])];
        lines.extend(prefix_lines(body, "  └ ".dim(), "    ".into()));
        lines
    }
}

// ---- proposed plan ----------------------------------------------------------------------------

/// `<proposed_plan>` text from plan mode: a header, then the plan on the same tint as a user
/// message (spec B.7.2). Source-backed so a resize lays it out again.
#[derive(Clone, Debug)]
pub struct ProposedPlanCell {
    pub markdown: String,
    pub cwd: Option<std::path::PathBuf>,
}

impl HistoryCell for ProposedPlanCell {
    fn display_lines(&self, width: u16) -> Vec<Line<'static>> {
        let style = crate::style::palette().user_message_style();
        let wrap = (width as usize).saturating_sub(4).max(1);
        let mut body = crate::markdown::agent::render_markdown_agent(
            &self.markdown,
            Some(wrap),
            self.cwd.as_deref(),
        );
        if body.is_empty() {
            body.push(Line::from("(empty)".dim().italic()));
        }
        let mut lines = vec![
            Line::from(vec!["• ".dim(), "Proposed Plan".bold()]),
            Line::from(" "),
        ];
        let mut tinted = vec![Line::from(" ")];
        tinted.extend(prefix_lines(body, "  ".into(), "  ".into()));
        tinted.push(Line::from(" "));
        lines.extend(tinted.into_iter().map(|l| l.style(style)));
        lines
    }
}

// ---- web search -------------------------------------------------------------------------------

/// `• Searching the web q` while it runs, `• Searched the web for q` after (spec B.8.2).
#[derive(Clone, Debug)]
pub struct WebSearchCell {
    pub id: String,
    pub detail: String,
    pub completed: bool,
    pub animations: bool,
}

impl WebSearchCell {
    pub fn new(id: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            detail: detail.into(),
            completed: false,
            animations: true,
        }
    }
}

impl HistoryCell for WebSearchCell {
    fn display_lines(&self, width: u16) -> Vec<Line<'static>> {
        let bullet = if self.completed {
            "•".dim()
        } else {
            running_bullet(self.animations)
        };
        let header = if self.completed {
            "Searched the web"
        } else {
            "Searching the web"
        };
        // Codex learns the query only when the search is done; the running cell is the bare header.
        let line = if self.detail.is_empty() || !self.completed {
            Line::from(vec![header.bold()])
        } else {
            let sep = if self.completed { " for " } else { " " };
            Line::from(vec![header.bold(), sep.into(), self.detail.clone().into()])
        };
        let opts = WrapOpts::new(width as usize)
            .initial_indent(Line::from(vec![bullet, " ".into()]))
            .subsequent_indent(Line::from("  "));
        adaptive_wrap_line(&line, &opts)
    }
}

// ---- MCP --------------------------------------------------------------------------------------

/// Text shortened to `max` graphemes with a three dot tail (spec B.0.4).
pub fn truncate_text(text: &str, max: usize) -> String {
    let mut graphemes = text.grapheme_indices(true);
    let Some((byte_index, _)) = graphemes.nth(max) else {
        return text.to_string();
    };
    if max >= 3 {
        match text.grapheme_indices(true).nth(max - 3) {
            Some((i, _)) => format!("{}...", &text[..i]),
            None => text.to_string(),
        }
    } else {
        text[..byte_index].to_string()
    }
}

/// JSON on one line with a space after `:` and `,`, which gives wrapping places to break.
pub fn format_json_compact(text: &str) -> Option<String> {
    let json = serde_json::from_str::<serde_json::Value>(text).ok()?;
    let pretty = serde_json::to_string_pretty(&json).unwrap_or_else(|_| json.to_string());
    let mut result = String::new();
    let mut chars = pretty.chars().peekable();
    let mut in_string = false;
    let mut escape_next = false;
    while let Some(ch) = chars.next() {
        match ch {
            '"' if !escape_next => {
                in_string = !in_string;
                result.push(ch);
            }
            '\\' if in_string => {
                escape_next = !escape_next;
                result.push(ch);
            }
            '\n' | '\r' if !in_string => {}
            ' ' | '\t' if !in_string => {
                if let Some(&next) = chars.peek()
                    && let Some(last) = result.chars().last()
                    && (last == ':' || last == ',')
                    && !matches!(next, '}' | ']')
                {
                    result.push(' ');
                }
            }
            _ => {
                if escape_next && in_string {
                    escape_next = false;
                }
                result.push(ch);
            }
        }
    }
    Some(result)
}

pub fn format_and_truncate_tool_result(text: &str, max_lines: usize, line_width: usize) -> String {
    let max = (max_lines * line_width).saturating_sub(max_lines);
    match format_json_compact(text) {
        Some(json) => truncate_text(&json, max),
        None => truncate_text(text, max),
    }
}

/// `server.tool(args)`: names cyan, arguments dim compact JSON. A tool whose server is not
/// known (wizard names MCP tools in its own way) shows the bare name.
fn invocation_line(server: &str, tool: &str, args: &serde_json::Value) -> Line<'static> {
    let args = if args.is_null() {
        String::new()
    } else {
        serde_json::to_string(args).unwrap_or_else(|_| args.to_string())
    };
    let mut spans: Vec<Span<'static>> = Vec::new();
    if !server.is_empty() {
        spans.push(server.to_string().cyan());
        spans.push(".".into());
    }
    spans.push(tool.to_string().cyan());
    spans.push("(".into());
    spans.push(args.dim());
    spans.push(")".into());
    Line::from(spans)
}

/// A tool call from a server (spec B.8.1). `result` is `Ok(text)` or `Err(message)`.
#[derive(Clone, Debug)]
pub struct McpCell {
    pub id: String,
    pub server: String,
    pub tool: String,
    pub args: serde_json::Value,
    pub started: Instant,
    pub result: Option<Result<String, String>>,
    pub animations: bool,
}

impl McpCell {
    pub fn new(
        id: impl Into<String>,
        server: impl Into<String>,
        tool: impl Into<String>,
        args: serde_json::Value,
    ) -> Self {
        Self {
            id: id.into(),
            server: server.into(),
            tool: tool.into(),
            args,
            started: Instant::now(),
            result: None,
            animations: true,
        }
    }

    pub fn complete(&mut self, result: Result<String, String>) {
        self.result = Some(result);
    }

    pub fn mark_failed(&mut self) {
        self.result = Some(Err("interrupted".to_string()));
    }
}

impl HistoryCell for McpCell {
    fn display_lines(&self, width: u16) -> Vec<Line<'static>> {
        let w = width as usize;
        let mut lines: Vec<Line<'static>> = Vec::new();
        let bullet = match &self.result {
            Some(Ok(_)) => "•".green().bold(),
            Some(Err(_)) => "•".red().bold(),
            None => running_bullet(self.animations),
        };
        let header = if self.result.is_some() {
            "Called"
        } else {
            "Calling"
        };
        let invocation = invocation_line(&self.server, &self.tool, &self.args);
        let mut spans = vec![bullet, " ".into(), header.bold(), " ".into()];
        let reserved: usize = spans
            .iter()
            .map(|s| crate::width::display_width(&s.content))
            .sum();
        let inline = crate::wrap::line_width(&invocation) <= w.saturating_sub(reserved);
        if inline {
            spans.extend(invocation.spans.clone());
            lines.push(Line::from(spans));
        } else {
            spans.pop();
            lines.push(Line::from(spans));
            let opts = WrapOpts::new(w.saturating_sub(4))
                .initial_indent(Line::default())
                .subsequent_indent(Line::from("    "));
            let wrapped: Vec<_> = adaptive_wrap_line(&invocation, &opts)
                .iter()
                .map(line_to_static)
                .collect();
            lines.extend(prefix_lines(wrapped, "  └ ".dim(), "    ".into()));
        }

        let detail_w = w.saturating_sub(4).max(1);
        let opts = WrapOpts::new(detail_w)
            .initial_indent(Line::default())
            .subsequent_indent(Line::from("    "));
        let mut detail: Vec<Line<'static>> = Vec::new();
        match &self.result {
            Some(Ok(text)) if !text.is_empty() => {
                let text = format_and_truncate_tool_result(text, TOOL_CALL_MAX_LINES, detail_w);
                for segment in text.split('\n') {
                    let line = Line::from(segment.to_string().dim());
                    detail.extend(adaptive_wrap_line(&line, &opts));
                }
            }
            Some(Err(err)) => {
                let text = format_and_truncate_tool_result(
                    &format!("Error: {err}"),
                    TOOL_CALL_MAX_LINES,
                    w,
                );
                detail.extend(adaptive_wrap_line(&Line::from(text.dim()), &opts));
            }
            _ => {}
        }
        if !detail.is_empty() {
            let first: Span<'static> = if inline {
                "  └ ".dim()
            } else {
                "    ".into()
            };
            lines.extend(prefix_lines(detail, first, "    ".into()));
        }
        lines
    }
}

/// Server and tool from the name wizard gives an MCP tool: `mcp__server__tool` or
/// `server__tool`. `None` when the name has no such structure.
pub fn split_mcp_name(name: &str) -> Option<(String, String)> {
    let rest = name.strip_prefix("mcp__").unwrap_or(name);
    let (server, tool) = rest.split_once("__")?;
    (!server.is_empty() && !tool.is_empty()).then(|| (server.to_string(), tool.to_string()))
}

#[allow(dead_code)]
fn plain(l: &Line<'_>) -> String {
    line_text(l)
}
