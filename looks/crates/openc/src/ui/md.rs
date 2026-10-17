// OWNER: transcript
//! Markdown to rows, with the design's look: no `#` characters, `•` bullets, rules-only tables,
//! code on `surface` with a faint language tag, soft-wrapped code with `↳`.
//!
//! [`MdState`] is the streaming half. A source that only grows is cut into chunks at blank
//! lines; closed chunks are rendered once and frozen, so a delta re-lays out only the tail
//! (design section e, "Streaming markdown without jitter"). [`visible`] decides how much of
//! the tail may be shown yet: a half-typed word or an unmatched `**` is held back so a row
//! that was drawn is never redrawn differently.

use pulldown_cmark::{Alignment, CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use ratatui::style::{Modifier, Style};
use ratatui::text::Span;
use tuikit::width::{clip_spans, spans_width, split_spans_on_newline, wrap_spans, WrapMode};

use super::wrapcode;

use super::links;
use super::row::{sp, spaces, Cx, Row};

/// Indent of prose inside the column.
pub const INDENT: usize = 2;

struct Cont {
    quote: bool,
    marker: Option<Vec<Span<'static>>>,
    cont: Vec<Span<'static>>,
    width: usize,
    /// `rows.len()` when the container opened, so a block that is the first thing inside it
    /// does not get a blank row of the container's own above it (a nested quote printed
    /// `▎ ▎ ▎` rows before each level, round 2 finding 13).
    start: usize,
}

struct Table {
    aligns: Vec<Alignment>,
    head: Vec<Vec<Span<'static>>>,
    rows: Vec<Vec<Vec<Span<'static>>>>,
    cur: Vec<Vec<Span<'static>>>,
    in_head: bool,
}

struct Renderer<'a> {
    cx: &'a Cx<'a>,
    rows: Vec<Row>,
    inline: Vec<Span<'static>>,
    styles: Vec<Style>,
    stack: Vec<Cont>,
    lists: Vec<Option<u64>>,
    code: Option<(String, String)>,
    table: Option<Table>,
    links: Vec<String>,
    /// `inline.len()` where each open link's text began.
    link_starts: Vec<usize>,
    /// True right after an item marker was set and before its first row: no blank row between
    /// the marker and its text in a tight list.
    item_fresh: bool,
    /// The text ends inside a code fence that is still open and on a line that is not finished:
    /// that line is laid out so that it only ever grows (`wrapcode::wrap_code_partial`).
    partial_tail: bool,
}

pub fn render(src: &str, cx: &Cx) -> Vec<Row> {
    render_with(src, cx, false)
}

/// Stands in for a single newline inside a paragraph until the line after it is known.
const SOFT_BREAK: &str = "\u{e000}";

