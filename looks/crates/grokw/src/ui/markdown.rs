// OWNER: markdown (rules of spec 3.8; display math, mermaid and raw toggle are not built)
//! Markdown to styled rows the way Grok Build draws it: markers hidden, headings coloured by
//! level with no extra blank rows between them, `• ` bullets two columns per level, quotes with
//! a dim `│`, tables with a divider between every body row, code blocks as an unbroken fill.
//!
//! Output is one [`MdLine`] per terminal row with spans that carry only fg and modifiers; the
//! caller positions them (text starts at column 5) and paints the code fill.

use pulldown_cmark::{Alignment, CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Span;
use tuikit::width::{display_width, wrap_spans, WrapMode};

use super::syntax;
use crate::theme::{blend, Theme};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LineKind {
    Text,
    /// Filled `md_code_bg` across the full code width.
    Code,
    Table,
}

#[derive(Clone, Debug)]
pub struct MdLine {
    pub spans: Vec<Span<'static>>,
    pub kind: LineKind,
}

impl MdLine {
    fn text(spans: Vec<Span<'static>>) -> Self {
        MdLine {
            spans,
            kind: LineKind::Text,
        }
    }

    fn blank() -> Self {
        MdLine::text(Vec::new())
    }
}

#[derive(Clone, Debug)]
enum Block {
    Heading(Vec<Span<'static>>),
    Para(Vec<Span<'static>>),
    Quote(Vec<Block>),
    List {
        start: Option<u64>,
        items: Vec<Item>,
    },
    Rule,
    Table(Table),
    Code {
        lang: String,
        text: String,
    },
}

#[derive(Clone, Debug, Default)]
struct Item {
    task: Option<bool>,
    blocks: Vec<Block>,
}

#[derive(Clone, Debug, Default)]
struct Table {
    aligns: Vec<Alignment>,
    head: Vec<Vec<Span<'static>>>,
    rows: Vec<Vec<Vec<Span<'static>>>>,
}

/// Where a block is built: the container's finished blocks plus the open inline text.
enum Frame {
    Root(Vec<Block>),
    Quote(Vec<Block>),
    List {
        start: Option<u64>,
        items: Vec<Item>,
    },
    Item(Item),
}

struct Builder<'a> {
    th: &'a Theme,
    frames: Vec<Frame>,
    inline: Vec<Span<'static>>,
    styles: Vec<Style>,
    link: Option<String>,
    heading: Option<u8>,
    table: Option<Table>,
    row: Vec<Vec<Span<'static>>>,
    in_head: bool,
    code: Option<(String, String)>,
}

impl<'a> Builder<'a> {
    fn base(&self) -> Style {
        Style::new().fg(self.th.md_text)
    }

    fn cur(&self) -> Style {
        self.styles.last().copied().unwrap_or_else(|| self.base())
    }

    fn push_text(&mut self, t: &str) {
        if t.is_empty() {
            return;
        }
        let st = self.cur();
        match self.inline.last_mut() {
            Some(last) if last.style == st => last.content.to_mut().push_str(t),
            _ => self.inline.push(Span::styled(t.to_string(), st)),
        }
    }

    fn container(&mut self) -> &mut Vec<Block> {
        match self.frames.last_mut().expect("root frame") {
            Frame::Root(b) | Frame::Quote(b) => b,
            Frame::Item(i) => &mut i.blocks,
            Frame::List { .. } => unreachable!("list holds items"),
        }
    }

    fn flush_para(&mut self) {
        if self.inline.is_empty() {
            return;
        }
        let spans = std::mem::take(&mut self.inline);
        match self.heading.take() {
            Some(_) => self.container().push(Block::Heading(spans)),
            None => self.container().push(Block::Para(spans)),
        }
    }

    fn end_cell(&mut self) {
        let cell = std::mem::take(&mut self.inline);
        self.row.push(cell);
    }
}

fn parse(src: &str, th: &Theme) -> Vec<Block> {
    let opts = Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS;
    let mut b = Builder {
        th,
        frames: vec![Frame::Root(Vec::new())],
        inline: Vec::new(),
        styles: Vec::new(),
        link: None,
        heading: None,
        table: None,
        row: Vec::new(),
        in_head: false,
        code: None,
    };
    for (ev, range) in Parser::new_ext(src, opts).into_offset_iter() {
        match ev {
            Event::Start(tag) => match tag {
                Tag::Paragraph => {}
                Tag::Heading { level, .. } => {
                    b.flush_para();
                    let l = match level {
                        HeadingLevel::H1 => 1,
                        HeadingLevel::H2 => 2,
                        HeadingLevel::H3 => 3,
                        HeadingLevel::H4 => 4,
                        HeadingLevel::H5 => 5,
                        HeadingLevel::H6 => 6,
                    };
                    b.heading = Some(l);
                    let c = match l {
                        1 => th.md_h1,
                        2 => th.md_h2,
                        3 => th.md_h3,
                        4 => th.md_h4,
                        5 => th.md_h5,
                        _ => th.md_h6,
                    };
                    b.styles
                        .push(Style::new().fg(c).add_modifier(th.heading[l as usize - 1]));
                }
                Tag::BlockQuote(_) => {
                    b.flush_para();
                    b.frames.push(Frame::Quote(Vec::new()));
                }
                Tag::CodeBlock(kind) => {
                    b.flush_para();
                    let lang = match kind {
                        CodeBlockKind::Fenced(l) => {
                            l.split_whitespace().next().unwrap_or("").to_string()
                        }
                        CodeBlockKind::Indented => String::new(),
                    };
                    b.code = Some((lang, String::new()));
                }
                Tag::List(start) => {
                    b.flush_para();
                    b.frames.push(Frame::List {
                        start,
                        items: Vec::new(),
                    });
                }
                Tag::Item => b.frames.push(Frame::Item(Item::default())),
                Tag::Emphasis => {
                    let s = b.cur().add_modifier(Modifier::ITALIC);
                    b.styles.push(s);
                }
                Tag::Strong => {
                    let s = b.cur().add_modifier(Modifier::BOLD);
                    b.styles.push(s);
                }
                Tag::Strikethrough => {
                    let s = b.cur().add_modifier(Modifier::CROSSED_OUT);
                    b.styles.push(s);
                }
                Tag::Link { dest_url, .. } => {
                    let s = b.cur().fg(th.link_fg).add_modifier(Modifier::UNDERLINED);
                    b.styles.push(s);
                    b.link = Some(dest_url.to_string());
                }
                Tag::Image { .. } => {}
                Tag::Table(aligns) => {
                    b.flush_para();
                    b.table = Some(Table {
                        aligns,
                        ..Default::default()
                    });
                }
                Tag::TableHead => {
                    b.in_head = true;
                    b.row.clear();
                }
                Tag::TableRow => b.row.clear(),
                Tag::TableCell => {
                    b.inline.clear();
                    let s = if b.in_head {
                        Style::new().fg(th.md_text).add_modifier(Modifier::BOLD)
                    } else {
                        Style::new().fg(th.md_text)
                    };
                    b.styles.push(s);
                }
                _ => {}
            },
            Event::End(tag) => match tag {
                TagEnd::Paragraph => b.flush_para(),
                TagEnd::Heading(_) => {
                    b.styles.pop();
                    b.flush_para();
                }
                TagEnd::BlockQuote(_) => {
                    b.flush_para();
                    if let Some(Frame::Quote(blocks)) = b.frames.pop() {
                        b.container().push(Block::Quote(blocks));
                    }
                }
                TagEnd::CodeBlock => {
                    if let Some((lang, text)) = b.code.take() {
                        b.container().push(Block::Code { lang, text });
                    }
                }
                TagEnd::List(_) => {
                    if let Some(Frame::List { start, items }) = b.frames.pop() {
                        b.container().push(Block::List { start, items });
                    }
                }
                TagEnd::Item => {
                    b.flush_para();
                    if let Some(Frame::Item(item)) = b.frames.pop() {
                        if let Some(Frame::List { items, .. }) = b.frames.last_mut() {
                            items.push(item);
                        }
                    }
                }
                TagEnd::Emphasis | TagEnd::Strong | TagEnd::Strikethrough => {
                    b.styles.pop();
                }
                TagEnd::Link => {
                    b.styles.pop();
                    if let Some(url) = b.link.take() {
                        let st = Style::new().fg(th.md_muted);
                        // the url is shown after the label, in the muted colour
                        let shown = format!(" ({url})");
                        b.inline.push(Span::styled(shown, st));
                    }
                }
                TagEnd::TableCell => {
                    b.styles.pop();
                    b.end_cell();
                }
                TagEnd::TableHead => {
                    b.in_head = false;
                    let head = std::mem::take(&mut b.row);
                    if let Some(t) = b.table.as_mut() {
                        t.head = head;
                    }
                }
                TagEnd::TableRow => {
                    let row = std::mem::take(&mut b.row);
                    if let Some(t) = b.table.as_mut() {
                        t.rows.push(row);
                    }
                }
                TagEnd::Table => {
                    if let Some(t) = b.table.take() {
                        b.container().push(Block::Table(t));
                    }
                }
                _ => {}
            },
            Event::Text(t) => {
                if let Some((_, buf)) = b.code.as_mut() {
                    buf.push_str(&t);
                } else {
                    b.push_text(&t);
                }
            }
            Event::Code(t) => {
                let s = b.cur().fg(th.md_code).add_modifier(Modifier::BOLD);
                b.inline.push(Span::styled(t.to_string(), s));
            }
            Event::SoftBreak => {
                // a soft break stays a line break when the next line starts with an indent, a
                // quote marker or a table bar (a block continuation); otherwise it is a space
                let next = src.as_bytes().get(range.end);
                if matches!(next, Some(b' ' | b'\t' | b'>' | b'|')) {
                    b.push_text("\n");
                } else {
                    b.push_text(" ");
                }
            }
            Event::HardBreak => b.push_text("\n"),
            Event::Rule => {
                b.flush_para();
                b.container().push(Block::Rule);
            }
            Event::TaskListMarker(done) => {
                if let Some(Frame::Item(i)) = b.frames.last_mut() {
                    i.task = Some(done);
                }
            }
            Event::Html(t) | Event::InlineHtml(t) => b.push_text(&t),
            Event::FootnoteReference(t) => b.push_text(&t),
            _ => {}
        }
    }
    b.flush_para();
    while b.frames.len() > 1 {
        // an unterminated container (streaming): close it as it stands
        match b.frames.pop() {
            Some(Frame::Quote(blocks)) => b.container().push(Block::Quote(blocks)),
            Some(Frame::List { start, items }) => b.container().push(Block::List { start, items }),
            Some(Frame::Item(item)) => {
                if let Some(Frame::List { items, .. }) = b.frames.last_mut() {
                    items.push(item);
                }
            }
            _ => {}
        }
    }
    match b.frames.pop() {
        Some(Frame::Root(blocks)) => blocks,
        _ => Vec::new(),
    }
}

struct Cx<'a> {
    th: &'a Theme,
    thinking: bool,
    /// Width of the code fill.
    code_w: usize,
}

impl Cx<'_> {
    /// Thinking blocks draw the same markdown blended 30% toward the background.
    fn tone(&self, c: Color) -> Color {
        if self.thinking {
            blend(self.th.bg_base, c, 0.7)
        } else {
            c
        }
    }

    fn tone_spans(&self, spans: Vec<Span<'static>>) -> Vec<Span<'static>> {
        if !self.thinking {
            return spans;
        }
        spans
            .into_iter()
            .map(|s| {
                let fg = s.style.fg.map(|c| self.tone(c));
                let mut st = s.style;
                st.fg = fg;
                Span::styled(s.content, st)
            })
            .collect()
    }
}

fn gap_between(prev: &Block, next: &Block) -> bool {
    !matches!(
        (prev, next),
        (Block::Heading(_), Block::Heading(_)) | (Block::List { .. }, Block::List { .. })
    )
}

fn render_blocks(blocks: &[Block], width: usize, cx: &Cx, tight: bool) -> Vec<MdLine> {
    let mut out: Vec<MdLine> = Vec::new();
    for (i, b) in blocks.iter().enumerate() {
        if i > 0 && !tight && gap_between(&blocks[i - 1], b) {
            out.push(MdLine::blank());
        }
        render_block(b, width, cx, &mut out);
    }
    out
}

fn dim(c: Color) -> Style {
    Style::new().fg(c).add_modifier(Modifier::DIM)
}

fn render_block(b: &Block, width: usize, cx: &Cx, out: &mut Vec<MdLine>) {
    let th = cx.th;
    match b {
        Block::Heading(spans) | Block::Para(spans) => {
            let spans = cx.tone_spans(spans.clone());
            for line in wrap_lines(&spans, width) {
                out.push(MdLine::text(line));
            }
        }
        Block::Quote(children) => {
            let inner = render_blocks(children, width.saturating_sub(2), cx, false);
            for mut l in inner {
                let mut spans = vec![Span::styled("│", dim(cx.tone(th.md_muted))), Span::raw(" ")];
                spans.append(&mut l.spans);
                out.push(MdLine::text(spans));
            }
        }
        Block::List { start, items } => {
            for (n, item) in items.iter().enumerate() {
                let marker = match start {
                    Some(s) => format!("{}. ", s + n as u64),
                    None => "• ".to_string(),
                };
                let mw = display_width(&marker);
                let mut inner = render_blocks(&item.blocks, width.saturating_sub(mw), cx, true);
                if let Some(done) = item.task {
                    let (tag, st) = if done {
                        ("[x]", Style::new().fg(th.md_task_checked))
                    } else {
                        (
                            "[ ]",
                            Style::new()
                                .fg(th.md_task_unchecked)
                                .add_modifier(Modifier::DIM),
                        )
                    };
                    let tag = Span::styled(
                        tag.to_string(),
                        Style::new()
                            .fg(cx.tone(st.fg.unwrap_or(th.md_text)))
                            .add_modifier(st.add_modifier),
                    );
                    if inner.is_empty() {
                        inner.push(MdLine::blank());
                    }
                    inner[0].spans.insert(0, Span::raw(" "));
                    inner[0].spans.insert(0, tag);
                }
                if inner.is_empty() {
                    inner.push(MdLine::blank());
                }
                for (li, mut l) in inner.into_iter().enumerate() {
                    let mut spans = Vec::new();
                    if li == 0 {
                        spans.push(Span::styled(
                            marker.clone(),
                            Style::new().fg(cx.tone(th.md_muted)),
                        ));
                    } else if l.kind == LineKind::Text {
                        spans.push(Span::raw(" ".repeat(mw)));
                    }
                    if l.kind == LineKind::Code {
                        // code inside a list item keeps its own fill from the left margin
                        out.push(l);
                        continue;
                    }
                    spans.append(&mut l.spans);
                    out.push(MdLine {
                        spans,
                        kind: l.kind,
                    });
                }
            }
        }
        Block::Rule => out.push(MdLine::text(vec![Span::styled(
            "───",
            Style::new().fg(cx.tone(th.md_muted)),
        )])),
        Block::Table(t) => render_table(t, width, cx, out),
        Block::Code { lang, text } => {
            let fb = th.md_text;
            let lines = syntax::highlight(th.syntax(), lang, text, fb);
            for spans in lines {
                // tabs become four spaces; rows are clipped, not wrapped, at the fill width
                let spans = spans
                    .into_iter()
                    .map(|s| Span::styled(s.content.replace('\t', "    "), s.style))
                    .collect::<Vec<_>>();
                let spans = tuikit::width::clip_spans(spans, cx.code_w);
                out.push(MdLine {
                    spans: cx.tone_spans(spans),
                    kind: LineKind::Code,
                });
            }
        }
    }
}

fn wrap_lines(spans: &[Span<'static>], width: usize) -> Vec<Vec<Span<'static>>> {
    // hard breaks arrive as embedded newlines
    let mut lines: Vec<Vec<Span<'static>>> = Vec::new();
    for part in tuikit::width::split_spans_on_newline(spans.to_vec()) {
        if part.is_empty() {
            lines.push(Vec::new());
            continue;
        }
        lines.extend(wrap_spans(&part, width.max(1), WrapMode::Word));
    }
    lines
}

fn cell_width(cell: &[Span<'static>]) -> usize {
    cell.iter().map(|s| display_width(&s.content)).sum()
}

fn render_table(t: &Table, width: usize, cx: &Cx, out: &mut Vec<MdLine>) {
    let th = cx.th;
    let cols = t
        .head
        .len()
        .max(t.rows.iter().map(|r| r.len()).max().unwrap_or(0));
    if cols == 0 {
        return;
    }
    let mut w = vec![1usize; cols];
    for row in std::iter::once(&t.head).chain(t.rows.iter()) {
        for (c, cell) in row.iter().enumerate() {
            w[c] = w[c].max(cell_width(cell));
        }
    }
    let bs = dim(cx.tone(th.md_muted));
    let border = |l: &str, m: &str, r: &str| {
        let mut s = String::from(l);
        for (i, cw) in w.iter().enumerate() {
            s.push_str(&"─".repeat(cw + 2));
            s.push_str(if i + 1 == cols { r } else { m });
        }
        MdLine {
            spans: vec![Span::styled(s, bs)],
            kind: LineKind::Table,
        }
    };
    let data_row = |cells: &Vec<Vec<Span<'static>>>| {
        let mut spans = vec![Span::styled("│", bs)];
        for (c, &col_w) in w.iter().enumerate() {
            let cell = cells.get(c).cloned().unwrap_or_default();
            let cw = cell_width(&cell);
            let pad = col_w - cw.min(col_w);
            let (l, r) = match t.aligns.get(c).copied().unwrap_or(Alignment::None) {
                Alignment::Right => (pad, 0),
                Alignment::Center => (pad / 2, pad - pad / 2),
                _ => (0, pad),
            };
            spans.push(Span::raw(" ".repeat(1 + l)));
            spans.extend(cx.tone_spans(cell));
            spans.push(Span::raw(" ".repeat(r + 1)));
            spans.push(Span::styled("│", bs));
        }
        MdLine {
            spans,
            kind: LineKind::Table,
        }
    };
    let first = out.len();
    out.push(border("┌", "┬", "┐"));
    out.push(data_row(&t.head));
    // a divider sits above every body row (including the first) and none below the last
    for row in &t.rows {
        out.push(border("├", "┼", "┤"));
        out.push(data_row(row));
    }
    out.push(border("└", "┴", "┘"));
    // rows are clipped to the wrap width, never wrapped
    for l in &mut out[first..] {
        l.spans = tuikit::width::clip_spans(std::mem::take(&mut l.spans), width.max(1));
    }
}

/// Render `src` into rows. `text_w` is the prose wrap width (content minus 10), `code_w` the
/// width of the code fill (the full content width).
pub fn render(src: &str, text_w: usize, code_w: usize, th: &Theme, thinking: bool) -> Vec<MdLine> {
    let blocks = parse(src, th);
    let cx = Cx {
        th,
        thinking,
        code_w,
    };
    render_blocks(&blocks, text_w, &cx, false)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plain(lines: &[MdLine]) -> Vec<String> {
        lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect()
    }

    fn th() -> Theme {
        Theme::groknight()
    }

    #[test]
    fn headings_run_together_and_paragraph_follows_a_blank() {
        let r = render("# One\n## Two\n\ntext", 80, 100, &th(), false);
        assert_eq!(plain(&r), ["One", "Two", "", "text"]);
    }

    #[test]
    fn lists_nest_two_columns_per_level_and_ordered_lists_indent_three() {
        let r = render(
            "- a\n  - b\n    - c\n1. x\n2. y\n   1. z\n",
            80,
            100,
            &th(),
            false,
        );
        assert_eq!(
            plain(&r),
            ["• a", "  • b", "    • c", "1. x", "2. y", "   1. z"]
        );
    }

    #[test]
    fn task_items_keep_the_marker_text() {
        let r = render("- [x] Done\n- [ ] Open\n", 80, 100, &th(), false);
        assert_eq!(plain(&r), ["• [x] Done", "• [ ] Open"]);
    }

    #[test]
    fn table_has_a_divider_between_every_row_and_right_aligns() {
        let md = "| Name | Qty |\n|:--|--:|\n| apples | 3 |\n| pears | 12 |\n";
        let r = render(md, 80, 100, &th(), false);
        assert_eq!(
            plain(&r),
            [
                "┌────────┬─────┐",
                "│ Name   │ Qty │",
                "├────────┼─────┤",
                "│ apples │   3 │",
                "├────────┼─────┤",
                "│ pears  │  12 │",
                "└────────┴─────┘"
            ]
        );
    }

    #[test]
    fn quote_repeats_its_bar_on_wrapped_rows() {
        let r = render("> aaa bbb ccc ddd", 10, 100, &th(), false);
        assert_eq!(plain(&r), ["│ aaa bbb", "│ ccc ddd"]);
    }

    #[test]
    fn link_prints_text_then_url() {
        let r = render("see [docs](https://x.y/z) now", 80, 100, &th(), false);
        assert_eq!(plain(&r), ["see docs (https://x.y/z) now"]);
    }

    #[test]
    fn code_block_rows_are_code_kind_and_unwrapped() {
        let r = render("```rust\nfn main() {}\n```\n", 80, 100, &th(), false);
        assert_eq!(plain(&r), ["fn main() {}"]);
        assert_eq!(r[0].kind, LineKind::Code);
    }

    #[test]
    fn rule_is_three_cells() {
        let r = render("a\n\n---\n\nb", 80, 100, &th(), false);
        assert_eq!(plain(&r), ["a", "", "───", "", "b"]);
    }
}
