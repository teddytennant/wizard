// OWNER: pager
//! A thin diff renderer for the full-screen patch approval (`P A T C H`), laid out like the
//! patch cells of spec B.6.3: right-aligned line numbers, a sign column, whole-row tints for
//! added and removed lines, `⋮` between hunks. No syntax colours. The cells owner's patch
//! rendering replaces this when it lands.

use agent_core::FileDiff;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthChar;

use crate::style::palette;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Op {
    Eq,
    Del,
    Ins,
}

/// Line diff by longest common subsequence. Beyond ~4M table cells the files are shown as one
/// removal and one insertion block instead.
fn diff_ops(old: &[&str], new: &[&str]) -> Vec<(Op, usize, usize)> {
    let (n, m) = (old.len(), new.len());
    let mut out = Vec::new();
    if n.saturating_mul(m) > 4_000_000 {
        out.extend((0..n).map(|i| (Op::Del, i, 0)));
        out.extend((0..m).map(|j| (Op::Ins, 0, j)));
        return out;
    }
    let mut t = vec![0u32; (n + 1) * (m + 1)];
    let at = |i: usize, j: usize| i * (m + 1) + j;
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            t[at(i, j)] = if old[i] == new[j] {
                t[at(i + 1, j + 1)] + 1
            } else {
                t[at(i + 1, j)].max(t[at(i, j + 1)])
            };
        }
    }
    let (mut i, mut j) = (0, 0);
    while i < n && j < m {
        if old[i] == new[j] {
            out.push((Op::Eq, i, j));
            i += 1;
            j += 1;
        } else if t[at(i + 1, j)] >= t[at(i, j + 1)] {
            out.push((Op::Del, i, 0));
            i += 1;
        } else {
            out.push((Op::Ins, 0, j));
            j += 1;
        }
    }
    out.extend((i..n).map(|i| (Op::Del, i, 0)));
    out.extend((j..m).map(|j| (Op::Ins, 0, j)));
    out
}

fn split_lines(s: &str) -> Vec<&str> {
    let mut v: Vec<&str> = s.split('\n').collect();
    if v.last() == Some(&"") {
        v.pop();
    }
    v
}

/// Hard wrap by display columns, never splitting a wide character.
fn chunks(s: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut out = vec![String::new()];
    let mut used = 0;
    for c in s.replace('\t', "    ").chars() {
        let w = c.width().unwrap_or(0);
        if used + w > width && used > 0 {
            out.push(String::new());
            used = 0;
        }
        out.last_mut().unwrap().push(c);
        used += w;
    }
    out
}

pub fn counts(d: &FileDiff) -> (usize, usize) {
    let new = split_lines(&d.new);
    match &d.old {
        None => (new.len(), 0),
        Some(o) => {
            let old = split_lines(o);
            let ops = diff_ops(&old, &new);
            (
                ops.iter().filter(|o| o.0 == Op::Ins).count(),
                ops.iter().filter(|o| o.0 == Op::Del).count(),
            )
        }
    }
}

