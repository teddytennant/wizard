// OWNER: markdown (Pi's Markdown component: pulldown-cmark to styled rows, highlight.js-style scopes)
//! Markdown as Pi renders it (spec 6.3, 3.4): a port of pi-tui's `Markdown` component. The source
//! is parsed with pulldown-cmark into a small block tree, each block becomes logical rows the way
//! `renderToken` builds them (blank-row rules included), and rows are wrapped like
//! `wrapTextWithAnsi`. `render` returns rows without padding; callers add it.

#![allow(clippy::while_let_on_iterator)]

use std::ops::Range;

use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use tuikit::width::{display_width, wrap_spans, WrapMode};

use super::Lines;
use crate::theme::{PiTheme, Tok};

/// Render `src` to rows no wider than `width` cells, without padding. `base` is the style plain
/// text carries (assistant text: none, user text: `userMessageText`, thinking: `thinkingText`
/// italic). A `base` whose colour is `userMessageText` turns on the user-message options: ordered
/// list markers and backslash escapes are kept as typed.
pub fn render(src: &str, width: u16, theme: &PiTheme, base: Style) -> Lines {
    let user = theme.has_bg(Tok::UserMessageBg)
        && base.fg.is_some()
        && base.fg == theme.fg(Tok::UserMessageText).fg;
    render_with(src, width, theme, base, &Options_ { preserve: user })
}

/// [`render`] with the user-message options forced on.
pub fn render_user(src: &str, width: u16, theme: &PiTheme, base: Style) -> Lines {
    render_with(src, width, theme, base, &Options_ { preserve: true })
}

/// Highlighted rows for `code` in `lang` (a fence info string or a language name). `None` when the
/// language is unknown, in which case callers use `mdCodeBlock` or `toolOutput` for every row.
pub fn highlight_code(
    lang: Option<&str>,
    code: &str,
    theme: &PiTheme,
) -> Option<Vec<Vec<Span<'static>>>> {
    super::syntax::highlight(lang?, code, theme)
}

/// Language name for a file path by extension (spec 3.4), `None` when unknown.
pub fn lang_for_path(path: &str) -> Option<&'static str> {
    super::syntax::lang_for_path(path)
}

#[allow(non_camel_case_types)]
struct Options_ {
    /// `preserveOrderedListMarkers` and `preserveBackslashEscapes`.
    preserve: bool,
}

// ---- block tree --------------------------------------------------------------------------

#[derive(Debug, Clone)]
enum Inl {
    Text(String),
    Code(String),
    Strong(Vec<Inl>),
    Em(Vec<Inl>),
    Del(Vec<Inl>),
    Link { href: String, kids: Vec<Inl> },
    Br,
}

#[derive(Debug)]
enum Kind {
    Heading(u8, Vec<Inl>),
    Para(Vec<Inl>),
    Code {
        lang: String,
        text: String,
    },
    List(List),
    Table(Table),
    Quote(Vec<Blk>),
    Hr,
    Html(String),
    /// `[label]: url`, which draws nothing but is a token, so the blank lines around it count.
    Def,
}

#[derive(Debug)]
struct Blk {
    kind: Kind,
    /// A blank line follows in the source, which marked turns into a `space` token.
    space_after: bool,
    /// The source text of a table, for the too-narrow fallback.
    raw: String,
}

#[derive(Debug)]
struct List {
    ordered: bool,
    start: u64,
    loose: bool,
    items: Vec<Item>,
}

#[derive(Debug)]
struct Item {
    task: Option<bool>,
    /// The marker as typed (`-`, `1.`, `2)`), for user messages.
    marker: Option<String>,
    blocks: Vec<Blk>,
}

#[derive(Debug)]
struct Table {
    header: Vec<Vec<Inl>>,
    rows: Vec<Vec<Vec<Inl>>>,
}

type Ev<'a> = (Event<'a>, Range<usize>);

struct P<'a> {
    src: &'a str,
    it: std::iter::Peekable<std::vec::IntoIter<Ev<'a>>>,
    preserve: bool,
    /// Link reference definitions, which pulldown-cmark swallows and marked keeps as `def` tokens.
    defs: Vec<Range<usize>>,
    /// How deep in blocks (quote, list item) the parser is; defs are only top-level here.
    depth: usize,
}

fn newline_run(src: &str, from: usize) -> usize {
    src[from..]
        .chars()
        .take_while(|c| matches!(c, ' ' | '\t' | '\n' | '\r'))
        .filter(|c| *c == '\n')
        .count()
}

