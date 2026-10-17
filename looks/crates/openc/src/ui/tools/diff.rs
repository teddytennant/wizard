//! Diff data shared by tool cards, the permission panel and the full diff viewer: a diff of
//! one edit with line numbers, word-level emphasis and syntax colour.

use std::ops::Range;

use ratatui::style::{Modifier, Style};
use ratatui::text::Span;
use similar::{ChangeTag, TextDiff};

use crate::ui::row::{sp, spaces, Cx, Row};

// ---- diffs -------------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DKind {
    Ctx,
    Add,
    Del,
    /// `╌ n unchanged lines`
    Gap(usize),
}

#[derive(Clone, Debug)]
pub struct DRow {
    pub kind: DKind,
    pub no: usize,
    pub text: String,
    pub emph: Vec<Range<usize>>,
}

#[derive(Clone, Debug, Default)]
pub struct DiffData {
    pub rows: Vec<DRow>,
    pub added: usize,
    pub removed: usize,
    pub path: String,
    /// Line numbers and context came from the file on disk.
    pub from_disk: bool,
    pub new_file: bool,
    /// The line numbers are real: the hunk was found in the file, or the diff covers a whole
    /// file. A hunk that cannot be placed has no numbers rather than wrong ones.
    pub numbered: bool,
}

/// How a diff is cut.
#[derive(Clone, Copy, Debug)]
pub struct DiffOpts {
    /// Unchanged lines kept around each change.
    pub context: usize,
    /// `old` and `new` are whole files (a `Write`), so numbering from 1 is always right.
    pub full: bool,
}

impl Default for DiffOpts {
    fn default() -> Self {
        DiffOpts {
            context: 2,
            full: false,
        }
    }
}

const MAX_FILE: u64 = 2_000_000;

fn find_lines(hay: &str, needle: &str) -> Option<usize> {
    if needle.is_empty() {
        return None;
    }
    hay.find(needle).map(|i| hay[..i].matches('\n').count())
}

impl DiffData {
    /// `old` is `None` for a new file. `applied` says the file on disk already has `new`
    /// (a finished edit); before that (a permission prompt) the old text is what is there.
    pub fn build(old: Option<&str>, new: &str, path: &str, applied: bool) -> DiffData {
        Self::build_with(old, new, path, applied, DiffOpts::default())
    }

    pub fn build_with(
        old: Option<&str>,
        new: &str,
        path: &str,
        applied: bool,
        opts: DiffOpts,
    ) -> DiffData {
        let context = opts.context;
        let mut d = DiffData {
            path: path.to_string(),
            new_file: old.is_none(),
            numbered: old.is_none() || opts.full,
            ..DiffData::default()
        };
        let Some(old) = old else {
            for (i, l) in new.lines().enumerate() {
                d.rows.push(DRow {
                    kind: DKind::Add,
                    no: i + 1,
                    text: l.to_string(),
                    emph: Vec::new(),
                });
            }
            d.added = d.rows.len();
            return d;
        };
        let mut base = 1;
        let (mut before, mut after) = (String::new(), String::new());
        let file = std::fs::metadata(path)
            .ok()
            .filter(|m| m.is_file() && m.len() <= MAX_FILE)
            .and_then(|_| std::fs::read_to_string(path).ok());
        if let Some(text) = file {
            let hit = if applied {
                find_lines(&text, new).map(|n| (n, new))
            } else {
                None
            }
            .or_else(|| find_lines(&text, old).map(|n| (n, old)))
            .or_else(|| find_lines(&text, new).map(|n| (n, new)));
            if let Some((n0, found)) = hit {
                let lines: Vec<&str> = text.lines().collect();
                let first = n0;
                let last = (n0 + found.lines().count().max(1)).min(lines.len());
                let b0 = first.saturating_sub(context);
                before = lines[b0..first].iter().map(|l| format!("{l}\n")).collect();
                after = lines[last..(last + context).min(lines.len())]
                    .iter()
                    .map(|l| format!("{l}\n"))
                    .collect();
                base = b0 + 1;
                d.from_disk = true;
                d.numbered = true;
            }
        }
        let o = format!(
            "{before}{}{}{after}",
            old,
            if old.ends_with('\n') || old.is_empty() {
                ""
            } else {
                "\n"
            }
        );
        let n = format!(
            "{before}{}{}{after}",
            new,
            if new.ends_with('\n') || new.is_empty() {
                ""
            } else {
                "\n"
            }
        );
        let diff = TextDiff::from_lines(&o, &n);
        let groups = diff.grouped_ops(context);
        let mut prev_end: Option<usize> = None;
        for g in &groups {
            let first_old = g.first().map_or(0, |op| op.old_range().start);
            if let Some(pe) = prev_end {
                let gap = first_old.saturating_sub(pe);
                if gap > 0 {
                    d.rows.push(DRow {
                        kind: DKind::Gap(gap),
                        no: 0,
                        text: String::new(),
                        emph: Vec::new(),
                    });
                }
            }
            if let Some(op) = g.last() {
                prev_end = Some(op.old_range().end);
            }
            let start = d.rows.len();
            for op in g {
                for ch in diff.iter_changes(op) {
                    let text = ch.value().trim_end_matches(['\n', '\r']).to_string();
                    let (kind, idx) = match ch.tag() {
                        ChangeTag::Equal => (DKind::Ctx, ch.new_index()),
                        ChangeTag::Insert => (DKind::Add, ch.new_index()),
                        ChangeTag::Delete => (DKind::Del, ch.old_index()),
                    };
                    match kind {
                        DKind::Add => d.added += 1,
                        DKind::Del => d.removed += 1,
                        _ => {}
                    }
                    d.rows.push(DRow {
                        kind,
                        no: base + idx.unwrap_or(0),
                        text,
                        emph: Vec::new(),
                    });
                }
            }
            pair_emphasis(&mut d.rows[start..]);
        }
        d
    }

