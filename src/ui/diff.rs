//! A line diff of an edit: context, a gutter, and the words that moved.
//!
//! Unchanged lines stay dim. A run of them longer than the context window
//! folds to `┄ n unchanged lines`. Changed lines keep a `-` or `+` and, when
//! the edit is a minority of the line, a bold underline on that span. The
//! gutter is the file's line number, dim, so the sign still carries the
//! meaning when color is gone. Nothing is filled.

use ratatui::style::Modifier;
use ratatui::text::{Line, Span};

use crate::theme::{self, Token};

/// Unchanged lines kept on each side of a change before the rest fold.
const CONTEXT: usize = 2;

/// Above this fraction of the line, the change is the line. Skip the highlight.
const HIGHLIGHT_LIMIT_NUM: usize = 3;
const HIGHLIGHT_LIMIT_DEN: usize = 5;

/// One rendered edit: the rows, and how many lines were added and removed.
pub(super) struct Rendered {
    pub rows: Vec<Line<'static>>,
    pub added: usize,
    pub removed: usize,
}

#[derive(Clone, Copy)]
enum Op<'a> {
    Equal(&'a str),
    Delete(&'a str),
    Insert(&'a str),
}

enum Row<'a> {
    Line {
        number: u32,
        sign: char,
        text: &'a str,
        other: Option<&'a str>,
        token: Token,
    },
    Fold(usize),
}

/// The body of an edit, numbered from `origin` (the file line of the first
/// old line; `0` is treated as line 1).
pub(super) fn render(old: &str, new: &str, origin: u32) -> Rendered {
    let old_lines = split(old);
    let new_lines = split(new);
    let ops = diff(&old_lines, &new_lines);
    let added = ops.iter().filter(|op| matches!(op, Op::Insert(_))).count();
    let removed = ops.iter().filter(|op| matches!(op, Op::Delete(_))).count();
    let rows = layout(&ops, origin.max(1));
    Rendered {
        rows: paint(&rows),
        added,
        removed,
    }
}

fn split(text: &str) -> Vec<&str> {
    if text.is_empty() {
        Vec::new()
    } else {
        text.lines().collect()
    }
}

/// Myers' shortest edit script on whole lines. O((N+M)D).
fn diff<'a>(old: &[&'a str], new: &[&'a str]) -> Vec<Op<'a>> {
    let n = old.len();
    let m = new.len();
    if n == 0 && m == 0 {
        return Vec::new();
    }
    let max = n + m;
    let offset = max as isize;
    let mut v = vec![0isize; 2 * max + 1];
    let mut trace: Vec<Vec<isize>> = Vec::with_capacity(max + 1);

    for d in 0..=max {
        trace.push(v.clone());
        for k in (-(d as isize)..=d as isize).step_by(2) {
            let index = (k + offset) as usize;
            let mut x = if k == -(d as isize)
                || (k != d as isize && v[(k - 1 + offset) as usize] < v[(k + 1 + offset) as usize])
            {
                v[(k + 1 + offset) as usize]
            } else {
                v[(k - 1 + offset) as usize] + 1
            };
            let mut y = x - k;
            while (x as usize) < n && (y as usize) < m && old[x as usize] == new[y as usize] {
                x += 1;
                y += 1;
            }
            v[index] = x;
            if (x as usize) >= n && (y as usize) >= m {
                return backtrack(&trace, old, new, offset);
            }
        }
    }
    // A path of length n+m always exists (delete everything, insert everything).
    backtrack(&trace, old, new, offset)
}

fn backtrack<'a>(
    trace: &[Vec<isize>],
    old: &[&'a str],
    new: &[&'a str],
    offset: isize,
) -> Vec<Op<'a>> {
    let mut x = old.len() as isize;
    let mut y = new.len() as isize;
    let mut ops = Vec::new();
    for d in (0..trace.len()).rev() {
        let v = &trace[d];
        let k = x - y;
        let d = d as isize;
        let prev_k =
            if k == -d || (k != d && v[(k - 1 + offset) as usize] < v[(k + 1 + offset) as usize]) {
                k + 1
            } else {
                k - 1
            };
        let prev_x = v[(prev_k + offset) as usize];
        let prev_y = prev_x - prev_k;
        while x > prev_x && y > prev_y {
            x -= 1;
            y -= 1;
            ops.push(Op::Equal(old[x as usize]));
        }
        if d > 0 {
            if x == prev_x {
                y -= 1;
                ops.push(Op::Insert(new[y as usize]));
            } else {
                x -= 1;
                ops.push(Op::Delete(old[x as usize]));
            }
        }
    }
    ops.reverse();
    ops
}

/// Number the script, pair a deletion with the insertion beside it for the
/// word pass, and fold untouched runs.
fn layout<'a>(ops: &[Op<'a>], origin: u32) -> Vec<Row<'a>> {
    let annotated = pair(ops);
    let mut numbered = Vec::new();
    let mut old_no = origin;
    let mut new_no = origin;
    let mut saw_change = false;
    for (op, other) in &annotated {
        match *op {
            Op::Equal(text) => {
                numbered.push((
                    false,
                    Row::Line {
                        number: old_no,
                        sign: ' ',
                        text,
                        other: None,
                        token: Token::Faint,
                    },
                ));
                old_no = old_no.saturating_add(1);
                new_no = new_no.saturating_add(1);
            }
            Op::Delete(text) => {
                saw_change = true;
                numbered.push((
                    true,
                    Row::Line {
                        number: old_no,
                        sign: '-',
                        text,
                        other: *other,
                        token: Token::DiffDel,
                    },
                ));
                old_no = old_no.saturating_add(1);
            }
            Op::Insert(text) => {
                saw_change = true;
                numbered.push((
                    true,
                    Row::Line {
                        number: new_no,
                        sign: '+',
                        text,
                        other: *other,
                        token: Token::DiffAdd,
                    },
                ));
                new_no = new_no.saturating_add(1);
            }
        }
    }
    if !saw_change {
        return numbered.into_iter().map(|(_, row)| row).collect();
    }
    fold(&numbered)
}