/// `path (+N -M)`, a blank row, then the numbered diff inset by two columns.
pub fn file_diff_lines(d: &FileDiff, width: u16) -> Vec<Line<'static>> {
    let (add, del) = counts(d);
    let green = Style::default().fg(Color::Green);
    let red = Style::default().fg(Color::Red);
    let mut out = vec![
        Line::from(vec![
            Span::from(format!("{} (", d.path)),
            Span::styled(format!("+{add}"), green),
            Span::from(" "),
            Span::styled(format!("-{del}"), red),
            Span::from(")"),
        ]),
        Line::default(),
    ];
    let old_lines: Vec<&str> = d.old.as_deref().map(split_lines).unwrap_or_default();
    let new_lines = split_lines(&d.new);
    let ops = diff_ops(&old_lines, &new_lines);
    let w = old_lines
        .len()
        .max(new_lines.len())
        .max(1)
        .to_string()
        .len();
    let p = palette();
    let (add_bg, del_bg) = if p.light_bg() {
        (Color::Rgb(218, 251, 225), Color::Rgb(255, 235, 233))
    } else {
        (Color::Rgb(33, 58, 43), Color::Rgb(74, 34, 29))
    };
    let body_w = (width as usize).saturating_sub(2 + w + 2).max(1);
    const CONTEXT: usize = 3;
    let changed: Vec<usize> = ops
        .iter()
        .enumerate()
        .filter(|(_, o)| o.0 != Op::Eq)
        .map(|(i, _)| i)
        .collect();
    let mut keep = vec![false; ops.len()];
    for &c in &changed {
        let (lo, hi) = (c.saturating_sub(CONTEXT), (c + CONTEXT + 1).min(ops.len()));
        keep[lo..hi].fill(true);
    }
    let dim = Style::default().add_modifier(Modifier::DIM);
    let mut prev_kept = true;
    for (idx, &(op, i, j)) in ops.iter().enumerate() {
        if !keep[idx] {
            prev_kept = false;
            continue;
        }
        if !prev_kept && idx > 0 {
            out.push(Line::from(vec![
                Span::from("  "),
                Span::styled(format!("{}⋮", " ".repeat(w + 1)), dim),
            ]));
        }
        prev_kept = true;
        let (num, sign, text, row) = match op {
            Op::Eq => (j + 1, ' ', new_lines[j], Style::default()),
            Op::Del => (i + 1, '-', old_lines[i], Style::default().bg(del_bg)),
            Op::Ins => (j + 1, '+', new_lines[j], Style::default().bg(add_bg)),
        };
        let sign_style = match op {
            Op::Eq => Style::default(),
            Op::Del => Style::default().fg(Color::Red),
            Op::Ins => Style::default().fg(Color::Green),
        };
        for (k, chunk) in chunks(text, body_w).into_iter().enumerate() {
            let spans = if k == 0 {
                vec![
                    Span::from("  "),
                    Span::styled(format!("{num:>w$} "), dim),
                    Span::styled(sign.to_string(), sign_style),
                    Span::from(chunk),
                ]
            } else {
                vec![
                    Span::from("  "),
                    Span::from(" ".repeat(w + 2)),
                    Span::from(chunk),
                ]
            };
            out.push(Line::from(spans).style(row));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::style::{ColorLevel, Palette, set_palette};

    fn text(l: &Line<'_>) -> String {
        l.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    fn dark() {
        set_palette(Palette::new(
            Some((230, 230, 230)),
            Some((0, 0, 0)),
            ColorLevel::TrueColor,
        ));
    }

    // codex_tui__diff_render__tests__apply_update_block, with the two column inset of the pager
    #[test]
    fn update_block_layout() {
        dark();
        let d = FileDiff {
            path: "example.txt".into(),
            old: Some("line one\nline two\nline three\n".into()),
            new: "line one\nline two changed\nline three\n".into(),
        };
        let rows: Vec<String> = file_diff_lines(&d, 80).iter().map(text).collect();
        assert_eq!(
            rows,
            vec![
                "example.txt (+1 -1)",
                "",
                "  1  line one",
                "  2 -line two",
                "  2 +line two changed",
                "  3  line three",
            ]
        );
    }

    // codex_tui__diff_render__tests__apply_add_block
    #[test]
    fn add_block_and_tints() {
        dark();
        let d = FileDiff {
            path: "new_file.txt".into(),
            old: None,
            new: "alpha\nbeta\n".into(),
        };
        let rows = file_diff_lines(&d, 80);
        assert_eq!(text(&rows[0]), "new_file.txt (+2 -0)");
        assert_eq!(text(&rows[2]), "  1 +alpha");
        assert_eq!(rows[2].style.bg, Some(Color::Rgb(33, 58, 43)));
    }

    // ..._vertical_ellipsis_between_hunks and ..._three_digits
    #[test]
    fn hunks_get_an_ellipsis_and_numbers_widen() {
        dark();
        let old: String = (1..=130).map(|i| format!("line {i}\n")).collect();
        let new = old
            .replace("line 3\n", "line 3 x\n")
            .replace("line 120\n", "line 120 x\n");
        let d = FileDiff {
            path: "h.txt".into(),
            old: Some(old),
            new,
        };
        let rows: Vec<String> = file_diff_lines(&d, 80).iter().map(text).collect();
        assert!(rows.contains(&"    3 -line 3".to_string()), "{rows:?}");
        assert!(rows.iter().any(|r| r.trim() == "⋮"), "{rows:?}");
        assert!(rows.contains(&"  120 +line 120 x".to_string()));
    }

    // ..._update_block_wraps_long_lines_text: hard wrap by columns
    #[test]
    fn long_lines_wrap_by_column() {
        dark();
        let d = FileDiff {
            path: "w.txt".into(),
            old: None,
            new: "abcdefghijklmnopqrstuvwxyz\n".into(),
        };
        let rows: Vec<String> = file_diff_lines(&d, 16).iter().map(text).collect();
        // width 16 - 2 inset - (1+2) = 11 columns of content
        assert_eq!(rows[2], "  1 +abcdefghijk");
        assert_eq!(rows[3], "     lmnopqrstuv");
        assert_eq!(rows[4], "     wxyz");
    }
}
