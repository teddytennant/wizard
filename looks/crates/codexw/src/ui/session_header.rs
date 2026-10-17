// OWNER: header (session card, tooltip, first-run help)
//! The boxed session header and the tip under it (spec A.14.6, B.13.2).

use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};

use super::HistoryCell;
use crate::wrap::{line_width, width_of};

pub const SESSION_HEADER_MAX_INNER_WIDTH: usize = 56;

#[derive(Clone, Debug)]
pub struct SessionHeaderCell {
    /// `None` renders the `loading` placeholder (dim italic) used before the session is ready.
    pub model: Option<String>,
    /// Reasoning effort label shown after the model, e.g. `high`. Empty for the default.
    pub effort: String,
    pub fast: bool,
    pub directory: String,
    /// Full access and no approvals: adds the `permissions: YOLO mode` row.
    pub yolo: bool,
    pub version: String,
}

impl SessionHeaderCell {
    pub fn new(model: Option<String>, effort: &str, directory: &str) -> Self {
        Self {
            model,
            effort: effort.to_string(),
            fast: false,
            directory: directory.to_string(),
            yolo: false,
            version: String::new(),
        }
    }
}

fn truncate_line_ellipsis(line: Line<'static>, max: usize) -> Line<'static> {
    if line_width(&line) <= max {
        return line;
    }
    let keep = max.saturating_sub(1);
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut used = 0;
    let mut last_style = Style::default();
    for s in line.spans {
        last_style = s.style;
        let mut text = String::new();
        for ch in s.content.chars() {
            let w = width_of(&ch.to_string());
            if used + w > keep {
                break;
            }
            used += w;
            text.push(ch);
        }
        let done = text.len() < s.content.len();
        if !text.is_empty() {
            spans.push(Span::styled(text, s.style));
        }
        if done {
            break;
        }
    }
    spans.push(Span::styled("…", last_style));
    Line::from(spans)
}

impl HistoryCell for SessionHeaderCell {
    fn raw_lines(&self) -> Vec<Line<'static>> {
        let model = self.model.as_deref().unwrap_or("loading");
        let effort = if self.effort.is_empty() {
            String::new()
        } else {
            format!(" {}", self.effort)
        };
        let mut lines = vec![
            Line::from(match self.version.is_empty() {
                true => "Wizard".to_string(),
                false => format!("Wizard (v{})", self.version),
            }),
            Line::from(format!("model: {model}{effort}")),
            Line::from(format!("directory: {}", self.directory)),
        ];
        if self.yolo {
            lines.push(Line::from("permissions: YOLO mode"));
        }
        lines
    }

    fn display_lines(&self, width: u16) -> Vec<Line<'static>> {
        let width = width as usize;
        if width < 4 {
            return Vec::new();
        }
        let inner_max = (width - 4).min(SESSION_HEADER_MAX_INNER_WIDTH);
        let label_w = if self.yolo { 12 } else { 10 };
        let label = |s: &str| -> Span<'static> { Span::from(format!("{s:<label_w$} ")).dim() };

        // the version is wizard's, unknown until it answers
        let mut title = vec![Span::from(">_ ").dim(), Span::from("Wizard").bold()];
        if !self.version.is_empty() {
            title.push(Span::from(" ").dim());
            title.push(Span::from(format!("(v{})", self.version)).dim());
        }
        let title = Line::from(title);
        let mut model = vec![label("model:")];
        match &self.model {
            Some(m) => model.push(Span::from(m.clone())),
            None => model.push(Span::styled(
                "loading",
                Style::default().add_modifier(Modifier::DIM | Modifier::ITALIC),
            )),
        }
        if !self.effort.is_empty() {
            model.push(Span::from(format!(" {}", self.effort)));
        }
        if self.fast {
            model.push(Span::from("   ").dim());
            model.push(Span::from("fast").magenta());
        }
        model.push(Span::from("   ").dim());
        model.push(Span::from("/model").cyan());
        model.push(Span::from(" to change").dim());

        let dir_avail = inner_max.saturating_sub(label_w + 1);
        let dir = center_truncate_path(&self.directory, dir_avail);
        let mut lines = vec![title, Line::default(), Line::from(model)];
        lines.push(Line::from(vec![label("directory:"), Span::from(dir)]));
        if self.yolo {
            lines.push(Line::from(vec![
                label("permissions:"),
                Span::styled(
                    "YOLO mode",
                    Style::default()
                        .fg(Color::Magenta)
                        .add_modifier(Modifier::BOLD),
                ),
            ]));
        }
        let lines: Vec<Line<'static>> = lines
            .into_iter()
            .map(|l| truncate_line_ellipsis(l, inner_max))
            .collect();
        let content_w = lines.iter().map(line_width).max().unwrap_or(0);
        with_border(lines, content_w)
    }
}