fn pair<'a>(ops: &[Op<'a>]) -> Vec<(Op<'a>, Option<&'a str>)> {
    let mut out = Vec::with_capacity(ops.len());
    let mut index = 0;
    while index < ops.len() {
        if matches!(ops[index], Op::Equal(_)) {
            out.push((ops[index], None));
            index += 1;
            continue;
        }
        let start = index;
        while index < ops.len() && !matches!(ops[index], Op::Equal(_)) {
            index += 1;
        }
        let hunk = &ops[start..index];
        let deletes: Vec<&str> = hunk
            .iter()
            .filter_map(|op| match op {
                Op::Delete(text) => Some(*text),
                _ => None,
            })
            .collect();
        let inserts: Vec<&str> = hunk
            .iter()
            .filter_map(|op| match op {
                Op::Insert(text) => Some(*text),
                _ => None,
            })
            .collect();
        let mut deleted = 0;
        let mut inserted = 0;
        for op in hunk {
            let other = match op {
                Op::Delete(_) => {
                    let other = inserts.get(deleted).copied();
                    deleted += 1;
                    other
                }
                Op::Insert(_) => {
                    let other = deletes.get(inserted).copied();
                    inserted += 1;
                    other
                }
                Op::Equal(_) => None,
            };
            out.push((*op, other));
        }
    }
    out
}

/// `changed` marks a delete or insert. Equal runs fold once a change exists.
fn fold<'a>(rows: &[(bool, Row<'a>)]) -> Vec<Row<'a>> {
    let mut out = Vec::new();
    let mut index = 0;
    let last_change = rows.iter().rposition(|(changed, _)| *changed);
    while index < rows.len() {
        if rows[index].0 {
            out.push(copy_row(&rows[index].1));
            index += 1;
            continue;
        }
        let start = index;
        while index < rows.len() && !rows[index].0 {
            index += 1;
        }
        let count = index - start;
        let leading = start == 0;
        let trailing = last_change.is_none_or(|at| start > at);
        if leading {
            emit_ends(&mut out, rows, start, index, 0, CONTEXT.min(count));
        } else if trailing {
            emit_ends(&mut out, rows, start, index, CONTEXT.min(count), 0);
        } else {
            let hidden = count.saturating_sub(CONTEXT * 2);
            if hidden < 2 {
                push_range(&mut out, rows, start, index);
            } else {
                push_range(&mut out, rows, start, start + CONTEXT);
                out.push(Row::Fold(hidden));
                push_range(&mut out, rows, index - CONTEXT, index);
            }
        }
    }
    out
}

