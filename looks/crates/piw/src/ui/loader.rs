// OWNER: loader (working, retry and compaction indicators in the editor's top rule)
//! Pi 1.0 draws the working indicator inside the editor's top rule: `── ⠙ Working ───…`. The
//! spinner is the Braille set at 80 ms a frame. Port of `custom-editor.js` `renderTopBorder` and
//! pi-tui's `createScrollBorder`. Spec 5.2.

use std::time::Duration;

use ratatui::style::Style;
use ratatui::text::{Line, Span};
use tuikit::width::{clip_spans, display_width, spans_width};

use super::{span, Cx};
use crate::theme::Tok;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LoaderKind {
    /// Streaming a response or running tools.
    Working,
    /// `/compact`.
    Compacting,
    /// Esc was pressed and the backend has not stopped yet.
    Aborting,
    /// Wizard reported a retry; no countdown is available over ACP, so the label stays generic.
    Retry,
}

#[derive(Clone, Debug)]
pub struct Loader {
    pub spinner: &'static str,
    pub spinner_style: Style,
    pub label: String,
    pub label_style: Style,
}

impl Loader {
    pub fn new(
        kind: LoaderKind,
        cx: &Cx,
        rule: Style,
        interrupt_key: &str,
        clock: Duration,
    ) -> Loader {
        let spinner = tuikit::spinner::braille_frame(clock);
        match kind {
            LoaderKind::Working => Loader {
                spinner,
                spinner_style: rule,
                label: "Working".into(),
                label_style: rule,
            },
            LoaderKind::Compacting => Loader {
                spinner,
                spinner_style: cx.th().fg(Tok::Accent),
                label: format!("Compacting context... ({interrupt_key} to cancel)"),
                label_style: cx.th().fg(Tok::Muted),
            },
            LoaderKind::Aborting => Loader {
                spinner,
                spinner_style: cx.th().fg(Tok::Warning),
                label: "Aborting...".into(),
                label_style: cx.th().fg(Tok::Muted),
            },
            LoaderKind::Retry => Loader {
                spinner,
                spinner_style: cx.th().fg(Tok::Warning),
                label: format!("Retrying... ({interrupt_key} to cancel)"),
                label_style: cx.th().fg(Tok::Muted),
            },
        }
    }

    fn status(&self, max: usize) -> Vec<Span<'static>> {
        let v = vec![
            span(self.spinner, self.spinner_style),
            Span::raw(" "),
            span(self.label.clone(), self.label_style),
        ];
        clip_spans(v, max)
    }

    fn spinner_only(&self, max: usize) -> Vec<Span<'static>> {
        clip_spans(vec![span(self.spinner, self.spinner_style)], max)
    }
}

fn dashes(n: usize) -> String {
    "─".repeat(n)
}

/// ` ↑ N more ` centred in a rule of `width`, or the shorter forms when it does not fit.
fn scroll_border(dir: &str, hidden: usize, width: usize) -> String {
    let label = format!(" {dir} {hidden} more ");
    let lw = display_width(&label);
    if lw + 2 <= width {
        let left = (width - lw) / 2;
        return format!("{}{}{}", dashes(left), label, dashes(width - left - lw));
    }
    let ind = format!("─── {dir} {hidden} more ");
    let iw = display_width(&ind);
    if width >= iw {
        return format!("{ind}{}", dashes(width - iw));
    }
    let ell = &"..."[..width.min(3)];
    let keep = width - ell.len();
    let cut: String = ind.chars().take(keep).collect();
    format!("{cut}{ell}")
}

pub fn bottom_border(width: usize, rule: Style, hidden_below: usize) -> Line<'static> {
    if hidden_below > 0 {
        Line::from(span(scroll_border("↓", hidden_below, width), rule))
    } else {
        Line::from(span(dashes(width), rule))
    }
}

pub fn top_border(
    width: usize,
    rule: Style,
    loader: Option<&Loader>,
    hidden_above: usize,
) -> Line<'static> {
    let plain = || {
        if hidden_above > 0 {
            Line::from(span(scroll_border("↑", hidden_above, width), rule))
        } else {
            Line::from(span(dashes(width), rule))
        }
    };
    let Some(l) = loader else { return plain() };
    if width == 0 {
        return plain();
    }
    let mut status = l.status(width.saturating_sub(5).max(1));
    let mut sw = spans_width(&status);
    if sw == 0 {
        return plain();
    }
    let overflow = (hidden_above > 0).then(|| format!(" ↑ {hidden_above} more "));
    let ow = overflow.as_deref().map_or(0, display_width);
    let ostart = width.saturating_sub(ow) / 2;
    let can_fit = |sw: usize| overflow.is_some() && ow + 2 <= width && ostart > 3 + sw + 1;
    if overflow.is_some() && !can_fit(sw) {
        status = l.spinner_only(width);
        sw = spans_width(&status);
    }
    if can_fit(sw) {
        let left = 3 + sw + 1;
        let mut v = vec![span("── ", rule)];
        v.extend(status);
        v.push(span(
            format!(
                " {}{}{}",
                dashes(ostart - left),
                overflow.unwrap_or_default(),
                dashes(width - ostart - ow)
            ),
            rule,
        ));
        return Line::from(v);
    }
    if width >= sw + 5 {
        let mut v = vec![span("── ", rule)];
        v.extend(status);
        v.push(span(format!(" {}", dashes(width - sw - 4)), rule));
        return Line::from(v);
    }
    let status = l.spinner_only(width);
    let sw = spans_width(&status);
    let prefix = 3.min(width.saturating_sub(sw));
    let mut v = vec![span(dashes(prefix), rule)];
    v.extend(status);
    v.push(span(dashes(width.saturating_sub(prefix + sw)), rule));
    Line::from(v)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::PiTheme;

    fn cx() -> Cx {
        Cx {
            theme: PiTheme::dark(),
            width: 120,
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

    #[test]
    fn working_rule_is_120_cells() {
        let cx = cx();
        let rule = cx.th().fg(Tok::ThinkingMedium);
        let l = Loader::new(LoaderKind::Working, &cx, rule, "escape", Duration::ZERO);
        let t = text(&top_border(120, rule, Some(&l), 0));
        assert!(t.starts_with("── ⠋ Working ───"));
        assert_eq!(display_width(&t), 120);
    }

    #[test]
    fn scroll_label_is_centred() {
        let t = text(&top_border(120, Style::default(), None, 3));
        assert_eq!(display_width(&t), 120);
        assert_eq!(
            t.find(" ↑ 3 more ").map(|i| t[..i].chars().count()),
            Some(55)
        );
    }

    #[test]
    fn narrow_rule_keeps_the_spinner_only() {
        let cx = cx();
        let l = Loader::new(
            LoaderKind::Working,
            &cx,
            Style::default(),
            "escape",
            Duration::ZERO,
        );
        let t = text(&top_border(6, Style::default(), Some(&l), 0));
        assert_eq!(display_width(&t), 6);
        assert!(t.contains('⠋') && !t.contains("Work"));
    }
}
