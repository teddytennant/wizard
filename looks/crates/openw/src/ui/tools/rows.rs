//! The two shapes every tool takes in the transcript: `InlineToolRow` (one muted line with an
//! icon) and `BlockTool` (panel with a left bar, optional title, children separated by a blank
//! row, and a red error line).

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use tuikit::spinner::braille_frame;
use tuikit::Theme;

use super::text::wrap_text;
use crate::ui::session::{panel_lines, Block, RenderCx};

/// How the row reads, from the call's status.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    /// Arguments still arriving: `~ <pending text>`.
    Pending,
    /// Running or finished.
    Ready,
    Failed,
    /// Refused by the user or a rule: muted and struck through.
    Denied,
}

pub struct Inline {
    pub icon: String,
    /// May hold several lines (`\n`); each wraps on its own and continues under the text.
    pub text: String,
    pub pending: &'static str,
    pub phase: Phase,
    /// Text for a failed row whose own text is not ready yet (opencode's `failure`).
    pub failure: Option<&'static str>,
    /// opencode's `complete`: the key argument is known. A failed row that is not complete
    /// prints `failure` instead of its text.
    pub complete: bool,
    pub spinner: bool,
    pub separate: bool,
    pub color: Option<Color>,
    /// Muted `↳ ...` rows under the main one, drawn at the text's left edge.
    pub extra: Vec<String>,
    /// Error text, shown under a failed row once clicked open.
    pub error: Option<String>,
    pub error_open: bool,
    /// Id for the click handler (set for failed rows so their error can open).
    pub click: Option<String>,
}

impl Inline {
    pub fn new(icon: &str, text: String, pending: &'static str) -> Self {
        Inline {
            icon: icon.into(),
            text,
            pending,
            phase: Phase::Ready,
            failure: None,
            complete: true,
            spinner: false,
            separate: false,
            color: None,
            extra: Vec::new(),
            error: None,
            error_open: false,
            click: None,
        }
    }
}

