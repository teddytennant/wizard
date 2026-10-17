//! Diff rendering: unified and split, from `(old, new)` text or a unified patch.
//!
//! Colours follow opencode's `<diff>` props: added/removed/context backgrounds, a line number
//! gutter with its own backgrounds, a `+`/`-` sign in the highlight colour, and (when it is
//! cheap) a stronger background on the words that changed inside a replaced line.

use crate::syntax;
use crate::theme::Theme;
use crate::width::{clip_spans, sanitize, spans_width, wrap_spans, WrapMode};
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use similar::{ChangeTag, TextDiff};
use std::ops::Range;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum DiffView {
    #[default]
    Unified,
    Split,
}

impl DiffView {
    /// opencode's "auto": side by side once the transcript is wider than 120 columns.
    pub fn auto(width: u16) -> Self {
        if width > 120 {
            DiffView::Split
        } else {
            DiffView::Unified
        }
    }
}

#[derive(Clone, Debug)]
pub struct DiffOptions {
    pub view: DiffView,
    pub line_numbers: bool,
    /// Lines of context around each change (text input only; a patch carries its own).
    pub context: usize,
    /// Highlight changed words inside replaced lines.
    pub word_highlight: bool,
    pub wrap: bool,
    /// Fence info string or file extension for syntax colouring.
    pub lang: Option<String>,
    /// Draw `@@ -a,b +c,d @@` lines between hunks.
    pub hunk_headers: bool,
    /// Draw `--- a/x` / `+++ b/x` lines of a patch.
    pub file_headers: bool,
}

