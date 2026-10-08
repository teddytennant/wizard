//! An edit shown as `-` / `+` lines, with the changed words marked.
//!
//! No row is filled. The sign and the text take the diff color; the span that
//! actually changed is bold and underlined, which still reads when the
//! terminal's background is transparent. A line that is mostly new stays
//! plain colored text — underlining all of it would just be a second way to
//! say "this line changed", which the sign already says.

use ratatui::style::Modifier;
use ratatui::text::{Line, Span};

use crate::theme::{self, Token};

/// How many removed lines are paired with added lines for a word-level pass.
const MAX_PAIRS: usize = 8;

/// Above this fraction of the line, the change is the line. Skip the highlight.
const HIGHLIGHT_LIMIT_NUM: usize = 3;
const HIGHLIGHT_LIMIT_DEN: usize = 5;

/// The body of an edit: every removed line, then every added line.
pub(super) fn lines(old: &str, new: &str) -> Vec<Line<'static>> {
    let old_lines = split(old);
    let new_lines = split(new);
    let pairs = old_lines.len().min(new_lines.len()).min(MAX_PAIRS);
    let mut rows = Vec::new();
    for (index, line) in old_lines.iter().enumerate() {
        let other = (index < pairs).then(|| new_lines[index]);
        rows.push(signed('-', line, other, Token::DiffDel));
    }
    for (index, line) in new_lines.iter().enumerate() {
        let other = (index < pairs).then(|| old_lines[index]);
        rows.push(signed('+', line, other, Token::DiffAdd));
    }
    rows
}

fn split(text: &str) -> Vec<&str> {
    if text.is_empty() {
        Vec::new()
    } else {
        text.lines().collect()
    }
}

fn signed(sign: char, line: &str, other: Option<&str>, token: Token) -> Line<'static> {
    let style = theme::style(token);
    let mut spans = vec![Span::styled(format!("{sign} "), style)];
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

    #[test]
    fn a_small_edit_is_underlined_and_a_rewritten_line_is_not() {
        let rows = lines("let a = 1;", "let a = 2;");
        assert_eq!(rows.len(), 2);
        let changed = rows[0]
            .spans
            .iter()
            .find(|span| span.content.contains('1'))
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

        let rewritten = lines("alpha beta", "entirely different");
        assert!(
            rewritten.iter().all(|row| {
                row.spans
                    .iter()
                    .all(|span| !span.style.add_modifier.contains(Modifier::UNDERLINED))
            }),
            "a line that is mostly new stays plain"
        );
    }
}