impl<'a> P<'a> {
    fn inlines(&mut self, until: TagEnd) -> Vec<Inl> {
        let mut out: Vec<Inl> = Vec::new();
        while let Some((ev, range)) = self.it.next() {
            match ev {
                Event::End(e) if e == until => break,
                Event::End(_) => break,
                Event::Text(t) => {
                    // pulldown drops the backslash of an escape and starts the text after it
                    let escaped = self.preserve
                        && range.start > 0
                        && self.src.as_bytes()[range.start - 1] == b'\\'
                        && self.src[range.clone()].chars().next() == t.chars().next();
                    if escaped {
                        push_text(&mut out, &format!("\\{t}"));
                    } else {
                        push_text(&mut out, &t);
                    }
                }
                Event::Code(c) => out.push(Inl::Code(c.to_string())),
                Event::SoftBreak | Event::HardBreak => out.push(Inl::Br),
                Event::Html(h) | Event::InlineHtml(h) => push_text(&mut out, &h),
                Event::Start(Tag::Strong) => {
                    let k = self.inlines(TagEnd::Strong);
                    out.push(Inl::Strong(k));
                }
                Event::Start(Tag::Emphasis) => {
                    let k = self.inlines(TagEnd::Emphasis);
                    out.push(Inl::Em(k));
                }
                Event::Start(Tag::Strikethrough) => {
                    let k = self.inlines(TagEnd::Strikethrough);
                    out.push(Inl::Del(k));
                }
                Event::Start(Tag::Link { dest_url, .. }) => {
                    let k = self.inlines(TagEnd::Link);
                    out.push(Inl::Link {
                        href: dest_url.to_string(),
                        kids: k,
                    });
                }
                // marked has an `image` token, and Pi's inline renderer has no case for it: the
                // fallback prints the token's text, which is the alt text, plain
                Event::Start(Tag::Image { .. }) => {
                    let k = self.inlines(TagEnd::Image);
                    push_text(&mut out, &plain_text(&k));
                }
                Event::Start(_) => {
                    // anything else inline-shaped: keep its text
                    let k = self.inlines(TagEnd::Paragraph);
                    out.extend(k);
                }
                Event::TaskListMarker(_) | Event::Rule | Event::FootnoteReference(_) => {}
                _ => {}
            }
        }
        out
    }

    /// Definitions that start before `at` (all of them with `usize::MAX`), as blocks.
    fn take_defs(&mut self, at: usize, out: &mut Vec<Blk>) {
        while self.defs.first().is_some_and(|d| d.start < at) {
            let d = self.defs.remove(0);
            let space = self.space_after(&d, 2);
            out.push(Blk::new(Kind::Def, space));
        }
    }

    fn blocks(&mut self, _in_item: bool) -> Vec<Blk> {
        let top = self.depth == 0;
        self.depth += 1;
        let out = self.blocks_inner(top);
        self.depth -= 1;
        out
    }

    fn blocks_inner(&mut self, top: bool) -> Vec<Blk> {
        let mut out: Vec<Blk> = Vec::new();
        while let Some((ev, range)) = self.it.next() {
            if top && !matches!(ev, Event::End(_)) {
                self.take_defs(range.start, &mut out);
            }
            match ev {
                Event::End(_) => break,
                Event::Start(Tag::Paragraph) => {
                    let k = self.inlines(TagEnd::Paragraph);
                    let space = self.space_after(&range, 2);
                    out.push(Blk::new(Kind::Para(k), space));
                }
                Event::Start(Tag::Heading { level, .. }) => {
                    let k = self.inlines(TagEnd::Heading(level));
                    out.push(Blk::new(Kind::Heading(level_n(level), k), false));
                }
                Event::Start(Tag::BlockQuote(_)) => {
                    let inner = self.blocks(false);
                    let space = self.space_after(&range, 3);
                    out.push(Blk::new(Kind::Quote(inner), space));
                }
                Event::Start(Tag::CodeBlock(kind)) => {
                    let lang = match &kind {
                        CodeBlockKind::Fenced(l) => {
                            l.split_whitespace().next().unwrap_or("").to_string()
                        }
                        CodeBlockKind::Indented => String::new(),
                    };
                    let mut text = String::new();
                    while let Some((ev, _)) = self.it.next() {
                        match ev {
                            Event::End(TagEnd::CodeBlock) => break,
                            Event::Text(t) => text.push_str(&t),
                            _ => {}
                        }
                    }
                    let fenced = matches!(kind, CodeBlockKind::Fenced(_));
                    let raw = &self.src[range.clone()];
                    if fenced {
                        trim_partial_fence(raw, &mut text);
                    }
                    if text.ends_with('\n') {
                        text.pop();
                    }
                    out.push(Blk::new(Kind::Code { lang, text }, false));
                }
                Event::Start(Tag::List(start)) => {
                    let list = self.list(start, &range);
                    let space = self.space_after(&range, 2);
                    out.push(Blk::new(Kind::List(list), space));
                }
                Event::Start(Tag::Table(_)) => {
                    let t = self.table();
                    let mut b = Blk::new(Kind::Table(t), false);
                    b.raw = self.src[range.clone()].trim_end().to_string();
                    out.push(b);
                }
                Event::Start(Tag::HtmlBlock) => {
                    let mut html = String::new();
                    while let Some((ev, _)) = self.it.next() {
                        match ev {
                            Event::End(TagEnd::HtmlBlock) => break,
                            Event::Html(t) | Event::Text(t) => html.push_str(&t),
                            _ => {}
                        }
                    }
                    let space = self.space_after(&range, 2);
                    out.push(Blk::new(Kind::Html(html.trim().to_string()), space));
                }
                Event::Rule => out.push(Blk::new(Kind::Hr, false)),
                // loose text directly inside a tight list item
                Event::Text(_)
                | Event::Code(_)
                | Event::SoftBreak
                | Event::HardBreak
                | Event::InlineHtml(_)
                | Event::Html(_)
                | Event::Start(Tag::Strong)
                | Event::Start(Tag::Emphasis)
                | Event::Start(Tag::Strikethrough)
                | Event::Start(Tag::Link { .. })
                | Event::Start(Tag::Image { .. }) => {
                    let mut inl = Vec::new();
                    self.push_back(ev, range);
                    self.tight_text(&mut inl);
                    out.push(Blk::new(Kind::Para(inl), false));
                }
                Event::TaskListMarker(_) => {}
                _ => {}
            }
        }
        if top {
            self.take_defs(usize::MAX, &mut out);
        }
        out
    }

