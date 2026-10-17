//! Pi's edit diff: `generateDiffString` from `core/tools/edit-diff.js` and `renderDiff` from
//! `components/diff.js`. Text format, one logical row each: a `+`, `-` or space, the line number
//! right-aligned to the widest number, a space, the text.

use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use similar::{Algorithm, DiffOp, TextDiff};

use super::util::clean;
use crate::theme::{PiTheme, Tok};
use crate::ui::span;

#[derive(PartialEq, Clone, Copy)]
enum Kind {
    Equal,
    Added,
    Removed,
}

fn lines_of(tokens: &[&str]) -> Vec<String> {
    tokens
        .iter()
        .map(|l| l.strip_suffix('\n').unwrap_or(l).to_string())
        .collect()
}

/// Port of `generateDiffString`. `numbered: false` leaves the number column blank (used when the
/// fragment could not be located in a file).
pub fn generate(old: &str, new: &str, context: usize, numbered: bool) -> String {
    let diff = TextDiff::configure()
        .algorithm(Algorithm::Myers)
        .diff_lines(old, new);
    let old_toks: Vec<&str> = old.split_inclusive('\n').collect();
    let new_toks: Vec<&str> = new.split_inclusive('\n').collect();
    // jsdiff reports a removed part before the added part of the same change
    let mut parts: Vec<(Kind, Vec<String>)> = Vec::new();
    for op in diff.ops() {
        match *op {
            DiffOp::Equal { old_index, len, .. } => {
                parts.push((Kind::Equal, lines_of(&old_toks[old_index..old_index + len])))
            }
            DiffOp::Delete {
                old_index, old_len, ..
            } => parts.push((
                Kind::Removed,
                lines_of(&old_toks[old_index..old_index + old_len]),
            )),
            DiffOp::Insert {
                new_index, new_len, ..
            } => parts.push((
                Kind::Added,
                lines_of(&new_toks[new_index..new_index + new_len]),
            )),
            DiffOp::Replace {
                old_index,
                old_len,
                new_index,
                new_len,
            } => {
                parts.push((
                    Kind::Removed,
                    lines_of(&old_toks[old_index..old_index + old_len]),
                ));
                parts.push((
                    Kind::Added,
                    lines_of(&new_toks[new_index..new_index + new_len]),
                ));
            }
        }
    }
    let max_line = old.split('\n').count().max(new.split('\n').count());
    let width = max_line.to_string().len();
    let num = |n: usize| {
        if numbered {
            format!("{n:>width$}")
        } else {
            " ".repeat(width)
        }
    };
    let mut out: Vec<String> = Vec::new();
    let (mut old_n, mut new_n) = (1usize, 1usize);
    let mut last_was_change = false;
    for i in 0..parts.len() {
        let (kind, raw) = &parts[i];
        if *kind != Kind::Equal {
            for line in raw {
                if *kind == Kind::Added {
                    out.push(format!("+{} {line}", num(new_n)));
                    new_n += 1;
                } else {
                    out.push(format!("-{} {line}", num(old_n)));
                    old_n += 1;
                }
            }
            last_was_change = true;
            continue;
        }
        let next_is_change = i + 1 < parts.len() && parts[i + 1].0 != Kind::Equal;
        let blank = " ".repeat(width);
        let push_ctx =
            |out: &mut Vec<String>, line: &String, old_n: &mut usize, new_n: &mut usize| {
                out.push(format!(" {} {line}", num(*old_n)));
                *old_n += 1;
                *new_n += 1;
            };
        if last_was_change && next_is_change {
            if raw.len() <= context * 2 {
                for l in raw {
                    push_ctx(&mut out, l, &mut old_n, &mut new_n);
                }
            } else {
                for l in &raw[..context] {
                    push_ctx(&mut out, l, &mut old_n, &mut new_n);
                }
                let skipped = raw.len() - 2 * context;
                out.push(format!(" {blank} ..."));
                old_n += skipped;
                new_n += skipped;
                for l in &raw[raw.len() - context..] {
                    push_ctx(&mut out, l, &mut old_n, &mut new_n);
                }
            }
        } else if last_was_change {
            let shown = context.min(raw.len());
            for l in &raw[..shown] {
                push_ctx(&mut out, l, &mut old_n, &mut new_n);
            }
            let skipped = raw.len() - shown;
            if skipped > 0 {
                out.push(format!(" {blank} ..."));
                old_n += skipped;
                new_n += skipped;
            }
        } else if next_is_change {
            let skipped = raw.len().saturating_sub(context);
            if skipped > 0 {
                out.push(format!(" {blank} ..."));
                old_n += skipped;
                new_n += skipped;
            }
            for l in &raw[skipped..] {
                push_ctx(&mut out, l, &mut old_n, &mut new_n);
            }
        } else {
            old_n += raw.len();
            new_n += raw.len();
        }
        last_was_change = false;
    }
    out.join("\n")
}