impl Default for DiffOptions {
    fn default() -> Self {
        Self {
            view: DiffView::Unified,
            line_numbers: true,
            context: 3,
            word_highlight: true,
            wrap: true,
            lang: None,
            hunk_headers: false,
            file_headers: false,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Ctx,
    Add,
    Del,
}

#[derive(Clone, Debug)]
struct Row {
    kind: Kind,
    old_no: Option<usize>,
    new_no: Option<usize>,
    text: String,
    emph: Vec<Range<usize>>,
}

#[derive(Clone, Debug)]
enum Item {
    Row(Row),
    Hunk(String),
    File(String),
}

fn clean(s: &str) -> String {
    sanitize(s.trim_end_matches(['\n', '\r'])).into_owned()
}

fn items_from_texts(old: &str, new: &str, context: usize) -> Vec<Item> {
    let diff = TextDiff::from_lines(old, new);
    let mut items = Vec::new();
    for group in diff.grouped_ops(context) {
        let (mut o0, mut n0, mut o1, mut n1) = (usize::MAX, usize::MAX, 0usize, 0usize);
        let mut rows = Vec::new();
        for op in &group {
            for ch in diff.iter_changes(op) {
                let kind = match ch.tag() {
                    ChangeTag::Equal => Kind::Ctx,
                    ChangeTag::Insert => Kind::Add,
                    ChangeTag::Delete => Kind::Del,
                };
                let old_no = ch.old_index().map(|i| i + 1);
                let new_no = ch.new_index().map(|i| i + 1);
                if let Some(o) = old_no {
                    o0 = o0.min(o);
                    o1 = o1.max(o);
                }
                if let Some(n) = new_no {
                    n0 = n0.min(n);
                    n1 = n1.max(n);
                }
                rows.push(Item::Row(Row {
                    kind,
                    old_no,
                    new_no,
                    text: clean(ch.value()),
                    emph: Vec::new(),
                }));
            }
        }
        let span = |a: usize, b: usize| {
            if a == usize::MAX {
                (0, 0)
            } else {
                (a, b - a + 1)
            }
        };
        let (os, ol) = span(o0, o1);
        let (ns, nl) = span(n0, n1);
        items.push(Item::Hunk(format!("@@ -{os},{ol} +{ns},{nl} @@")));
        items.extend(rows);
    }
    items
}

fn parse_hunk_header(line: &str) -> Option<(usize, Option<usize>, usize, Option<usize>)> {
    let rest = line.strip_prefix("@@ -")?;
    let (old, rest) = rest.split_once(" +")?;
    let (new, _) = rest.split_once(" @@").or_else(|| rest.split_once(" @"))?;
    let pair = |s: &str| -> Option<(usize, Option<usize>)> {
        match s.split_once(',') {
            Some((a, b)) => Some((a.parse().ok()?, Some(b.parse().ok()?))),
            None => Some((s.parse().ok()?, Some(1))),
        }
    };
    let (os, ol) = pair(old)?;
    let (ns, nl) = pair(new)?;
    Some((os, ol, ns, nl))
}

fn items_from_patch(patch: &str) -> Vec<Item> {
    let mut items = Vec::new();
    // (old line, new line, old remaining, new remaining) while inside a hunk
    let mut cur: Option<(usize, usize, Option<usize>, Option<usize>)> = None;
    for raw in patch.lines() {
        let raw = raw.strip_suffix('\r').unwrap_or(raw);
        if let Some((o, n, orem, nrem)) = cur.as_mut() {
            let done = *orem == Some(0) && *nrem == Some(0);
            if !done && !raw.starts_with("@@") {
                let (kind, body) = match raw.chars().next() {
                    Some('+') => (Kind::Add, &raw[1..]),
                    Some('-') => (Kind::Del, &raw[1..]),
                    Some(' ') => (Kind::Ctx, &raw[1..]),
                    Some('\\') => continue, // "\ No newline at end of file"
                    None => (Kind::Ctx, ""),
                    Some(_) => {
                        cur = None;
                        items.push(Item::File(clean(raw)));
                        continue;
                    }
                };
                let row = Row {
                    kind,
                    old_no: (kind != Kind::Add).then_some(*o),
                    new_no: (kind != Kind::Del).then_some(*n),
                    text: clean(body),
                    emph: Vec::new(),
                };
                if kind != Kind::Add {
                    *o += 1;
                    *orem = orem.map(|r| r.saturating_sub(1));
                }
                if kind != Kind::Del {
                    *n += 1;
                    *nrem = nrem.map(|r| r.saturating_sub(1));
                }
                items.push(Item::Row(row));
                continue;
            }
        }
        if let Some((os, ol, ns, nl)) = parse_hunk_header(raw) {
            cur = Some((os.max(1), ns.max(1), ol, nl));
            items.push(Item::Hunk(clean(raw)));
        } else {
            cur = None;
            items.push(Item::File(clean(raw)));
        }
    }
    items
}

/// Mark the changed words on paired del/add lines.
fn annotate_words(items: &mut [Item]) {
    let mut i = 0;
    while i < items.len() {
        let is = |it: &Item, k: Kind| matches!(it, Item::Row(r) if r.kind == k);
        if !is(&items[i], Kind::Del) {
            i += 1;
            continue;
        }
        let ds = i;
        while i < items.len() && is(&items[i], Kind::Del) {
            i += 1;
        }
        let de = i;
        while i < items.len() && is(&items[i], Kind::Add) {
            i += 1;
        }
        let ae = i;
        let as_ = de;
        for k in 0..(de - ds).min(ae - as_) {
            let (a, b) = {
                let (Item::Row(a), Item::Row(b)) = (&items[ds + k], &items[as_ + k]) else {
                    continue;
                };
                (a.text.clone(), b.text.clone())
            };
            if a.len() > 2000 || b.len() > 2000 {
                continue;
            }
            let (ta, tb) = (tokens(&a), tokens(&b));
            let td = TextDiff::from_slices(&ta, &tb);
            // Wholly different lines would just light up everything.
            if td.ratio() < 0.35 {
                continue;
            }
            let (mut ra, mut rb) = (Vec::new(), Vec::new());
            let (mut pa, mut pb) = (0usize, 0usize);
            for ch in td.iter_all_changes() {
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
            if let Item::Row(r) = &mut items[ds + k] {
                r.emph = ra;
            }
            if let Item::Row(r) = &mut items[as_ + k] {
                r.emph = rb;
            }
        }
    }
}

/// Split into identifier runs, whitespace runs, and single punctuation characters, so
/// `foo(1);` against `bar(1);` differs in `foo`/`bar` only.
fn tokens(s: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = 0;
    let class = |c: char| {
        if c.is_alphanumeric() || c == '_' {
            1
        } else if c.is_whitespace() {
            2
        } else {
            0
        }
    };
    let mut prev: Option<(usize, i8)> = None;
    for (i, c) in s.char_indices() {
        let k = class(c);
        match prev {
            Some((_, pk)) if pk == k && k != 0 => {}
            Some(_) => {
                out.push(&s[start..i]);
                start = i;
            }
            None => {}
        }
        prev = Some((i, k));
    }
    if start < s.len() {
        out.push(&s[start..]);
    }
    out
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

/// Per-row colouring: base spans (syntax or plain) with the emphasised ranges re-backgrounded.
fn colour_row(
    row: &Row,
    syn: Option<&Vec<Span<'static>>>,
    theme: &Theme,
    bg: Color,
    emph_bg: Color,
) -> Vec<Span<'static>> {
    let base: Vec<Span<'static>> = match syn {
        Some(s) if !s.is_empty() => s.clone(),
        _ if row.text.is_empty() => Vec::new(),
        _ => vec![Span::styled(row.text.clone(), Style::new().fg(theme.text))],
    };
    let mut out: Vec<Span<'static>> = Vec::new();
    let mut off = 0usize;
    for sp in base {
        let len = sp.content.len();
        let (s, e) = (off, off + len);
        off = e;
        // Cut the span at every emphasis boundary inside it.
        let mut cuts = vec![0usize];
        for r in &row.emph {
            for p in [r.start, r.end] {
                if p > s && p < e {
                    cuts.push(p - s);
                }
            }
        }
        cuts.push(len);
        cuts.sort_unstable();
        cuts.dedup();
        for w in cuts.windows(2) {
            let (a, b) = (w[0], w[1]);
            if a == b || !sp.content.is_char_boundary(a) || !sp.content.is_char_boundary(b) {
                continue;
            }
            let mid = s + a;
            let emph = row.emph.iter().any(|r| r.start <= mid && mid < r.end);
            let style = sp.style.bg(if emph { emph_bg } else { bg });
            out.push(Span::styled(sp.content[a..b].to_string(), style));
        }
    }
    out
}

struct Palette {
    add_bg: Color,
    del_bg: Color,
    ctx_bg: Color,
    add_emph: Color,
    del_emph: Color,
    add_num_bg: Color,
    del_num_bg: Color,
    num_fg: Color,
    add_sign: Color,
    del_sign: Color,
}

fn palette(t: &Theme) -> Palette {
    let base = t.base_bg();
    let blend = |hi: Color, bg: Color| {
        let bg = if bg == Color::Reset { base } else { bg };
        Theme::alpha_over(hi, bg, 0.30)
    };
    Palette {
        add_bg: t.diff_added_bg,
        del_bg: t.diff_removed_bg,
        ctx_bg: t.diff_context_bg,
        add_emph: blend(t.diff_highlight_added, t.diff_added_bg),
        del_emph: blend(t.diff_highlight_removed, t.diff_removed_bg),
        add_num_bg: t.diff_added_line_number_bg,
        del_num_bg: t.diff_removed_line_number_bg,
        num_fg: t.diff_line_number,
        add_sign: t.diff_highlight_added,
        del_sign: t.diff_highlight_removed,
    }
}

struct Ctx<'a> {
    theme: &'a Theme,
    pal: Palette,
    opts: &'a DiffOptions,
    num_w: usize,
}

impl Ctx<'_> {
    fn gutter_w(&self) -> usize {
        if self.num_w == 0 {
            0
        } else {
            self.num_w + 2
        }
    }

