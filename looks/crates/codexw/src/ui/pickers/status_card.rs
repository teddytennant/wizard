//! The `/status` card (spec B.13.4), ported from Codex's `status/card.rs`.
//!
//! The cell is the magenta `/status` echo, a blank row and a dim bordered card. Wizard has no
//! permission, approval or limit data (spec Part E rows 29 to 31, 25 and 70), so those lines say
//! what it does instead of what Codex would show.

use std::path::Path;

use agent_core::Usage;
use ratatui::style::Stylize;
use ratatui::text::{Line, Span};
use unicode_segmentation::UnicodeSegmentation;

use crate::ui::HistoryCell;
use crate::ui::session_header::{center_truncate_path, with_border};
use crate::wrap::{line_width, width_of};

const INDENT: &str = " ";

/// Everything the card shows, collected when `/status` runs.
#[derive(Clone, Debug, Default)]
pub struct StatusData {
    pub model: String,
    pub effort: String,
    pub provider: Option<String>,
    pub directory: String,
    pub permissions: String,
    pub approval: String,
    pub agents: String,
    pub mode: Option<String>,
    pub session: Option<String>,
    pub usage: Usage,
    /// wizard's version, as it reported it.
    pub version: String,
}

#[derive(Debug)]
pub struct StatusCell {
    pub data: StatusData,
}

/// Label column: `" " + label + ":"` then `3 + widest - this` spaces, all dim.
struct Fields {
    label_width: usize,
    value_offset: usize,
}

impl Fields {
    fn from_labels(labels: &[&str]) -> Self {
        let label_width = labels.iter().map(|l| width_of(l)).max().unwrap_or(0);
        Fields {
            label_width,
            value_offset: INDENT.len() + label_width + 1 + 3,
        }
    }

    fn line(&self, label: &str, value: Vec<Span<'static>>) -> Line<'static> {
        let pad = 3 + self.label_width.saturating_sub(width_of(label));
        let mut spans = vec![Span::from(format!("{INDENT}{label}:{}", " ".repeat(pad))).dim()];
        spans.extend(value);
        Line::from(spans)
    }

    fn value_width(&self, inner: usize) -> usize {
        inner.saturating_sub(self.value_offset)
    }
}

/// `format_tokens_compact`: exact under a thousand, then K/M/B/T with trailing zeros trimmed.
pub fn format_tokens_compact(value: u64) -> String {
    if value == 0 {
        return "0".into();
    }
    if value < 1_000 {
        return value.to_string();
    }
    let v = value as f64;
    let (scaled, suffix) = if value >= 1_000_000_000_000 {
        (v / 1e12, "T")
    } else if value >= 1_000_000_000 {
        (v / 1e9, "B")
    } else if value >= 1_000_000 {
        (v / 1e6, "M")
    } else {
        (v / 1e3, "K")
    };
    let decimals = if scaled < 10.0 {
        2
    } else if scaled < 100.0 {
        1
    } else {
        0
    };
    let mut s = format!("{scaled:.decimals$}");
    if s.contains('.') {
        while s.ends_with('0') {
            s.pop();
        }
        if s.ends_with('.') {
            s.pop();
        }
    }
    format!("{s}{suffix}")
}

/// Codex counts the first 12k tokens of every context as overhead.
const BASELINE_TOKENS: u64 = 12_000;

pub fn percent_of_context_remaining(in_context: u64, window: u64) -> u64 {
    if window <= BASELINE_TOKENS {
        return 0;
    }
    let effective = window - BASELINE_TOKENS;
    let used = in_context.saturating_sub(BASELINE_TOKENS);
    let remaining = effective.saturating_sub(used);
    ((remaining as f64 / effective as f64) * 100.0)
        .clamp(0.0, 100.0)
        .round() as u64
}

fn truncate_line_to_width(line: Line<'static>, max: usize) -> Line<'static> {
    if max == 0 {
        return Line::default();
    }
    let mut used = 0usize;
    let mut out: Vec<Span<'static>> = Vec::new();
    for span in line.spans {
        let w = width_of(&span.content);
        if w == 0 {
            out.push(span);
            continue;
        }
        if used >= max {
            break;
        }
        if used + w <= max {
            used += w;
            out.push(span);
            continue;
        }
        let mut cut = String::new();
        for g in span.content.graphemes(true) {
            let gw = width_of(g);
            if used + gw > max {
                break;
            }
            cut.push_str(g);
            used += gw;
        }
        if !cut.is_empty() {
            out.push(Span::styled(cut, span.style));
        }
        break;
    }
    Line::from(out)
}

impl StatusData {
    fn model_spans(&self) -> Vec<Span<'static>> {
        let mut spans = vec![Span::from(self.model.clone())];
        let effort = if self.effort.is_empty() {
            "default"
        } else {
            self.effort.as_str()
        };
        spans.push(Span::from(" (").dim());
        spans.push(Span::from(format!("reasoning {effort}")).dim());
        spans.push(Span::from(")").dim());
        spans
    }

    fn token_usage_spans(&self) -> Vec<Span<'static>> {
        let u = &self.usage;
        let non_cached = u.input_tokens.saturating_sub(u.cached_tokens);
        let total = non_cached + u.output_tokens;
        vec![
            Span::from(format_tokens_compact(total)),
            Span::from(" total "),
            Span::from(" (").dim(),
            Span::from(format_tokens_compact(non_cached)).dim(),
            Span::from(" input").dim(),
            Span::from(" + ").dim(),
            Span::from(format_tokens_compact(u.output_tokens)).dim(),
            Span::from(" output").dim(),
            Span::from(")").dim(),
        ]
    }