/// A single newline is a line break only where a model or a person means one: before a list
/// marker, a table row, a quote or a heading mark, or after a line shorter than half the
/// column. A line that fills most of the column and runs on into the next is a hard wrap of
/// the source (pasted text, a model that wraps at 70), and breaking it again leaves ragged
/// rows at every width (round 2 finding 14); there the newline is a space. The deciding facts
/// are the length of the line before and the first character of the line after, both known
/// as soon as the line after has a character, so a row drawn once is never taken back: the
/// worst case is a row that gets text appended to it.
fn resolve_soft_breaks(spans: Vec<Span<'static>>, w: usize) -> Vec<Span<'static>> {
    if !spans.iter().any(|s| s.content.contains(SOFT_BREAK)) {
        return spans;
    }
    let mut out: Vec<Span<'static>> = Vec::with_capacity(spans.len());
    let mut line_w = 0usize;
    for (i, sp) in spans.iter().enumerate() {
        if sp.content != SOFT_BREAK {
            if let Some(pos) = sp.content.rfind('\n') {
                line_w = tuikit::width::display_width(&sp.content[pos + 1..]);
            } else {
                line_w += tuikit::width::display_width(&sp.content);
            }
            out.push(sp.clone());
            continue;
        }
        let next = spans[i + 1..]
            .iter()
            .flat_map(|s| s.content.chars())
            .find(|c| *c != '\u{e000}');
        let marker =
            next.is_some_and(|c| matches!(c, '-' | '*' | '+' | '|' | '>' | '#' | '0'..='9'));
        let short = line_w * 2 < w;
        let text = if marker || short { "\n" } else { " " };
        out.push(Span::styled(text, sp.style));
        line_w = 0;
    }
    out
}

/// [`render`] for the tail of an answer that is still arriving, `partial_tail` as on `Renderer`.
fn render_with(src: &str, cx: &Cx, partial_tail: bool) -> Vec<Row> {
    let mut r = Renderer {
        cx,
        rows: Vec::new(),
        inline: Vec::new(),
        styles: vec![cx.p.s_text()],
        stack: Vec::new(),
        lists: Vec::new(),
        code: None,
        table: None,
        links: Vec::new(),
        link_starts: Vec::new(),
        item_fresh: false,
        partial_tail,
    };
    let opts = Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS;
    // Tabs have no fixed width in a cell grid; four spaces keep layout and painting agreeing.
    let src = tuikit::width::normalize_newlines(src).replace('\t', "    ");
    let src = split_glued_tables(&src);
    for ev in Parser::new_ext(&src, opts) {
        r.event(ev);
    }
    r.flush();
    while r
        .rows
        .last()
        .is_some_and(|row| row.spans.is_empty() && row.bg.is_none())
    {
        r.rows.pop();
    }
    r.rows
}

impl Renderer<'_> {
    fn style(&self) -> Style {
        *self.styles.last().expect("base style")
    }

    fn push_style(&mut self, f: impl FnOnce(Style) -> Style) {
        let s = f(self.style());
        self.styles.push(s);
    }

    fn prefix_width(&self) -> usize {
        INDENT + self.stack.iter().map(|c| c.width).sum::<usize>()
    }

    fn avail(&self) -> usize {
        self.cx.width.saturating_sub(self.prefix_width()).max(8)
    }

    /// Leading spans of the next row, consuming item markers.
    fn prefix(&mut self) -> Vec<Span<'static>> {
        let mut out = vec![spaces(INDENT)];
        for c in &mut self.stack {
            match c.marker.take() {
                Some(m) => out.extend(m),
                None => out.extend(c.cont.iter().cloned()),
            }
        }
        out
    }

    /// Prefix for a blank row inside containers: quote bars continue, items are blank.
    fn blank_prefix(&self) -> Vec<Span<'static>> {
        let mut out = vec![spaces(INDENT)];
        for c in &self.stack {
            if c.quote {
                out.extend(c.cont.iter().cloned());
            } else {
                out.push(spaces(c.width));
            }
        }
        out
    }

    fn gap(&mut self) {
        if self.item_fresh {
            self.item_fresh = false;
            return;
        }
        // The first block inside a quote or an item has nothing above it to be apart from.
        if self
            .stack
            .last()
            .is_some_and(|c| c.quote && c.start == self.rows.len())
        {
            return;
        }
        if self
            .rows
            .last()
            .is_some_and(|r| r.spans.is_empty() && r.bg.is_none())
            || self.rows.is_empty()
        {
            return;
        }
        let spans = if self.stack.iter().any(|c| c.quote) {
            self.blank_prefix()
        } else {
            Vec::new()
        };
        self.rows.push(Row::new(spans));
    }

    fn flush(&mut self) {
        if self.inline.is_empty() {
            return;
        }
        let w = self.avail();
        let spans = resolve_soft_breaks(std::mem::take(&mut self.inline), w);
        for line in split_spans_on_newline(spans) {
            if line.is_empty() {
                let p = self.prefix();
                self.rows.push(Row::new(p));
                continue;
            }
            for wrapped in wrap_spans(&line, w, WrapMode::Word) {
                let mut p = self.prefix();
                p.extend(wrapped);
                self.rows.push(Row::new(p));
            }
        }
        self.item_fresh = false;
    }

    fn event(&mut self, ev: Event<'_>) {
        let p = self.cx.p;
        match ev {
            Event::Start(tag) => self.start(tag),
            Event::End(tag) => self.end(tag),
            Event::Text(t) => {
                if let Some((_, buf)) = self.code.as_mut() {
                    buf.push_str(&t);
                } else {
                    let t = tuikit::width::sanitize(&t).into_owned();
                    if self.links.is_empty() {
                        let url_style =
                            Style::new().fg(p.accent).add_modifier(Modifier::UNDERLINED);
                        self.inline
                            .extend(links::linkify(&t, self.style(), url_style));
                    } else {
                        self.inline.push(sp(t, self.style()));
                    }
                }
            }
            Event::Code(c) => {
                let c = tuikit::width::sanitize(&c).into_owned();
                let mut st = p.s_chip();
                if self.links.is_empty() {
                    if let Some(url) = links::target_of(c.trim()) {
                        st = links::with_link(st, &url);
                    }
                }
                self.inline.push(sp(c, st));
            }
            Event::Html(h) => {
                // A block of html is shown as its own lines, in dim.
                let st = p.s_dim();
                self.inline
                    .push(sp(h.trim_end_matches('\n').to_string(), st));
                if h.ends_with('\n') {
                    self.inline.push(sp("\n", st));
                }
            }
            Event::InlineHtml(h) => {
                self.inline
                    .push(sp(h.trim_end_matches('\n').to_string(), p.s_dim()));
            }
            // Model text puts a newline where it means one (a list, a table, a stanza), and
            // joining the lines made `| a | b |` rows one long bullet (finding 15).
            // Whether a single newline is a break or a space depends on the line after it, which
            // `flush` can see: see `resolve_soft_breaks`.
            Event::SoftBreak => self.inline.push(sp(SOFT_BREAK, self.style())),
            Event::HardBreak => self.inline.push(sp("\n", self.style())),
            Event::Rule => {
                self.flush();
                self.gap();
                let w = self.avail();
                let mut r = self.prefix();
                r.push(sp(self.cx.g.rule.repeat(w), p.s_line()));
                self.rows.push(Row::new(r));
            }
            // The glyphs the todo dock uses: `✓` done, `○` open. The bracket pair read as text.
            Event::TaskListMarker(done) => {
                let mark = if done {
                    sp(format!("{} ", self.cx.g.ok), p.s_ok())
                } else {
                    sp(format!("{} ", self.cx.g.pending), p.s_dim())
                };
                if let Some(c) = self.stack.last_mut() {
                    c.marker = Some(vec![mark]);
                    c.cont = vec![spaces(2)];
                    c.width = 2;
                }
            }
            Event::FootnoteReference(f) => self.inline.push(sp(format!("[{f}]"), p.s_dim())),
            _ => {}
        }
    }

    fn start(&mut self, tag: Tag<'_>) {
        let p = self.cx.p;
        match tag {
            Tag::Paragraph => {
                self.gap();
            }
            Tag::Heading { level, .. } => {
                self.flush();
                self.gap();
                // The louder colour goes to the higher level: H1 in `accent`, H2 bold `text`
                // (round 1 had it the other way, so `Findings` outshouted `Plan`).
                let st = match level {
                    HeadingLevel::H1 => p.s_accent(),
                    HeadingLevel::H2 => p.s_text(),
                    _ => p.s_dim(),
                };
                self.push_style(|_| st.add_modifier(Modifier::BOLD));
            }
            Tag::BlockQuote(_) => {
                self.flush();
                self.gap();
                self.stack.push(Cont {
                    quote: true,
                    marker: None,
                    cont: vec![sp(format!("{} ", self.cx.g.bar), p.s_faint())],
                    width: 2,
                    start: self.rows.len(),
                });
                self.push_style(|_| p.s_dim());
            }
            Tag::CodeBlock(kind) => {
                self.flush();
                self.gap();
                let lang = match kind {
                    CodeBlockKind::Fenced(l) => {
                        l.split_whitespace().next().unwrap_or("").to_string()
                    }
                    CodeBlockKind::Indented => String::new(),
                };
                self.code = Some((lang, String::new()));
            }
            Tag::List(start) => {
                self.flush();
                if self.stack.iter().all(|c| c.quote) {
                    self.gap();
                }
                self.lists.push(start);
            }
            Tag::Item => {
                self.flush();
                // Bullets change with the depth of the list so seven levels are not told apart by
                // indent alone.
                let depth = self.lists.len().saturating_sub(1);
                let bullet = self.cx.g.bullets[depth % self.cx.g.bullets.len()];
                let (marker, w) = match self.lists.last_mut() {
                    Some(Some(n)) => {
                        let m = format!("{n}. ");
                        *n += 1;
                        let w = m.chars().count();
                        (sp(m, p.s_dim()), w)
                    }
                    _ => (sp(format!("{bullet} "), p.s_dim()), 2),
                };
                self.stack.push(Cont {
                    quote: false,
                    marker: Some(vec![marker]),
                    cont: vec![spaces(w)],
                    width: w,
                    start: self.rows.len(),
                });
                self.item_fresh = true;
            }
            Tag::Table(aligns) => {
                self.flush();
                self.gap();
                self.table = Some(Table {
                    aligns,
                    head: Vec::new(),
                    rows: Vec::new(),
                    cur: Vec::new(),
                    in_head: false,
                });
            }
            Tag::TableHead => {
                if let Some(t) = self.table.as_mut() {
                    t.in_head = true;
                }
            }
            Tag::TableRow | Tag::TableCell => {}
            Tag::HtmlBlock => {
                self.flush();
                self.gap();
            }
            Tag::Emphasis => self.push_style(|s| s.add_modifier(Modifier::ITALIC)),
            Tag::Strong => self.push_style(|s| s.add_modifier(Modifier::BOLD)),
            Tag::Strikethrough => self.push_style(|s| s.add_modifier(Modifier::CROSSED_OUT)),
            Tag::Link { dest_url, .. } => {
                self.links.push(dest_url.to_string());
                self.link_starts.push(self.inline.len());
                // `file:`, `ssh:`, `javascript:` and other schemes are not links a reader of an
                // answer expects to be clickable.
                let target = if links::clickable_scheme(&dest_url) {
                    Some(dest_url.to_string())
                } else {
                    links::target_of(&dest_url)
                };
                self.push_style(|s| {
                    let s = s.fg(p.accent).add_modifier(Modifier::UNDERLINED);
                    match &target {
                        Some(t) => links::with_link(s, t),
                        None => s,
                    }
                });
            }
            Tag::Image { dest_url, .. } => {
                self.links.push(dest_url.to_string());
                self.inline.push(sp("[image: ", p.s_dim()));
            }
            _ => {}
        }
    }

    fn end(&mut self, tag: TagEnd) {
        let p = self.cx.p;
        match tag {
            TagEnd::Paragraph => self.flush(),
            TagEnd::Heading(_) => {
                self.flush();
                self.styles.pop();
            }
            TagEnd::BlockQuote(_) => {
                self.flush();
                self.stack.pop();
                self.styles.pop();
            }
            TagEnd::CodeBlock => {
                if let Some((lang, text)) = self.code.take() {
                    self.code_block(&lang, &text);
                }
            }
            TagEnd::List(_) => {
                self.flush();
                self.lists.pop();
            }
            TagEnd::Item => {
                self.flush();
                self.stack.pop();
                self.item_fresh = false;
            }
            TagEnd::Table => {
                if let Some(t) = self.table.take() {
                    self.finish_table(t);
                }
            }
            TagEnd::TableHead => {
                if let Some(t) = self.table.as_mut() {
                    t.head = std::mem::take(&mut t.cur);
                    t.in_head = false;
                }
            }
            TagEnd::TableRow => {
                if let Some(t) = self.table.as_mut() {
                    let row = std::mem::take(&mut t.cur);
                    t.rows.push(row);
                }
            }
            TagEnd::TableCell => {
                let cell = std::mem::take(&mut self.inline);
                if let Some(t) = self.table.as_mut() {
                    t.cur.push(cell);
                }
            }
            TagEnd::HtmlBlock => {
                if self.inline.last().is_some_and(|s| s.content == "\n") {
                    self.inline.pop();
                }
                self.flush();
            }
            TagEnd::Emphasis | TagEnd::Strong | TagEnd::Strikethrough => {
                self.styles.pop();
            }
            TagEnd::Link => {
                self.styles.pop();
                let url = self.links.pop().unwrap_or_default();
                let from = self.link_starts.pop().unwrap_or(self.inline.len());
                if self.cx.detail && !url.is_empty() {
                    self.inline.push(sp(format!(" ({url})"), p.s_dim()));
                } else if links::clickable_scheme(&url) {
                    // Text that is a URL pointing somewhere else is the usual disguise.
                    let text: String = self.inline[from.min(self.inline.len())..]
                        .iter()
                        .map(|s| s.content.as_ref())
                        .collect();
                    if let Some(host) = links::spoofed_host(&text, &url) {
                        self.inline.push(sp(format!(" ({host})"), p.s_dim()));
                    }
                }
            }
            TagEnd::Image => {
                let url = self.links.pop().unwrap_or_default();
                self.inline.push(sp(format!("] {url}"), p.s_dim()));
            }
            _ => {}
        }
    }

    fn code_block(&mut self, lang: &str, text: &str) {
        let cx = self.cx;
        let w = code_width(cx, self.prefix_width());
        // A fence that is still empty has no lines yet, so the first code line is appended
        // rather than written over a blank one.
        let lines = if text.is_empty() {
            Vec::new()
        } else {
            tuikit::syntax::highlight_token(lang, text, cx.theme)
        };
        self.rows.push(code_header(cx, lang));
        let partial_from = if self.partial_tail && !text.ends_with('\n') {
            lines.len().saturating_sub(1)
        } else {
            usize::MAX
        };
        for (i, line) in lines.into_iter().enumerate() {
            let mut rows = std::mem::take(&mut self.rows);
            code_line_rows(cx, &line, w, i >= partial_from, || self.prefix(), &mut rows);
            self.rows = rows;
        }
        self.rows.push(code_pad(cx));
    }

    fn finish_table(&mut self, t: Table) {
        let p = self.cx.p;
        let ncols = t
            .head
            .len()
            .max(t.rows.iter().map(Vec::len).max().unwrap_or(0));
        if ncols == 0 {
            return;
        }
        let bold = |spans: &[Span<'static>]| -> Vec<Span<'static>> {
            spans
                .iter()
                .map(|s| Span::styled(s.content.clone(), s.style.add_modifier(Modifier::BOLD)))
                .collect()
        };
        let head: Vec<Vec<Span<'static>>> = (0..ncols)
            .map(|i| t.head.get(i).map(|c| bold(c)).unwrap_or_default())
            .collect();
        let body: Vec<Vec<Vec<Span<'static>>>> = t
            .rows
            .iter()
            .map(|r| {
                (0..ncols)
                    .map(|i| r.get(i).cloned().unwrap_or_default())
                    .collect()
            })
            .collect();
        let mut nat = vec![1usize; ncols];
        for row in std::iter::once(&head).chain(body.iter()) {
            for (i, c) in row.iter().enumerate() {
                nat[i] = nat[i].max(spans_width(c));
            }
        }
        let avail = self.avail();
        let gaps = 2 * (ncols - 1);
        let mut widths = nat.clone();
        // Shrink the widest columns first until the table fits.
        while widths.iter().sum::<usize>() + gaps > avail {
            let (i, &w) = widths
                .iter()
                .enumerate()
                .max_by_key(|(_, w)| **w)
                .expect("ncols > 0");
            if w <= 1 {
                break;
            }
            widths[i] -= 1;
        }
        let shrunk_small = widths.iter().zip(&nat).any(|(w, n)| w < n && *w < 8);
        if shrunk_small || widths.iter().sum::<usize>() + gaps > avail {
            return self.stacked_table(&head, &body);
        }
        let align = |i: usize| t.aligns.get(i).copied().unwrap_or(Alignment::None);
        let emit = |this: &mut Self, cells: &[Vec<Span<'static>>]| {
            let wrapped: Vec<Vec<Vec<Span<'static>>>> = cells
                .iter()
                .enumerate()
                .map(|(i, c)| {
                    if c.is_empty() {
                        vec![Vec::new()]
                    } else {
                        wrap_spans(c, widths[i], WrapMode::Word)
                    }
                })
                .collect();
            let h = wrapped.iter().map(Vec::len).max().unwrap_or(1);
            for li in 0..h {
                let mut row = this.prefix();
                for (i, w) in wrapped.iter().enumerate() {
                    let line = w.get(li).cloned().unwrap_or_default();
                    let lw = spans_width(&line);
                    let pad = widths[i].saturating_sub(lw);
                    let (l, r) = match align(i) {
                        Alignment::Right => (pad, 0),
                        Alignment::Center => (pad / 2, pad - pad / 2),
                        _ => (0, pad),
                    };
                    row.push(spaces(l));
                    row.extend(line);
                    row.push(spaces(r));
                    if i + 1 < ncols {
                        row.push(spaces(2));
                    }
                }
                this.rows.push(Row::new(row));
            }
        };
        emit(self, &head);
        let total = widths.iter().sum::<usize>() + gaps;
        let mut rule = self.prefix();
        rule.push(sp(self.cx.g.rule.repeat(total), p.s_line()));
        self.rows.push(Row::new(rule));
        // A table whose rows all fit on one line needs no dividers, and a three-row table stays
        // at five rows. Once a cell wraps the eye can no longer tell where a row ends, so every
        // row gets a faint divider.
        let wraps = body.iter().any(|r| {
            r.iter()
                .enumerate()
                .any(|(i, c)| !c.is_empty() && spans_width(c) > widths[i])
        });
        for (k, r) in body.iter().enumerate() {
            if wraps && k > 0 {
                let mut div = self.prefix();
                div.push(sp(self.cx.g.dots.repeat(total), p.s_line()));
                self.rows.push(Row::new(div));
            }
            emit(self, r);
        }
    }

    /// Too narrow for columns: one `name: value` card per row.
    fn stacked_table(&mut self, head: &[Vec<Span<'static>>], body: &[Vec<Vec<Span<'static>>>]) {
        let p = self.cx.p;
        let names: Vec<String> = head
            .iter()
            .map(|c| c.iter().map(|s| s.content.as_ref()).collect::<String>())
            .collect();
        for (ri, r) in body.iter().enumerate() {
            if ri > 0 {
                self.rows.push(Row::new(Vec::new()));
            }
            for (i, cell) in r.iter().enumerate() {
                let mut spans = vec![sp(
                    format!("{}: ", names.get(i).cloned().unwrap_or_default()),
                    p.s_dim(),
                )];
                spans.extend(cell.iter().cloned());
                let w = self.avail();
                for wrapped in wrap_spans(&spans, w, WrapMode::Word) {
                    let mut row = self.prefix();
                    row.extend(wrapped);
                    self.rows.push(Row::new(row));
                }
            }
        }
    }
}

