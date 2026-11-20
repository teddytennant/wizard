//! File edits as Claude Code draws them: line numbers, a sign column, added and removed rows on
//! a tinted background, the changed words of an added line on a stronger one, and syntax colours
//! on the rows that are not removals.

use std::sync::OnceLock;

use agent_core::FileDiff;
use ratatui::style::{Modifier, Style};
use similar::{Algorithm, ChangeTag, DiffOp, TextDiff};
use syntect::parsing::{ParseState, ScopeStack, SyntaxSet};

use crate::text::{Row, Span};
use crate::theme::Theme;

pub struct Rendered {
    pub rows: Vec<Row>,
    pub added: usize,
    pub removed: usize,
}

fn syntaxes() -> &'static SyntaxSet {
    static SET: OnceLock<SyntaxSet> = OnceLock::new();
    SET.get_or_init(SyntaxSet::load_defaults_newlines)
}

/// Load the grammars off the main thread so the first diff does not stall a frame.
pub fn warm() {
    let _ = syntaxes();
}

pub struct Highlighter {
    state: Option<ParseState>,
    stack: ScopeStack,
}

impl Highlighter {
    /// For a file path, or a fenced block's language tag.
    pub fn new(path: &str) -> Highlighter {
        let ext = path.rsplit('.').next().unwrap_or("");
        let set = syntaxes();
        let state = set
            .find_syntax_by_extension(ext)
            .or_else(|| set.find_syntax_by_token(ext))
            .map(ParseState::new);
        Highlighter {
            state,
            stack: ScopeStack::new(),
        }
    }

    pub fn known(&self) -> bool {
        self.state.is_some()
    }

    /// `line` as (text, colour slot) runs; slot indexes `Theme::code`.
    pub fn line(&mut self, line: &str) -> Vec<(String, usize)> {
        let Some(state) = &mut self.state else {
            return vec![(line.to_string(), 0)];
        };
        let with_nl = format!("{line}\n");
        let Ok(ops) = state.parse_line(&with_nl, syntaxes()) else {
            return vec![(line.to_string(), 0)];
        };
        let mut out: Vec<(String, usize)> = Vec::new();
        let mut at = 0;
        let push = |from: usize, to: usize, stack: &ScopeStack, out: &mut Vec<(String, usize)>| {
            let to = to.min(line.len());
            if from >= to {
                return;
            }
            let slot = slot_of(stack);
            match out.last_mut() {
                Some((s, k)) if *k == slot => s.push_str(&line[from..to]),
                _ => out.push((line[from..to].to_string(), slot)),
            }
        };
        for (pos, op) in ops {
            push(at, pos, &self.stack, &mut out);
            at = pos.min(line.len());
            let _ = self.stack.apply(&op);
        }
        push(at, line.len(), &self.stack, &mut out);
        out
    }
}

fn slot_of(stack: &ScopeStack) -> usize {
    for scope in stack.as_slice().iter().rev() {
        let name = scope.build_string();
        if name.starts_with("comment") {
            return 5;
        }
        if name.starts_with("string") || name.starts_with("punctuation.definition.string") {
            return 3;
        }
        if name.starts_with("constant.numeric") || name.starts_with("constant.language") {
            return 4;
        }
        if name.starts_with("keyword") || name.starts_with("storage") {
            return 1;
        }
        if name.starts_with("support.function") || name.starts_with("support.macro") {
            return 6;
        }
        if name.starts_with("entity.name") {
            return 2;
        }
    }
    0
}

fn tokens(s: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = 0;
    let mut word = false;
    for (i, c) in s.char_indices() {
        let w = c.is_alphanumeric() || c == '_';
        if i > start && !(w && word) {
            out.push(&s[start..i]);
            start = i;
        }
        word = w;
    }
    if start < s.len() {
        out.push(&s[start..]);
    }
    out
}

/// Byte ranges of `new` that are not in `old`, by word.
fn changed_ranges(old: &str, new: &str) -> Vec<(usize, usize)> {
    let (a, b) = (tokens(old), tokens(new));
    let mut offsets = Vec::with_capacity(b.len() + 1);
    let mut at = 0;
    for t in &b {
        offsets.push(at);
        at += t.len();
    }
    offsets.push(at);
    let mut out = Vec::new();
    for op in similar::capture_diff_slices(Algorithm::Myers, &a, &b) {
        match op {
            DiffOp::Insert {
                new_index, new_len, ..
            }
            | DiffOp::Replace {
                new_index, new_len, ..
            } => {
                out.push((offsets[new_index], offsets[new_index + new_len]));
            }
            _ => {}
        }
    }
    out
}

fn digits(n: usize) -> usize {
    n.max(1).to_string().len()
}