    fn push_back(&mut self, ev: Event<'a>, range: Range<usize>) {
        let rest: Vec<Ev<'a>> = std::iter::once((ev, range))
            .chain(self.it.by_ref())
            .collect();
        self.it = rest.into_iter().peekable();
    }

    /// Inline events of a tight list item, up to the next block-level event.
    fn tight_text(&mut self, out: &mut Vec<Inl>) {
        let mut inner: Vec<Ev<'a>> = Vec::new();
        let mut depth = 0i32;
        while let Some((ev, r)) = self.it.peek().cloned() {
            let stop = match &ev {
                Event::Start(Tag::Strong)
                | Event::Start(Tag::Emphasis)
                | Event::Start(Tag::Strikethrough)
                | Event::Start(Tag::Link { .. })
                | Event::Start(Tag::Image { .. }) => {
                    depth += 1;
                    false
                }
                Event::End(TagEnd::Strong)
                | Event::End(TagEnd::Emphasis)
                | Event::End(TagEnd::Strikethrough)
                | Event::End(TagEnd::Link)
                | Event::End(TagEnd::Image) => {
                    depth -= 1;
                    false
                }
                Event::Text(_)
                | Event::Code(_)
                | Event::SoftBreak
                | Event::HardBreak
                | Event::InlineHtml(_)
                | Event::Html(_) => false,
                _ => true,
            };
            if stop || depth < 0 {
                break;
            }
            self.it.next();
            inner.push((ev, r));
        }
        let mut sub = P {
            src: self.src,
            it: inner.into_iter().peekable(),
            preserve: self.preserve,
            defs: Vec::new(),
            depth: 1,
        };
        // a sentinel end stops the loop; the events are consumed to exhaustion
        out.extend(sub.inlines(TagEnd::Paragraph));
    }

    fn list(&mut self, start: Option<u64>, range: &Range<usize>) -> List {
        let mut items = Vec::new();
        let mut loose = false;
        while let Some((ev, r)) = self.it.next() {
            match ev {
                Event::End(TagEnd::List(_)) | Event::End(_) => break,
                Event::Start(Tag::Item) => {
                    let mut task = None;
                    if let Some((Event::TaskListMarker(c), _)) = self.it.peek() {
                        task = Some(*c);
                    }
                    if matches!(self.it.peek(), Some((Event::Start(Tag::Paragraph), _))) {
                        loose = true;
                    }
                    let marker = item_marker(&self.src[r.start..], start.is_some());
                    let blocks = self.blocks(true);
                    items.push(Item {
                        task,
                        marker,
                        blocks,
                    });
                }
                _ => {}
            }
        }
        // marked calls a list loose when any item is separated from the next by a blank line
        let _ = range;
        List {
            ordered: start.is_some(),
            start: start.unwrap_or(1),
            loose,
            items,
        }
    }