/// Keep `head` lines, fold the middle when at least two lines are hidden,
/// then keep `tail` lines.
fn emit_ends<'a>(
    out: &mut Vec<Row<'a>>,
    rows: &[(bool, Row<'a>)],
    start: usize,
    end: usize,
    head: usize,
    tail: usize,
) {
    let count = end - start;
    let hidden = count.saturating_sub(head + tail);
    if hidden < 2 {
        push_range(out, rows, start, end);
        return;
    }
    push_range(out, rows, start, start + head);
    out.push(Row::Fold(hidden));
    push_range(out, rows, end - tail, end);
}

fn push_range<'a>(out: &mut Vec<Row<'a>>, rows: &[(bool, Row<'a>)], start: usize, end: usize) {
    for row in &rows[start..end] {
        out.push(copy_row(&row.1));
    }
}

fn copy_row<'a>(row: &Row<'a>) -> Row<'a> {
    match *row {
        Row::Line {
            number,
            sign,
            text,
            other,
            token,
        } => Row::Line {
            number,
            sign,
            text,
            other,
            token,
        },
        Row::Fold(n) => Row::Fold(n),
    }
}

fn paint(rows: &[Row<'_>]) -> Vec<Line<'static>> {
    let width = rows
        .iter()
        .filter_map(|row| match row {
            Row::Line { number, .. } => Some(number.to_string().len()),
            Row::Fold(_) => None,
        })
        .max()
        .unwrap_or(1);
    rows.iter()
        .map(|row| match row {
            Row::Fold(n) => {
                let word = if *n == 1 { "line" } else { "lines" };
                let pad = " ".repeat(width + 3);
                Line::from(Span::styled(
                    format!("{pad}┄ {n} unchanged {word}"),
                    theme::style(Token::Faint),
                ))
            }
            Row::Line {
                number,
                sign,
                text,
                other,
                token,
            } => signed(*number, width, *sign, text, *other, *token),
        })
        .collect()
}

fn signed(
    number: u32,
    width: usize,
    sign: char,
    line: &str,
    other: Option<&str>,
    token: Token,
) -> Line<'static> {
    let gutter = theme::style(Token::Faint);
    let style = theme::style(token);
    let mut spans = vec![
        Span::styled(format!("{number:>width$} "), gutter),
        Span::styled(format!("{sign} "), style),
    ];
    if let Some((start, end)) = other.and_then(|other| changed_range(line, other)) {
        let (pre, mid, post) = split_chars(line, start, end);
        if !pre.is_empty() {
            spans.push(Span::styled(pre, style));
        }
        if !mid.is_empty() {
            spans.push(Span::styled(
                mid,
                style.add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
            ));
        }
        if !post.is_empty() {
            spans.push(Span::styled(post, style));
        }
    } else {
        spans.push(Span::styled(line.to_string(), style));
    }
    Line::from(spans)
}

/// Character range of `line` that differs from `other`, when it is a minority
/// of the line. Indexes are in chars, and the ends sit on char boundaries.
fn changed_range(line: &str, other: &str) -> Option<(usize, usize)> {
    let a: Vec<char> = line.chars().collect();
    let b: Vec<char> = other.chars().collect();
    if a.is_empty() {
        return None;
    }
    let mut pre = 0;
    while pre < a.len() && pre < b.len() && a[pre] == b[pre] {
        pre += 1;
    }
    let mut suffix = 0;
    while suffix < a.len() - pre
        && suffix < b.len() - pre
        && a[a.len() - 1 - suffix] == b[b.len() - 1 - suffix]
    {
        suffix += 1;
    }
    let end = a.len() - suffix;
    if end <= pre {
        return None;
    }
    let changed = end - pre;
    if changed * HIGHLIGHT_LIMIT_DEN > a.len() * HIGHLIGHT_LIMIT_NUM {
        return None;
    }
    Some((pre, end))
}

fn split_chars(line: &str, start: usize, end: usize) -> (String, String, String) {
    let chars: Vec<char> = line.chars().collect();
    let pre = chars.get(..start).unwrap_or(&[]).iter().collect();
    let mid = chars.get(start..end).unwrap_or(&[]).iter().collect();
    let post = chars.get(end..).unwrap_or(&[]).iter().collect();
    (pre, mid, post)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(rows: &[Line]) -> Vec<String> {
        rows.iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect()
            })
            .collect()
    }

    fn replay(old: &str, new: &str) {
        let old_lines = split(old);
        let new_lines = split(new);
        let ops = diff(&old_lines, &new_lines);
        let mut got_old = Vec::new();
        let mut got_new = Vec::new();
        let mut edits = 0;
        for op in &ops {
            match *op {
                Op::Equal(line) => {
                    got_old.push(line);
                    got_new.push(line);
                }
                Op::Delete(line) => {
                    got_old.push(line);
                    edits += 1;
                }
                Op::Insert(line) => {
                    got_new.push(line);
                    edits += 1;
                }
            }
        }
        assert_eq!(got_old, old_lines);
        assert_eq!(got_new, new_lines);
        assert_eq!(edits, distance(&old_lines, &new_lines));
    }

    fn distance(a: &[&str], b: &[&str]) -> usize {
        let mut dp = vec![vec![0; b.len() + 1]; a.len() + 1];
        for (i, row) in dp.iter_mut().enumerate() {
            row[0] = i;
        }
        for (j, cell) in dp[0].iter_mut().enumerate() {
            *cell = j;
        }
        for i in 1..=a.len() {
            for j in 1..=b.len() {
                let mut best = dp[i - 1][j].min(dp[i][j - 1]) + 1;
                if a[i - 1] == b[j - 1] {
                    best = best.min(dp[i - 1][j - 1]);
                }
                dp[i][j] = best;
            }
        }
        dp[a.len()][b.len()]
    }

    #[test]
    fn the_script_reproduces_both_sides_in_the_minimum_of_edits() {
        replay("", "");
        replay("", "a\nb");
        replay("a\nb", "");
        replay("a\nb\nc", "a\nb\nc");
        replay("let a = 1;", "let a = 2;\nlet b = 3;");
        replay("a\nb\nc\nd", "a\nB\nc\nD");
        replay("only", "entirely different");
    }

    #[test]
    fn a_small_edit_is_underlined_and_a_rewritten_line_is_not() {
        let rendered = render("let a = 1;", "let a = 2;", 12);
        assert_eq!(rendered.added, 1);
        assert_eq!(rendered.removed, 1);
        let changed = rendered
            .rows
            .iter()
            .flat_map(|row| row.spans.iter())
            .find(|span| span.content.as_ref() == "1")
            .expect("the old digit");
        assert!(
            changed
                .style
                .add_modifier
                .contains(Modifier::BOLD | Modifier::UNDERLINED)
        );
        assert!(matches!(
            changed.style.bg,
            Some(ratatui::style::Color::Reset) | None
        ));

        let rewritten = render("alpha beta", "entirely different", 1);
        assert!(
            rewritten.rows.iter().all(|row| {
                row.spans
                    .iter()
                    .all(|span| !span.style.add_modifier.contains(Modifier::UNDERLINED))
            }),
            "a line that is mostly new stays plain"
        );
    }

    #[test]
    fn unchanged_lines_are_context_and_a_long_run_folds() {
        let old = "\
pub fn fold() {\n\
    let mut tools = 0;\n\
    for item in turn.items() {\n\
        if let Item::Tool(t) = item {\n\
            tools += 1;\n\
        } else {\n\
            let _ = item;\n\
        }\n\
    }\n\
    format!(\"{tools}\")\n\
}";
        let new = old.replacen(
            "let mut tools = 0;",
            "let mut tools = 0;\n    let mut images = 0;",
            1,
        );
        let rendered = render(old, &new, 3);
        let rows = text(&rendered.rows);
        assert_eq!(rendered.added, 1);
        assert_eq!(rendered.removed, 0);
        assert!(
            rows.iter()
                .any(|row| row.contains("┄") && row.contains("unchanged")),
            "a long untouched run folds: {rows:?}"
        );
        assert!(
            rows.iter()
                .any(|row| row.contains("+") && row.contains("images")),
            "the new line is marked: {rows:?}"
        );
        assert!(
            rows.iter().any(|row| row.contains("pub fn fold()")),
            "nearby context stays: {rows:?}"
        );
        // The gutter is a number, and a fold does not invent one.
        let fold = rows.iter().find(|row| row.contains('┄')).unwrap();
        assert!(
            !fold
                .trim_start()
                .starts_with(|ch: char| ch.is_ascii_digit()),
            "fold row: {fold}"
        );
    }
}