    fn bg_of(&self, k: Kind) -> Color {
        match k {
            Kind::Add => self.pal.add_bg,
            Kind::Del => self.pal.del_bg,
            Kind::Ctx => self.pal.ctx_bg,
        }
    }

    /// One pane worth of rows for `row` in exactly `pane_w` cells.
    fn pane(
        &self,
        row: &Row,
        no: Option<usize>,
        syn: Option<&Vec<Span<'static>>>,
        pane_w: usize,
    ) -> Vec<Vec<Span<'static>>> {
        let bg = self.bg_of(row.kind);
        let emph_bg = match row.kind {
            Kind::Add => self.pal.add_emph,
            Kind::Del => self.pal.del_emph,
            Kind::Ctx => bg,
        };
        let gw = self.gutter_w();
        let text_w = pane_w.saturating_sub(gw + 2).max(1);
        let spans = colour_row(row, syn, self.theme, bg, emph_bg);
        let rows = if self.opts.wrap {
            wrap_spans(&spans, text_w, WrapMode::Word)
        } else {
            vec![clip_spans(spans, text_w)]
        };
        let num_bg = match row.kind {
            Kind::Add => self.pal.add_num_bg,
            Kind::Del => self.pal.del_num_bg,
            Kind::Ctx => self.pal.ctx_bg,
        };
        let (sign, sign_fg) = match row.kind {
            Kind::Add => ("+", self.pal.add_sign),
            Kind::Del => ("-", self.pal.del_sign),
            Kind::Ctx => (" ", self.pal.num_fg),
        };
        rows.into_iter()
            .enumerate()
            .map(|(i, body)| {
                let mut line = Vec::new();
                if gw > 0 {
                    let n = if i == 0 {
                        no.map(|n| n.to_string()).unwrap_or_default()
                    } else {
                        String::new()
                    };
                    let txt = format!(" {n:>w$} ", w = self.num_w);
                    line.push(Span::styled(
                        txt,
                        Style::new().fg(self.pal.num_fg).bg(num_bg),
                    ));
                }
                let s = if i == 0 { sign } else { " " };
                // The sign and the pad after it belong to the gutter and share its background.
                let sign_bg = if gw > 0 { num_bg } else { bg };
                line.push(Span::styled(
                    s.to_string(),
                    Style::new().fg(sign_fg).bg(sign_bg),
                ));
                line.push(Span::styled(" ", Style::new().bg(sign_bg)));
                let used = spans_width(&body);
                line.extend(body);
                line.push(Span::styled(
                    " ".repeat(text_w.saturating_sub(used)),
                    Style::new().bg(bg),
                ));
                clip_spans(line, pane_w)
            })
            .collect()
    }

    fn blank_pane(&self, pane_w: usize) -> Vec<Span<'static>> {
        vec![Span::styled(
            " ".repeat(pane_w),
            Style::new().bg(self.pal.ctx_bg),
        )]
    }

    fn header(&self, text: &str, width: usize, file: bool) -> Line<'static> {
        let fg = if file {
            self.theme.text_muted
        } else {
            self.theme.diff_hunk_header
        };
        let style = Style::new().fg(fg).bg(self.pal.ctx_bg);
        let t = crate::width::truncate(text, width);
        let pad_n = width.saturating_sub(crate::width::display_width(&t));
        Line::from(vec![
            Span::styled(t, style),
            Span::styled(" ".repeat(pad_n), Style::new().bg(self.pal.ctx_bg)),
        ])
    }
}