    fn table(&mut self) -> Table {
        let mut header = Vec::new();
        let mut rows = Vec::new();
        while let Some((ev, _)) = self.it.next() {
            match ev {
                Event::End(TagEnd::Table) => break,
                Event::Start(Tag::TableHead) => {
                    while let Some((ev, _)) = self.it.next() {
                        match ev {
                            Event::End(TagEnd::TableHead) => break,
                            Event::Start(Tag::TableCell) => {
                                header.push(self.inlines(TagEnd::TableCell))
                            }
                            _ => {}
                        }
                    }
                }
                Event::Start(Tag::TableRow) => {
                    let mut row = Vec::new();
                    while let Some((ev, _)) = self.it.next() {
                        match ev {
                            Event::End(TagEnd::TableRow) => break,
                            Event::Start(Tag::TableCell) => {
                                row.push(self.inlines(TagEnd::TableCell))
                            }
                            _ => {}
                        }
                    }
                    rows.push(row);
                }
                _ => {}
            }
        }
        Table { header, rows }
    }

    /// Does marked emit a `space` token after the block that ended at `range.end`? `need` is the
    /// number of newlines in the whitespace that follows the block's last character (2 for a
    /// paragraph: one blank line; a block quote swallows one of its own).
    fn space_after(&self, range: &Range<usize>, need: usize) -> bool {
        let end = self.src[..range.end].trim_end().len();
        newline_run(self.src, end) >= need
    }
}

impl Blk {
    fn new(kind: Kind, space_after: bool) -> Blk {
        Blk {
            kind,
            space_after,
            raw: String::new(),
        }
    }
}

fn level_n(l: HeadingLevel) -> u8 {
    match l {
        HeadingLevel::H1 => 1,
        HeadingLevel::H2 => 2,
        HeadingLevel::H3 => 3,
        HeadingLevel::H4 => 4,
        HeadingLevel::H5 => 5,
        HeadingLevel::H6 => 6,
    }
}

/// A list marker as the renderer writes it: `- `, `1. `, `2) ` and a task box after it.
fn looks_like_marker(text: &str) -> bool {
    let Some(t) = text.strip_suffix(' ') else {
        return false;
    };
    let t = t
        .strip_suffix(" [ ]")
        .or_else(|| t.strip_suffix(" [x]"))
        .unwrap_or(t);
    matches!(t, "-" | "+" | "*")
        || t.strip_suffix(['.', ')'])
            .is_some_and(|d| !d.is_empty() && d.chars().all(|c| c.is_ascii_digit()))
}

/// `1.`, `2)`, `-`, `*` or `+` at the start of a list item's source.
fn item_marker(s: &str, ordered: bool) -> Option<String> {
    let t = s.trim_start_matches(' ');
    if ordered {
        let d: String = t.chars().take_while(|c| c.is_ascii_digit()).collect();
        let c = t[d.len()..].chars().next()?;
        (!d.is_empty() && matches!(c, '.' | ')')).then(|| format!("{d}{c}"))
    } else {
        let c = t.chars().next()?;
        matches!(c, '-' | '+' | '*').then(|| c.to_string())
    }
}

/// While streaming, a closing fence arrives one backtick at a time; drop the partial one.
fn trim_partial_fence(raw: &str, text: &mut String) {
    let t = raw.trim_start();
    let marker: String = t.chars().take_while(|c| *c == '`' || *c == '~').collect();
    if marker.len() < 3 {
        return;
    }
    let last = raw.rsplit('\n').next().unwrap_or("");
    let mc = marker.chars().next().unwrap();
    if last.is_empty()
        || last.len() >= marker.len()
        || !last.chars().all(|c| c == mc)
        || !text.ends_with(last)
    {
        return;
    }
    let keep = text.len() - last.len();
    text.truncate(keep);
    if text.ends_with('\n') {
        text.pop();
    }
}

/// Plain text with bare `http(s)://` URLs turned into links, as marked's GFM autolinks do.
fn push_text(out: &mut Vec<Inl>, s: &str) {
    let mut rest = s;
    loop {
        let Some(at) = find_url(rest) else {
            if !rest.is_empty() {
                out.push(Inl::Text(rest.to_string()));
            }
            return;
        };
        if at > 0 {
            out.push(Inl::Text(rest[..at].to_string()));
        }
        let tail = &rest[at..];
        let mut end = tail
            .find(|c: char| c.is_whitespace() || c == '<')
            .unwrap_or(tail.len());
        // trailing punctuation is not part of the address
        while end > 0 {
            let c = tail[..end].chars().next_back().unwrap();
            let strip = matches!(
                c,
                '.' | ',' | ':' | ';' | '!' | '?' | '"' | '\'' | '*' | '_' | '~'
            ) || (c == ')'
                && tail[..end].matches('(').count() < tail[..end].matches(')').count());
            if strip {
                end -= c.len_utf8();
            } else {
                break;
            }
        }
        if end < "https://".len() {
            out.push(Inl::Text(tail[..1].to_string()));
            rest = &tail[1..];
            continue;
        }
        let url = &tail[..end];
        out.push(Inl::Link {
            href: url.to_string(),
            kids: vec![Inl::Text(url.to_string())],
        });
        rest = &tail[end..];
    }
}