    pub fn changed(&self) -> usize {
        self.added + self.removed
    }
}

/// Mark the changed words of a run of deletions followed by the same number of additions.
fn pair_emphasis(rows: &mut [DRow]) {
    let mut i = 0;
    while i < rows.len() {
        if rows[i].kind != DKind::Del {
            i += 1;
            continue;
        }
        let dels = rows[i..]
            .iter()
            .take_while(|r| r.kind == DKind::Del)
            .count();
        let adds = rows[i + dels..]
            .iter()
            .take_while(|r| r.kind == DKind::Add)
            .count();
        if adds == dels {
            for k in 0..dels {
                let (a, b) = (rows[i + k].text.clone(), rows[i + dels + k].text.clone());
                let (ea, eb) = word_ranges(&a, &b);
                rows[i + k].emph = ea;
                rows[i + dels + k].emph = eb;
            }
        }
        i += dels + adds;
    }
}

/// Byte ranges of `a` and `b` that differ, at word granularity.
fn word_ranges(a: &str, b: &str) -> (Vec<Range<usize>>, Vec<Range<usize>>) {
    let d = TextDiff::from_unicode_words(a, b);
    let (mut ra, mut rb): (Vec<Range<usize>>, Vec<Range<usize>>) = (Vec::new(), Vec::new());
    let (mut pa, mut pb) = (0usize, 0usize);
    for ch in d.iter_all_changes() {
        let len = ch.value().len();
        match ch.tag() {
            ChangeTag::Equal => {
                pa += len;
                pb += len;
            }
            ChangeTag::Delete => {
                push_range(&mut ra, pa..pa + len);
                pa += len;
            }
            ChangeTag::Insert => {
                push_range(&mut rb, pb..pb + len);
                pb += len;
            }
        }
    }
    // When almost the whole line changed, emphasis carries no information.
    let covered =
        |r: &[Range<usize>], s: &str| r.iter().map(|x| x.len()).sum::<usize>() * 10 > s.len() * 8;
    if covered(&ra, a) && covered(&rb, b) {
        return (Vec::new(), Vec::new());
    }
    (ra, rb)
}

fn push_range(v: &mut Vec<Range<usize>>, r: Range<usize>) {
    if let Some(last) = v.last_mut() {
        if last.end == r.start {
            last.end = r.end;
            return;
        }
    }
    v.push(r);
}

/// Re-style the byte ranges of `spans` that fall inside `emph`.
fn apply_emphasis(
    spans: Vec<Span<'static>>,
    emph: &[Range<usize>],
    extra: Style,
    legible: &dyn Fn(Style) -> Style,
) -> Vec<Span<'static>> {
    if emph.is_empty() {
        return spans;
    }
    let mut out = Vec::new();
    let mut pos = 0usize;
    for s in spans {
        let text = s.content.as_ref();
        let (start, end) = (pos, pos + text.len());
        pos = end;
        let mut cuts: Vec<usize> = vec![0];
        for r in emph {
            for c in [r.start, r.end] {
                if c > start && c < end && text.is_char_boundary(c - start) {
                    cuts.push(c - start);
                }
            }
        }
        cuts.push(text.len());
        cuts.sort_unstable();
        cuts.dedup();
        for w in cuts.windows(2) {
            let piece = &text[w[0]..w[1]];
            let mid = start + w[0];
            let on = emph.iter().any(|r| mid >= r.start && mid < r.end);
            let st = if on {
                legible(s.style.patch(extra))
            } else {
                s.style
            };
            out.push(Span::styled(piece.to_string(), st));
        }
    }
    out
}