/// Highlight each hunk as two streams (old side, new side) so multi-line tokens survive.
fn highlight_hunks(
    items: &[Item],
    lang: &Option<String>,
    theme: &Theme,
) -> Vec<Option<Vec<Span<'static>>>> {
    let mut out: Vec<Option<Vec<Span<'static>>>> = vec![None; items.len()];
    let Some(syn) = lang
        .as_deref()
        .and_then(|l| syntax::find_syntax(l).or_else(|| syntax::find_syntax_for_path(l)))
    else {
        return out;
    };
    let mut start = 0;
    while start < items.len() {
        let mut end = start + 1;
        while end < items.len() && !matches!(items[end], Item::Hunk(_) | Item::File(_)) {
            end += 1;
        }
        let (mut old_idx, mut new_idx) = (Vec::new(), Vec::new());
        let (mut old_txt, mut new_txt) = (String::new(), String::new());
        for (i, it) in items.iter().enumerate().take(end).skip(start) {
            if let Item::Row(r) = it {
                if r.kind != Kind::Add {
                    old_idx.push(i);
                    old_txt.push_str(&r.text);
                    old_txt.push('\n');
                }
                if r.kind != Kind::Del {
                    new_idx.push(i);
                    new_txt.push_str(&r.text);
                    new_txt.push('\n');
                }
            }
        }
        // Context rows take the new-side colouring; deletions come from the old side.
        for (idx, txt) in [(old_idx, old_txt), (new_idx, new_txt)] {
            if idx.is_empty() {
                continue;
            }
            let lines = syntax::highlight(Some(syn), &txt, theme);
            for (k, i) in idx.into_iter().enumerate() {
                let is_ctx = matches!(&items[i], Item::Row(r) if r.kind == Kind::Ctx);
                if let Some(l) = lines.get(k) {
                    if !(is_ctx && out[i].is_some()) {
                        out[i] = Some(l.clone());
                    }
                }
            }
        }
        start = end;
    }
    out
}