/// Wrap lines in a dim rounded border; the box is as wide as its widest line.
pub fn with_border(lines: Vec<Line<'static>>, content_w: usize) -> Vec<Line<'static>> {
    let mut out = Vec::with_capacity(lines.len() + 2);
    out.push(Line::from(format!("╭{}╮", "─".repeat(content_w + 2))).dim());
    for l in lines {
        let pad = content_w - line_width(&l);
        let mut spans = vec![Span::from("│ ").dim()];
        spans.extend(l.spans);
        spans.push(Span::from(" ".repeat(pad)).dim());
        spans.push(Span::from(" │").dim());
        out.push(Line::from(spans));
    }
    out.push(Line::from(format!("╰{}╯", "─".repeat(content_w + 2))).dim());
    out
}

/// Shorten a long path in the middle, keeping the first and last two segments when it can.
pub fn center_truncate_path(path: &str, max: usize) -> String {
    if width_of(path) <= max {
        return path.to_string();
    }
    let segs: Vec<&str> = path.split('/').collect();
    if segs.len() >= 5 {
        for head in (1..=2).rev() {
            let cand = format!(
                "{}/…/{}",
                segs[..head + 1].join("/"),
                segs[segs.len() - 2..].join("/")
            );
            if width_of(&cand) <= max {
                return cand;
            }
        }
    }
    tuikit::width::truncate_left(path, max)
}

/// The `  Tip: ...` row under the header.
#[derive(Clone, Debug)]
pub struct TooltipCell {
    pub text: String,
}

impl TooltipCell {
    pub fn new(text: &str) -> Self {
        Self {
            text: text.to_string(),
        }
    }
}

/// `*x*` italic and `**x**` bold, the only markdown a tip uses.
fn tip_spans(text: &str) -> Vec<Span<'static>> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut rest = text;
    while !rest.is_empty() {
        if let Some(r) = rest.strip_prefix("**") {
            if let Some(end) = r.find("**") {
                spans.push(Span::from(r[..end].to_string()).bold());
                rest = &r[end + 2..];
                continue;
            }
        }
        if let Some(r) = rest.strip_prefix('*') {
            if let Some(end) = r.find('*') {
                spans.push(Span::from(r[..end].to_string()).italic());
                rest = &r[end + 1..];
                continue;
            }
        }
        let next = rest
            .find('*')
            .map(|i| if i == 0 { 1 } else { i })
            .unwrap_or(rest.len());
        spans.push(Span::from(rest[..next].to_string()));
        rest = &rest[next..];
    }
    spans
}

impl HistoryCell for TooltipCell {
    fn raw_lines(&self) -> Vec<Line<'static>> {
        vec![Line::from(format!("Tip: {}", self.text))]
    }

    fn display_lines(&self, width: u16) -> Vec<Line<'static>> {
        let mut spans = vec![Span::from("Tip:").bold(), Span::from(" ")];
        spans.extend(tip_spans(&self.text));
        let line = Line::from(spans);
        let opts = crate::wrap::WrapOpts::new((width as usize).saturating_sub(2).max(1));
        crate::wrap::word_wrap_line(&line, &opts)
            .into_iter()
            .map(|mut l| {
                l.spans.insert(0, Span::from("  "));
                l
            })
            .collect()
    }
}