struct Parsed<'a> {
    prefix: char,
    num: &'a str,
    content: &'a str,
}

/// `^([+-\s])(\s*\d*)\s(.*)$`
fn parse(line: &str) -> Option<Parsed<'_>> {
    let prefix = line.chars().next()?;
    if !matches!(prefix, '+' | '-' | ' ') {
        return None;
    }
    let rest = &line[1..];
    let ws = rest.len() - rest.trim_start_matches(' ').len();
    let after_ws = &rest[ws..];
    let digits = after_ws.len()
        - after_ws
            .trim_start_matches(|c: char| c.is_ascii_digit())
            .len();
    if digits > 0 {
        let after = &after_ws[digits..];
        let content = after.strip_prefix(' ')?;
        Some(Parsed {
            prefix,
            num: &rest[..ws + digits],
            content,
        })
    } else if ws >= 1 {
        Some(Parsed {
            prefix,
            num: &rest[..ws - 1],
            content: after_ws,
        })
    } else {
        None
    }
}

fn tokenize(s: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = 0;
    let class = |c: char| -> u8 {
        if c.is_alphanumeric() || c == '_' {
            1
        } else if c.is_whitespace() {
            2
        } else {
            0
        }
    };
    let mut prev: Option<(usize, u8)> = None;
    for (i, c) in s.char_indices() {
        let k = class(c);
        if let Some((_, pk)) = prev {
            if k == 0 || k != pk {
                out.push(&s[start..i]);
                start = i;
            }
        }
        prev = Some((i, k));
    }
    if start < s.len() {
        out.push(&s[start..]);
    }
    out
}

/// Segments of `a` (removed side) and `b` (added side) as `(changed, text)`; whitespace between
/// two changed tokens joins the change, as jsdiff's `diffWords` attaches it.
type Segments = Vec<(bool, String)>;

fn word_segments(a: &str, b: &str) -> (Segments, Segments) {
    let ta = tokenize(a);
    let tb = tokenize(b);
    let ops = similar::capture_diff_slices(Algorithm::Myers, &ta, &tb);
    let mut sa: Vec<(bool, &str)> = Vec::new();
    let mut sb: Vec<(bool, &str)> = Vec::new();
    for op in ops {
        match op {
            DiffOp::Equal {
                old_index,
                new_index,
                len,
            } => {
                for k in 0..len {
                    sa.push((false, ta[old_index + k]));
                    sb.push((false, tb[new_index + k]));
                }
            }
            DiffOp::Delete {
                old_index, old_len, ..
            } => ta[old_index..old_index + old_len]
                .iter()
                .for_each(|t| sa.push((true, t))),
            DiffOp::Insert {
                new_index, new_len, ..
            } => tb[new_index..new_index + new_len]
                .iter()
                .for_each(|t| sb.push((true, t))),
            DiffOp::Replace {
                old_index,
                old_len,
                new_index,
                new_len,
            } => {
                ta[old_index..old_index + old_len]
                    .iter()
                    .for_each(|t| sa.push((true, t)));
                tb[new_index..new_index + new_len]
                    .iter()
                    .for_each(|t| sb.push((true, t)));
            }
        }
    }
    let join = |s: Vec<(bool, &str)>| -> Vec<(bool, String)> {
        let mut s = s;
        for i in 1..s.len().saturating_sub(1) {
            if !s[i].0 && s[i].1.trim().is_empty() && s[i - 1].0 && s[i + 1].0 {
                s[i].0 = true;
            }
        }
        let mut out: Vec<(bool, String)> = Vec::new();
        for (c, t) in s {
            match out.last_mut() {
                Some((lc, lt)) if *lc == c => lt.push_str(t),
                _ => out.push((c, t.to_string())),
            }
        }
        out
    };
    (join(sa), join(sb))
}