fn render_items(
    mut items: Vec<Item>,
    width: u16,
    theme: &Theme,
    opts: &DiffOptions,
) -> Vec<Line<'static>> {
    if width == 0 {
        return Vec::new();
    }
    let width = width as usize;
    if opts.word_highlight {
        annotate_words(&mut items);
    }
    let syn = highlight_hunks(&items, &opts.lang, theme);
    let max_no = items
        .iter()
        .filter_map(|i| match i {
            Item::Row(r) => r.old_no.max(r.new_no),
            _ => None,
        })
        .max()
        .unwrap_or(0);
    let mut num_w = if opts.line_numbers {
        max_no.max(1).to_string().len()
    } else {
        0
    };
    // On very narrow screens the gutter would eat the text; drop it.
    if width < num_w + 2 + 2 + 4 {
        num_w = 0;
    }
    let cx = Ctx {
        theme,
        pal: palette(theme),
        opts,
        num_w,
    };
    let mut out: Vec<Line<'static>> = Vec::new();

    let split = opts.view == DiffView::Split && width >= 20;
    let (lw, rw) = (width / 2, width - width / 2);
    let mut i = 0;
    while i < items.len() {
        match &items[i] {
            Item::Hunk(h) => {
                if opts.hunk_headers {
                    out.push(cx.header(h, width, false));
                }
                i += 1;
            }
            Item::File(f) => {
                if opts.file_headers {
                    out.push(cx.header(f, width, true));
                }
                i += 1;
            }
            Item::Row(r) if !split => {
                let no = if r.kind == Kind::Del {
                    r.old_no
                } else {
                    r.new_no
                };
                for l in cx.pane(r, no, syn[i].as_ref(), width) {
                    out.push(Line::from(l));
                }
                i += 1;
            }
            Item::Row(r) if r.kind == Kind::Ctx => {
                let l = cx.pane(r, r.old_no, syn[i].as_ref(), lw);
                let rr = cx.pane(r, r.new_no, syn[i].as_ref(), rw);
                push_split(&mut out, &cx, l, rr, lw, rw);
                i += 1;
            }
            Item::Row(_) => {
                // A run of deletions followed by additions: pair them side by side.
                let ds = i;
                while i < items.len() && matches!(&items[i], Item::Row(r) if r.kind == Kind::Del) {
                    i += 1;
                }
                let de = i;
                while i < items.len() && matches!(&items[i], Item::Row(r) if r.kind == Kind::Add) {
                    i += 1;
                }
                let n = (de - ds).max(i - de);
                for k in 0..n {
                    let get = |idx: usize, lo: usize, hi: usize| -> Option<(usize, &Row)> {
                        (lo + idx < hi).then(|| match &items[lo + idx] {
                            Item::Row(r) => (lo + idx, r),
                            _ => unreachable!(),
                        })
                    };
                    let l = match get(k, ds, de) {
                        Some((ix, r)) => cx.pane(r, r.old_no, syn[ix].as_ref(), lw),
                        None => vec![cx.blank_pane(lw)],
                    };
                    let rr = match get(k, de, i) {
                        Some((ix, r)) => cx.pane(r, r.new_no, syn[ix].as_ref(), rw),
                        None => vec![cx.blank_pane(rw)],
                    };
                    push_split(&mut out, &cx, l, rr, lw, rw);
                }
            }
        }
    }
    out
}