/// The tip Codex shows on every fresh start of this build.
/// The startup tip. Codex's advertises one of its own models, which wizard does not run.
pub const DEFAULT_TIP: &str = "Use /ui to try another look, and /ui wizard to go back to the house one. /model picks from the models wizard offers.";

/// Header, one blank row, tip: what `SessionInfoCell` is when a session starts.
#[derive(Clone, Debug)]
pub struct SessionInfoCell {
    pub header: SessionHeaderCell,
    pub tip: Option<TooltipCell>,
}

impl HistoryCell for SessionInfoCell {
    fn raw_lines(&self) -> Vec<Line<'static>> {
        let mut lines = self.header.raw_lines();
        if let Some(t) = &self.tip {
            lines.push(Line::default());
            lines.extend(t.raw_lines());
        }
        lines
    }

    fn display_lines(&self, width: u16) -> Vec<Line<'static>> {
        let mut lines = self.header.display_lines(width);
        if let Some(t) = &self.tip {
            let tl = t.display_lines(width);
            if !tl.is_empty() {
                lines.push(Line::default());
                lines.extend(tl);
            }
        }
        lines
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(lines: &[Line<'_>]) -> Vec<String> {
        lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect()
    }

    #[test]
    fn header_matches_the_start_capture() {
        let mut c = SessionHeaderCell::new(Some("gpt-5.5".into()), "", "~/proj");
        c.version = "3.8.12".into();
        let t = text(&c.display_lines(120));
        assert_eq!(t[0], "╭───────────────────────────────────────╮");
        let title = "│ >_ Wizard (v3.8.12)";
        assert!(t[1].starts_with(title) && t[1].ends_with('│'), "{}", t[1]);
        assert_eq!(t[1].chars().count(), t[0].chars().count());
        assert_eq!(t[3], "│ model:     gpt-5.5   /model to change │");
        assert_eq!(t[4], "│ directory: ~/proj                     │");
        assert_eq!(t.len(), 6);
    }

    #[test]
    fn yolo_row_widens_the_label_column() {
        let mut c = SessionHeaderCell::new(Some("gpt-5".into()), "", "/tmp/project");
        c.yolo = true;
        c.version = "test".into();
        let t = text(&c.display_lines(120));
        assert_eq!(
            t,
            vec![
                "╭───────────────────────────────────────╮",
                "│ >_ Wizard (vtest)                     │",
                "│                                       │",
                "│ model:       gpt-5   /model to change │",
                "│ directory:   /tmp/project             │",
                "│ permissions: YOLO mode                │",
                "╰───────────────────────────────────────╯",
            ]
        );
    }

    #[test]
    fn narrow_width_truncates_with_ellipsis() {
        let c = SessionHeaderCell::new(Some("gpt-5.5".into()), "", "~/proj");
        let t = text(&c.display_lines(40));
        assert_eq!(t[3], "│ model:     gpt-5.5   /model to chan… │");
    }

    #[test]
    fn long_directory_keeps_head_and_tail() {
        let d = "~/proj/a-very-long-directory-name/with/several/nested/levels/of/subdirectories/for-truncation-tests";
        let c = SessionHeaderCell::new(Some("gpt-5.5".into()), "", d);
        let t = text(&c.display_lines(120));
        assert!(
            t[4].contains("directory: ~/proj/…/subdirectories/for-truncation-tests"),
            "{}",
            t[4]
        );
    }

    #[test]
    fn tip_wraps_at_120() {
        let t = TooltipCell::new(DEFAULT_TIP);
        let l = text(&t.display_lines(120));
        assert_eq!(l.len(), 2);
        assert!(l[0].starts_with("  Tip: Use /ui to try another look"));
    }
}
