// OWNER: messages (user, assistant, thinking, status, error and summary blocks)
//! Transcript components. Every message starts with a blank row it owns; there is no shared gap
//! logic (spec 6.1). Assistant text and thinking go through `markdown_theme`, tool calls through
//! `tools`.

use agent_core::transcript::{Message, Part};
use agent_core::{StopReason, ToolCall, ToolStatus};
use ratatui::style::{Modifier, Style};
use ratatui::text::Line;

use super::markdown_theme;
use super::tools::{self, ToolCx, ToolTime};
use super::{blank, boxed, indent, span, wrap_text, Cx, Lines};
use crate::theme::Tok;

fn pad_lines(lines: Lines, n: u16) -> Lines {
    lines.into_iter().map(|l| indent(l, n)).collect()
}

/// A user message: markdown with padding 1x1 on `userMessageBg`, one blank row before it.
pub fn user(text: &str, cx: &Cx) -> Lines {
    let th = cx.th();
    let inner = cx.width.saturating_sub(2 * cx.out_pad).max(1);
    let md = markdown_theme::render(text.trim_end(), inner, th, th.fg(Tok::UserMessageText));
    let mut out = vec![blank()];
    out.extend(boxed(
        md,
        cx.width,
        cx.out_pad,
        1,
        th.bg(Tok::UserMessageBg),
    ));
    out
}

fn visible(p: &Part) -> bool {
    match p {
        Part::Text(t) => !t.trim().is_empty(),
        Part::Thought { text, .. } => !text.trim().is_empty(),
        Part::Tool(_) => false,
    }
}

/// What a click on a block of an assistant message toggles.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Click {
    /// A tool call that has a result: its output expands or collapses.
    Tool(String),
    /// A thinking run (the n-th in the message): shown or hidden.
    Think(usize),
    /// A compaction summary: its text shown or hidden.
    Compaction,
}

/// Rows of an assistant message that a click lands on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Region {
    pub rows: std::ops::Range<usize>,
    pub click: Click,
}

/// Per-block state a click sets, over the global `ctrl+o` and `ctrl+t` settings.
#[derive(Clone, Copy, Default)]
pub struct Overrides<'a> {
    /// Tool call id to expanded.
    pub tools: Option<&'a std::collections::HashMap<String, bool>>,
    /// Thinking run to hidden.
    pub think: Option<&'a std::collections::HashMap<usize, bool>>,
}

/// An assistant message. Text and thinking between tool calls form one component each (a blank
/// row, then the blocks); tool calls are separate components with their own blank row.
pub fn assistant(msg: &Message, cx: &Cx, time_of: &dyn Fn(&ToolCall) -> ToolTime) -> Lines {
    assistant_with(msg, cx, time_of, Overrides::default()).0
}

