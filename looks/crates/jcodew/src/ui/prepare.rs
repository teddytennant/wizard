//! The transcript as wrapped rows: top padding, the header, then every message.
//!
//! One blank row goes before every message except a tool row and a turn footer, and nothing
//! after one. Text wraps at `width - 1`, the first line of a prompt at `width - 2`.

use agent_core::transcript::{Part, Role};
use agent_core::{NoticeLevel, ToolStatus};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Span;
use unicode_width::UnicodeWidthStr;

use super::line::{truncate_spans, wrap, Hang, RLine};
use super::{header, markdown, tools};
use crate::app::{App, Footer};
use crate::theme;

#[derive(Clone, Debug, Default)]
pub struct Row {
    pub spans: Vec<Span<'static>>,
    /// Set on the rows of a prompt's first line: the prompt's number.
    pub prompt: Option<usize>,
}

impl Row {
    pub fn width(&self) -> usize {
        self.spans.iter().map(|s| s.content.width()).sum()
    }
}

#[derive(Clone, Debug, Default)]
pub struct Prepared {
    pub rows: Vec<Row>,
    /// First row of each user prompt, with its number and text.
    pub prompts: Vec<(usize, usize, String)>,
}

/// `(height - 5 - header_lines) / 2` blank rows above the header of a fresh screen.
pub fn initial_pad_top(height: u16, header_lines: usize) -> usize {
    (height as usize)
        .saturating_sub(5)
        .saturating_sub(header_lines)
        / 2
}

pub fn format_duration(d: std::time::Duration) -> String {
    let ms = d.as_millis() as u64;
    if ms < 1000 {
        format!("{ms}ms")
    } else if ms < 10_000 {
        format!("{:.1}s", ms as f64 / 1000.0)
    } else if ms < 60_000 {
        format!("{}s", ms / 1000)
    } else if (ms / 1000).is_multiple_of(60) {
        format!("{}m", ms / 60_000)
    } else {
        format!("{}m {}s", ms / 60_000, (ms / 1000) % 60)
    }
}

pub fn format_tokens(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.0}k", n as f64 / 1_000.0)
    } else {
        n.to_string()
    }
}

pub fn footer_text(f: &Footer) -> String {
    let mut parts = vec![format_duration(f.took)];
    if let Some((i, o)) = f.tokens {
        parts.push(format!("↑{} ↓{}", format_tokens(i), format_tokens(o)));
    }
    parts.join(" · ")
}

fn user_lines(num: usize, distance: usize, text: &str, out: &mut Vec<RLine>) {
    let bg = theme::user_bg();
    let num_s = num.to_string();
    let prefix_width = num_s.width() + 2;
    let text = text.replace("\r\n", "\n").replace('\r', "\n");
    for (i, line) in text.split('\n').enumerate() {
        let body = Span::styled(
            line.to_string(),
            Style::default().fg(theme::user_text()).bg(bg),
        );
        if i == 0 {
            let mut l = RLine::new(vec![
                Span::styled(
                    num_s.clone(),
                    Style::default().fg(theme::rainbow_prompt(distance)).bg(bg),
                ),
                Span::styled("› ", Style::default().fg(theme::user()).bg(bg)),
                body,
            ])
            .hang(Hang::None);
            l.prompt = Some(num);
            out.push(l);
        } else {
            let pad = Span::styled(
                " ".repeat(prefix_width),
                Style::default().fg(theme::user()).bg(bg),
            );
            out.push(RLine::new(vec![pad.clone(), body]).hang(Hang::Spans(vec![pad])));
        }
    }
}

fn recolor(lines: Vec<RLine>, color: Color) -> Vec<RLine> {
    lines
        .into_iter()
        .map(|mut l| {
            for s in &mut l.spans {
                s.style = s.style.fg(color);
            }
            l
        })
        .collect()
}

fn looks_like_markdown(text: &str) -> bool {
    if ["**", "__", "`", "]("].iter().any(|m| text.contains(m)) {
        return true;
    }
    text.lines().any(|l| {
        let l = l.trim_start();
        [
            "# ", "## ", "### ", "- ", "* ", "+ ", "> ", "```", "~~~", "|",
        ]
        .iter()
        .any(|p| l.starts_with(p))
            || l.split_once(". ")
                .is_some_and(|(n, _)| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()))
    })
}

/// A system notice: every span pink, markdown structure kept when there is any.
fn system_lines(text: &str, width: usize, out: &mut Vec<RLine>) {
    let color = theme::system();
    if looks_like_markdown(text) {
        let hard = text.replace('\n', "  \n");
        out.extend(markdown::render_tinted(
            &hard,
            width.saturating_sub(4),
            color,
        ));
    } else {
        for line in text.split('\n') {
            out.push(RLine::styled(line.to_string(), Style::default().fg(color)));
        }
    }
}

fn reasoning_lines(text: &str, out: &mut Vec<RLine>) {
    let st = Style::default()
        .fg(theme::md_dim())
        .add_modifier(Modifier::ITALIC);
    for line in text.trim().split('\n') {
        out.push(RLine::styled(line.to_string(), st).hang(Hang::None));
    }
}