fn push_split(
    out: &mut Vec<Line<'static>>,
    cx: &Ctx<'_>,
    mut left: Vec<Vec<Span<'static>>>,
    mut right: Vec<Vec<Span<'static>>>,
    lw: usize,
    rw: usize,
) {
    let h = left.len().max(right.len());
    // A shorter side continues its background down the extra rows.
    while left.len() < h {
        left.push(cx.blank_pane(lw));
    }
    while right.len() < h {
        right.push(cx.blank_pane(rw));
    }
    for (mut l, r) in left.into_iter().zip(right) {
        l.extend(r);
        out.push(Line::from(l));
    }
}

/// Render the difference between two texts.
pub fn from_texts(
    old: &str,
    new: &str,
    width: u16,
    theme: &Theme,
    opts: &DiffOptions,
) -> Vec<Line<'static>> {
    render_items(items_from_texts(old, new, opts.context), width, theme, opts)
}

/// Render a unified patch (`@@` hunks, optional file headers).
pub fn from_patch(
    patch: &str,
    width: u16,
    theme: &Theme,
    opts: &DiffOptions,
) -> Vec<Line<'static>> {
    render_items(items_from_patch(patch), width, theme, opts)
}

/// `(added, removed)` line counts between two texts.
pub fn count(old: &str, new: &str) -> (usize, usize) {
    let d = TextDiff::from_lines(old, new);
    let (mut a, mut r) = (0, 0);
    for c in d.iter_all_changes() {
        match c.tag() {
            ChangeTag::Insert => a += 1,
            ChangeTag::Delete => r += 1,
            ChangeTag::Equal => {}
        }
    }
    (a, r)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::width::display_width;

    fn theme() -> Theme {
        Theme::builtin("opencode").unwrap()
    }

    fn text(lines: &[Line<'_>]) -> Vec<String> {
        lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect()
    }

    const OLD: &str = "one\ntwo\nthree\nfour\nfive\n";
    const NEW: &str = "one\n2\nthree\nfour\nfive\nsix\n";

    #[test]
    fn unified_shows_numbers_signs_and_context() {
        let t = theme();
        let out = text(&from_texts(OLD, NEW, 40, &t, &DiffOptions::default()));
        assert_eq!(
            out,
            vec![
                " 1   one",
                " 2 - two",
                " 2 + 2",
                " 3   three",
                " 4   four",
                " 5   five",
                " 6 + six",
            ]
        );
    }

    #[test]
    fn rows_use_the_theme_backgrounds_and_fill_the_width() {
        let t = theme();
        let lines = from_texts(
            OLD,
            NEW,
            40,
            &t,
            &DiffOptions {
                word_highlight: false,
                ..Default::default()
            },
        );
        for l in &lines {
            assert_eq!(spans_width(&l.spans), 40);
        }
        // line 2 is the removal, line 3 the addition
        let del = &lines[1];
        assert!(del
            .spans
            .iter()
            .any(|s| s.style.bg == Some(t.diff_removed_bg)));
        assert_eq!(del.spans[1].content, "-");
        assert_eq!(del.spans[1].style.fg, Some(t.diff_highlight_removed));
        assert_eq!(del.spans[0].style.bg, Some(t.diff_removed_line_number_bg));
        let add = &lines[2];
        assert!(add
            .spans
            .iter()
            .any(|s| s.style.bg == Some(t.diff_added_bg)));
        assert_eq!(lines[0].spans[0].style.bg, Some(t.diff_context_bg));
    }

    #[test]
    fn context_option_trims_unchanged_lines() {
        let t = theme();
        let o = DiffOptions {
            context: 0,
            ..Default::default()
        };
        let out = text(&from_texts(OLD, NEW, 40, &t, &o));
        assert_eq!(out.len(), 3);
        assert!(out[0].contains("- two") && out[1].contains("+ 2") && out[2].contains("+ six"));
    }

    #[test]
    fn identical_inputs_render_nothing() {
        assert!(from_texts("a\nb\n", "a\nb\n", 40, &theme(), &DiffOptions::default()).is_empty());
        assert_eq!(count("a\nb\n", "a\nc\nd\n"), (2, 1));
    }

    #[test]
    fn changed_words_get_a_stronger_background() {
        let t = theme();
        let lines = from_texts(
            "let x = foo(1);\n",
            "let x = bar(1);\n",
            50,
            &t,
            &DiffOptions::default(),
        );
        let add = lines
            .iter()
            .find(|l| l.spans.iter().any(|s| s.style.bg == Some(t.diff_added_bg)))
            .unwrap();
        let emph: Vec<&str> = add
            .spans
            .iter()
            .filter(|s| {
                s.style.bg.is_some()
                    && s.style.bg != Some(t.diff_added_bg)
                    && s.style.bg != Some(t.diff_added_line_number_bg)
                    && !s.content.trim().is_empty()
            })
            .map(|s| s.content.as_ref())
            .collect();
        assert_eq!(emph, vec!["bar"]);
        // and it is a different colour from the line background
        let strong = add
            .spans
            .iter()
            .find(|s| s.content == "bar")
            .unwrap()
            .style
            .bg;
        assert_ne!(strong, Some(t.diff_added_bg));
    }

    #[test]
    fn totally_different_lines_are_not_word_highlighted() {
        let t = theme();
        let lines = from_texts(
            "aaaa bbbb\n",
            "xxxx yyyy\n",
            40,
            &t,
            &DiffOptions::default(),
        );
        for l in &lines {
            assert!(l.spans.iter().all(|s| s.style.bg.is_none()
                || s.style.bg == Some(t.diff_added_bg)
                || s.style.bg == Some(t.diff_removed_bg)
                || s.style.bg == Some(t.diff_added_line_number_bg)
                || s.style.bg == Some(t.diff_removed_line_number_bg)));
        }
    }

    #[test]
    fn long_lines_wrap_with_a_blank_gutter() {
        let t = theme();
        let o = DiffOptions {
            word_highlight: false,
            ..Default::default()
        };
        let lines = from_texts("", "aaaa bbbb cccc dddd\n", 16, &t, &o);
        let out = text(&lines);
        assert_eq!(out.len(), 2, "{out:?}");
        assert!(out[0].starts_with(" 1 + "));
        assert!(out[1].starts_with("     "));
        for l in &lines {
            assert_eq!(spans_width(&l.spans), 16);
        }
    }

    #[test]
    fn split_view_pairs_deletions_with_additions() {
        let t = theme();
        let o = DiffOptions {
            view: DiffView::Split,
            word_highlight: false,
            ..Default::default()
        };
        let lines = from_texts(OLD, NEW, 60, &t, &o);
        let out = text(&lines);
        assert_eq!(out[0], format!(" 1   one{} 1   one", " ".repeat(22)));
        assert_eq!(out[1], format!(" 2 - two{} 2 + 2", " ".repeat(22)));
        // the trailing addition has nothing opposite it
        assert_eq!(out.last().unwrap().trim(), "6 + six");
        assert!(out.last().unwrap().starts_with("  "));
        for l in &lines {
            assert_eq!(spans_width(&l.spans), 60);
        }
    }

    #[test]
    fn split_view_rows_align_when_one_side_wraps() {
        let t = theme();
        let o = DiffOptions {
            view: DiffView::Split,
            word_highlight: false,
            ..Default::default()
        };
        let lines = from_texts(
            "short\n",
            "a much much longer replacement line here\n",
            40,
            &t,
            &o,
        );
        assert!(lines.len() >= 2);
        for l in &lines {
            assert_eq!(spans_width(&l.spans), 40);
        }
    }

    #[test]
    fn patch_input_matches_text_input() {
        let t = theme();
        let patch = "--- a/f.txt\n+++ b/f.txt\n@@ -1,5 +1,6 @@\n one\n-two\n+2\n three\n four\n five\n+six\n";
        let o = DiffOptions {
            word_highlight: false,
            ..Default::default()
        };
        let from_p = text(&from_patch(patch, 40, &t, &o));
        let from_t = text(&from_texts(OLD, NEW, 40, &t, &o));
        assert_eq!(from_p, from_t);
    }

    #[test]
    fn patch_headers_are_optional_and_multiple_files_work() {
        let t = theme();
        let patch = "diff --git a/a b/a\n--- a/a\n+++ b/a\n@@ -1 +1 @@\n-x\n+y\ndiff --git a/b b/b\n--- a/b\n+++ b/b\n@@ -3,2 +3,2 @@\n k\n-m\n+n\n";
        let plain = text(&from_patch(
            patch,
            30,
            &t,
            &DiffOptions {
                word_highlight: false,
                ..Default::default()
            },
        ));
        assert_eq!(plain.len(), 5);
        assert!(
            plain[2].contains("3") && plain[2].ends_with("k"),
            "{plain:?}"
        );
        let o = DiffOptions {
            hunk_headers: true,
            file_headers: true,
            ..Default::default()
        };
        let full = text(&from_patch(patch, 30, &t, &o));
        assert!(full.iter().any(|l| l.starts_with("@@ -1 +1 @@")));
        assert!(full.iter().any(|l| l.starts_with("+++ b/b")));
    }

    #[test]
    fn dashes_inside_a_hunk_are_deletions_not_headers() {
        let t = theme();
        // "-- comment" removed: the leading '-' is the diff marker, not "---"
        let patch = "@@ -1,2 +1,1 @@\n--- comment\n keep\n";
        let out = text(&from_patch(
            patch,
            30,
            &t,
            &DiffOptions {
                word_highlight: false,
                ..Default::default()
            },
        ));
        assert_eq!(out.len(), 2);
        assert!(out[0].contains("- -- comment"));
    }

    #[test]
    fn smoke_no_newline_marker_and_garbage_do_not_panic() {
        let t = theme();
        let patch = "@@ -1 +1 @@\n-a\n\\ No newline at end of file\n+b\n\\ No newline at end of file\nrandom trailing text\n@@ nonsense\n";
        let _ = from_patch(patch, 30, &t, &DiffOptions::default());
        let _ = from_patch("", 30, &t, &DiffOptions::default());
        let _ = from_patch("\n\n\n", 30, &t, &DiffOptions::default());
    }

    #[test]
    fn syntax_colours_come_from_theme_tokens() {
        let t = theme();
        let o = DiffOptions {
            lang: Some("rust".into()),
            word_highlight: false,
            ..Default::default()
        };
        let lines = from_texts("fn a() {}\n", "fn b() {}\n", 40, &t, &o);
        let kw = lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .find(|s| s.content == "fn")
            .unwrap();
        assert_eq!(kw.style.fg, Some(t.syntax_function));
    }

    #[test]
    fn wide_chars_tabs_and_crlf_keep_rows_inside_the_width() {
        let t = theme();
        for w in [10u16, 24, 61] {
            for view in [DiffView::Unified, DiffView::Split] {
                let o = DiffOptions {
                    view,
                    ..Default::default()
                };
                let lines = from_texts(
                    "\t日本語 old\r\n",
                    "\t日本語のテキスト new e\u{301}\r\n",
                    w,
                    &t,
                    &o,
                );
                for l in &lines {
                    assert!(
                        spans_width(&l.spans) <= w as usize,
                        "{view:?} w={w}: {} wide",
                        spans_width(&l.spans)
                    );
                }
            }
        }
    }

    #[test]
    fn tiny_and_zero_widths_survive() {
        let t = theme();
        assert!(from_texts(OLD, NEW, 0, &t, &DiffOptions::default()).is_empty());
        for w in 1..8 {
            for view in [DiffView::Unified, DiffView::Split] {
                let o = DiffOptions {
                    view,
                    ..Default::default()
                };
                let _ = from_texts(OLD, NEW, w, &t, &o);
            }
        }
    }

    #[test]
    fn auto_view_switches_past_120_columns() {
        assert_eq!(DiffView::auto(120), DiffView::Unified);
        assert_eq!(DiffView::auto(121), DiffView::Split);
    }

    #[test]
    fn no_line_numbers_option_removes_the_gutter() {
        let t = theme();
        let o = DiffOptions {
            line_numbers: false,
            word_highlight: false,
            ..Default::default()
        };
        let out = text(&from_texts("a\n", "b\n", 20, &t, &o));
        assert_eq!(out, vec!["- a", "+ b"]);
        assert!(display_width(&out[0]) < 20);
    }
}