fn find_url(s: &str) -> Option<usize> {
    let mut from = 0;
    while let Some(i) = s[from..].find("http") {
        let at = from + i;
        let t = &s[at..];
        let ok_start = at == 0
            || s[..at]
                .chars()
                .next_back()
                .is_some_and(|c| !c.is_alphanumeric());
        if ok_start && (t.starts_with("https://") || t.starts_with("http://")) {
            return Some(at);
        }
        from = at + 4;
    }
    None
}

fn parse(src: &str, preserve: bool) -> Vec<Blk> {
    let opts = Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS;
    let parser = Parser::new_ext(src, opts);
    let mut defs: Vec<Range<usize>> = parser
        .reference_definitions()
        .iter()
        .map(|(_, d)| d.span.clone())
        .collect();
    defs.sort_by_key(|d| d.start);
    let evs: Vec<Ev> = parser.into_offset_iter().collect();
    let mut p = P {
        src,
        it: evs.into_iter().peekable(),
        preserve,
        defs,
        depth: 0,
    };
    let mut out = Vec::new();
    while p.it.peek().is_some() {
        let before = out.len();
        let mut b = p.blocks(false);
        out.append(&mut b);
        if out.len() == before && p.it.peek().is_none() {
            break;
        }
    }
    out
}

// ---- rendering ---------------------------------------------------------------------------

type Row = Vec<Span<'static>>;

struct R<'t> {
    th: &'t PiTheme,
    base: Style,
    preserve: bool,
}

const NL: &str = "\n";

fn push_str(out: &mut Row, text: &str, style: Style) {
    let mut first = true;
    for seg in text.split('\n') {
        if !first {
            out.push(Span::raw(NL));
        }
        first = false;
        if !seg.is_empty() {
            out.push(Span::styled(seg.to_string(), style));
        }
    }
}

/// `outer` underneath whatever each span already carries (inner SGR wins).
fn under(outer: Style, spans: Row) -> Row {
    spans
        .into_iter()
        .map(|s| {
            if s.content == NL {
                s
            } else {
                Span::styled(s.content, outer.patch(s.style))
            }
        })
        .collect()
}

fn plain_text(k: &[Inl]) -> String {
    let mut s = String::new();
    for i in k {
        match i {
            Inl::Text(t) | Inl::Code(t) => s.push_str(t),
            Inl::Strong(k) | Inl::Em(k) | Inl::Del(k) => s.push_str(&plain_text(k)),
            Inl::Link { kids, .. } => s.push_str(&plain_text(kids)),
            Inl::Br => s.push('\n'),
        }
    }
    s
}

/// Split a row on its newline spans and wrap each part to `width`.
fn wrap_row(row: &Row, width: usize) -> Vec<Row> {
    let mut out = Vec::new();
    let mut cur: Row = Vec::new();
    let flush = |cur: &mut Row, out: &mut Vec<Row>| {
        if cur.is_empty() {
            out.push(Vec::new());
        } else {
            out.extend(wrap_spans(cur, width.max(1), WrapMode::Word));
        }
        cur.clear();
    };
    for s in row {
        if s.content == NL {
            flush(&mut cur, &mut out);
        } else {
            cur.push(s.clone());
        }
    }
    flush(&mut cur, &mut out);
    out
}

fn row_width(r: &Row) -> usize {
    r.iter().map(|s| display_width(&s.content)).sum()
}

impl<'t> R<'t> {
    fn fg(&self, t: Tok) -> Style {
        self.th.fg(t)
    }

    fn inline(&self, items: &[Inl], ctx: Style) -> Row {
        let mut out: Row = Vec::new();
        for it in items {
            match it {
                Inl::Text(t) => push_str(&mut out, t, ctx),
                Inl::Br => out.push(Span::raw(NL)),
                Inl::Code(c) => push_str(&mut out, c, self.fg(Tok::MdCode)),
                Inl::Strong(k) => {
                    let inner = self.inline(k, ctx);
                    out.extend(under(Style::default().add_modifier(Modifier::BOLD), inner));
                }
                Inl::Em(k) => {
                    let inner = self.inline(k, ctx);
                    out.extend(under(
                        Style::default().add_modifier(Modifier::ITALIC),
                        inner,
                    ));
                }
                Inl::Del(k) => {
                    let inner = self.inline(k, ctx);
                    out.extend(under(
                        Style::default().add_modifier(Modifier::CROSSED_OUT),
                        inner,
                    ));
                }
                Inl::Link { href, kids } => {
                    let inner = self.inline(kids, ctx);
                    let st = self.fg(Tok::MdLink).add_modifier(Modifier::UNDERLINED);
                    out.extend(under(st, inner));
                    let text = plain_text(kids);
                    let cmp = href.strip_prefix("mailto:").unwrap_or(href);
                    if text != *href && text != cmp {
                        push_str(&mut out, &format!(" ({href})"), self.fg(Tok::MdLinkUrl));
                    }
                }
            }
        }
        out
    }