/// One syntax-coloured span list per row of `d`, in row order (gap rows are empty). Added and
/// context text go through the highlighter together and removed text separately, so each side
/// reads as code that exists.
pub fn highlight_rows(d: &DiffData, cx: &Cx) -> Vec<Vec<Span<'static>>> {
    let syn = tuikit::syntax::find_syntax_for_path(&d.path);
    let join = |keep: &dyn Fn(&DRow) -> bool| -> String {
        d.rows
            .iter()
            .filter(|r| !matches!(r.kind, DKind::Gap(_)) && keep(r))
            .map(|r| format!("{}\n", r.text))
            .collect()
    };
    let new_text = join(&|r| r.kind != DKind::Del);
    let old_text = join(&|r| r.kind == DKind::Del);
    let hl_new = tuikit::syntax::highlight(syn, &new_text, cx.theme);
    let hl_old = tuikit::syntax::highlight(syn, &old_text, cx.theme);
    let (mut ni, mut oi) = (0usize, 0usize);
    d.rows
        .iter()
        .map(|r| match r.kind {
            DKind::Gap(_) => Vec::new(),
            DKind::Del => {
                oi += 1;
                hl_old.get(oi - 1).cloned().unwrap_or_default()
            }
            _ => {
                ni += 1;
                hl_new.get(ni - 1).cloned().unwrap_or_default()
            }
        })
        .collect()
}

/// Width of the line-number column for `d`.
pub fn number_width(d: &DiffData) -> usize {
    d.rows
        .iter()
        .map(|r| r.no)
        .max()
        .unwrap_or(0)
        .to_string()
        .len()
        .max(3)
}

/// Rows of a diff at column offset 4: `no`, sign, code. A diff whose hunk could not be placed in
/// its file has no line numbers and so no number column either, which also gives the code its
/// width back (round 2 finding 5: at 44 columns the code started at column 10).
pub fn diff_rows(d: &DiffData, cx: &Cx, cap: usize) -> Vec<Row> {
    let p = cx.p;
    let hl = highlight_rows(d, cx);
    let num_w = if d.numbered { number_width(d) } else { 0 };
    let gutter = if d.numbered { num_w + 1 } else { 0 };
    let code_w = cx.width.saturating_sub(4 + gutter + 2 + 1).max(8);
    let mut out = Vec::new();
    for (r, spans) in d.rows.iter().zip(hl) {
        if out.len() >= cap {
            break;
        }
        if let DKind::Gap(n) = r.kind {
            let word = if n == 1 { "line" } else { "lines" };
            out.push(
                Row::new(vec![
                    spaces(4),
                    sp(format!("{} {n} unchanged {word}", cx.g.dashed), p.s_faint()),
                ])
                .bg(p.surface),
            );
            continue;
        }
        push_diff_row(&mut out, r, spans, cx, num_w, code_w, d.numbered);
    }
    out
}

/// Spans of `r`'s code with syntax colour and the changed words emphasised.
pub fn styled_code(r: &DRow, spans: Vec<Span<'static>>, cx: &Cx) -> Vec<Span<'static>> {
    let p = cx.p;
    let word_bg = match r.kind {
        DKind::Add => p.add_word,
        DKind::Del => p.del_word,
        _ => p.surface,
    };
    let spans = if spans.is_empty() && !r.text.is_empty() {
        vec![sp(r.text.clone(), p.s_text())]
    } else {
        spans
    };
    // A syntax colour that does not clear 4.5:1 on the word tint (a comma, a comment) is
    // replaced by `text` there (round 2 finding 17: 3.5:1 for a `,` in an added word).
    let legible = |st: Style| {
        let fg = st.fg.unwrap_or(p.text);
        if crate::palette::contrast(p.seen(fg), p.seen(word_bg)) < 4.5 {
            st.fg(p.text)
        } else {
            st
        }
    };
    apply_emphasis(
        spans,
        &r.emph,
        Style::new().bg(word_bg).add_modifier(Modifier::BOLD),
        &legible,
    )
}

/// Background, gutter colour and sign of a row kind.
pub fn row_look(
    kind: DKind,
    cx: &Cx,
) -> (ratatui::style::Color, ratatui::style::Color, &'static str) {
    let p = cx.p;
    match kind {
        DKind::Add => (p.add_bg, p.add_fg, "+"),
        DKind::Del => (p.del_bg, p.del_fg, "-"),
        _ => (p.surface, p.faint, " "),
    }
}

fn push_diff_row(
    out: &mut Vec<Row>,
    r: &DRow,
    spans: Vec<Span<'static>>,
    cx: &Cx,
    num_w: usize,
    code_w: usize,
    numbered: bool,
) {
    let p = cx.p;
    let (bg, fg, sign) = row_look(r.kind, cx);
    let spans = styled_code(r, spans, cx);
    let gutter_style = Style::new().fg(fg);
    let wrapped = if spans.is_empty() {
        vec![Vec::new()]
    } else {
        crate::ui::wrapcode::wrap_code(&spans, code_w)
    };
    for (i, line) in wrapped.into_iter().enumerate() {
        let mut row = vec![spaces(4)];
        if i == 0 {
            if numbered {
                row.push(sp(format!("{:>num_w$} ", r.no), gutter_style));
            }
            row.push(sp(sign, gutter_style));
            row.push(spaces(1));
        } else {
            if numbered {
                row.push(spaces(num_w + 1));
            }
            row.push(sp(cx.g.wrap, p.s_faint()));
            row.push(spaces(1));
        }
        row.extend(line);
        out.push(Row::new(row).bg(bg));
    }
}