/// The diff of `d` as rows starting `indent` columns in.
pub fn render(th: &Theme, d: &FileDiff, indent: usize, w: usize) -> Rendered {
    let old = d.old.clone().unwrap_or_default();
    let diff = TextDiff::from_lines(&old, &d.new);
    let groups = diff.grouped_ops(3);
    let last_no = groups
        .iter()
        .flatten()
        .map(|op| op.old_range().end.max(op.new_range().end))
        .max()
        .unwrap_or(1);
    let nw = digits(last_no);
    let pad = " ".repeat(indent);
    let room = w.saturating_sub(indent + nw + 3).max(1);
    let mut hl = Highlighter::new(&d.path);
    let mut rows = Vec::new();
    let (mut added, mut removed) = (0, 0);
    for (gi, group) in groups.iter().enumerate() {
        if gi > 0 {
            rows.push(Row::of(format!("{pad} {:>nw$} ", "…"), th.dim()));
        }
        let changes: Vec<_> = group.iter().flat_map(|op| diff.iter_changes(op)).collect();
        for (ci, ch) in changes.iter().enumerate() {
            let text = ch.value().trim_end_matches('\n');
            let text: String = text.chars().take(room).collect();
            let mut row = Row::of(pad.clone(), Style::new());
            match ch.tag() {
                ChangeTag::Equal => {
                    let no = ch.new_index().map_or(0, |i| i + 1);
                    row.push(" ", Style::new());
                    row.push(
                        format!("{no:>nw$}"),
                        th.fg(th.code[0]).add_modifier(Modifier::DIM),
                    );
                    row.push("  ", Style::new());
                    for (s, slot) in hl.line(&text) {
                        row.push(s, th.fg(th.code[slot]));
                    }
                }
                ChangeTag::Delete => {
                    removed += 1;
                    let bg = Style::new().bg(th.removed_bg);
                    let no = ch.old_index().map_or(0, |i| i + 1);
                    row.push(" ", bg);
                    row.push(format!("{no:>nw$} -"), bg.fg(th.removed_fg));
                    row.push(text, bg.fg(th.code[0]));
                }
                ChangeTag::Insert => {
                    added += 1;
                    let bg = Style::new().bg(th.added_bg);
                    let no = ch.new_index().map_or(0, |i| i + 1);
                    row.push(" ", bg);
                    row.push(format!("{no:>nw$} +"), bg.fg(th.added_fg));
                    let pair = ci
                        .checked_sub(1)
                        .map(|p| &changes[p])
                        .filter(|p| p.tag() == ChangeTag::Delete)
                        .filter(|_| {
                            changes
                                .get(ci + 1)
                                .is_none_or(|n| n.tag() != ChangeTag::Insert)
                                && ci
                                    .checked_sub(2)
                                    .is_none_or(|p| changes[p].tag() != ChangeTag::Delete)
                        });
                    let marks = pair
                        .map(|p| changed_ranges(p.value().trim_end_matches('\n'), &text))
                        .unwrap_or_default();
                    let mut at = 0;
                    for (s, slot) in hl.line(&text) {
                        let fg = bg.fg(th.code[slot]);
                        row.0
                            .extend(split_marks(&s, at, &marks, fg, fg.bg(th.added_word)));
                        at += s.len();
                    }
                }
            }
            if ch.tag() != ChangeTag::Equal && indent > 0 {
                let fill = w.saturating_sub(7).saturating_sub(row.width());
                let bg = Style::new().bg(if ch.tag() == ChangeTag::Insert {
                    th.added_bg
                } else {
                    th.removed_bg
                });
                row.push(" ".repeat(fill), bg);
            }
            rows.push(row);
        }
    }
    Rendered {
        rows,
        added,
        removed,
    }
}

fn split_marks(s: &str, at: usize, marks: &[(usize, usize)], base: Style, hit: Style) -> Vec<Span> {
    let mut out = Vec::new();
    let mut pos = 0;
    for &(a, b) in marks {
        let a = a.saturating_sub(at).min(s.len());
        let b = b.saturating_sub(at).min(s.len());
        if a >= b || a < pos || !s.is_char_boundary(a) || !s.is_char_boundary(b) {
            continue;
        }
        if a > pos {
            out.push((s[pos..a].to_string(), base));
        }
        out.push((s[a..b].to_string(), hit));
        pos = b;
    }
    if pos < s.len() {
        out.push((s[pos..].to_string(), base));
    }
    out
}

/// `Added 1 line, removed 1 line`, the counts in bold.
pub fn summary(r: &Rendered, new_file: bool, th: &Theme) -> Vec<Span> {
    let plural = |n: usize| if n == 1 { "line" } else { "lines" };
    let mut out: Vec<Span> = Vec::new();
    if new_file {
        out.push(("Wrote ".into(), Style::new()));
        out.push((r.added.to_string(), th.bold()));
        out.push((format!(" {}", plural(r.added)), Style::new()));
        return out;
    }
    if r.added > 0 {
        out.push(("Added ".into(), Style::new()));
        out.push((r.added.to_string(), th.bold()));
        out.push((format!(" {}", plural(r.added)), Style::new()));
    }
    if r.removed > 0 {
        let lead = if out.is_empty() {
            "Removed "
        } else {
            ", removed "
        };
        out.push((lead.into(), Style::new()));
        out.push((r.removed.to_string(), th.bold()));
        out.push((format!(" {}", plural(r.removed)), Style::new()));
    }
    if out.is_empty() {
        out.push(("No changes".into(), Style::new()));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn marks_only_the_changed_word() {
        let old = "    println!(\"hello\");";
        let new = "    println!(\"hi\");";
        let r = changed_ranges(old, new);
        assert_eq!(r.len(), 1);
        assert_eq!(&new[r[0].0..r[0].1], "hi");
    }

    #[test]
    fn counts_added_and_removed_lines() {
        let d = FileDiff {
            path: "main.rs".into(),
            old: Some("fn main() {\n    println!(\"hello\");\n}\n".into()),
            new: "fn main() {\n    println!(\"hi\");\n}\n".into(),
        };
        let r = render(&Theme::dark(), &d, 5, 80);
        assert_eq!((r.added, r.removed), (1, 1));
        assert_eq!(r.rows.len(), 4);
        assert_eq!(
            r.rows[1].text().trim_end(),
            "      2 -    println!(\"hello\");"
        );
        assert_eq!(
            r.rows[1].width(),
            73,
            "a removed row is tinted to seven columns short of the edge"
        );
    }
}