    /// Rows for a run of blocks. `top` says whether spacing follows the top-level rules.
    fn blocks(
        &self,
        blocks: &[Blk],
        width: usize,
        ctx: Style,
        quote: bool,
        item: bool,
    ) -> Vec<Row> {
        let mut out: Vec<Row> = Vec::new();
        for (i, b) in blocks.iter().enumerate() {
            let next = blocks.get(i + 1);
            let next_is_list = next.is_some_and(|n| matches!(n.kind, Kind::List(_)));
            // marked emits a `space` token after a block when a blank line follows; the space
            // token renders one blank row, and the block itself then adds none
            let mut space = b.space_after
                || (quote && next.is_some() && !next_is_list && matches!(b.kind, Kind::Para(_)));
            // the last block of a list item or quote carries no trailing blank row
            if next.is_none() && (item || quote) {
                space = false;
            }
            let more = next.is_some();
            let gap = |kind_wants: bool| space || (more && kind_wants);
            match &b.kind {
                Kind::Heading(level, inl) => {
                    let mut st = self.fg(Tok::MdHeading).add_modifier(Modifier::BOLD);
                    if *level == 1 {
                        st = st.add_modifier(Modifier::UNDERLINED);
                    }
                    let mut row = Vec::new();
                    if *level >= 3 {
                        push_str(&mut row, &format!("{} ", "#".repeat(*level as usize)), st);
                    }
                    row.extend(self.inline(inl, st));
                    out.push(row);
                    if gap(true) {
                        out.push(Vec::new());
                    }
                }
                Kind::Para(inl) => {
                    out.push(self.inline(inl, ctx));
                    if space || (more && !next_is_list) {
                        out.push(Vec::new());
                    }
                }
                Kind::Code { lang, text } => {
                    let border = self.fg(Tok::MdCodeBlockBorder);
                    out.push(vec![Span::styled(format!("```{lang}"), border)]);
                    let hl = if lang.is_empty() {
                        None
                    } else {
                        super::syntax::highlight(lang, text, self.th)
                    };
                    match hl {
                        Some(lines) => {
                            for mut l in lines {
                                l.insert(0, Span::raw("  "));
                                out.push(l);
                            }
                        }
                        None => {
                            for l in text.split('\n') {
                                let mut row = vec![Span::raw("  ")];
                                if !l.is_empty() {
                                    row.push(Span::styled(
                                        l.to_string(),
                                        self.fg(Tok::MdCodeBlock),
                                    ));
                                }
                                out.push(row);
                            }
                        }
                    }
                    out.push(vec![Span::styled("```", border)]);
                    if gap(true) {
                        out.push(Vec::new());
                    }
                }
                Kind::List(l) => {
                    out.extend(self.list(l, 0, width, ctx));
                    if space {
                        out.push(Vec::new());
                    }
                }
                Kind::Table(t) => {
                    out.extend(self.table(t, &b.raw, width, ctx, more && !space));
                    if space {
                        out.push(Vec::new());
                    }
                }
                Kind::Quote(inner) => {
                    let qs = self.fg(Tok::MdQuote).add_modifier(Modifier::ITALIC);
                    let qw = width.saturating_sub(2).max(1);
                    let mut rows = self.blocks(inner, qw, Style::default(), true, false);
                    while rows.last().is_some_and(|r| r.is_empty()) {
                        rows.pop();
                    }
                    for r in rows {
                        let styled = self.under_quote(qs, r);
                        for w in wrap_row(&styled, qw) {
                            let mut line = vec![Span::styled("│ ", self.fg(Tok::MdQuoteBorder))];
                            line.extend(w);
                            out.push(line);
                        }
                    }
                    if gap(true) {
                        out.push(Vec::new());
                    }
                }
                Kind::Hr => {
                    out.push(vec![Span::styled(
                        "─".repeat(width.min(80)),
                        self.fg(Tok::MdHr),
                    )]);
                    if gap(true) {
                        out.push(Vec::new());
                    }
                }
                Kind::Html(h) => {
                    let mut row = Vec::new();
                    push_str(&mut row, h, ctx);
                    out.push(row);
                    if space {
                        out.push(Vec::new());
                    }
                }
                // a `def` token has no text, so only the `space` token after it shows
                Kind::Def => {
                    if space {
                        out.push(Vec::new());
                    }
                }
            }
        }
        out
    }