    /// `None` until a turn has run: Codex hides the row with no tokens.
    fn context_window_spans(&self) -> Option<Vec<Span<'static>>> {
        let u = &self.usage;
        if u.context_tokens == 0 && u.input_tokens == 0 && u.output_tokens == 0 {
            return None;
        }
        if u.context_window == 0 {
            return Some(vec![
                Span::from("unknown"),
                Span::from(" (wizard does not report the window size)").dim(),
            ]);
        }
        let used = if u.context_tokens > 0 {
            u.context_tokens
        } else {
            u.input_tokens
        };
        let pct = percent_of_context_remaining(used, u.context_window);
        Some(vec![
            Span::from(format!("{pct}% left")),
            Span::from(" (").dim(),
            Span::from(format_tokens_compact(used)).dim(),
            Span::from(" used / ").dim(),
            Span::from(format_tokens_compact(u.context_window)).dim(),
            Span::from(")").dim(),
        ])
    }

    fn card_lines(&self, width: u16) -> Vec<Line<'static>> {
        let available = usize::from(width.saturating_sub(4));
        if available == 0 {
            return Vec::new();
        }
        let mut labels = vec!["Model"];
        if self.provider.is_some() {
            labels.push("Model provider");
        }
        labels.extend(["Directory", "Permissions", "Approval policy", "Agents.md"]);
        if self.mode.is_some() {
            labels.push("Collaboration mode");
        }
        if self.session.is_some() {
            labels.push("Session");
        }
        labels.push("Token usage");
        let ctx = self.context_window_spans();
        if ctx.is_some() {
            labels.push("Context window");
        }
        labels.push("Limits");
        let f = Fields::from_labels(&labels);
        let value_width = f.value_width(available);

        let mut lines: Vec<Line<'static>> = vec![
            Line::from(vec![
                Span::from(format!("{INDENT}>_ ")).dim(),
                Span::from("Wizard").bold(),
                Span::from(" ").dim(),
                Span::from(format!("(v{})", self.version)).dim(),
            ]),
            Line::default(),
        ];
        lines.push(f.line("Model", self.model_spans()));
        if let Some(p) = &self.provider {
            lines.push(f.line("Model provider", vec![Span::from(p.clone())]));
        }
        let dir = if width_of(&self.directory) > value_width {
            center_truncate_path(&self.directory, value_width)
        } else {
            self.directory.clone()
        };
        lines.push(f.line("Directory", vec![Span::from(dir)]));
        lines.push(f.line("Permissions", vec![Span::from(self.permissions.clone())]));
        lines.push(f.line("Approval policy", vec![Span::from(self.approval.clone())]));
        lines.push(f.line("Agents.md", vec![Span::from(self.agents.clone())]));
        if let Some(m) = &self.mode {
            lines.push(f.line("Collaboration mode", vec![Span::from(m.clone())]));
        }
        if let Some(s) = &self.session {
            lines.push(f.line("Session", vec![Span::from(s.clone())]));
        }
        lines.push(Line::default());
        lines.push(f.line("Token usage", self.token_usage_spans()));
        if let Some(c) = ctx {
            lines.push(f.line("Context window", c));
        }
        lines.push(f.line(
            "Limits",
            vec![Span::from("unknown (no subscription signed in)").dim()],
        ));

        let content = lines.iter().map(line_width).max().unwrap_or(0);
        let inner = content.min(available);
        let lines: Vec<Line<'static>> = lines
            .into_iter()
            .map(|l| truncate_line_to_width(l, inner))
            .collect();
        with_border(lines, inner)
    }
}

impl HistoryCell for StatusCell {
    fn display_lines(&self, width: u16) -> Vec<Line<'static>> {
        let card = self.data.card_lines(width);
        if card.is_empty() {
            return Vec::new();
        }
        let mut out = vec![Line::from("/status".magenta()), Line::default()];
        out.extend(card);
        out
    }
}