/// [`assistant`], with the blocks a click may have toggled, and where a click lands.
pub fn assistant_with(
    msg: &Message,
    cx: &Cx,
    time_of: &dyn Fn(&ToolCall) -> ToolTime,
    ov: Overrides<'_>,
) -> (Lines, Vec<Region>) {
    let th = cx.th();
    let mut regions: Vec<Region> = Vec::new();
    let mut think_run = 0usize;
    let w = cx.width.saturating_sub(2 * cx.out_pad).max(1);
    let mut out: Lines = Vec::new();
    let parts = &msg.parts;
    let aborted = matches!(msg.stop, Some(StopReason::Cancelled));
    let mut i = 0;
    while i < parts.len() {
        match &parts[i] {
            Part::Tool(call) => {
                let mut local = cx.clone();
                if let Some(e) = ov.tools.and_then(|m| m.get(&call.id)) {
                    local.expanded = *e;
                }
                let mut tcx = ToolCx::new(&local, time_of(call));
                if aborted && matches!(call.status, ToolStatus::Pending | ToolStatus::Running) {
                    tcx.abort_message = Some("Operation aborted".into());
                }
                let rows = tools::render(call, &tcx);
                // blank row, padding, content, padding: only the content takes a click, and only
                // once the call has a result (Pi's `MouseRegion` around the result component)
                let done = matches!(call.status, ToolStatus::Completed | ToolStatus::Failed);
                if done && rows.len() > 3 {
                    regions.push(Region {
                        rows: out.len() + 2..out.len() + rows.len() - 1,
                        click: Click::Tool(call.id.clone()),
                    });
                }
                out.extend(rows);
                i += 1;
            }
            _ => {
                // one run of text and thinking
                let start = i;
                while i < parts.len() && !matches!(parts[i], Part::Tool(_)) {
                    i += 1;
                }
                let run = &parts[start..i];
                if !run.iter().any(visible) {
                    continue;
                }
                out.push(blank());
                let mut k = 0;
                while k < run.len() {
                    match &run[k] {
                        Part::Text(t) if !t.trim().is_empty() => {
                            let md = super::stream::render_md(t.trim(), w, th, Style::default());
                            out.extend(pad_lines(md, cx.out_pad));
                            k += 1;
                        }
                        Part::Thought { .. } => {
                            let mut blocks: Vec<&str> = Vec::new();
                            while k < run.len() {
                                match &run[k] {
                                    Part::Thought { text, .. } => {
                                        if !text.trim().is_empty() {
                                            blocks.push(text.trim());
                                        }
                                        k += 1;
                                    }
                                    _ => break,
                                }
                            }
                            if blocks.is_empty() {
                                continue;
                            }
                            let more = run[k..].iter().any(visible);
                            let base = th.fg(Tok::ThinkingText).add_modifier(Modifier::ITALIC);
                            let n = think_run;
                            think_run += 1;
                            let hidden = ov
                                .think
                                .and_then(|m| m.get(&n))
                                .copied()
                                .unwrap_or(cx.hide_thinking);
                            let lines = if hidden {
                                vec![Line::from(span("Thinking...", base))]
                            } else {
                                super::stream::render_md(&blocks.join("\n\n"), w, th, base)
                            };
                            regions.push(Region {
                                rows: out.len()..out.len() + lines.len(),
                                click: Click::Think(n),
                            });
                            out.extend(pad_lines(lines, cx.out_pad));
                            if more {
                                out.push(blank());
                            }
                        }
                        _ => k += 1,
                    }
                }
            }
        }
    }
    let has_tools = parts.iter().any(|p| matches!(p, Part::Tool(_)));
    if aborted && !has_tools {
        out.extend(error_line("Operation aborted", cx));
    }
    (out, regions)
}

/// A dim status line: blank row, one row, padding 1.
pub fn status(text: &str, cx: &Cx) -> Lines {
    let mut out = vec![blank()];
    for l in wrap_text(
        text,
        cx.width.saturating_sub(2).max(1),
        cx.th().fg(Tok::Dim),
    ) {
        out.push(indent(l, 1));
    }
    out
}

pub fn warning(text: &str, cx: &Cx) -> Lines {
    let mut out = vec![blank()];
    let t = format!("Warning: {text}");
    for l in wrap_text(
        &t,
        cx.width.saturating_sub(2).max(1),
        cx.th().fg(Tok::Warning),
    ) {
        out.push(indent(l, 1));
    }
    out
}

pub fn error(text: &str, cx: &Cx) -> Lines {
    error_line(&format!("Error: {text}"), cx)
}

/// An `error`-coloured row with the output padding, preceded by a blank row.
pub fn error_line(text: &str, cx: &Cx) -> Lines {
    let mut out = vec![blank()];
    for l in wrap_text(
        text,
        cx.width.saturating_sub(2 * cx.out_pad).max(1),
        cx.th().fg(Tok::Error),
    ) {
        out.push(indent(l, cx.out_pad));
    }
    out
}

