//! The subagent tree: a `Task` card's nested calls, drawn the way top-level cards are, with
//! `├─` and `└─` in `line`. Collapsed it is one summary row with live counts; running, the
//! last three calls trail under it; unfolded, eight calls; focused, all of them.

use ratatui::style::Style;

use super::{child_summary, ToolEntry};
use crate::ui::row::{fmt_dur, sp, spaces, two_col, Cx, Row};

/// Calls shown when a subagent is unfolded and not focused.
pub const TREE_ROWS: usize = 8;

/// How many of `e`'s calls are running, finished and failed.
pub fn counts(e: &ToolEntry) -> (usize, usize, usize) {
    let running = e.children.iter().filter(|c| c.running()).count();
    let failed = e.children.iter().filter(|c| c.failed()).count();
    (running, e.children.len() - running - failed, failed)
}

/// `6 tools` or `6 tools · 1 failed`, for the summary row.
pub fn count_text(e: &ToolEntry, sep: &str) -> String {
    let (_, _, failed) = counts(e);
    let n = e.children.len();
    let mut s = format!("{n} {}", if n == 1 { "tool" } else { "tools" });
    if failed > 0 {
        s.push_str(&format!(" {sep} {failed} failed"));
    }
    s
}

/// One row per call. `limit` caps the rows; the rest is `+n more`.
pub fn rows(e: &ToolEntry, cx: &Cx, limit: usize) -> Vec<Row> {
    let p = cx.p;
    let n = e.children.len();
    let shown = n.min(limit);
    let mut out = Vec::new();
    for (i, c) in e.children.iter().take(shown).enumerate() {
        let last = i + 1 == n;
        let branch = if last { cx.g.elbow } else { cx.g.tee };
        let glyph = if c.running() {
            sp(cx.spinner(), p.s_accent())
        } else if c.failed() {
            sp(cx.g.fail, p.s_err())
        } else {
            sp(cx.g.ok, p.s_ok())
        };
        let left = vec![
            spaces(4),
            sp(format!("{branch} "), p.s_faint()),
            glyph,
            spaces(1),
            sp(format!("{:<6}  ", c.verb()), p.s_dim()),
            sp(c.target(), p.s_text()),
        ];
        let right = vec![sp(child_right(c, cx), p.s_faint())];
        let mut spans = two_col(left, right, cx.width.saturating_sub(1), Style::default());
        spans.push(spaces(1));
        out.push(Row::new(spans).bg(p.surface));
    }
    if n > shown {
        out.push(
            Row::new(vec![
                spaces(4),
                sp(format!("+{} more", n - shown), p.s_faint()),
            ])
            .bg(p.surface),
        );
    }
    out
}

/// A running or failed call says so; a finished one says what it produced and, past a second,
/// how long it took.
fn child_right(c: &ToolEntry, cx: &Cx) -> String {
    if c.running() {
        return c.elapsed(cx.now).map(fmt_dur).unwrap_or_default();
    }
    let mut parts: Vec<String> = Vec::new();
    let s = child_summary(c);
    if !s.is_empty() {
        parts.push(s);
    }
    if let Some(t) = c.took.filter(|t| t.as_secs_f64() >= 1.0) {
        parts.push(fmt_dur(t));
    }
    parts.join(&format!(" {} ", cx.g.sep))
}

/// The last `n` rows, for a running subagent that is still folded.
pub fn tail(e: &ToolEntry, cx: &Cx, n: usize) -> Vec<Row> {
    let all = rows(e, cx, e.children.len());
    let from = all.len().saturating_sub(n);
    all.into_iter().skip(from).collect()
}
