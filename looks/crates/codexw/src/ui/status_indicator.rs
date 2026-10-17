// OWNER: bottom-pane (status row, queued input preview)
//! The `• Working (3s • esc to interrupt)` row above the composer (spec B.16).

use std::time::{Duration, Instant};

use ratatui::style::{Style, Stylize};
use ratatui::text::{Line, Span};

use crate::style::{palette, shimmer_spans_at};
use crate::wrap::{line_width, width_of};

/// 0s, 59s, 1m 00s, 59m 59s, 1h 00m 00s.
pub fn fmt_elapsed_compact(secs: u64) -> String {
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m {:02}s", secs / 60, secs % 60)
    } else {
        format!(
            "{}h {:02}m {:02}s",
            secs / 3600,
            (secs % 3600) / 60,
            secs % 60
        )
    }
}

#[derive(Clone, Debug)]
pub struct StatusIndicator {
    pub header: String,
    /// Dim detail rows under the header (`  └ ...`).
    pub details: Vec<String>,
    started: Instant,
    /// Clock paused while a modal view is open.
    paused_at: Option<Instant>,
    paused_total: Duration,
}

impl StatusIndicator {
    pub fn new() -> Self {
        Self {
            header: "Working".into(),
            details: Vec::new(),
            started: Instant::now(),
            paused_at: None,
            paused_total: Duration::ZERO,
        }
    }

    pub fn elapsed(&self) -> Duration {
        let end = self.paused_at.unwrap_or_else(Instant::now);
        end.saturating_duration_since(self.started)
            .saturating_sub(self.paused_total)
    }

    pub fn pause(&mut self) {
        if self.paused_at.is_none() {
            self.paused_at = Some(Instant::now());
        }
    }

    pub fn resume(&mut self) {
        if let Some(p) = self.paused_at.take() {
            self.paused_total += p.elapsed();
        }
    }

    pub fn desired_height(&self) -> u16 {
        1 + self.details.len().min(3) as u16
    }

    /// The row at `elapsed_shimmer` seconds of shimmer phase (process time).
    pub fn lines(&self, width: u16, shimmer_t: f32) -> Vec<Line<'static>> {
        let p = palette();
        let secs = self.elapsed().as_secs();
        let mut spans: Vec<Span<'static>> = Vec::new();
        spans.extend(shimmer_spans_at("•", shimmer_t, &p));
        spans.push(Span::from(" "));
        spans.extend(shimmer_spans_at(&self.header, shimmer_t, &p));
        spans.push(Span::from(" "));
        spans.push(
            Span::from(format!(
                "({} • esc to interrupt)",
                fmt_elapsed_compact(secs)
            ))
            .dim(),
        );
        let line = truncate_with_ellipsis(Line::from(spans), width as usize);
        let mut out = vec![line];
        for (i, d) in self.details.iter().take(3).enumerate() {
            let prefix = if i == 0 { "  └ " } else { "    " };
            out.push(Line::from(format!("{prefix}{d}")).dim());
        }
        out
    }
}

impl Default for StatusIndicator {
    fn default() -> Self {
        Self::new()
    }
}

/// Cut a line to `max` cells, ending in `…` carrying the style of the last kept span.
pub fn truncate_with_ellipsis(line: Line<'static>, max: usize) -> Line<'static> {
    if line_width(&line) <= max {
        return line;
    }
    let keep = max.saturating_sub(1);
    let mut out: Vec<Span<'static>> = Vec::new();
    let mut used = 0;
    let mut last = Style::default();
    'outer: for s in line.spans {
        let mut text = String::new();
        for ch in s.content.chars() {
            let w = width_of(&ch.to_string());
            if used + w > keep {
                last = s.style;
                if !text.is_empty() {
                    out.push(Span::styled(text, s.style));
                }
                break 'outer;
            }
            used += w;
            text.push(ch);
        }
        last = s.style;
        out.push(Span::styled(text, s.style));
    }
    out.push(Span::styled("…", last));
    Line::from(out)
}

/// Queued and steered messages previewed above the composer (spec B.16.2).
pub fn pending_preview_lines(
    steers: &[String],
    queued: &[String],
    width: u16,
) -> Vec<Line<'static>> {
    let w = width as usize;
    if w < 4 || (steers.is_empty() && queued.is_empty()) {
        return Vec::new();
    }
    let opts = |init: &str, sub: &str| {
        crate::wrap::WrapOpts::new(w)
            .initial_indent(Line::from(Span::from(init.to_string()).dim()))
            .subsequent_indent(Line::from(Span::from(sub.to_string()).dim()))
    };
    let mut out: Vec<Line<'static>> = Vec::new();
    if !steers.is_empty() {
        let header = Line::from(vec![
            Span::from("• ").dim(),
            Span::from("Messages to be submitted after next tool call"),
            Span::from(" (press esc to interrupt and send immediately)").dim(),
        ]);
        out.extend(crate::wrap::word_wrap_line(
            &header,
            &crate::wrap::WrapOpts::new(w).subsequent_indent(Line::from(Span::from("  ").dim())),
        ));
        for s in steers {
            push_preview_item(&mut out, s, &opts("  ↳ ", "    "), false);
        }
    }
    if !queued.is_empty() {
        if !out.is_empty() {
            out.push(Line::default());
        }
        out.push(Line::from(vec![
            Span::from("• ").dim(),
            Span::from("Queued follow-up inputs"),
        ]));
        for s in queued {
            push_preview_item(&mut out, s, &opts("  ↳ ", "    "), true);
        }
        out.push(Line::from("    shift + ← edit last queued message").dim());
    }
    out
}

fn push_preview_item(
    out: &mut Vec<Line<'static>>,
    text: &str,
    opts: &crate::wrap::WrapOpts,
    italic: bool,
) {
    let style = if italic {
        Style::default().dim().italic()
    } else {
        Style::default().dim()
    };
    let line = Line::from(Span::styled(text.replace('\n', " "), style));
    let rows = crate::wrap::word_wrap_line(&line, opts);
    let limit = 3;
    let more = rows.len() > limit;
    out.extend(rows.into_iter().take(limit));
    if more {
        out.push(Line::from("    …").dim());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::style::{Palette, set_palette};

    fn plain(l: &Line<'_>) -> String {
        l.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    #[test]
    fn elapsed_format() {
        assert_eq!(fmt_elapsed_compact(0), "0s");
        assert_eq!(fmt_elapsed_compact(65), "1m 05s");
        assert_eq!(fmt_elapsed_compact(3600), "1h 00m 00s");
        assert_eq!(fmt_elapsed_compact(25 * 3600 + 2 * 60 + 3), "25h 02m 03s");
    }

    #[test]
    fn row_text_and_truncation() {
        set_palette(Palette::default());
        let s = StatusIndicator::new();
        assert_eq!(
            plain(&s.lines(120, 0.0)[0]),
            "• Working (0s • esc to interrupt)"
        );
        assert_eq!(
            plain(&s.lines(30, 0.0)[0]),
            "• Working (0s • esc to interr…"
        );
    }

    #[test]
    fn steer_preview_wraps_like_40_columns() {
        let rows: Vec<String> =
            pending_preview_lines(&["a queued follow up fake:text".into()], &[], 40)
                .iter()
                .map(plain)
                .collect();
        assert_eq!(
            rows,
            vec![
                "• Messages to be submitted after next",
                "  tool call (press esc to interrupt and",
                "  send immediately)",
                "  ↳ a queued follow up fake:text",
            ]
        );
    }
}