// ---- streaming ---------------------------------------------------------------------------

/// `|---|---|`, `| :-- | --: |` and the like: the row that makes the line above a table header.
fn is_table_delimiter(line: &str) -> bool {
    let t = line.trim();
    t.contains('-') && t.contains('|') && t.chars().all(|c| matches!(c, '|' | '-' | ':' | ' '))
}

/// A table whose header line follows a paragraph or a list item with no blank line between
/// them is, by CommonMark, a lazy continuation of that paragraph, so the pipes came out as
/// text. Models do this all the time; a blank line in front of the header makes it a table.
fn split_glued_tables(src: &str) -> std::borrow::Cow<'_, str> {
    if !src.contains('|') {
        return std::borrow::Cow::Borrowed(src);
    }
    let lines: Vec<&str> = src.lines().collect();
    let mut out = String::with_capacity(src.len() + 8);
    let mut in_fence = false;
    let mut changed = false;
    for (i, l) in lines.iter().enumerate() {
        if fence_of(l).is_some() {
            in_fence = !in_fence;
        }
        let starts_table = !in_fence
            && l.contains('|')
            && lines.get(i + 1).is_some_and(|n| is_table_delimiter(n))
            && i > 0
            && !lines[i - 1].trim().is_empty()
            && !lines[i - 1].trim_start().starts_with('|');
        if starts_table {
            out.push('\n');
            changed = true;
        }
        out.push_str(l);
        out.push('\n');
    }
    if !changed {
        return std::borrow::Cow::Borrowed(src);
    }
    if !src.ends_with('\n') {
        out.pop();
    }
    std::borrow::Cow::Owned(out)
}