fn row(prefix: &str, parts: Vec<Span<'static>>) -> Line<'static> {
    let mut v = vec![Span::raw(prefix.to_string())];
    v.extend(parts);
    Line::from(v)
}

pub fn inline_block(i: Inline, cx: &RenderCx) -> Block {
    let t = cx.theme;
    let content_w = (cx.width as usize).saturating_sub(3).max(1);
    let fg = i.color.unwrap_or(match i.phase {
        Phase::Failed => t.error,
        Phase::Pending => t.text,
        Phase::Ready | Phase::Denied => t.text_muted,
    });
    let mut style = Style::new().fg(fg);
    if i.phase == Phase::Denied {
        style = style.add_modifier(Modifier::CROSSED_OUT);
    }
    let mut lines: Vec<Line<'static>> = Vec::new();
    // Container padding 3 puts everything at transcript column 5; our lines start at column 2.
    let pad = "   ";
    match i.phase {
        Phase::Pending if !i.spinner => {
            for (n, l) in wrap_text(&format!("~ {}", i.pending), content_w)
                .into_iter()
                .enumerate()
            {
                let _ = n;
                lines.push(row(pad, vec![Span::styled(l, style)]));
            }
        }
        _ if i.spinner => {
            let first = braille_frame(cx.anim).to_string();
            let body = wrap_text(&i.text, content_w.saturating_sub(2));
            for (n, l) in body.into_iter().enumerate() {
                if n == 0 {
                    lines.push(row(
                        pad,
                        vec![
                            Span::styled(first.clone(), style),
                            Span::raw(" "),
                            Span::styled(l, style),
                        ],
                    ));
                } else {
                    lines.push(row("     ", vec![Span::styled(l, style)]));
                }
            }
        }
        phase => {
            let failed = phase == Phase::Failed;
            let text = if failed && !i.complete {
                i.failure.map_or_else(|| i.text.clone(), String::from)
            } else {
                i.text.clone()
            };
            let icon_style = if failed {
                Style::new().fg(t.error)
            } else {
                style
            };
            let body = wrap_text(&text, content_w.saturating_sub(2));
            for (n, l) in body.into_iter().enumerate() {
                if n == 0 {
                    lines.push(row(
                        pad,
                        vec![
                            Span::styled(format!("{:<2}", i.icon), icon_style),
                            Span::styled(l, style),
                        ],
                    ));
                } else {
                    lines.push(row("     ", vec![Span::styled(l, style)]));
                }
            }
        }
    }
    for e in &i.extra {
        for l in wrap_text(e, content_w) {
            lines.push(row(
                pad,
                vec![Span::styled(l, Style::new().fg(t.text_muted))],
            ));
        }
    }
    if i.phase == Phase::Failed && i.error_open {
        if let Some(e) = &i.error {
            for l in wrap_text(e.trim(), content_w.saturating_sub(2)) {
                lines.push(row(
                    "     ",
                    vec![Span::styled(l, Style::new().fg(t.error))],
                ));
            }
        }
    }
    let mut b = Block::inline(lines, i.separate);
    if i.phase == Phase::Failed {
        b.click = i.click;
    }
    b
}

// ---- block tool --------------------------------------------------------------------------

/// A block's content: rows already laid out, each a list of spans.
pub type Rows = Vec<Vec<Span<'static>>>;

pub fn text_rows(text: &str, width: usize, style: Style) -> Rows {
    wrap_text(text, width)
        .into_iter()
        .map(|l| vec![Span::styled(l, style)])
        .collect()
}

/// `BlockTool`: bar, padding top and bottom, `gap 1` between the title, each child and the
/// error. Content width is the column width minus the bar and its two columns of padding.
pub struct BlockSpec {
    pub title: Option<String>,
    /// Draw the title as `⠋ title-without-"# "` (a running shell).
    pub spinner_title: bool,
    pub children: Vec<Rows>,
    pub error: Option<String>,
    pub click: Option<String>,
}

pub fn block_width(cx: &RenderCx) -> usize {
    (cx.width as usize).saturating_sub(3).max(1)
}

pub fn block(spec: BlockSpec, cx: &RenderCx) -> Block {
    let t = cx.theme;
    let w = block_width(cx);
    let mut body: Rows = Vec::new();
    let mut parts: Vec<Rows> = Vec::new();
    if let Some(title) = &spec.title {
        let muted = Style::new().fg(t.text_muted);
        if spec.spinner_title {
            let first = braille_frame(cx.anim).to_string();
            let rows = wrap_text(title.trim_start_matches("# "), w.saturating_sub(2));
            parts.push(
                rows.into_iter()
                    .enumerate()
                    .map(|(n, l)| {
                        if n == 0 {
                            vec![
                                Span::styled(first.clone(), muted),
                                Span::raw(" "),
                                Span::styled(l, muted),
                            ]
                        } else {
                            vec![Span::raw("  "), Span::styled(l, muted)]
                        }
                    })
                    .collect(),
            );
        } else {
            parts.push(text_rows(title, w, muted));
        }
    }
    parts.extend(spec.children);
    if let Some(e) = &spec.error {
        parts.push(text_rows(e, w, Style::new().fg(t.error)));
    }
    for (n, p) in parts.into_iter().enumerate() {
        if n > 0 {
            body.push(Vec::new());
        }
        body.extend(p);
    }
    let lines = panel_lines(t, t.background, None, body);
    let mut b = Block::sep(lines);
    b.click = spec.click;
    b
}

/// Muted `Click to expand` / `Click to collapse` row.
pub fn click_hint(t: &Theme, expanded: bool) -> Rows {
    vec![vec![Span::styled(
        if expanded {
            "Click to collapse"
        } else {
            "Click to expand"
        },
        Style::new().fg(t.text_muted),
    )]]
}