fn intra_line(
    segs: Vec<(bool, String)>,
    base: ratatui::style::Style,
    lead: &str,
) -> Vec<Span<'static>> {
    let mut out = vec![span(lead.to_string(), base)];
    let mut first = true;
    for (changed, text) in segs {
        if !changed {
            out.push(span(text, base));
            continue;
        }
        let mut t = text.as_str();
        if first {
            let trimmed = t.trim_start();
            if t.len() != trimmed.len() {
                out.push(span(t[..t.len() - trimmed.len()].to_string(), base));
            }
            t = trimmed;
            first = false;
        }
        if !t.is_empty() {
            out.push(span(t.to_string(), base.add_modifier(Modifier::REVERSED)));
        }
    }
    out
}

/// Port of `renderDiff`: coloured rows, with inverse words when exactly one line was replaced by
/// one line.
pub fn render(diff: &str, th: &PiTheme) -> Vec<Line<'static>> {
    let lines: Vec<&str> = diff.split('\n').collect();
    let (ctx, add, rem) = (
        th.fg(Tok::ToolDiffContext),
        th.fg(Tok::ToolDiffAdded),
        th.fg(Tok::ToolDiffRemoved),
    );
    let mut out: Vec<Line<'static>> = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let Some(p) = parse(lines[i]) else {
            out.push(Line::from(span(lines[i].to_string(), ctx)));
            i += 1;
            continue;
        };
        match p.prefix {
            '-' => {
                let mut removed = Vec::new();
                while let Some(p) = lines
                    .get(i)
                    .and_then(|l| parse(l))
                    .filter(|p| p.prefix == '-')
                {
                    removed.push((p.num, p.content));
                    i += 1;
                }
                let mut added = Vec::new();
                while let Some(p) = lines
                    .get(i)
                    .and_then(|l| parse(l))
                    .filter(|p| p.prefix == '+')
                {
                    added.push((p.num, p.content));
                    i += 1;
                }
                if removed.len() == 1 && added.len() == 1 {
                    let (rn, rc) = removed[0];
                    let (an, ac) = added[0];
                    let (sa, sb) = word_segments(&clean(rc), &clean(ac));
                    out.push(Line::from(intra_line(sa, rem, &format!("-{rn} "))));
                    out.push(Line::from(intra_line(sb, add, &format!("+{an} "))));
                } else {
                    for (n, c) in removed {
                        out.push(Line::from(span(format!("-{n} {}", clean(c)), rem)));
                    }
                    for (n, c) in added {
                        out.push(Line::from(span(format!("+{n} {}", clean(c)), add)));
                    }
                }
            }
            '+' => {
                out.push(Line::from(span(
                    format!("+{} {}", p.num, clean(p.content)),
                    add,
                )));
                i += 1;
            }
            _ => {
                out.push(Line::from(span(
                    format!(" {} {}", p.num, clean(p.content)),
                    ctx,
                )));
                i += 1;
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_line_change_with_context() {
        let d = generate(
            "fn main() {\n    println!(\"hi\");\n}\n",
            "fn main() {\n    println!(\"hello, world\");\n    println!(\"bye\");\n}\n",
            4,
            true,
        );
        assert_eq!(
            d,
            " 1 fn main() {\n-2     println!(\"hi\");\n+2     println!(\"hello, world\");\n+3     println!(\"bye\");\n 3 }"
        );
    }

    #[test]
    fn long_runs_collapse_to_dots() {
        let old: String = (1..=60).map(|i| format!("l{i}\n")).collect();
        let new = old.replace("l3\n", "L3\n").replace("l41\n", "L41\n");
        let d = generate(&old, &new, 4, true);
        assert!(d.contains("\n    ...\n"), "{d}");
        assert!(d.contains("-41 l41\n+41 L41"), "{d}");
    }

    #[test]
    fn blank_gutter_when_unnumbered() {
        let d = generate("a\nb\n", "a\nc\n", 4, false);
        assert_eq!(d, "   a\n-  b\n+  c");
    }

    #[test]
    fn word_marks_exclude_indent_and_join_spaces() {
        let (a, b) = word_segments("line 41 of the long file", "line 41 changed");
        assert_eq!(a[0], (false, "line 41 ".to_string()));
        assert_eq!(a[1], (true, "of the long file".to_string()));
        assert_eq!(b[1], (true, "changed".to_string()));
        let (a, b) = word_segments("    println!(\"hi\");", "    println!(\"hello, world\");");
        assert!(a.iter().any(|(c, t)| *c && t == "hi"));
        assert!(b.iter().any(|(c, t)| *c && t == "hello, world"));
    }

    #[test]
    fn parse_handles_dots_and_blank_gutter() {
        let p = parse("     ...").unwrap();
        assert_eq!(p.content, "...");
        let p = parse("-12 x y").unwrap();
        assert_eq!((p.num, p.content), ("12", "x y"));
    }
}