/// Wrap one code line to `w` cells between tokens, with look-ahead so the last row is never a
/// lone `;`. A line that is still being written (`partial`) is laid out so that what is drawn
/// is only appended to as it grows and the finished line extends it (checklist 31); see
/// `wrapcode`.
fn wrap_code(line: &[Span<'static>], w: usize, partial: bool) -> Vec<Vec<Span<'static>>> {
    if partial {
        wrapcode::wrap_code_partial(line, w)
    } else {
        wrapcode::wrap_code(line, w)
    }
}

/// Width of the code inside a fence nested under `prefix_width` columns of indent.
fn code_width(cx: &Cx, prefix_width: usize) -> usize {
    cx.width
        .saturating_sub(prefix_width)
        .max(8)
        .saturating_sub(2)
        .max(4)
}

/// The padding row above a code block; it carries the language tag, top right.
fn code_header(cx: &Cx, lang: &str) -> Row {
    // The tag is the fence's info string: cap it so a long or wide one cannot overflow.
    let lang = tuikit::width::truncate(lang, 24);
    let tag_w = tuikit::width::display_width(&lang);
    let mut spans = vec![spaces(cx.width.saturating_sub(tag_w + 2))];
    if !lang.is_empty() {
        spans.push(sp(lang, cx.p.s_faint()));
    }
    Row::new(spans).bg(cx.p.surface)
}

fn code_pad(cx: &Cx) -> Row {
    Row::new(Vec::new()).bg(cx.p.surface)
}

/// The rows of one highlighted code line: soft-wrapped at `w`, each continuation marked.
fn code_line_rows(
    cx: &Cx,
    line: &[Span<'static>],
    w: usize,
    partial: bool,
    mut prefix: impl FnMut() -> Vec<Span<'static>>,
    out: &mut Vec<Row>,
) {
    let bg = cx.p.surface;
    if line.is_empty() {
        out.push(Row::new(prefix()).bg(bg));
        return;
    }
    for (i, wrapped) in wrap_code(line, w, partial).into_iter().enumerate() {
        let mut pre = prefix();
        if i > 0 {
            // The continuation marker sits in the padding with a cell of air before the code,
            // which stays aligned with the first row.
            let pw = spans_width(&pre);
            pre = clip_spans(pre, pw.saturating_sub(2));
            pre.push(sp(cx.g.wrap, cx.p.s_faint()));
            pre.push(spaces(1));
        }
        pre.extend(wrapped);
        out.push(Row::new(pre).bg(bg));
    }
}

fn is_list_marker(t: &str) -> bool {
    let t = t.trim_start();
    let mut it = t.chars();
    match it.next() {
        Some('-' | '*' | '+') => it.next().is_some_and(char::is_whitespace),
        Some(c) if c.is_ascii_digit() => {
            let rest = t.trim_start_matches(|c: char| c.is_ascii_digit());
            let mut r = rest.chars();
            matches!(r.next(), Some('.' | ')')) && r.next().is_some_and(char::is_whitespace)
        }
        _ => false,
    }
}

/// A line that is only the beginning of something that would make it a list item or a fence:
/// `2`, `2.`, `-`, `*`, `+`, one or two backticks or tildes.
fn marker_prefix(t: &str) -> bool {
    let t = t.trim();
    let digits = t.trim_end_matches(['.', ')']);
    (!digits.is_empty()
        && digits.bytes().all(|b| b.is_ascii_digit())
        && t.len() - digits.len() <= 1)
        || matches!(t, "-" | "*" | "+")
        || (t.len() <= 2 && (t.bytes().all(|b| b == b'`') || t.bytes().all(|b| b == b'~')))
}

fn fence_of(line: &str) -> Option<(char, usize)> {
    let t = line.trim_start();
    if line.len() - t.len() > 3 {
        return None;
    }
    let c = t.chars().next()?;
    if c != '`' && c != '~' {
        return None;
    }
    let n = t.chars().take_while(|x| *x == c).count();
    (n >= 3).then_some((c, n))
}

/// Does `t` close a fence opened with `n` repeats of `c`? (Same character, at least as many, and
/// nothing but blanks after it: a closing fence cannot carry an info string.)
fn closes_fence(t: &str, c: char, n: usize) -> bool {
    matches!(fence_of(t), Some((c2, n2)) if c2 == c && n2 >= n)
        && t.trim_start().trim_start_matches(c).trim().is_empty()
}

/// Where blocks end in a source that only grows, found one line at a time. `advance` consumes
/// the complete lines it has not seen yet, so a stream costs O(delta) per frame instead of a
/// rescan of everything received.
#[derive(Clone, Default)]
struct Scan {
    /// Start of the first line not yet consumed.
    pos: usize,
    fence: Option<(char, usize)>,
    /// Offset of the line that opened `fence`.
    fence_start: usize,
    /// The fence is not indented into a list item, so it is a block of its own.
    fence_top: bool,
    pending_blank: bool,
    best: Option<usize>,
}

impl Scan {
    fn step(&mut self, line: &str, start: usize) {
        let ends_nl = line.ends_with('\n');
        let t = line.trim_end_matches(['\n', '\r']);
        if let Some((c, n)) = self.fence {
            if closes_fence(t, c, n) {
                self.fence = None;
                // Whatever follows a closed top-level fence is a new block, blank line or not.
                // (A fence inside a list item is part of that item.)
                if ends_nl && self.fence_top {
                    self.best = Some(start + line.len());
                }
            }
            return;
        }
        if t.trim().is_empty() {
            self.pending_blank = true;
            return;
        }
        let indent = t.len() - t.trim_start().len();
        // The last line may still be growing into a list marker (`2` before `2.`), a quote or
        // a fence; a boundary in front of it would freeze the list in two.
        let growing = !ends_nl && marker_prefix(t);
        let opener = fence_of(t);
        // A fence opener interrupts whatever came before it, so it starts a block even with
        // no blank line in front.
        if indent < 2
            && !growing
            && ((self.pending_blank && !is_list_marker(t)) || opener.is_some())
        {
            self.best = Some(start);
        }
        self.pending_blank = false;
        if let Some(f) = opener {
            self.fence = Some(f);
            self.fence_start = start;
            self.fence_top = indent < 2;
        }
    }

    /// Consume the complete lines of `src` past `pos`.
    fn advance(&mut self, src: &str) {
        while let Some(i) = src[self.pos..].find('\n') {
            let end = self.pos + i + 1;
            let line = &src[self.pos..end];
            self.step(line, self.pos);
            self.pos = end;
        }
    }

    /// The best boundary, counting the unfinished last line `rest` (`src[pos..]`) without
    /// consuming it.
    fn boundary(&self, rest: &str) -> Option<usize> {
        if rest.is_empty() {
            return self.best;
        }
        let mut tmp = self.clone();
        tmp.step(rest, self.pos);
        tmp.best
    }
}

/// Byte offset of the last point where everything before it is a closed block and everything
/// after starts a new top-level one.
pub fn stable_boundary(src: &str) -> Option<usize> {
    let mut scan = Scan::default();
    scan.advance(src);
    scan.boundary(&src[scan.pos..])
}

/// The prefix of `src` that is safe to show while it is still growing.
pub fn visible(src: &str, allow_partial: bool) -> &str {
    let mut scan = Scan::default();
    scan.advance(src);
    visible_scan(src, allow_partial, &scan)
}

/// [`visible`] with the scan of `src`'s complete lines already done, which tells whether the
/// last line is inside a fence without reading the text above it.
fn visible_scan<'a>(src: &'a str, allow_partial: bool, scan: &Scan) -> &'a str {
    let mut end = src.len();
    // A half-typed word waits for its delimiter, unless the stream has paused.
    if !allow_partial {
        if let Some(i) = src.rfind(|c: char| c.is_whitespace()) {
            end = i + src[i..].chars().next().map_or(1, char::len_utf8);
        } else {
            end = 0;
        }
    }
    let mut head = &src[..end];
    // A backslash at the end of the text waits: `\` then a newline is a hard break only once
    // more text follows, and until then it prints as a backslash that would vanish.
    let mut cut = false;
    let t = head.trim_end_matches(['\r', '\n']);
    let bs = t.len() - t.trim_end_matches('\\').len();
    if bs % 2 == 1 && !in_open_fence(head) {
        head = &head[..t.len() - 1];
        cut = true;
    }
    let line_start = head.rfind('\n').map_or(0, |i| i + 1);
    let line = &head[line_start..];
    // The scan has consumed every complete line of `src`, so its fence state is the state at
    // the start of the last line, unless the backslash cut moved that line.
    let in_fence = if !cut && line_start == scan.pos {
        scan.fence.is_some()
    } else {
        in_open_fence(&head[..line_start])
    };
    // Inside an open fence the line is code: nothing to hold.
    if in_fence {
        // A closing fence still being typed is not code: it would show and then vanish.
        let t = line.trim();
        let closing = !t.is_empty()
            && (t.bytes().all(|b| b == b'`') || t.bytes().all(|b| b == b'~'))
            && !src[line_start..].contains('\n');
        return if closing { &head[..line_start] } else { head };
    }
    if (fence_of(line).is_some() || marker_prefix(line) || bare_marker(line))
        && !src[line_start..].contains('\n')
    {
        return &head[..line_start];
    }
    match open_inline(line) {
        Some(off) if line.len() - off <= 40 || !allow_partial => &head[..line_start + off],
        _ => head,
    }
}

/// A last line made only of `-`, `=`, `*`, `_` or `+`: under a paragraph it is a setext
/// underline or a rule, at the start of a line a list marker. Either way it changes what the
/// rows above it are, so it waits for its newline or for some text after it.
fn bare_marker(line: &str) -> bool {
    let t = line.trim();
    !t.is_empty()
        && t.bytes()
            .all(|b| matches!(b, b'-' | b'=' | b'*' | b'_' | b'+'))
}

fn in_open_fence(s: &str) -> bool {
    let mut fence: Option<(char, usize)> = None;
    for line in s.lines() {
        match fence {
            Some((c, n)) => {
                if closes_fence(line, c, n) {
                    fence = None;
                }
            }
            None => fence = fence_of(line),
        }
    }
    fence.is_some()
}

/// Byte offset in `line` of the earliest inline opener that has not been closed yet.
fn open_inline(line: &str) -> Option<usize> {
    let b = line.as_bytes();
    let mut i = 0;
    let mut open: Vec<(u8, usize)> = Vec::new();
    // A list marker or a quote mark at the start of the line is not an opener.
    let lead = line.len() - line.trim_start().len();
    if is_list_marker(line) {
        i = lead + 1;
    }
    let mut in_code = None::<usize>;
    while i < b.len() {
        match b[i] {
            b'`' => {
                match in_code {
                    Some(_) => in_code = None,
                    None => in_code = Some(i),
                }
                i += 1;
            }
            _ if in_code.is_some() => i += 1,
            b'*' => {
                let n = b[i..].iter().take_while(|c| **c == b'*').count();
                let kind = if n >= 2 { 2 } else { 1 };
                if let Some(pos) = open.iter().rposition(|(k, _)| *k == kind) {
                    open.truncate(pos);
                } else {
                    open.push((kind, i));
                }
                i += n;
            }
            b'~' => {
                let n = b[i..].iter().take_while(|c| **c == b'~').count();
                if n >= 2 {
                    if let Some(pos) = open.iter().rposition(|(k, _)| *k == b'~') {
                        open.truncate(pos);
                    } else {
                        open.push((b'~', i));
                    }
                }
                i += n;
            }
            b'[' => {
                // `![` opens an image; the `!` goes with it.
                let at = if i > 0 && b[i - 1] == b'!' { i - 1 } else { i };
                open.push((b'[', at));
                i += 1;
            }
            b']' => {
                if let Some(pos) = open.iter().rposition(|(k, _)| *k == b'[') {
                    // `[text]` closes, but `[text](` opens a url that must close too.
                    if b.get(i + 1) == Some(&b'(') {
                        open[pos] = (b'(', open[pos].1);
                    } else if i + 1 == b.len() {
                        // `]` at the end of the text may still be followed by `(`.
                    } else {
                        open.truncate(pos);
                    }
                }
                i += 1;
            }
            b')' => {
                if let Some(pos) = open.iter().rposition(|(k, _)| *k == b'(') {
                    open.truncate(pos);
                }
                i += 1;
            }
            _ => i += 1,
        }
    }
    let code = in_code;
    let other = open.first().map(|(_, o)| *o);
    match (code, other) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    }
}

/// A delimiter row, or the start of one: only pipes, dashes, colons and blanks.
fn delimiter_like(line: &str) -> bool {
    let t = line.trim();
    t.starts_with('|') && t.chars().all(|c| matches!(c, '|' | '-' | ':' | ' ' | '\t'))
}

/// While a table is still being written, where it starts in `tail` and how many body rows it
/// has so far. A table can follow a paragraph line directly, so the start is not always the
/// start of the tail. `None` once a blank line ends it, or when the pipe line turns out to be
/// plain text (the second line is not a delimiter row).
fn open_table(tail: &str) -> Option<(usize, usize)> {
    let mut off = 0;
    let mut start = None;
    let mut lines: Vec<&str> = Vec::new();
    for line in tail.split_inclusive('\n') {
        let t = line.trim_end_matches(['\n', '\r']);
        if start.is_none() {
            let indent = t.len() - t.trim_start().len();
            if indent < 4 && t.trim_start().starts_with('|') && !in_open_fence(&tail[..off]) {
                start = Some(off);
                lines.push(t);
            }
        } else if t.trim().is_empty() {
            return None;
        } else {
            lines.push(t);
        }
        off += line.len();
    }
    let start = start?;
    if lines.len() >= 2 && !delimiter_like(lines[1]) {
        return None;
    }
    Some((start, lines.len().saturating_sub(2)))
}

/// Does `src` define a link reference or footnote (`[id]: target` at the start of a line)?
fn has_definitions(src: &str) -> bool {
    src.lines().any(|l| {
        let t = l.trim_start();
        l.len() - t.len() < 4
            && t.starts_with('[')
            && t.find("]:").is_some_and(|i| i > 1 && !t[..i].contains(']'))
    })
}