/// Everything above the composer at `width` text columns, for a screen `height` rows tall.
pub fn prepare(app: &App, width: usize, height: u16) -> Prepared {
    let mut lines: Vec<RLine> = Vec::new();
    let head = header::lines(app, width);
    let head_rows: usize = head
        .iter()
        .map(|l| wrap(l, width.saturating_sub(1)).len())
        .sum();
    for _ in 0..initial_pad_top(height, head_rows) {
        lines.push(RLine::blank());
    }
    lines.extend(head);

    let content_width = width.saturating_sub(4);
    let total_prompts = app.user_count();
    let pending = app.queue.len();
    let mut prompt_num = 0usize;
    let mut body = false;
    let first = app.cleared_at.unwrap_or(0);
    if app.cleared_at.is_some() {
        for _ in 0..height.saturating_sub(1) {
            lines.push(RLine::blank());
        }
    }
    macro_rules! sep {
        () => {
            if body {
                lines.push(RLine::blank());
            }
            body = true;
        };
    }
    for (mi, m) in app.tr.messages.iter().enumerate() {
        if m.role == Role::User {
            prompt_num += 1;
        }
        if mi < first {
            continue;
        }
        match m.role {
            Role::User => {
                sep!();
                let text = m
                    .parts
                    .iter()
                    .filter_map(|p| match p {
                        Part::Text(t) => Some(t.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                let distance = app
                    .prompt_distance
                    .get(&prompt_num)
                    .copied()
                    .unwrap_or((total_prompts + pending + 1).saturating_sub(prompt_num));
                user_lines(prompt_num, distance, &text, &mut lines);
            }
            Role::Assistant => {
                for p in &m.parts {
                    match p {
                        Part::Text(t) => {
                            if t.trim().is_empty() {
                                continue;
                            }
                            sep!();
                            lines.extend(markdown::render(t, content_width));
                        }
                        Part::Thought { text, .. } => {
                            if text.trim().is_empty() {
                                continue;
                            }
                            sep!();
                            reasoning_lines(text, &mut lines);
                        }
                        Part::Tool(c) => {
                            if !matches!(c.status, ToolStatus::Completed | ToolStatus::Failed) {
                                continue;
                            }
                            if app.todos_pinned && tools::is_todo(c) {
                                continue;
                            }
                            body = true;
                            tools::render(c, width, app.diff_mode, &mut lines);
                        }
                    }
                }
                if let Some(f) = app.footers.get(&mi) {
                    lines.push(RLine::new(vec![
                        Span::raw("  "),
                        Span::styled(footer_text(f), Style::default().fg(theme::dim())),
                    ]));
                }
            }
            Role::Notice(level) => {
                let text = m
                    .parts
                    .iter()
                    .filter_map(|p| match p {
                        Part::Text(t) => Some(t.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                if text.trim().is_empty() {
                    continue;
                }
                sep!();
                match level {
                    NoticeLevel::Error => {
                        let red = Style::default().fg(Color::Red);
                        lines.push(RLine::new(vec![
                            Span::styled("  ✗ ", red),
                            Span::styled(text.replace('\n', " "), red),
                        ]));
                    }
                    NoticeLevel::Warn => {
                        let mut l = Vec::new();
                        system_lines(&text, width, &mut l);
                        lines.extend(recolor(l, theme::warning()));
                    }
                    NoticeLevel::Info => system_lines(&text, width, &mut lines),
                }
            }
        }
    }

    let full = width.saturating_sub(1).max(1);
    let user = width.saturating_sub(2).max(1);
    let mut out = Prepared::default();
    for l in &lines {
        if l.nowrap {
            out.rows.push(Row {
                spans: truncate_spans(&l.spans, full),
                prompt: None,
            });
            continue;
        }
        let w = if l.prompt.is_some() { user } else { full };
        if let Some(n) = l.prompt {
            let text: String = l.spans.iter().skip(2).map(|s| s.content.as_ref()).collect();
            out.prompts.push((out.rows.len(), n, text));
        }
        for spans in wrap(l, w) {
            out.rows.push(Row {
                spans,
                prompt: l.prompt,
            });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn durations_and_tokens() {
        assert_eq!(format_duration(Duration::from_millis(870)), "870ms");
        assert_eq!(format_duration(Duration::from_millis(8_740)), "8.7s");
        assert_eq!(format_duration(Duration::from_secs(12)), "12s");
        assert_eq!(format_duration(Duration::from_secs(125)), "2m 5s");
        assert_eq!(format_tokens(18), "18");
        assert_eq!(format_tokens(17_400), "17k");
        assert_eq!(format_tokens(2_300_000), "2.3M");
    }

    #[test]
    fn pad_top_centres_the_header() {
        assert_eq!(initial_pad_top(40, 14), 10);
        assert_eq!(initial_pad_top(24, 14), 2);
    }
}