/// `✓ New session started`: spacer, then a text with padding 1 on every side.
pub fn accent_note(text: &str, cx: &Cx) -> Lines {
    vec![
        blank(),
        blank(),
        indent(
            Line::from(span(text.to_string(), cx.th().fg(Tok::Accent))),
            1,
        ),
        blank(),
    ]
}

/// Plain default-colour text block (`/session`, command output): blank row, padding 1.
pub fn text_block(text: &str, cx: &Cx) -> Lines {
    let mut out = vec![blank()];
    for l in wrap_text(text, cx.width.saturating_sub(2).max(1), Style::default()) {
        out.push(indent(l, 1));
    }
    out
}

/// Rows already styled by the caller, in a block with a blank row before and padding 1.
pub fn lines_block(lines: &[Line<'static>]) -> Lines {
    let mut out = vec![blank()];
    out.extend(lines.iter().cloned().map(|l| indent(l, 1)));
    out
}

/// The `[compaction]` box: `customMessageBg`, label, blank row, then the summary line.
pub fn compaction(tokens: Option<u64>, summary: &str, cx: &Cx, expand_key: &str) -> Lines {
    let th = cx.th();
    let label = Line::from(span("[compaction]", th.bold(Tok::CustomMessageLabel)));
    let mut content = vec![label, blank()];
    let head = match tokens {
        Some(n) => format!("Compacted from {} tokens", group_digits(n)),
        None => "Compacted".to_string(),
    };
    if cx.expanded {
        content.extend(markdown_theme::render(
            &format!("**{head}**\n\n{summary}"),
            cx.width.saturating_sub(2).max(1),
            th,
            th.fg(Tok::CustomMessageText),
        ));
    } else {
        content.push(Line::from(vec![
            span(head, th.fg(Tok::CustomMessageText)),
            span(" (", th.fg(Tok::CustomMessageText)),
            span(expand_key.to_string(), th.fg(Tok::Dim)),
            span(" to expand)", th.fg(Tok::Dim)),
        ]));
    }
    let mut out = vec![blank()];
    out.extend(boxed(content, cx.width, 1, 1, th.bg(Tok::CustomMessageBg)));
    out
}

pub fn group_digits(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::PiTheme;
    use agent_core::transcript::Role;
    use std::time::{Duration, Instant};

    fn cx() -> Cx {
        Cx {
            theme: PiTheme::dark(),
            width: 40,
            expanded: false,
            hide_thinking: false,
            out_pad: 1,
            cwd: String::new(),
            home: String::new(),
            clock: Duration::ZERO,
            version: "1.0.3".into(),
        }
    }

    fn text(l: &Line) -> String {
        l.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    fn msg(parts: Vec<Part>) -> Message {
        Message {
            role: Role::Assistant,
            parts,
            started: Instant::now(),
            took: Some(Duration::ZERO),
            stop: None,
            model: String::new(),
        }
    }

    #[test]
    fn user_message_is_three_rows_on_the_bg() {
        let rows = user("hi", &cx());
        assert_eq!(rows.len(), 4); // blank, pad, text, pad
        assert_eq!(text(&rows[2]), " hi".to_string() + &" ".repeat(37));
        assert!(rows[1].spans[0].style.bg.is_some());
    }

    #[test]
    fn assistant_text_has_one_blank_row_and_no_background() {
        let m = msg(vec![Part::Text("hello".into())]);
        let rows = assistant(&m, &cx(), &|_| ToolTime::default());
        assert_eq!(rows.len(), 2);
        assert_eq!(text(&rows[0]), "");
        assert_eq!(text(&rows[1]).trim_end(), " hello");
    }

    #[test]
    fn empty_assistant_message_renders_nothing() {
        let m = msg(vec![]);
        assert!(assistant(&m, &cx(), &|_| ToolTime::default()).is_empty());
    }

    #[test]
    fn digits_group_with_commas() {
        assert_eq!(group_digits(1261), "1,261");
        assert_eq!(group_digits(12), "12");
    }
}