/// A reference label as CommonMark compares it: trimmed, whitespace collapsed, lowercase.
fn norm_label(l: &str) -> String {
    l.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

/// The label a `[label]: target` definition line defines.
fn definition_label(line: &str) -> Option<String> {
    let t = line.trim_start();
    if line.len() - t.len() >= 4 || !t.starts_with('[') {
        return None;
    }
    let i = t.find("]:")?;
    let label = &t[1..i];
    (i > 1 && !label.contains(']')).then(|| norm_label(label))
}

/// Labels of reference-style uses in `text`: `[text][label]` and `[label][]`. A bare `[word]` is
/// left out: it is as likely a citation or an array index as a reference whose definition is
/// still to come. Footnote marks `[^label]` are left out too: without footnote support the
/// parser reads them as links only when the definition happens to follow a certain way, so a
/// stand-in would not predict what the finished answer shows.
fn reference_labels(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let b = text.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i] != b'[' {
            i += 1;
            continue;
        }
        let Some(close) = text[i + 1..]
            .find([']', '['])
            .filter(|k| b[i + 1 + k] == b']')
        else {
            i += 1;
            continue;
        };
        let j = i + 1 + close;
        let inner = &text[i + 1..j];
        if b.get(j + 1) == Some(&b'[') {
            if let Some(k) = text[j + 2..]
                .find([']', '[', '\n'])
                .filter(|k| b[j + 2 + k] == b']')
            {
                let label = &text[j + 2..j + 2 + k];
                out.push(norm_label(if label.is_empty() { inner } else { label }));
            }
        }
        i = j + 1;
    }
    out.retain(|l| !l.is_empty() && !l.contains(['\n', ']', '[']));
    out.sort();
    out.dedup();
    out
}

/// A tail this long is re-rendered only when it has grown by a quarter since the last render.
/// Prose and lists have no cheap incremental form, and a model that never writes a blank line
/// for hundreds of kilobytes would otherwise pay for the whole tail on every frame.
const BIG_TAIL: usize = 64 * 1024;

/// An open top-level code fence, rendered a line at a time. Its finished rows stay in
/// [`MdState::rows`]; only the line being typed and the closing padding are redone per frame.
struct FenceInc {
    /// Where the fence's opener line starts (`MdState::frozen_src` while it is the tail).
    start: usize,
    /// First byte of the code.
    body_start: usize,
    /// Start of the first code line not yet turned into rows.
    body_pos: usize,
    /// Leading spaces of the opener, which pulldown strips from each code line.
    indent: usize,
    lang: String,
    hl: tuikit::syntax::LineHighlighter,
    plain: bool,
    /// `MdState::rows.len()` after the header and every finished line.
    committed_len: usize,
}

impl FenceInc {
    /// `None` when the opener uses anything this renderer does not reproduce exactly (tabs,
    /// escapes, entities, a backtick in a backtick fence's info string); the general path
    /// handles those.
    fn open(src: &str, start: usize, plain: bool) -> Option<FenceInc> {
        let rest = &src[start..];
        let end = rest.find('\n')?;
        let line = &rest[..end];
        if line.contains(['\t', '\r', '&', '\\']) {
            return None;
        }
        let (c, _) = fence_of(line)?;
        let indent = line.len() - line.trim_start().len();
        let info = line.trim_start().trim_start_matches(c).trim();
        if c == '`' && info.contains('`') {
            return None;
        }
        let lang = info.split_whitespace().next().unwrap_or("").to_string();
        let syntax = if plain {
            None
        } else {
            tuikit::syntax::find_syntax(&lang)
        };
        Some(FenceInc {
            start,
            body_start: start + end + 1,
            body_pos: start + end + 1,
            indent,
            lang,
            hl: tuikit::syntax::LineHighlighter::new(syntax),
            plain,
            committed_len: 0,
        })
    }

    /// The text of one code line as the full renderer sees it: tabs widened, the opener's
    /// indent stripped.
    fn code_text(&self, raw: &str) -> String {
        let line = raw.strip_suffix('\r').unwrap_or(raw);
        let line = if line.contains('\t') {
            std::borrow::Cow::Owned(line.replace('\t', "    "))
        } else {
            std::borrow::Cow::Borrowed(line)
        };
        let strip = line
            .bytes()
            .take(self.indent)
            .take_while(|b| *b == b' ')
            .count();
        line[strip..].to_string()
    }
}

/// Streaming render state for one growing source.
#[derive(Default)]
pub struct MdState {
    key: (usize, bool, bool),
    frozen_src: usize,
    frozen_len: usize,
    pub rows: Vec<Row>,
    scan: Scan,
    inc: Option<Box<FenceInc>>,
    /// `(start, length)` of the tail the rows after `frozen_len` were last rendered from.
    tail_done: Option<(usize, usize)>,
    /// Labels with a `[label]: target` line somewhere in the text so far, and how far that was
    /// read.
    defs: std::collections::HashSet<String>,
    defs_pos: usize,
    /// Render the open tail from scratch every time (the reference behaviour the incremental
    /// code fence path and the big-tail throttle are tested against).
    pub no_inc: bool,
}

impl MdState {
    fn reset(&mut self, key: (usize, bool, bool)) {
        *self = MdState {
            key,
            no_inc: self.no_inc,
            ..MdState::default()
        };
    }

    /// Note the definitions on the complete lines of `src` not read yet.
    fn read_definitions(&mut self, src: &str) {
        while let Some(i) = src[self.defs_pos..].find('\n') {
            let line = &src[self.defs_pos..self.defs_pos + i];
            if let Some(l) = definition_label(line) {
                self.defs.insert(l);
            }
            self.defs_pos += i + 1;
        }
    }

    /// `render`, for text of an answer that is still arriving. A reference-style link or a
    /// footnote mark whose definition has not come yet would print as raw `[text][1]` and change
    /// to the link text when the definitions arrive at the end, which moves every row below it.
    /// So a use whose label has no definition yet gets a stand-in one after the text (it renders
    /// as nothing), and the row keeps its final shape.
    fn render_streaming(&self, text: &str, cx: &Cx, closed: bool) -> Vec<Row> {
        // A finished line of an open fence has its final layout; only the one being typed does not.
        let partial = !closed && !text.ends_with('\n') && in_open_fence(text);
        if closed || !text.contains("][") {
            return render_with(text, cx, partial);
        }
        let missing: Vec<String> = reference_labels(text)
            .into_iter()
            .filter(|l| !self.defs.contains(l))
            .collect();
        // Inside an open code fence the stand-ins would be code.
        if missing.is_empty() || in_open_fence(text) {
            return render_with(text, cx, partial);
        }
        let mut with = String::with_capacity(text.len() + 16 * missing.len());
        with.push_str(text);
        with.push_str("\n\n");
        for l in &missing {
            with.push_str(&format!("[{l}]: #\n"));
        }
        render(&with, cx)
    }

    /// Render `src` at the column in `cx`. `closed` means the stream is over: no holding back.
    /// `partial_ok` lets a trailing partial word show (the stream paused for 120 ms).
    pub fn update(&mut self, src: &str, cx: &Cx, closed: bool, partial_ok: bool) -> &[Row] {
        let key = (cx.width, cx.p.is_dark(), cx.detail);
        if self.key != key || self.frozen_src > src.len() || self.scan.pos > src.len() {
            self.reset(key);
        }
        if closed && has_definitions(src) {
            // A link definition can come after the text that uses it, so the frozen chunks
            // may have rendered `[ref][1]` as plain text. Once the stream is over the whole
            // answer is laid out in one go.
            self.reset(key);
            self.rows = render(src, cx);
            self.frozen_src = src.len();
            self.frozen_len = self.rows.len();
            return &self.rows;
        }
        self.scan.advance(src);
        self.read_definitions(src);
        let shown = if closed {
            src
        } else {
            visible_scan(src, partial_ok, &self.scan)
        };
        let boundary = if shown.len() >= self.scan.pos {
            self.scan.boundary(&shown[self.scan.pos..])
        } else {
            stable_boundary(shown)
        };
        if let Some(b) = boundary {
            if b > self.frozen_src {
                let chunk = &shown[self.frozen_src..b];
                let rows = self.render_streaming(chunk, cx, closed);
                self.rows.truncate(self.frozen_len);
                if self.frozen_len > 0 && !rows.is_empty() {
                    self.rows.push(Row::blank());
                }
                self.rows.extend(rows);
                self.frozen_len = self.rows.len();
                self.frozen_src = b;
                self.inc = None;
                self.tail_done = None;
            }
        }
        let start = self.frozen_src.min(shown.len());
        if self.fence_tail(shown, start, cx, closed) {
            self.tail_done = None;
            return &self.rows;
        }
        self.inc = None;
        let tail = &shown[start..];
        if !closed && !self.no_inc && tail.len() > BIG_TAIL {
            if let Some((s, n)) = self.tail_done {
                if s == start && tail.len() < n + n / 4 {
                    return &self.rows;
                }
            }
        }
        self.tail_done = Some((start, tail.len()));
        self.rows.truncate(self.frozen_len);
        let mut rows = match open_table(tail).filter(|_| !closed) {
            Some((split, n)) => {
                let mut r = if split > 0 {
                    self.render_streaming(&tail[..split], cx, closed)
                } else {
                    Vec::new()
                };
                if !r.is_empty() {
                    r.push(Row::blank());
                }
                let word = if n == 1 { "row" } else { "rows" };
                r.push(Row::new(vec![
                    spaces(INDENT),
                    sp(
                        format!("{} table, {n} {word} so far", cx.g.dots),
                        cx.p.s_faint(),
                    ),
                ]));
                r
            }
            None => self.render_streaming(tail, cx, closed),
        };
        if self.frozen_len > 0 && !rows.is_empty() {
            self.rows.push(Row::blank());
        }
        self.rows.append(&mut rows);
        &self.rows
    }

