//! `!cmd` and `!!cmd` blocks (spec 6.7, `bash-execution.js`): no background, a rule above and below,
//! the command in bold `bashMode`, the output in `muted`.

use std::time::{Duration, Instant};

use ratatui::text::{Line, Span};

use super::util::{clean, wrap_plain, wrap_row, EXPAND_KEY};
use super::ToolCx;
use crate::theme::Tok;
use crate::ui::{blank, indent, rule, span, Lines};

/// A `!cmd` or `!!cmd` run by piw itself.
#[derive(Clone, Debug)]
pub struct UserBash {
    pub cmd: String,
    /// `!!`: the output is not added to the model's context.
    pub exclude: bool,
    pub output: String,
    pub running: bool,
    pub exit: Option<i32>,
    pub cancelled: bool,
    pub started: Instant,
    pub took: Option<Duration>,
}

const PREVIEW: usize = 20;

pub fn render_user_bash(b: &UserBash, tcx: &ToolCx) -> Lines {
    let cx = tcx.cx;
    let th = cx.th();
    let rule_tok = if b.exclude { Tok::Dim } else { Tok::BashMode };
    let w = cx.width.saturating_sub(2).max(1);
    let muted = th.fg(Tok::Muted);
    let mut body: Lines = wrap_row(
        vec![span(format!("$ {}", clean(&b.cmd)), th.bold(Tok::BashMode))],
        w,
    );

    let out = clean(&b.output);
    let all: Vec<&str> = if out.is_empty() {
        Vec::new()
    } else {
        out.split('\n').collect()
    };
    let preview: Vec<&str> = all[all.len().saturating_sub(PREVIEW)..].to_vec();
    let hidden = all.len() - preview.len();
    if !all.is_empty() {
        if cx.expanded {
            body.push(blank());
            body.extend(wrap_plain(&all.join("\n"), w, muted));
        } else {
            let mut visual: Lines = vec![blank()];
            visual.extend(wrap_plain(&preview.join("\n"), w, muted));
            if visual.len() > PREVIEW {
                visual = visual.split_off(visual.len() - PREVIEW);
            }
            body.extend(visual);
        }
    }
    if b.running {
        body.push(blank());
        body.push(Line::from(vec![
            span(tuikit::spinner::braille_frame(cx.clock), th.fg(rule_tok)),
            Span::raw(" "),
            span("Running... (escape/ctrl+c to cancel)", muted),
        ]));
    } else {
        let mut parts: Vec<Line<'static>> = Vec::new();
        if hidden > 0 {
            parts.push(if cx.expanded {
                Line::from(vec![
                    span("(", muted),
                    span(EXPAND_KEY, th.fg(Tok::Dim)),
                    span(" to collapse", muted),
                    span(")", muted),
                ])
            } else {
                Line::from(vec![
                    span(format!("... {hidden} more lines ("), muted),
                    span(EXPAND_KEY, th.fg(Tok::Dim)),
                    span(" to expand", muted),
                    span(")", muted),
                ])
            });
        }
        if b.cancelled {
            parts.push(Line::from(span("(cancelled)", th.fg(Tok::Warning))));
        } else if let Some(code) = b.exit.filter(|c| *c != 0) {
            parts.push(Line::from(span(
                format!("(exit {code})"),
                th.fg(Tok::Error),
            )));
        }
        if !parts.is_empty() {
            body.push(blank());
            body.extend(parts);
        }
    }
    let rule_row = rule(cx.width, th.fg(rule_tok));
    let mut out = vec![blank(), rule_row.clone()];
    out.extend(body.into_iter().map(|l| indent(l, 1)));
    out.push(rule_row);
    out
}