/// `AGENTS.md` and `WIZARD.md` from `cwd` up to the repository root, as paths relative to `cwd`.
/// Wizard reads them at the repo root; the card says which ones it would find.
pub fn agents_summary(cwd: &Path) -> String {
    let mut found: Vec<String> = Vec::new();
    let mut dir = Some(cwd);
    let mut ups = 0usize;
    while let Some(d) = dir {
        for name in ["AGENTS.md", "WIZARD.md"] {
            if d.join(name).is_file() {
                found.push(format!("{}{name}", "../".repeat(ups)));
            }
        }
        if d.join(".git").exists() {
            break;
        }
        dir = d.parent();
        ups += 1;
    }
    if found.is_empty() {
        "<none>".into()
    } else {
        found.join(", ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::style::{ColorLevel, Palette, set_palette};

    fn plain(lines: &[Line<'static>]) -> Vec<String> {
        lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect()
    }

    fn data() -> StatusData {
        StatusData {
            model: "gpt-5.5".into(),
            effort: "medium".into(),
            provider: Some("Mock".into()),
            directory: "~/proj".into(),
            permissions: "Full Access".into(),
            approval: "never (wizard does not ask)".into(),
            agents: "<none>".into(),
            mode: Some("Default".into()),
            session: Some("01a10e01-6dff-7fd2-9469-c820aa5968b7".into()),
            usage: Usage::default(),
            version: "3.8.12".into(),
        }
    }

    #[test]
    fn compact_token_counts() {
        assert_eq!(format_tokens_compact(0), "0");
        assert_eq!(format_tokens_compact(999), "999");
        assert_eq!(format_tokens_compact(1_400), "1.4K");
        assert_eq!(format_tokens_compact(2_250), "2.25K");
        assert_eq!(format_tokens_compact(9_000), "9K");
        assert_eq!(format_tokens_compact(7_200), "7.2K");
        assert_eq!(format_tokens_compact(272_000), "272K");
        assert_eq!(format_tokens_compact(1_500_000), "1.5M");
    }

    #[test]
    fn context_percent_uses_the_twelve_thousand_baseline() {
        assert_eq!(percent_of_context_remaining(0, 258_000), 100);
        assert_eq!(percent_of_context_remaining(1_500, 258_000), 100);
        assert_eq!(percent_of_context_remaining(12_000 + 123_000, 258_000), 50);
        assert_eq!(percent_of_context_remaining(10, 10_000), 0);
    }

    #[test]
    fn card_matches_the_capture_layout() {
        set_palette(Palette::new(None, None, ColorLevel::TrueColor));
        let mut d = data();
        d.usage = Usage {
            input_tokens: 7_200,
            output_tokens: 1_800,
            context_tokens: 1_500,
            context_window: 258_000,
            ..Default::default()
        };
        let rows = plain(&StatusCell { data: d }.display_lines(120));
        assert!(rows[5].starts_with("│  Model:                gpt-5.5"));
        assert!(rows[14].contains("Token usage:          9K total  (7.2K input + 1.8K output)"));
        assert!(rows[15].contains("Context window:       100% left (1.5K used / 258K)"));
        let w = rows[3].chars().count();
        assert!(
            rows[2..].iter().all(|r| r.chars().count() == w),
            "box is rectangular"
        );
    }

    #[test]
    fn zero_usage_has_no_context_row() {
        set_palette(Palette::new(None, None, ColorLevel::TrueColor));
        let rows = plain(&StatusCell { data: data() }.display_lines(120));
        assert!(
            rows.iter()
                .any(|r| r.contains("0 total  (0 input + 0 output)"))
        );
        assert!(!rows.iter().any(|r| r.contains("Context window")));
    }

    #[test]
    fn unknown_window_is_said_plainly() {
        set_palette(Palette::new(None, None, ColorLevel::TrueColor));
        let mut d = data();
        d.usage.input_tokens = 100;
        d.usage.context_tokens = 100;
        let rows = plain(&StatusCell { data: d }.display_lines(120));
        assert!(
            rows.iter()
                .any(|r| r.contains("unknown (wizard does not report the window size)"))
        );
    }

    #[test]
    fn every_width_renders_without_panicking() {
        set_palette(Palette::new(None, None, ColorLevel::TrueColor));
        for w in 0..=200u16 {
            let _ = StatusCell { data: data() }.display_lines(w);
        }
    }

    #[test]
    fn narrow_width_clips_the_card() {
        set_palette(Palette::new(None, None, ColorLevel::TrueColor));
        let rows = plain(&StatusCell { data: data() }.display_lines(40));
        assert!(
            rows.iter().skip(2).all(|r| r.chars().count() <= 40),
            "{rows:#?}"
        );
    }

    #[test]
    fn agents_summary_lists_files_up_to_the_repo_root() {
        let root = std::env::temp_dir().join(format!("cxw-agents-{}", std::process::id()));
        let sub = root.join("a/b");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::create_dir_all(root.join(".git")).unwrap();
        assert_eq!(agents_summary(&sub), "<none>");
        std::fs::write(root.join("AGENTS.md"), "x").unwrap();
        std::fs::write(sub.join("WIZARD.md"), "x").unwrap();
        assert_eq!(agents_summary(&sub), "WIZARD.md, ../../AGENTS.md");
        let _ = std::fs::remove_dir_all(root);
    }
}