    /// Draw the tail as an open code fence, one new line at a time. False when the tail is not
    /// an open top-level fence (or is one this path will not reproduce exactly), and the general
    /// render must draw it.
    fn fence_tail(&mut self, shown: &str, start: usize, cx: &Cx, closed: bool) -> bool {
        let Some((c, n)) = self.scan.fence else {
            return false;
        };
        if self.no_inc
            || !self.scan.fence_top
            || self.scan.fence_start != start
            || shown.len() < self.scan.pos
        {
            return false;
        }
        let partial = &shown[self.scan.pos..];
        // The closing fence is being typed, or the stream ended on it: the general render
        // knows how that block ends.
        if closes_fence(partial, c, n) {
            return false;
        }
        if self.inc.as_ref().is_none_or(|i| i.start != start) {
            let Some(inc) = FenceInc::open(shown, start, false) else {
                return false;
            };
            self.rows.truncate(self.frozen_len);
            if self.frozen_len > 0 {
                self.rows.push(Row::blank());
            }
            self.rows.push(code_header(cx, &inc.lang));
            self.inc = Some(Box::new(FenceInc {
                committed_len: self.rows.len(),
                ..inc
            }));
        }
        // Past the highlighter's size limit the full renderer draws the whole block plain; so
        // does this, from the first line.
        let body = &shown[self.inc.as_ref().map_or(0, |i| i.body_start)..];
        let body_len = body.len() - usize::from(body.ends_with('\n'));
        if body_len > tuikit::syntax::MAX_HIGHLIGHT_BYTES
            && self.inc.as_ref().is_some_and(|i| !i.plain)
        {
            self.rows.truncate(self.frozen_len);
            if self.frozen_len > 0 {
                self.rows.push(Row::blank());
            }
            let Some(mut inc) = FenceInc::open(shown, start, true) else {
                return false;
            };
            self.rows.push(code_header(cx, &inc.lang));
            inc.committed_len = self.rows.len();
            self.inc = Some(Box::new(inc));
        }
        let Some(inc) = self.inc.as_mut() else {
            return false;
        };
        self.rows.truncate(inc.committed_len);
        let w = code_width(cx, INDENT);
        let prefix = || vec![spaces(INDENT)];
        while let Some(i) = shown[inc.body_pos..self.scan.pos].find('\n') {
            let raw = &shown[inc.body_pos..inc.body_pos + i];
            if raw.trim_end_matches('\r').contains('\r') {
                // a lone CR is a line break to the renderer; leave it to the general path
                self.no_inc = true;
                self.inc = None;
                return false;
            }
            let text = inc.code_text(raw);
            let spans = inc.hl.line(&text, cx.theme);
            code_line_rows(cx, &spans, w, false, prefix, &mut self.rows);
            inc.body_pos += i + 1;
        }
        inc.committed_len = self.rows.len();
        if !partial.is_empty() {
            if partial.contains('\r') {
                self.no_inc = true;
                self.inc = None;
                return false;
            }
            let text = inc.code_text(partial);
            // A last line of one to three spaces and no newline is not code yet: pulldown
            // drops it, and keeps four or more.
            let raw = partial.replace('\t', "    ");
            if !(raw.len() < 4 && raw.bytes().all(|b| b == b' ')) {
                let spans = inc.hl.preview(&text, cx.theme);
                // Once the stream is over the last line is as finished as it will get.
                code_line_rows(cx, &spans, w, !closed, prefix, &mut self.rows);
            }
        }
        self.rows.push(code_pad(cx));
        true
    }
}