    /// Pi styles a quote row as a whole, `quote(italic(row))`, and a list bullet inside it ends
    /// with an SGR 39 that drops the quote colour for the rest of the row. Inline tokens put the
    /// quote's style back after themselves, plain text does not, so the text right after a bullet
    /// is italic in the default colour, and what follows the first styled token is quote coloured
    /// again.
    fn under_quote(&self, qs: Style, row: Row) -> Row {
        let is_bullet = |s: &Span<'_>| {
            s.style.fg == self.fg(Tok::MdListBullet).fg && looks_like_marker(&s.content)
        };
        let no_fg = Style { fg: None, ..qs };
        let mut lost = false;
        let mut out = Vec::with_capacity(row.len());
        for s in row {
            if s.content == NL {
                out.push(s);
                continue;
            }
            if is_bullet(&s) {
                out.push(Span::styled(s.content, qs.patch(s.style)));
                lost = true;
                continue;
            }
            let plain = s.style == Style::default();
            let under = if lost { no_fg } else { qs };
            out.push(Span::styled(s.content, under.patch(s.style)));
            if !plain {
                lost = false;
            }
        }
        out
    }

    fn list(&self, l: &List, depth: usize, width: usize, ctx: Style) -> Vec<Row> {
        let mut out: Vec<Row> = Vec::new();
        let indent = " ".repeat(4 * depth);
        for (i, item) in l.items.iter().enumerate() {
            let last = i + 1 == l.items.len();
            let bullet = match (&item.marker, self.preserve) {
                (Some(m), true) => format!("{m} "),
                _ if l.ordered => format!("{}. ", l.start + i as u64),
                _ => "- ".to_string(),
            };
            let task = item.task.map_or(String::new(), |c| {
                format!("[{}] ", if c { "x" } else { " " })
            });
            let marker = format!("{bullet}{task}");
            let first_prefix_w = indent.len() + display_width(&marker);
            let cont = " ".repeat(first_prefix_w);
            let item_w = width.saturating_sub(first_prefix_w).max(1);
            let mut any = false;
            for (bi, blk) in item.blocks.iter().enumerate() {
                if let Kind::List(sub) = &blk.kind {
                    out.extend(self.list(sub, depth + 1, width, ctx));
                    any = true;
                    continue;
                }
                let mut rows = self.blocks(std::slice::from_ref(blk), item_w, ctx, false, true);
                // a blank line between two blocks of one item is a `space` token
                if blk.space_after && bi + 1 < item.blocks.len() {
                    rows.push(Vec::new());
                }
                for r in rows {
                    for w in wrap_row(&r, item_w) {
                        let mut line: Row = Vec::new();
                        if any {
                            line.push(Span::raw(cont.clone()));
                        } else {
                            line.push(Span::raw(indent.clone()));
                            line.push(Span::styled(marker.clone(), self.fg(Tok::MdListBullet)));
                        }
                        line.extend(w);
                        out.push(line);
                        any = true;
                    }
                }
            }
            if !any {
                out.push(vec![
                    Span::raw(indent.clone()),
                    Span::styled(marker, self.fg(Tok::MdListBullet)),
                ]);
            }
            if l.loose && !last {
                out.push(Vec::new());
            }
        }
        out
    }

    fn table(&self, t: &Table, raw: &str, avail: usize, ctx: Style, gap: bool) -> Vec<Row> {
        let ncols = t.header.len();
        if ncols == 0 {
            return Vec::new();
        }
        let overhead = 3 * ncols + 1;
        let for_cells = avail.saturating_sub(overhead);
        if avail < overhead || for_cells < ncols {
            let mut row = Vec::new();
            push_str(&mut row, raw, ctx);
            let mut rows = wrap_row(&row, avail);
            if gap {
                rows.push(Vec::new());
            }
            return rows;
        }
        const MAX_WORD: usize = 30;
        let cell = |c: &Vec<Inl>| self.inline(c, ctx);
        let longest_word = |r: &Row| -> usize {
            let text: String = r.iter().map(|s| s.content.as_ref()).collect();
            text.split_whitespace()
                .map(display_width)
                .max()
                .unwrap_or(0)
                .min(MAX_WORD)
        };
        let mut natural = vec![0usize; ncols];
        let mut min_word = vec![1usize; ncols];
        let header: Vec<Row> = t.header.iter().map(cell).collect();
        for (i, h) in header.iter().enumerate() {
            natural[i] = row_width(h);
            min_word[i] = longest_word(h).max(1);
        }
        let body: Vec<Vec<Row>> = t
            .rows
            .iter()
            .map(|r| r.iter().map(cell).collect())
            .collect();
        for r in &body {
            for (i, c) in r.iter().enumerate().take(ncols) {
                natural[i] = natural[i].max(row_width(c));
                min_word[i] = min_word[i].max(longest_word(c));
            }
        }
        let mut min_cols = min_word.clone();
        let mut min_total: usize = min_cols.iter().sum();
        if min_total > for_cells {
            min_cols = vec![1; ncols];
            let remaining = for_cells.saturating_sub(ncols);
            if remaining > 0 {
                let weights: Vec<usize> = min_word.iter().map(|w| w.saturating_sub(1)).collect();
                let total_w: usize = weights.iter().sum();
                let growth: Vec<usize> = weights
                    .iter()
                    .map(|w| (w * remaining).checked_div(total_w).unwrap_or(0))
                    .collect();
                for i in 0..ncols {
                    min_cols[i] += growth[i];
                }
                let mut left = remaining - growth.iter().sum::<usize>();
                let mut i = 0;
                while left > 0 && i < ncols {
                    min_cols[i] += 1;
                    left -= 1;
                    i += 1;
                }
            }
            min_total = min_cols.iter().sum();
        }
        let total_natural: usize = natural.iter().sum::<usize>() + overhead;
        let widths: Vec<usize> = if total_natural <= avail {
            natural
                .iter()
                .zip(&min_cols)
                .map(|(n, m)| *n.max(m))
                .collect()
        } else {
            let potential: usize = natural
                .iter()
                .zip(&min_cols)
                .map(|(n, m)| n.saturating_sub(*m))
                .sum();
            let extra = for_cells.saturating_sub(min_total);
            let mut w: Vec<usize> = min_cols
                .iter()
                .zip(&natural)
                .map(|(m, n)| {
                    let delta = n.saturating_sub(*m);
                    let grow = (delta * extra).checked_div(potential).unwrap_or(0);
                    m + grow
                })
                .collect();
            let mut remaining = for_cells.saturating_sub(w.iter().sum());
            while remaining > 0 {
                let mut grew = false;
                for i in 0..ncols {
                    if remaining > 0 && w[i] < natural[i] {
                        w[i] += 1;
                        remaining -= 1;
                        grew = true;
                    }
                }
                if !grew {
                    break;
                }
            }
            w
        };
        let rule = |l: &str, m: &str, r: &str| -> Row {
            let cells: Vec<String> = widths.iter().map(|w| "─".repeat(*w)).collect();
            vec![Span::raw(format!(
                "{l}─{}─{r}",
                cells.join(&format!("─{m}─"))
            ))]
        };
        let mut out: Vec<Row> = vec![rule("┌", "┬", "┐")];
        let emit = |cells: &[Row], bold: bool, out: &mut Vec<Row>| {
            let wrapped: Vec<Vec<Row>> = cells
                .iter()
                .enumerate()
                .map(|(i, c)| wrap_row(c, widths[i]))
                .collect();
            let n = wrapped.iter().map(|c| c.len()).max().unwrap_or(1);
            for li in 0..n {
                let mut line: Row = vec![Span::raw("│ ")];
                for (ci, w) in widths.iter().enumerate() {
                    if ci > 0 {
                        line.push(Span::raw(" │ "));
                    }
                    let mut cellrow = wrapped
                        .get(ci)
                        .and_then(|c| c.get(li))
                        .cloned()
                        .unwrap_or_default();
                    let pad = w.saturating_sub(row_width(&cellrow));
                    cellrow.push(Span::raw(" ".repeat(pad)));
                    if bold {
                        cellrow = under(Style::default().add_modifier(Modifier::BOLD), cellrow);
                    }
                    line.extend(cellrow);
                }
                line.push(Span::raw(" │"));
                out.push(line);
            }
        };
        emit(&header, true, &mut out);
        out.push(rule("├", "┼", "┤"));
        for (ri, r) in body.iter().enumerate() {
            let mut cells = r.clone();
            cells.resize(ncols, Vec::new());
            emit(&cells, false, &mut out);
            if ri + 1 < body.len() {
                out.push(rule("├", "┼", "┤"));
            }
        }
        out.push(rule("└", "┴", "┘"));
        if gap {
            out.push(Vec::new());
        }
        out
    }
}

fn render_with(src: &str, width: u16, theme: &PiTheme, base: Style, o: &Options_) -> Lines {
    if src.trim().is_empty() {
        return Vec::new();
    }
    let w = width.max(1) as usize;
    let text = src.replace('\t', "   ");
    let blocks = parse(&text, o.preserve);
    let r = R {
        th: theme,
        base,
        preserve: o.preserve,
    };
    let rows = r.blocks(&blocks, w, r.base, false, false);
    let mut out: Lines = Vec::new();
    for row in rows {
        for wr in wrap_row(&row, w) {
            out.push(Line::from(wr));
        }
    }
    out
}