/// Flatten rows to text, for tests and search.
pub fn plain(rows: &[Row]) -> Vec<String> {
    rows.iter().map(Row::text).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::palette::{Depth, Kind, Palette, UNICODE};
    use std::time::Instant;

    fn cx_for<'a>(p: &'a Palette, t: &'a tuikit::theme::Theme, width: usize) -> Cx<'a> {
        Cx {
            p,
            theme: t,
            g: &UNICODE,
            width,
            detail: false,
            now: Instant::now(),
            spin: 0,
        }
    }

    fn md(src: &str, width: usize) -> Vec<String> {
        let p = Palette::new(Kind::Hearth, Depth::True);
        let t = p.theme();
        plain(&render(src, &cx_for(&p, &t, width)))
    }

    #[test]
    fn paragraph_has_no_markup_and_wraps_at_the_column() {
        let out = md(
            "# Title\n\nsome **bold** and `code` text that is long enough to wrap around",
            30,
        );
        assert_eq!(out[0], "  Title");
        assert_eq!(out[1], "");
        assert!(
            out.iter().all(|l| tuikit::width::display_width(l) <= 30),
            "{out:?}"
        );
        assert!(!out.iter().any(|l| l.contains("**") || l.contains('`')));
    }

    #[test]
    fn lists_use_bullets_and_hang() {
        let out = md("- one\n- two that is rather long and wraps onto a second row of text\n  - nested\n\n1. a\n2. b", 28);
        assert_eq!(out[0], "  • one");
        assert!(out[2].starts_with("    "), "hanging indent: {out:?}");
        assert!(out.iter().any(|l| l == "    ◦ nested"), "{out:?}");
        assert!(out.iter().any(|l| l == "  1. a"));
        assert!(out.iter().any(|l| l == "  2. b"));
    }

    #[test]
    fn a_single_newline_is_a_break_only_where_one_is_meant() {
        // Round 2 finding 14: a source hard-wrapped at 50 columns showed ragged rows.
        let p = Palette::new(Kind::Hearth, Depth::True);
        let t = p.theme();
        let at = |src: &str, w: usize| plain(&render(src, &cx_for(&p, &t, w)));
        // Hard-wrapped prose: the lines fill most of the column, so they run together.
        let wrapped = "The quick brown fox jumps over the lazy dog and keeps going until\nthe end of the line, where the source wrapped it for no reason of\nits own.";
        let rows = at(wrapped, 110);
        assert!(
            rows.iter()
                .any(|r| r.contains("going until the end of the line")),
            "{rows:?}"
        );
        assert!(
            rows.iter().any(|r| r.contains("wrapped it for no reason")),
            "{rows:?}"
        );
        // Short lines are lines.
        let rows = at("one\ntwo\nthree", 80);
        assert_eq!(rows.len(), 3, "{rows:?}");
        // Whatever the previous line, a marker starts a new row.
        for next in [
            "- item",
            "* item",
            "+ item",
            "1. item",
            "| a | b |",
            "> quote",
            "# head",
        ] {
            let src = format!("{}\n{next}", "x".repeat(70));
            let rows = at(&src, 80);
            assert!(rows.len() >= 2, "{next}: {rows:?}");
        }
        // An explicit break still breaks.
        let rows = at(&format!("{}  \nnext", "y".repeat(70)), 80);
        assert_eq!(rows.len(), 2, "{rows:?}");
    }

    #[test]
    fn code_block_gets_padding_rows_a_tag_and_wraps_with_a_marker() {
        let out = md("```rust\nfn main() { println!(\"a very long line that cannot fit in the column\"); }\n```", 30);
        assert!(out[0].trim_end().ends_with("rust"), "{out:?}");
        assert!(out.iter().any(|l| l.contains('↳')), "{out:?}");
        assert!(out.last().unwrap().is_empty());
    }

    #[test]
    fn a_table_glued_to_a_list_item_is_still_a_table() {
        // Finding 15, from the real haiku session: the header line directly under a list item
        // is a lazy continuation in CommonMark, so it printed as one bullet full of pipes.
        let out = md(
            "- Handle with match or ?\n| Method | Use |\n|--------|-----|\n| match | explicit |\n| ? | concise |",
            80,
        );
        assert!(!out.iter().any(|l| l.contains('|')), "{out:?}");
        assert!(out[0].contains("Handle with match or ?"), "{out:?}");
        assert!(
            out.iter()
                .any(|l| l.contains("Method") && l.contains("Use")),
            "{out:?}"
        );
        assert!(
            out.iter()
                .any(|l| l.contains("match") && l.contains("explicit")),
            "{out:?}"
        );
        // Code that merely shows pipes is left alone.
        let code = md("```\nsay hi\n| a | b |\n|---|---|\n```", 40);
        assert!(code.iter().any(|l| l.contains("| a | b |")), "{code:?}");
    }

    #[test]
    fn a_single_newline_in_model_text_is_a_line_break() {
        let out = md("first line\nsecond line", 60);
        assert_eq!(out, ["  first line", "  second line"], "{out:?}");
    }

    #[test]
    fn the_higher_heading_is_the_louder_one() {
        let p = Palette::new(Kind::Hearth, Depth::True);
        let t = p.theme();
        let rows = render("# Plan\n\n## Findings\n\n### Deeper", &cx_for(&p, &t, 60));
        let fg = |n: usize| {
            rows.iter()
                .filter(|r| !r.spans.is_empty())
                .nth(n)
                .unwrap()
                .spans
                .last()
                .unwrap()
                .style
                .fg
        };
        assert_eq!(fg(0), Some(p.accent), "H1");
        assert_eq!(fg(1), Some(p.text), "H2");
        assert_eq!(fg(2), Some(p.dim), "H3");
    }

    #[test]
    fn wrapped_code_breaks_at_punctuation_and_leaves_air_after_the_marker() {
        // Finding 21: `↳1).unwrap_or_else(...)` with no gap and breaks inside tokens.
        for width in [30usize, 36, 44, 60, 76] {
            let src = "```rust\nlet value = compute_something(first_argument, second_argument).unwrap_or_else(|| default_value(1));\n```";
            let out = md(src, width);
            let rows: Vec<&String> = out.iter().filter(|l| l.contains('↳')).collect();
            assert!(!rows.is_empty(), "{width}: {out:?}");
            for r in rows {
                let after = r.split_once('↳').unwrap().1;
                assert!(
                    after.starts_with(' ') && after.trim().chars().count() > 2,
                    "{width}: `{r}`"
                );
                // The marker is in the padding, text stays in the code column.
                assert!(r.find('↳').unwrap() <= 1, "{width}: `{r}`");
                assert!(tuikit::width::display_width(r) <= width, "{width}: `{r}`");
            }
        }
    }

    #[test]
    fn table_is_rules_only_and_three_rows_fit_in_five() {
        let out = md(
            "| file | lines |\n|------|------:|\n| a.rs | 3 |\n| b.rs | 120 |",
            60,
        );
        assert_eq!(out.len(), 4, "{out:?}");
        assert!(out[1].contains('─') && !out.iter().any(|l| l.contains('|')));
        assert!(out[3].trim_end().ends_with("120"));
    }

    #[test]
    fn wrapping_rows_get_faint_dividers_and_short_rows_do_not() {
        let out = md(
            "| k | description |\n|---|---|\n| a | one two three four five six seven eight nine ten |\n| b | short |",
            30,
        );
        assert!(
            out.iter().filter(|l| l.contains('┈')).count() == 1,
            "one divider between the two body rows: {out:?}"
        );
        let out = md("| a | b |\n|---|---|\n| 1 | 2 |\n| 3 | 4 |\n| 5 | 6 |", 40);
        assert_eq!(out.len(), 5, "no dividers when nothing wraps: {out:?}");
    }

    #[test]
    fn inline_code_is_a_chip_with_its_own_colours_and_no_padding() {
        let p = Palette::new(Kind::Hearth, Depth::True);
        let t = p.theme();
        let rows = render("run `cargo test` now", &cx_for(&p, &t, 40));
        assert_eq!(rows[0].text(), "  run cargo test now");
        let chip = rows[0]
            .spans
            .iter()
            .find(|s| s.content == "cargo test")
            .unwrap();
        assert_eq!(chip.style.bg, Some(p.chip_bg));
        assert_eq!(chip.style.fg, Some(p.chip_fg));
    }

    #[test]
    fn narrow_table_becomes_cards() {
        let out = md("| name | description |\n|---|---|\n| alpha | a very long description of alpha that wraps |", 16);
        assert!(out.iter().any(|l| l.contains("name: alpha")), "{out:?}");
    }

    #[test]
    fn nested_quotes_have_one_blank_row_between_levels_and_lists_vary_their_bullets() {
        // Round 2 finding 13: `▎`, `▎ ▎`, `▎ ▎ ▎` rows of nothing before each inner level, one
        // bullet glyph at all seven list depths, and `[x]` as bare text.
        let out = md("> one\n>\n> > two\n> >\n> > > three\n", 40);
        let blank = |r: &String| r.replace(['▎', ' '], "").is_empty();
        for pair in out.windows(2) {
            assert!(
                !(blank(&pair[0]) && blank(&pair[1])),
                "two empty rows: {out:?}"
            );
        }
        assert_eq!(out.iter().filter(|r| blank(r)).count(), 2, "{out:?}");
        let out = md("- a\n  - b\n    - c\n      - d\n", 40);
        let glyphs: Vec<char> = out
            .iter()
            .filter_map(|r| r.trim_start().chars().next())
            .collect();
        assert_eq!(glyphs, ['•', '◦', '▪', '•'], "{out:?}");
        let out = md("- [x] done\n- [ ] open\n", 40);
        assert!(
            out[0].contains("✓ done") && out[1].contains("○ open"),
            "{out:?}"
        );
        assert!(!out.iter().any(|r| r.contains('[')), "{out:?}");
    }

    #[test]
    fn quote_and_rule() {
        let out = md("> quoted\n\n---\n", 20);
        assert_eq!(out[0], "  ▎ quoted");
        assert!(out[2].contains("──"));
    }

    #[test]
    fn hold_back_partial_words_and_openers() {
        assert_eq!(visible("hello wor", false), "hello ");
        assert_eq!(visible("hello wor", true), "hello wor");
        assert_eq!(visible("see **bo", true), "see ");
        assert_eq!(visible("see **bold** and `co", true), "see **bold** and ");
        assert_eq!(visible("see [link](http://x", true), "see ");
        assert_eq!(visible("* item text ", false), "* item text ");
        assert_eq!(visible("a\n```ru", true), "a\n");
        assert_eq!(
            visible("```rust\nlet x = 1; // a `b", true),
            "```rust\nlet x = 1; // a `b"
        );
    }

    #[test]
    fn stable_boundary_keeps_lists_and_fences_whole() {
        assert_eq!(stable_boundary("a\n\nb"), Some(3));
        assert_eq!(stable_boundary("- a\n\n- b"), None);
        // text right after a closed fence is a new block, blank line or not
        assert_eq!(stable_boundary("```\na\n\nb\n```\nc"), Some(13));
        // inside the fence there is nothing to cut at, and the opener is the start of the block
        assert_eq!(stable_boundary("```\na\n\nb"), Some(0));
        assert_eq!(stable_boundary("text\n```\na\n\nb"), Some(5));
        assert_eq!(stable_boundary("```\na\n\nb\n```\n\nc"), Some(14));
    }

    #[test]
    fn streaming_never_changes_a_committed_row() {
        let p = Palette::new(Kind::Hearth, Depth::True);
        let t = p.theme();
        let cx = cx_for(&p, &t, 40);
        let src = "# Plan\n\nHere is what I found in **`src/`**, with a few *notes* and a long sentence that wraps.\n\n1. `main.rs` prints a greeting\n2. There is no test directory\n\n| a | b |\n|---|---|\n| 1 | 2 |\n\nThe end.";
        let mut st = MdState::default();
        let mut prev: Vec<String> = Vec::new();
        for end in (1..=src.len()).filter(|i| src.is_char_boundary(*i)) {
            let rows = plain(st.update(&src[..end], &cx, false, false));
            // every row but the last two must equal what was shown before
            let keep = prev.len().saturating_sub(2);
            assert_eq!(
                &rows[..keep.min(rows.len())],
                &prev[..keep.min(rows.len())],
                "at {end}: {rows:?} vs {prev:?}"
            );
            prev = rows;
        }
        let fin = plain(st.update(src, &cx, true, true));
        assert_eq!(fin, md(src, 40));
    }

    /// Styles as text, so two renders compare on colour and modifiers as well as characters.
    fn dump(rows: &[Row]) -> Vec<String> {
        rows.iter()
            .map(|r| {
                let spans: Vec<String> = r
                    .spans
                    .iter()
                    .map(|s| format!("{:?}{:?}", s.content, s.style))
                    .collect();
                format!("{:?} {}", r.bg, spans.join("|"))
            })
            .collect()
    }

    const FENCE_DOCS: &[&str] = &[
        "Here is the change.\n\n```rust\nfn main() {\n    let x = 1;\n\n    println!(\"{x} and a line long enough that it has to wrap around\");\n}\n```\n\nThat is all.",
        "Run this:\n```sh\ncargo test \\\n  --workspace\n```\nand then that.\n\nDone.",
        "```\nno language\n\n\ntrailing blanks\n\n",
        "~~~python title=x\ndef f(x):\n\treturn 'tab'\n~~~\n",
        "````md\n```rust\ninner\n```\n````\nafter",
        "- item\n\n```json\n{\"a\": [1, 2]}\n```\n- next\n",
        " ```rust,no_run\n  let indented = 1;\n let less = 2;\nno_indent();\n ```\n",
        "```klingon\nqapla'\n```\n```rust\nlet a = 1;\n```\ntext\n\n```\n",
        "| a | b |\n|---|---|\n| 1 | 2 |\n```\ncode\n```\n",
        "> quote\n```\ncode after a quote\n",
        "```\n\nstarts with a blank line\n```",
        "```rust\n日本語 and wide chars and a very long line of code that must wrap more than once at the narrow width, 日本語日本語\n```",
        "a\r\n```\r\ncrlf code\r\n```\r\nb",
        "```\nlone\rcr\n```",
        "``` `not a fence`\ntext",
        "```rust\nfn a() {}\n```\n- list\n  ```\n  nested fence\n  ```\n",
        "1. install:\n\n   ```sh\n   cargo build\n   ```\n\n2. run it\n\n- item\n\n  ```\n  raw\n\n  with blank\n  ```\n- after",
        "- a\n- b\n```\ncode\n```\n- c\n",
    ];

    /// Feed every prefix of `src` to a streaming state with the incremental fence path and to one
    /// without; they must draw the same rows (text, colours and background) at every step.
    fn assert_incremental_matches_reference(src: &str, width: usize, partial_ok: bool) {
        let p = Palette::new(Kind::Hearth, Depth::True);
        let t = p.theme();
        let cx = cx_for(&p, &t, width);
        let mut fast = MdState::default();
        let mut slow = MdState {
            no_inc: true,
            ..MdState::default()
        };
        for end in (1..=src.len()).filter(|i| src.is_char_boundary(*i)) {
            let part = &src[..end];
            let a = dump(fast.update(part, &cx, false, partial_ok));
            let b = dump(slow.update(part, &cx, false, partial_ok));
            assert_eq!(
                a, b,
                "width {width}, partial_ok {partial_ok}, prefix {part:?}"
            );
        }
        let a = dump(fast.update(src, &cx, true, true));
        let b = dump(slow.update(src, &cx, true, true));
        assert_eq!(a, b, "closed, {src:?}");
        // and the finished answer is what a one-shot render gives
        assert_eq!(
            plain(fast.update(src, &cx, true, true)),
            md(src, width),
            "{src:?}"
        );
    }

    #[test]
    fn an_open_code_fence_is_drawn_the_same_incrementally_as_from_scratch() {
        for doc in FENCE_DOCS {
            for width in [24, 60] {
                for partial_ok in [false, true] {
                    assert_incremental_matches_reference(doc, width, partial_ok);
                }
            }
        }
    }

    #[test]
    fn a_fence_past_the_highlight_limit_turns_plain_as_the_full_render_does() {
        let line = "let value = compute(a, b); // padding\n";
        let n = tuikit::syntax::MAX_HIGHLIGHT_BYTES / line.len() + 5;
        let src = format!("```rust\n{}", line.repeat(n));
        let p = Palette::new(Kind::Hearth, Depth::True);
        let t = p.theme();
        let cx = cx_for(&p, &t, 80);
        let mut fast = MdState::default();
        let mut slow = MdState {
            no_inc: true,
            ..MdState::default()
        };
        // a few steps either side of the limit, then the end
        let limit = "```rust\n".len() + tuikit::syntax::MAX_HIGHLIGHT_BYTES;
        // below the limit the fence is highlighted; one frame later it is past it
        let below = (limit - 5..).find(|i| src.is_char_boundary(*i)).unwrap();
        let above = (limit + 5..).find(|i| src.is_char_boundary(*i)).unwrap();
        let before = dump(fast.update(&src[..below], &cx, false, true));
        assert!(
            before.iter().any(|r| r.contains("italic")),
            "highlighted below the limit"
        );
        let a = dump(fast.update(&src[..above], &cx, false, true));
        let b = dump(slow.update(&src[..above], &cx, false, true));
        assert!(a == b, "differs past the limit");
    }

    #[test]
    fn a_long_fence_costs_the_new_lines_not_the_whole_block() {
        // Finding 5: every frame re-rendered and re-highlighted the open fence from its first line,
        // so a 1000 line block took about 70 ms a frame by the end. Frame time must stay flat.
        let p = Palette::new(Kind::Hearth, Depth::True);
        let t = p.theme();
        let cx = cx_for(&p, &t, 100);
        let mut text = String::from("Here it is.\n\n```rust\n");
        for i in 0..500 {
            text.push_str(&format!(
                "    let value_{i} = compute(a, b, \"{i}\"); // note\n"
            ));
        }
        let mut st = MdState::default();
        let bytes = text.as_bytes();
        let step = 200;
        let mut times = Vec::new();
        let mut at = 0;
        while at < bytes.len() {
            at = (at + step).min(bytes.len());
            while !text.is_char_boundary(at) {
                at += 1;
            }
            let t0 = std::time::Instant::now();
            st.update(&text[..at], &cx, false, false);
            times.push(t0.elapsed());
        }
        let n = times.len();
        let avg =
            |s: &[std::time::Duration]| s.iter().sum::<std::time::Duration>() / s.len() as u32;
        let early = avg(&times[n / 10..n / 5]);
        let late = avg(&times[n - n / 10..]);
        assert!(
            late < early * 5 + std::time::Duration::from_micros(300),
            "frames got slower as the block grew: early {early:?}, late {late:?}"
        );
    }

    #[test]
    fn a_huge_paragraph_is_rendered_again_only_after_it_grows_a_quarter() {
        let p = Palette::new(Kind::Hearth, Depth::True);
        let t = p.theme();
        let cx = cx_for(&p, &t, 80);
        let word = "stream ";
        let src = word.repeat(BIG_TAIL / word.len() * 3);
        let mut st = MdState::default();
        let mut exact = MdState {
            no_inc: true,
            ..MdState::default()
        };
        let mut renders = 0;
        let mut prev = Vec::new();
        let step = 3000;
        for end in (step..=src.len()).step_by(step) {
            let rows = plain(st.update(&src[..end], &cx, false, true));
            if rows != prev {
                renders += 1;
            }
            // an older render is shown, so the rows never outrun the text
            assert!(rows.len() <= end / 60 + 1);
            prev = rows;
        }
        let frames = src.len() / step;
        assert!(renders < frames / 2, "{renders} renders in {frames} frames");
        // the finished answer is exact
        assert_eq!(
            plain(st.update(&src, &cx, true, true)),
            plain(exact.update(&src, &cx, true, true))
        );
    }

    #[test]
    fn a_link_that_shows_one_host_and_opens_another_says_where_it_goes() {
        let one = |src: &str| md(src, 90).join("\n");
        let s = one("[https://github.com/x](https://evil.example/login)");
        assert!(s.contains("https://github.com/x (evil.example)"), "{s}");
        // credentials in front of the host do not fool it
        let s = one("[https://github.com](https://github.com@evil.example/)");
        assert!(s.contains("(evil.example)"), "{s}");
        // an honest link, a link with words, and a domain with and without www get nothing added
        for ok in [
            "[https://github.com/x](https://github.com/y)",
            "[docs](https://evil.example/)",
            "[github.com](https://www.github.com/a)",
            "[see the page](https://github.com/a)",
        ] {
            let s = one(ok);
            assert!(!s.contains('('), "{ok}: {s}");
        }
    }

    #[test]
    fn only_http_https_and_mailto_destinations_are_clickable() {
        let p = Palette::new(Kind::Hearth, Depth::True);
        let t = p.theme();
        let linked = |src: &str| -> bool {
            render(src, &cx_for(&p, &t, 80))
                .iter()
                .flat_map(|r| r.spans.iter())
                .any(|s| links::url_in(s.style).is_some())
        };
        assert!(linked("[x](https://example.com/a)"));
        assert!(linked("[x](http://example.com/a)"));
        assert!(linked("[x](mailto:a@example.com)"));
        for bad in [
            "[x](javascript:alert(1))",
            "[x](file:///etc/passwd)",
            "[x](ssh://host/)",
            "[x](vscode://file/etc/passwd)",
            "[x](data:text/html,hi)",
            "[x](ftp://example.com/)",
        ] {
            assert!(!linked(bad), "{bad} became clickable");
        }
    }

    #[test]
    fn hosts_are_read_the_way_a_browser_reads_them() {
        assert_eq!(
            links::host_of("https://github.com@evil.example/").as_deref(),
            Some("evil.example")
        );
        assert_eq!(
            links::host_of("HTTPS://WWW.Example.com:8080/a?b#c").as_deref(),
            Some("example.com")
        );
        assert_eq!(
            links::host_of("example.org/path").as_deref(),
            Some("example.org")
        );
        assert_eq!(links::host_of("not a host"), None);
        assert_eq!(
            links::spoofed_host("click here", "https://evil.example/"),
            None
        );
    }

    /// Stream `src` and then close it; rows that already had two rows after them must not change
    /// at any step, including the last one (the turn ending).
    fn assert_rows_stay_put_to_the_end(src: &str, width: usize, partial_ok: bool) {
        let p = Palette::new(Kind::Hearth, Depth::True);
        let t = p.theme();
        let cx = cx_for(&p, &t, width);
        let mut st = MdState::default();
        let mut prev: Vec<String> = Vec::new();
        let ends: Vec<usize> = (1..=src.len())
            .filter(|i| src.is_char_boundary(*i))
            .collect();
        for (n, end) in ends.iter().enumerate() {
            let closed = n + 1 == ends.len();
            let rows = plain(st.update(&src[..*end], &cx, closed, partial_ok || closed));
            let keep = prev.len().saturating_sub(2);
            assert!(
                rows.len() >= keep,
                "{src:?} at {end}: {} rows became {}",
                prev.len(),
                rows.len()
            );
            assert_eq!(
                &rows[..keep],
                &prev[..keep],
                "{src:?} at {end} (closed {closed})"
            );
            prev = rows;
        }
        assert_eq!(
            prev,
            md(src, width),
            "{src:?}: final rows differ from a one-shot render"
        );
    }

    #[test]
    fn reference_links_and_footnotes_keep_their_rows_when_the_definitions_arrive() {
        // Finding 23: `[the docs][1]` printed raw while streaming and became `the docs` when the
        // definition at the end made the whole answer be laid out again.
        for doc in [
            "See [the docs][1] and [other][] for more.\n\nSecond paragraph with some words that wrap around the narrow column.\n\n[1]: https://example.com/docs\n[other]: https://example.com/o\n",
            "A claim[^1] and another.\n\n[^1]: The footnote text.\n\nEnd paragraph.",
            "Intro [a link][Ref One] done.\n\nMiddle paragraph here.\n\nLast words.\n\n[ref one]: https://x.example/",
        ] {
            for width in [24, 60, 100] {
                for partial_ok in [false, true] {
                    assert_rows_stay_put_to_the_end(doc, width, partial_ok);
                }
            }
        }
    }

    #[test]
    fn stand_in_definitions_are_found_and_normalised() {
        assert_eq!(
            reference_labels("a [b][C d] e [f][] g[^1] [plain] [x](y) arr[i]"),
            ["c d", "f"]
        );
        assert_eq!(
            definition_label("  [Ref  One]: http://x"),
            Some("ref one".into())
        );
        assert_eq!(definition_label("[^1]: note"), Some("^1".into()));
        assert_eq!(definition_label("    [x]: indented code"), None);
        assert_eq!(definition_label("[]: nothing"), None);
        // a use inside an open fence gets no stand-in (it would print as code)
        let p = Palette::new(Kind::Hearth, Depth::True);
        let t = p.theme();
        let cx = cx_for(&p, &t, 60);
        let mut st = MdState::default();
        let rows = plain(st.update("```\nsee [a][1]\n", &cx, false, true));
        assert!(rows.iter().any(|r| r.contains("see [a][1]")), "{rows:?}");
        assert!(!rows.iter().any(|r| r.contains("#")), "{rows:?}");
    }
}
