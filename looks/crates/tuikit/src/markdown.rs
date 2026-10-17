//! Markdown to styled lines at a given width.
//!
//! [`render`] turns a whole document into `Line`s; [`StreamingMarkdown`] re-renders a growing
//! string by caching everything before the last safe block boundary, so appending a token to a
//! 100 KB reply costs one block, not the document.

use crate::syntax;
use crate::theme::Theme;
use crate::width::{
    display_width, sanitize, spans_width, split_spans_on_newline, wrap_spans, WrapMode,
};
use pulldown_cmark::{Alignment, CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

#[derive(Clone, Copy, Debug)]
pub struct MarkdownOptions {
    /// Hide markup that only exists for the source: heading `#`s and code fences.
    pub conceal: bool,
    /// Paint fenced code on `backgroundPanel`, padded to the full width.
    pub code_bg: bool,
    /// Hard-wrap long code lines instead of letting the viewport clip them.
    pub wrap_code: bool,
    /// opencode's `grid` tables: no cell padding, columns stretched to fill the whole width,
    /// a rule between every row and bold heading-colored header cells.
    pub grid_tables: bool,
    /// Keep a single newline inside a paragraph as a line break instead of a space, as
    /// opencode's renderer does (models separate lines with plain newlines).
    pub soft_break_newline: bool,
    /// opencode's inline quirks: `~~strike~~` is just muted text, and a task list item shows as
    /// a plain bullet with no checkbox.
    pub opencode_inline: bool,
    /// Inside quotes and list items, a blank row separates two blocks only when the source has a
    /// blank line there (top-level blocks always get one), and a task item shows its checkbox
    /// character without the brackets. This is what opencode's renderer does.
    pub opencode_blocks: bool,
    /// Wrap the way opentui does: also after `/ - . , ; : ? ( ) [ ] { } \\` inside a word.
    pub punct_wrap: bool,
    /// Wrap long code lines the way opencode's `<code>` does, at spaces and after punctuation,
    /// instead of at the last cell that fits. A word longer than the row is still broken.
    pub code_word_wrap: bool,
    /// Colours of the `> ` and of the text of a quote nested in a quote (`opencode_blocks`),
    /// which opencode shows as literal text. `None` uses the quote colour for both.
    pub literal_quote: Option<(Color, Color)>,
}

impl Default for MarkdownOptions {
    fn default() -> Self {
        Self {
            conceal: true,
            code_bg: false,
            wrap_code: true,
            grid_tables: false,
            soft_break_newline: false,
            opencode_inline: false,
            punct_wrap: false,
            opencode_blocks: false,
            code_word_wrap: false,
            literal_quote: None,
        }
    }
}

/// Render `src` into lines no wider than `width` (a zero width renders nothing).
pub fn render(src: &str, width: u16, theme: &Theme, opts: &MarkdownOptions) -> Vec<Line<'static>> {
    if width == 0 {
        return Vec::new();
    }
    let src = crate::width::normalize_newlines(src);
    let src = if src.contains('\x1b') {
        crate::ansi::strip(&src).into()
    } else {
        src
    };
    let mut r = Renderer::new(width as usize, theme, *opts);
    if opts.opencode_blocks {
        r.index_lines(&src);
    }
    let md = Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS;
    for (ev, range) in Parser::new_ext(&src, md).into_offset_iter() {
        r.cur = range;
        r.event(ev);
    }
    r.finish()
}

enum Ctx {
    Quote,
    /// A quote inside a quote under `opencode_blocks`: opencode shows the inner `> ` as text.
    LiteralQuote,
    List(Option<u64>),
    Item {
        marker: Option<Vec<Span<'static>>>,
        indent: usize,
    },
}

struct CodeBlock {
    lang: String,
    text: String,
}

struct TableBuilder {
    aligns: Vec<Alignment>,
    header: Vec<Vec<Span<'static>>>,
    rows: Vec<Vec<Vec<Span<'static>>>>,
    current: Vec<Vec<Span<'static>>>,
    in_head: bool,
}

struct Renderer<'t> {
    t: &'t Theme,
    opts: MarkdownOptions,
    width: usize,
    out: Vec<Line<'static>>,
    ctx: Vec<Ctx>,
    inline: Vec<Span<'static>>,
    styles: Vec<Style>,
    links: Vec<String>,
    heading: Option<HeadingLevel>,
    code: Option<CodeBlock>,
    html: Option<String>,
    table: Option<TableBuilder>,
    cell_stash: Option<Vec<Span<'static>>>,
    blank_pending: bool,
    /// Source range of the event being handled, for `opencode_blocks`.
    cur: std::ops::Range<usize>,
    /// Start of the block about to be drawn and end of the last one drawn.
    block_start: usize,
    block_end: usize,
    /// Byte offset of every source line and whether it is blank (quote markers ignored).
    lines: Vec<(usize, bool)>,
    /// The next block is the first one of a list nested in an item: no blank row before it.
    suppress_blank: bool,
    /// The previous event opened a paragraph. A task item's checkbox comes right after the
    /// paragraph start in a loose list and right after the item start in a tight one.
    after_paragraph: bool,
}

impl<'t> Renderer<'t> {
    fn new(width: usize, t: &'t Theme, opts: MarkdownOptions) -> Self {
        Self {
            t,
            opts,
            width,
            out: Vec::new(),
            ctx: Vec::new(),
            inline: Vec::new(),
            styles: vec![Style::new().fg(t.markdown_text)],
            links: Vec::new(),
            heading: None,
            code: None,
            html: None,
            table: None,
            cell_stash: None,
            blank_pending: false,
            cur: 0..0,
            block_start: 0,
            block_end: 0,
            lines: Vec::new(),
            suppress_blank: false,
            after_paragraph: false,
        }
    }

    fn index_lines(&mut self, src: &str) {
        let mut off = 0;
        for l in src.split('\n') {
            let blank = l
                .trim_matches(|c: char| c == '>' || c.is_whitespace())
                .is_empty();
            self.lines.push((off, blank));
            off += l.len() + 1;
        }
    }

    fn line_of(&self, offset: usize) -> usize {
        self.lines
            .partition_point(|(start, _)| *start <= offset)
            .saturating_sub(1)
    }

    /// Whether the source has a blank line between the end of the last block and the start of
    /// the next one.
    fn blank_in_source(&self) -> bool {
        if self.lines.is_empty() {
            return true;
        }
        // a block's range can swallow the blank lines after it; count from its last text line
        let mut prev = self.line_of(self.block_end.saturating_sub(1));
        while prev > 0 && self.lines[prev].1 {
            prev -= 1;
        }
        let next = self.line_of(self.block_start);
        (prev + 1..next).any(|l| self.lines[l].1)
    }

    fn style(&self) -> Style {
        *self.styles.last().expect("base style")
    }

    fn word_wrap(&self) -> WrapMode {
        if self.opts.punct_wrap {
            WrapMode::WordPunct
        } else {
            WrapMode::Word
        }
    }

    fn push_style(&mut self, f: impl FnOnce(Style) -> Style) {
        let s = f(self.style());
        self.styles.push(s);
    }

    fn pop_style(&mut self) {
        if self.styles.len() > 1 {
            self.styles.pop();
        }
    }

    fn in_quote(&self) -> bool {
        self.ctx.iter().any(|c| matches!(c, Ctx::Quote))
    }

    fn in_item(&self) -> bool {
        self.ctx.iter().any(|c| matches!(c, Ctx::Item { .. }))
    }

    fn prefix_width(&self) -> usize {
        self.ctx
            .iter()
            .map(|c| match c {
                Ctx::Quote => 2,
                Ctx::LiteralQuote | Ctx::List(_) => 0,
                Ctx::Item { indent, .. } => *indent,
            })
            .sum()
    }

    fn avail(&self) -> usize {
        self.width.saturating_sub(self.prefix_width()).max(1)
    }

    /// Prefix for the next emitted line. Item markers are used once, then become blank indent.
    fn take_prefix(&mut self) -> Vec<Span<'static>> {
        let mut out = Vec::new();
        let bar = Style::new().fg(self.t.markdown_block_quote);
        for c in &mut self.ctx {
            match c {
                Ctx::Quote => out.push(Span::styled("│ ", bar)),
                Ctx::LiteralQuote | Ctx::List(_) => {}
                Ctx::Item { marker, indent } => match marker.take() {
                    Some(m) => out.extend(m),
                    None => out.push(Span::raw(" ".repeat(*indent))),
                },
            }
        }
        out
    }

    fn emit(&mut self, spans: Vec<Span<'static>>) {
        let mut line = self.take_prefix();
        line.extend(spans);
        self.out.push(Line::from(line));
    }

    fn blank(&mut self) {
        if self.in_quote() {
            let bar = Style::new().fg(self.t.markdown_block_quote);
            let mut spans = Vec::new();
            for c in &self.ctx {
                match c {
                    Ctx::Quote => spans.push(Span::styled("│", bar)),
                    Ctx::Item { indent, .. } => spans.push(Span::raw(" ".repeat(*indent))),
                    Ctx::LiteralQuote | Ctx::List(_) => {}
                }
                if matches!(c, Ctx::Quote) {
                    spans.push(Span::raw(" "));
                }
            }
            // Trim trailing spaces so blank quote lines do not carry invisible cells.
            while spans.last().is_some_and(|s| s.content.trim().is_empty()) {
                spans.pop();
            }
            self.out.push(Line::from(spans));
        } else {
            self.out.push(Line::default());
        }
    }

    /// Call before the first line of a block.
    fn begin_block(&mut self) {
        if self.blank_pending && !self.out.is_empty() {
            let nested = self.opts.opencode_blocks && !self.ctx.is_empty();
            if !nested || (!self.suppress_blank && self.blank_in_source()) {
                self.blank();
            }
        }
        self.blank_pending = false;
        self.suppress_blank = false;
    }

    fn end_block(&mut self) {
        self.blank_pending = true;
    }

    fn text(&mut self, s: &str) {
        let s = sanitize(s);
        if s.is_empty() {
            return;
        }
        let style = self.style();
        match self.inline.last_mut() {
            Some(last) if last.style == style => last.content.to_mut().push_str(&s),
            _ => self.inline.push(Span::styled(s.into_owned(), style)),
        }
    }

    fn flush_inline(&mut self) {
        if self.inline.is_empty() {
            return;
        }
        let spans = std::mem::take(&mut self.inline);
        // Only whitespace between block elements, nothing to draw.
        if spans.iter().all(|s| s.content.trim().is_empty()) {
            return;
        }
        self.begin_block();
        let avail = self.avail();
        for logical in split_spans_on_newline(spans) {
            for row in wrap_spans(&logical, avail, self.word_wrap()) {
                self.emit(row);
            }
        }
    }

    fn event(&mut self, ev: Event<'_>) {
        let after_paragraph = std::mem::replace(
            &mut self.after_paragraph,
            matches!(ev, Event::Start(Tag::Paragraph)),
        );
        if self.code.is_some() {
            match ev {
                Event::Text(s) => {
                    self.code.as_mut().unwrap().text.push_str(&s);
                    return;
                }
                Event::End(TagEnd::CodeBlock) => {
                    self.finish_code();
                    return;
                }
                _ => return,
            }
        }
        if self.html.is_some() {
            match ev {
                Event::Html(s) | Event::Text(s) => {
                    self.html.as_mut().unwrap().push_str(&s);
                    return;
                }
                Event::End(TagEnd::HtmlBlock) => {
                    self.finish_html();
                    return;
                }
                _ => return,
            }
        }
        match ev {
            Event::Start(tag) => self.start(tag),
            Event::End(end) => self.end(end),
            Event::Text(s) => self.text(&s),
            Event::Code(s) => {
                let s = sanitize(&s).into_owned();
                // inside a heading everything keeps the heading color in opencode
                let style = if self.opts.opencode_inline && self.heading.is_some() {
                    self.style()
                } else {
                    self.style().fg(self.t.markdown_code)
                };
                self.inline.push(Span::styled(s, style));
            }
            Event::Html(s) | Event::InlineHtml(s) => self.text(&s),
            Event::SoftBreak if self.opts.soft_break_newline && self.table.is_none() => {
                let style = self.style();
                self.inline.push(Span::styled("\n", style));
            }
            Event::SoftBreak => self.text(" "),
            Event::HardBreak => {
                let style = self.style();
                self.inline.push(Span::styled("\n", style));
            }
            Event::Rule => {
                self.flush_inline();
                self.block_start = self.cur.start;
                self.begin_block();
                let w = self.avail();
                let style = Style::new().fg(self.t.markdown_horizontal_rule);
                self.emit(vec![Span::styled("─".repeat(w), style)]);
                self.block_end = self.cur.end;
                self.end_block();
            }
            // with concealment on, opencode's task item is a plain bullet in a tight list and
            // `-   task` / `- x task` in a loose one
            Event::TaskListMarker(done) if self.opts.opencode_blocks && self.opts.conceal => {
                if after_paragraph {
                    self.loose_task_marker(done);
                }
            }
            Event::TaskListMarker(done) if self.opts.opencode_blocks => {
                // opencode drops the brackets and keeps the character: `-   todo`, `- x done`
                if let Some(Ctx::Item { marker, indent }) = self.ctx.last_mut() {
                    let c = if done { "x" } else { " " };
                    *marker = Some(vec![
                        Span::styled("- ", Style::new().fg(self.t.markdown_list_item)),
                        Span::styled(c, Style::new().fg(self.t.markdown_list_enumeration)),
                        Span::raw(" "),
                    ]);
                    *indent = 2;
                }
            }
            Event::TaskListMarker(_) if self.opts.opencode_inline => {}
            Event::TaskListMarker(done) => {
                if let Some(Ctx::Item { marker, indent }) = self.ctx.last_mut() {
                    let text = if done { "[x] " } else { "[ ] " };
                    *marker = Some(vec![Span::styled(
                        text,
                        Style::new().fg(self.t.markdown_list_item),
                    )]);
                    *indent = 4;
                }
            }
            Event::FootnoteReference(s) => {
                let style = self.style().fg(self.t.markdown_link);
                self.inline.push(Span::styled(format!("[{s}]"), style));
            }
            Event::InlineMath(s) | Event::DisplayMath(s) => {
                let style = self.style().fg(self.t.markdown_code);
                self.inline
                    .push(Span::styled(sanitize(&s).into_owned(), style));
            }
        }
    }

    fn loose_task_marker(&mut self, done: bool) {
        if let Some(Ctx::Item { marker, indent }) = self.ctx.last_mut() {
            let bullet = marker
                .as_ref()
                .is_some_and(|m| m.first().is_some_and(|s| s.content == "- "));
            if bullet {
                let c = if done { "x" } else { " " };
                *marker = Some(vec![
                    Span::styled("- ", Style::new().fg(self.t.markdown_list_item)),
                    Span::styled(c, Style::new().fg(self.t.markdown_list_enumeration)),
                    Span::raw(" "),
                ]);
                *indent = 2;
            }
        }
    }

    fn start(&mut self, tag: Tag<'_>) {
        match tag {
            Tag::Paragraph => self.block_start = self.cur.start,
            Tag::Heading { level, .. } => {
                self.flush_inline();
                self.block_start = self.cur.start;
                self.heading = Some(level);
                let color = self.t.markdown_heading;
                let underline = level == HeadingLevel::H1;
                self.push_style(|s| {
                    let s = s.fg(color).add_modifier(Modifier::BOLD);
                    if underline {
                        s.add_modifier(Modifier::UNDERLINED)
                    } else {
                        s
                    }
                });
                if !self.opts.conceal {
                    let hashes = format!("{} ", "#".repeat(level as usize));
                    self.text(&hashes);
                }
            }
            Tag::BlockQuote(_) if self.opts.opencode_blocks && self.in_quote() => {
                self.flush_inline();
                self.block_start = self.cur.start;
                self.begin_block();
                self.ctx.push(Ctx::LiteralQuote);
                let quote = self.t.markdown_block_quote;
                let (marker, text) = self.opts.literal_quote.unwrap_or((quote, quote));
                self.push_style(|s| s.fg(text).add_modifier(Modifier::ITALIC));
                self.inline.push(Span::styled(
                    "> ",
                    Style::new().fg(marker).add_modifier(Modifier::ITALIC),
                ));
            }
            Tag::BlockQuote(_) => {
                self.flush_inline();
                self.block_start = self.cur.start;
                self.begin_block();
                self.ctx.push(Ctx::Quote);
                let color = self.t.markdown_block_quote;
                self.push_style(|s| s.fg(color).add_modifier(Modifier::ITALIC));
            }
            Tag::CodeBlock(kind) => {
                self.flush_inline();
                self.block_start = self.cur.start;
                let lang = match kind {
                    CodeBlockKind::Fenced(info) => info.to_string(),
                    CodeBlockKind::Indented => String::new(),
                };
                self.code = Some(CodeBlock {
                    lang,
                    text: String::new(),
                });
            }
            Tag::HtmlBlock => {
                self.flush_inline();
                self.html = Some(String::new());
            }
            Tag::List(start) => {
                self.flush_inline();
                if self.ctx.is_empty() {
                    self.begin_block();
                }
                // a list nested in an item sits right under the item's text
                self.suppress_blank = self.opts.opencode_blocks && self.in_item();
                self.ctx.push(Ctx::List(start));
            }
            Tag::Item => {
                self.flush_inline();
                self.block_start = self.cur.start;
                let (marker, indent) = match self.ctx.last_mut() {
                    Some(Ctx::List(Some(n))) => {
                        let text = format!("{n}. ");
                        *n += 1;
                        let w = display_width(&text);
                        (
                            // the binary draws ordered numbers in the bullet color, not in the
                            // theme's enumeration color (that one colors task checkboxes)
                            Span::styled(
                                text,
                                Style::new().fg(if self.opts.opencode_blocks {
                                    self.t.markdown_list_item
                                } else {
                                    self.t.markdown_list_enumeration
                                }),
                            ),
                            w,
                        )
                    }
                    _ => (
                        Span::styled("- ", Style::new().fg(self.t.markdown_list_item)),
                        2,
                    ),
                };
                self.ctx.push(Ctx::Item {
                    marker: Some(vec![marker]),
                    indent,
                });
            }
            Tag::Table(aligns) => {
                self.flush_inline();
                self.block_start = self.cur.start;
                self.table = Some(TableBuilder {
                    aligns,
                    header: Vec::new(),
                    rows: Vec::new(),
                    current: Vec::new(),
                    in_head: false,
                });
            }
            Tag::TableHead => {
                if let Some(t) = &mut self.table {
                    t.in_head = true;
                }
            }
            Tag::TableRow => {
                if let Some(t) = &mut self.table {
                    t.current.clear();
                }
            }
            Tag::TableCell => {
                self.cell_stash = Some(std::mem::take(&mut self.inline));
            }
            Tag::Emphasis => {
                let color = self.t.markdown_emph;
                let keep = self.opts.opencode_inline && self.heading.is_some();
                self.push_style(|s| {
                    let s = s.add_modifier(Modifier::ITALIC);
                    if keep {
                        s
                    } else {
                        s.fg(color)
                    }
                });
            }
            Tag::Strong => {
                let color = self.t.markdown_strong;
                let keep = self.opts.opencode_inline && self.heading.is_some();
                self.push_style(|s| {
                    let s = s.add_modifier(Modifier::BOLD);
                    if keep {
                        s
                    } else {
                        s.fg(color)
                    }
                });
            }
            Tag::Strikethrough if self.opts.opencode_inline => {
                let muted = self.t.text_muted;
                self.push_style(|s| s.fg(muted))
            }
            Tag::Strikethrough => self.push_style(|s| s.add_modifier(Modifier::CROSSED_OUT)),
            Tag::Link { dest_url, .. } => {
                self.links.push(dest_url.to_string());
                let color = self.t.markdown_link_text;
                self.push_style(|s| s.fg(color).add_modifier(Modifier::UNDERLINED));
            }
            Tag::Image { dest_url, .. } if self.opts.opencode_inline => {
                // opencode shows an image as its alt text alone
                self.links.push(dest_url.to_string());
                let color = self.t.markdown_link_text;
                self.push_style(|s| s.fg(color).add_modifier(Modifier::UNDERLINED));
            }
            Tag::Image { dest_url, .. } => {
                self.links.push(dest_url.to_string());
                let bracket = Style::new().fg(self.t.markdown_image);
                self.inline.push(Span::styled("[image: ", bracket));
                let color = self.t.markdown_image_text;
                self.push_style(|s| s.fg(color));
            }
            _ => {}
        }
    }

    fn end(&mut self, end: TagEnd) {
        match end {
            TagEnd::Paragraph => {
                self.flush_inline();
                self.block_end = self.cur.end;
                self.end_block();
            }
            TagEnd::Heading(_) => {
                self.flush_inline();
                self.pop_style();
                self.heading = None;
                self.block_end = self.cur.end;
                self.end_block();
            }
            TagEnd::BlockQuote(_) => {
                self.flush_inline();
                self.pop_style();
                self.ctx.pop();
                self.block_end = self.cur.end;
                self.end_block();
            }
            TagEnd::List(_) => {
                self.flush_inline();
                self.ctx.pop();
                if !self.in_item() {
                    self.end_block();
                }
            }
            TagEnd::Item => {
                self.flush_inline();
                self.block_end = self.cur.end;
                if let Some(Ctx::Item {
                    marker: Some(_), ..
                }) = self.ctx.last()
                {
                    // Empty item: still draw its marker.
                    self.emit(Vec::new());
                }
                self.ctx.pop();
                if self.opts.opencode_blocks {
                    // the next item (or block) decides from the source whether a blank row goes
                    // between
                    self.end_block();
                }
            }
            TagEnd::Table => self.finish_table(),
            TagEnd::TableHead => {
                if let Some(t) = &mut self.table {
                    t.header = std::mem::take(&mut t.current);
                    t.in_head = false;
                }
            }
            TagEnd::TableRow => {
                if let Some(t) = &mut self.table {
                    let row = std::mem::take(&mut t.current);
                    t.rows.push(row);
                }
            }
            TagEnd::TableCell => {
                let cell = std::mem::take(&mut self.inline);
                if let Some(stash) = self.cell_stash.take() {
                    self.inline = stash;
                }
                if let Some(t) = &mut self.table {
                    t.current.push(cell);
                }
            }
            TagEnd::Emphasis | TagEnd::Strong | TagEnd::Strikethrough => self.pop_style(),
            TagEnd::Link => {
                self.pop_style();
                let url = self.links.pop().unwrap_or_default();
                // Skip the URL when the label already is the URL.
                let label: String = self
                    .inline
                    .iter()
                    .rev()
                    .take_while(|s| s.style.add_modifier.contains(Modifier::UNDERLINED))
                    .map(|s| s.content.as_ref())
                    .collect::<Vec<_>>()
                    .into_iter()
                    .rev()
                    .collect();
                if !url.is_empty() && label.trim() != url && !url.starts_with('#') {
                    let style = Style::new().fg(self.t.markdown_link);
                    self.inline
                        .push(Span::styled(format!(" ({})", sanitize(&url)), style));
                }
            }
            TagEnd::Image if self.opts.opencode_inline => {
                self.pop_style();
                self.links.pop();
            }
            TagEnd::Image => {
                self.pop_style();
                let url = self.links.pop().unwrap_or_default();
                let style = Style::new().fg(self.t.markdown_image);
                let tail = if url.is_empty() {
                    "]".to_string()
                } else {
                    format!("] ({})", sanitize(&url))
                };
                self.inline.push(Span::styled(tail, style));
            }
            _ => {}
        }
    }

    fn finish_code(&mut self) {
        let Some(block) = self.code.take() else {
            return;
        };
        self.begin_block();
        let t = self.t;
        let syntax = if block.lang.trim().is_empty() {
            None
        } else {
            syntax::find_syntax(&block.lang)
        };
        let text = sanitize(&block.text);
        let mut lines = syntax::highlight(syntax, &text, t);
        let bg = self.opts.code_bg.then_some(t.background_panel);
        let fence_style = Style::new().fg(t.text_muted);
        let mut rows: Vec<Vec<Span<'static>>> = Vec::new();
        if !self.opts.conceal {
            rows.push(vec![Span::styled(
                format!("```{}", block.lang.trim()),
                fence_style,
            )]);
        }
        let pad = if bg.is_some() { 1 } else { 0 };
        let avail = self.avail().saturating_sub(pad * 2).max(1);
        for line in lines.drain(..) {
            if self.opts.wrap_code {
                let mode = if self.opts.code_word_wrap {
                    WrapMode::WordPunct
                } else {
                    WrapMode::Char
                };
                for r in wrap_spans(&line, avail, mode) {
                    rows.push(r);
                }
            } else {
                rows.push(line);
            }
        }
        if !self.opts.conceal {
            rows.push(vec![Span::styled("```", fence_style)]);
        }
        let full = self.avail();
        for mut row in rows {
            if let Some(bg) = bg {
                row.insert(0, Span::raw(" "));
                let used = spans_width(&row);
                row.push(Span::raw(" ".repeat(full.saturating_sub(used))));
                for s in &mut row {
                    s.style = s.style.bg(bg);
                }
            }
            self.emit(row);
        }
        self.block_end = self.cur.end;
        self.end_block();
    }

    fn finish_html(&mut self) {
        let Some(text) = self.html.take() else { return };
        let text = sanitize(&text).into_owned();
        let text = text.trim_end_matches('\n');
        if text.is_empty() {
            return;
        }
        self.begin_block();
        let style = Style::new().fg(self.t.text_muted);
        let avail = self.avail();
        for line in text.split('\n') {
            for row in wrap_spans(
                &[Span::styled(line.to_string(), style)],
                avail,
                self.word_wrap(),
            ) {
                self.emit(row);
            }
        }
        self.block_end = self.cur.end;
        self.end_block();
    }

    fn finish_table(&mut self) {
        let Some(tb) = self.table.take() else { return };
        self.begin_block();
        let ncols = tb
            .header
            .len()
            .max(tb.rows.iter().map(Vec::len).max().unwrap_or(0));
        if ncols == 0 {
            return;
        }
        let avail = self.avail();
        let cell_w = |c: &Vec<Span<'static>>| {
            split_spans_on_newline(c.clone())
                .iter()
                .map(|l| spans_width(l))
                .max()
                .unwrap_or(0)
        };
        let mut widths = vec![1usize; ncols];
        for row in std::iter::once(&tb.header).chain(tb.rows.iter()) {
            for (i, c) in row.iter().enumerate() {
                widths[i] = widths[i].max(cell_w(c));
            }
        }
        // Borders: ncols + 1 bars, plus one space of padding each side of every cell (none in
        // grid mode).
        let grid = self.opts.grid_tables;
        let pad = if grid { 0 } else { 1 };
        let chrome = ncols + 1 + ncols * 2 * pad;
        let budget = avail.saturating_sub(chrome).max(ncols);
        // opencode wraps a cell one column early when the table had to shrink (a line that would
        // fill the column exactly breaks before its last word); an unbroken word may still
        // use the whole column.
        let mut shrunk = false;
        if grid {
            shrunk = widths.iter().sum::<usize>() > budget;
            widths = grid_widths(&widths, budget);
        } else {
            while widths.iter().sum::<usize>() > budget {
                let (i, _) = widths.iter().enumerate().max_by_key(|(_, w)| **w).unwrap();
                if widths[i] <= 1 {
                    break;
                }
                widths[i] -= 1;
            }
        }
        let border = Style::new().fg(if grid {
            self.t.text_muted
        } else {
            self.t.border
        });
        let hline = |l: &str, m: &str, r: &str| -> Vec<Span<'static>> {
            let mut s = String::from(l);
            for (i, w) in widths.iter().enumerate() {
                s.push_str(&"─".repeat(w + 2 * pad));
                s.push_str(if i + 1 == widths.len() { r } else { m });
            }
            vec![Span::styled(s, border)]
        };
        let word_wrap = self.word_wrap();
        let aligns = &tb.aligns;
        let body_style = Style::new().fg(self.t.markdown_text);
        let render_row = |cells: &[Vec<Span<'static>>], bold: bool| -> Vec<Vec<Span<'static>>> {
            let wrapped: Vec<Vec<Vec<Span<'static>>>> = (0..ncols)
                .map(|i| {
                    let spans = cells.get(i).cloned().unwrap_or_default();
                    let spans: Vec<Span<'static>> = spans
                        .into_iter()
                        .map(|mut s| {
                            if bold {
                                s.style = s.style.add_modifier(Modifier::BOLD);
                                if grid {
                                    s.style = s.style.fg(self.t.markdown_heading);
                                }
                            }
                            s
                        })
                        .collect();
                    let mut rows = Vec::new();
                    for logical in split_spans_on_newline(spans) {
                        let w = widths[i];
                        let longest = spans_text(&logical)
                            .split_whitespace()
                            .map(display_width)
                            .max()
                            .unwrap_or(0);
                        let w = if shrunk && w > 1 && longest < w {
                            w - 1
                        } else {
                            w
                        };
                        rows.extend(wrap_spans(&logical, w, word_wrap));
                    }
                    if rows.is_empty() {
                        rows.push(Vec::new());
                    }
                    rows
                })
                .collect();
            let height = wrapped.iter().map(Vec::len).max().unwrap_or(1);
            (0..height)
                .map(|r| {
                    let mut line = vec![Span::styled("│", border)];
                    for (i, w) in widths.iter().enumerate() {
                        let cell = wrapped[i].get(r).cloned().unwrap_or_default();
                        let used = spans_width(&cell);
                        let slack = w.saturating_sub(used);
                        // opencode ignores `:--` / `--:` in its grid tables: every cell is left aligned
                        let align = if grid {
                            Alignment::None
                        } else {
                            aligns.get(i).copied().unwrap_or(Alignment::None)
                        };
                        let (l, rt) = match align {
                            Alignment::Right => (slack, 0),
                            Alignment::Center => (slack / 2, slack - slack / 2),
                            _ => (0, slack),
                        };
                        line.push(Span::styled(" ".repeat(l + pad), body_style));
                        line.extend(cell);
                        line.push(Span::styled(" ".repeat(rt + pad), body_style));
                        line.push(Span::styled("│", border));
                    }
                    line
                })
                .collect()
        };
        self.emit(hline("┌", "┬", "┐"));
        if !tb.header.is_empty() {
            for l in render_row(&tb.header, true) {
                self.emit(l);
            }
            self.emit(hline("├", "┼", "┤"));
        }
        for (n, row) in tb.rows.iter().enumerate() {
            if grid && n > 0 {
                self.emit(hline("├", "┼", "┤"));
            }
            for l in render_row(row, false) {
                self.emit(l);
            }
        }
        self.emit(hline("└", "┴", "┘"));
        self.block_end = self.cur.end;
        self.end_block();
    }

    fn finish(mut self) -> Vec<Line<'static>> {
        self.flush_inline();
        while matches!(self.out.last(), Some(l) if l.spans.is_empty()) {
            self.out.pop();
        }
        self.out
    }
}

fn spans_text(spans: &[Span<'static>]) -> String {
    spans.iter().map(|s| s.content.as_ref()).collect()
}

/// Column widths of opencode's `grid` tables. When everything fits, each column keeps its
/// natural width and the spare room is split evenly, the remainder going to the leftmost columns
/// (4 and 9 over 118 cells give 57/61). When it does not, the room is shared in proportion to
/// the square root of each column's natural width, and a column never gets more than it needs
/// (measured: 2, 132 and 84 over 97 cells give 2/53/42).
fn grid_widths(natural: &[usize], budget: usize) -> Vec<usize> {
    let n = natural.len();
    let total: usize = natural.iter().sum();
    if total <= budget {
        let spare = budget - total;
        return natural
            .iter()
            .enumerate()
            .map(|(i, w)| w + spare / n + usize::from(i < spare % n))
            .collect();
    }
    let mut share = vec![0f64; n];
    let mut capped = vec![false; n];
    let mut left = budget as f64;
    loop {
        let wsum: f64 = (0..n)
            .filter(|i| !capped[*i])
            .map(|i| (natural[i] as f64).sqrt())
            .sum();
        let mut changed = false;
        for i in 0..n {
            if capped[i] {
                continue;
            }
            let s = left * (natural[i] as f64).sqrt() / wsum;
            if s >= natural[i] as f64 {
                capped[i] = true;
                share[i] = natural[i] as f64;
                changed = true;
            } else {
                share[i] = s;
            }
        }
        if !changed {
            break;
        }
        left = budget as f64
            - (0..n)
                .filter(|i| capped[*i])
                .map(|i| natural[i] as f64)
                .sum::<f64>();
    }
    // whole cells: floor everything, then hand the rest to the largest fractions
    let mut out: Vec<usize> = share.iter().map(|s| (s.floor() as usize).max(1)).collect();
    let mut order: Vec<usize> = (0..n).collect();
    order.sort_by(|a, b| {
        let fa = share[*a] - share[*a].floor();
        let fb = share[*b] - share[*b].floor();
        fb.partial_cmp(&fa).unwrap_or(std::cmp::Ordering::Equal)
    });
    let missing = budget.saturating_sub(out.iter().sum::<usize>());
    for i in order.iter().cycle().take(missing) {
        out[*i] += 1;
    }
    out
}

/// How much unbroken text may sit in the live tail before it is cut at a line end. The tail is
/// rendered again on every frame, and a model that streams megabytes without a blank line (a
/// pasted log, a runaway loop) would make each frame cost the whole message.
const MAX_TAIL: usize = 16 * 1024;
/// The last bytes of the source that a forced cut leaves in the tail.
const FORCED_KEEP: usize = 2 * 1024;

/// `- x`, `* x`, `+ x`, `1. x` or `1) x` once the indent is gone.
fn is_list_item(t: &str) -> bool {
    let mut it = t.chars();
    match it.next() {
        Some('-' | '*' | '+') => it.next().is_some_and(|c| c == ' ' || c == '\t'),
        Some(c) if c.is_ascii_digit() => {
            let rest = t.trim_start_matches(|c: char| c.is_ascii_digit());
            let mut r = rest.chars();
            t.len() - rest.len() <= 9
                && matches!(r.next(), Some('.' | ')'))
                && r.next().is_some_and(|c| c == ' ' || c == '\t')
        }
        _ => false,
    }
}

/// Index just past the last blank line that is safe to cut at: not inside a code fence, and
/// the content after it does not look like a continuation (indented). `None` if there is none.
/// When more than `max_tail` bytes would be left after that, the end of a plain line near the
/// end is used instead, which splits the paragraph there.
fn stable_boundary_in(src: &str, max_tail: usize) -> Option<usize> {
    let mut fence: Option<(char, usize)> = None;
    let mut best = None;
    let mut forced = None;
    let mut pos = 0;
    let mut pending_blank_end: Option<usize> = None;
    // The last non-blank line began a list item or hung under one. A list item that follows a
    // blank line then belongs to the same list, which turns loose, so it is not a cut point.
    let mut in_list = false;
    for line in src.split_inclusive('\n') {
        let start = pos;
        pos += line.len();
        let trimmed = line.trim_end_matches(['\n', '\r']);
        let t = trimmed.trim_start();
        let indent = trimmed.len() - t.len();
        if let Some((ch, n)) = fence {
            let run = t.chars().take_while(|c| *c == ch).count();
            if indent < 4 && run >= n && t[run..].trim().is_empty() {
                fence = None;
            }
            pending_blank_end = None;
            continue;
        }
        if indent < 4 {
            for ch in ['`', '~'] {
                let run = t.chars().take_while(|c| *c == ch).count();
                if run >= 3 && !(ch == '`' && t[run..].contains('`')) {
                    fence = Some((ch, run));
                }
            }
        }
        if t.is_empty() {
            if pending_blank_end.is_none() {
                pending_blank_end = Some(pos);
            }
            continue;
        }
        if fence.is_none() && line.ends_with('\n') && pos + FORCED_KEEP <= src.len() {
            forced = Some(pos);
        }
        let item = is_list_item(t);
        // First non-blank line after a blank run: that is where a new chunk could start.
        if let Some(b) = pending_blank_end.take() {
            let continuation = indent >= 2 || (item && in_list);
            // A fence opening on this very line is fine as a chunk start. A last line that has
            // not ended may still grow into a list item (`-` before ` [ ] task`), so a cut
            // before it waits for its newline.
            if !continuation && b <= start && line.ends_with('\n') {
                best = Some(b);
            }
        }
        in_list = item || (in_list && indent >= 2);
    }
    if src.len() - best.unwrap_or(0) > max_tail && forced > best {
        return forced;
    }
    best
}

/// Cache for rendering a string that only grows at the end.
#[derive(Default)]
pub struct StreamingMarkdown {
    key: Option<(u16, String, crate::theme::Mode, u8)>,
    /// Source bytes already rendered into the stable head of `lines`.
    stable_src: String,
    /// `lines[..stable_len]` is the rendered stable prefix; the rest is the live tail.
    stable_len: usize,
    lines: Vec<Line<'static>>,
    /// Counts the times the stable rows were thrown away, so a caller that keeps a processed
    /// copy of them knows when to start over.
    epoch: u64,
}

/// The rows of a streaming render, split where they stop changing.
pub struct Split<'a> {
    /// Changes whenever `stable` is rebuilt from scratch (a new width, theme or document).
    /// Between two calls with the same epoch `stable` only grows at its end.
    pub epoch: u64,
    /// Rows that will not change until the epoch does.
    pub stable: &'a [Line<'static>],
    /// Rows of the block still being written, re-rendered on every call.
    pub tail: &'a [Line<'static>],
}

impl StreamingMarkdown {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn reset(&mut self) {
        let epoch = self.epoch + 1;
        *self = Self::default();
        self.epoch = epoch;
    }

    /// Render the current full text. Cheap when `src` extends the previous call's input.
    pub fn update(
        &mut self,
        src: &str,
        width: u16,
        theme: &Theme,
        opts: &MarkdownOptions,
    ) -> &[Line<'static>] {
        self.advance(src, width, theme, opts);
        &self.lines
    }

    /// Like [`update`](Self::update), with the rows split into the part that is final and the
    /// part still moving, so a caller can cache work on the first and redo only the second.
    pub fn update_split(
        &mut self,
        src: &str,
        width: u16,
        theme: &Theme,
        opts: &MarkdownOptions,
    ) -> Split<'_> {
        self.advance(src, width, theme, opts);
        let (stable, tail) = self.lines.split_at(self.stable_len);
        Split {
            epoch: self.epoch,
            stable,
            tail,
        }
    }

    fn advance(&mut self, src: &str, width: u16, theme: &Theme, opts: &MarkdownOptions) {
        let flags = opts.conceal as u8 | (opts.code_bg as u8) << 1 | (opts.wrap_code as u8) << 2;
        let key = (width, theme.name.clone(), theme.mode, flags);
        if self.key.as_ref() != Some(&key)
            || !src.as_bytes().starts_with(self.stable_src.as_bytes())
        {
            self.reset();
            self.key = Some(key);
        }
        // Only the part after the stable head is looked at: a boundary is never inside a fence, so
        // the scan can start there with no fence open, and a frame no longer reads the whole
        // message to find where the live tail begins.
        let from = self.stable_src.len();
        if let Some(b) = stable_boundary_in(&src[from..], MAX_TAIL).map(|b| from + b) {
            if b > self.stable_src.len() {
                let chunk = &src[self.stable_src.len()..b];
                let rendered = render(chunk, width, theme, opts);
                self.lines.truncate(self.stable_len);
                if !rendered.is_empty() {
                    if self.stable_len > 0 {
                        self.lines.push(Line::default());
                    }
                    self.lines.extend(rendered);
                }
                self.stable_len = self.lines.len();
                self.stable_src.push_str(chunk);
            }
        }
        let tail = render(&src[self.stable_src.len()..], width, theme, opts);
        self.lines.truncate(self.stable_len);
        if self.stable_len > 0 && !tail.is_empty() {
            self.lines.push(Line::default());
        }
        self.lines.extend(tail);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::style::Color;

    fn theme() -> Theme {
        Theme::builtin("opencode").unwrap()
    }

    fn text(lines: &[Line<'_>]) -> Vec<String> {
        lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect()
    }

    fn md(src: &str, w: u16) -> Vec<String> {
        text(&render(src, w, &theme(), &MarkdownOptions::default()))
    }

    fn find_span<'a>(lines: &'a [Line<'static>], needle: &str) -> &'a Span<'static> {
        lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .find(|s| s.content.contains(needle))
            .unwrap_or_else(|| panic!("no span containing {needle:?}"))
    }

    #[test]
    fn paragraphs_wrap_to_width_and_separate_with_one_blank() {
        let out = md("hello world foo bar\n\nsecond para", 11);
        assert_eq!(out, vec!["hello world", "foo bar", "", "second para"]);
    }

    #[test]
    fn inline_styles_use_theme_tokens() {
        let t = theme();
        let lines = render(
            "a **bold** *it* ~~gone~~ `code`",
            80,
            &t,
            &MarkdownOptions::default(),
        );
        let bold = find_span(&lines, "bold");
        assert!(bold.style.add_modifier.contains(Modifier::BOLD));
        assert_eq!(bold.style.fg, Some(t.markdown_strong));
        let it = find_span(&lines, "it");
        assert!(it.style.add_modifier.contains(Modifier::ITALIC));
        assert_eq!(it.style.fg, Some(t.markdown_emph));
        assert!(find_span(&lines, "gone")
            .style
            .add_modifier
            .contains(Modifier::CROSSED_OUT));
        assert_eq!(find_span(&lines, "code").style.fg, Some(t.markdown_code));
    }

    #[test]
    fn headings_hide_hashes_when_concealed() {
        let t = theme();
        let lines = render("# Title\n\n### Sub", 80, &t, &MarkdownOptions::default());
        assert_eq!(text(&lines), vec!["Title", "", "Sub"]);
        let h = find_span(&lines, "Title");
        assert!(h
            .style
            .add_modifier
            .contains(Modifier::BOLD | Modifier::UNDERLINED));
        let shown = render(
            "## Hi",
            80,
            &t,
            &MarkdownOptions {
                conceal: false,
                ..Default::default()
            },
        );
        assert_eq!(text(&shown), vec!["## Hi"]);
    }

    #[test]
    fn nested_and_ordered_lists_indent_and_hang() {
        let out = md("- one\n  - nested\n- two\n\n3. three\n4. four", 80);
        assert_eq!(
            out,
            vec!["- one", "  - nested", "- two", "", "3. three", "4. four"]
        );
        // wrapped item text hangs under the text, not the marker
        let out = md("- aaaa bbbb cccc dddd", 12);
        assert_eq!(out, vec!["- aaaa bbbb", "  cccc dddd"]);
    }

    #[test]
    fn loose_lists_get_blank_lines_between_items() {
        assert_eq!(md("- a\n\n- b", 20), vec!["- a", "", "- b"]);
    }

    #[test]
    fn task_list_markers() {
        assert_eq!(
            md("- [x] done\n- [ ] todo", 30),
            vec!["[x] done", "[ ] todo"]
        );
    }

    #[test]
    fn blockquote_bars_every_line_including_blanks() {
        let out = md("> one\n>\n> two", 20);
        assert_eq!(out, vec!["│ one", "│", "│ two"]);
        let t = theme();
        let lines = render("> q", 20, &t, &MarkdownOptions::default());
        assert_eq!(
            find_span(&lines, "q").style.fg,
            Some(t.markdown_block_quote)
        );
    }

    #[test]
    fn rule_fills_the_width() {
        assert_eq!(md("a\n\n---\n\nb", 6), vec!["a", "", "──────", "", "b"]);
    }

    #[test]
    fn fenced_code_is_highlighted_with_theme_syntax_colours() {
        let t = theme();
        let lines = render(
            "```rust\nfn main() {}\n```",
            40,
            &t,
            &MarkdownOptions::default(),
        );
        assert_eq!(text(&lines), vec!["fn main() {}"]);
        assert_eq!(find_span(&lines, "fn").style.fg, Some(t.syntax_function));
        assert_eq!(find_span(&lines, "main").style.fg, Some(t.syntax_function));
    }

    #[test]
    fn code_fences_show_when_not_concealed_and_long_lines_wrap() {
        let t = theme();
        let o = MarkdownOptions {
            conceal: false,
            ..Default::default()
        };
        let lines = render("```\nabcdefghij\n```", 4, &t, &o);
        assert_eq!(text(&lines), vec!["```", "abcd", "efgh", "ij", "```"]);
        let o = MarkdownOptions {
            wrap_code: false,
            ..Default::default()
        };
        assert_eq!(
            text(&render("```\nabcdefghij\n```", 4, &t, &o)),
            vec!["abcdefghij"]
        );
    }

    #[test]
    fn code_background_pads_to_full_width() {
        let t = theme();
        let o = MarkdownOptions {
            code_bg: true,
            ..Default::default()
        };
        let lines = render("```\nab\n```", 8, &t, &o);
        assert_eq!(text(&lines), vec![" ab     "]);
        assert!(lines[0]
            .spans
            .iter()
            .all(|s| s.style.bg == Some(t.background_panel)));
    }

    #[test]
    fn links_show_url_unless_it_is_the_label() {
        assert_eq!(
            md("[docs](https://x.dev/a)", 60),
            vec!["docs (https://x.dev/a)"]
        );
        assert_eq!(md("<https://x.dev>", 60), vec!["https://x.dev"]);
        let t = theme();
        let lines = render("[docs](https://x.dev)", 60, &t, &MarkdownOptions::default());
        assert_eq!(
            find_span(&lines, "docs").style.fg,
            Some(t.markdown_link_text)
        );
        assert_eq!(find_span(&lines, "(https").style.fg, Some(t.markdown_link));
    }

    #[test]
    fn images_render_as_labelled_placeholders() {
        assert_eq!(
            md("![a cat](http://i/c.png)", 60),
            vec!["[image: a cat] (http://i/c.png)"]
        );
    }

    #[test]
    fn tables_are_boxed_aligned_and_width_aware() {
        let out = md("| a | 日本 |\n|:--|--:|\n| longer | x |", 40);
        assert_eq!(
            out,
            vec![
                "┌────────┬──────┐",
                "│ a      │ 日本 │",
                "├────────┼──────┤",
                "│ longer │    x │",
                "└────────┴──────┘",
            ]
        );
    }

    #[test]
    fn grid_tables_fill_the_width_with_a_rule_between_rows() {
        let opts = MarkdownOptions {
            grid_tables: true,
            ..Default::default()
        };
        let src = "| Call | Result |\n|---|---|\n| glob | 1 match |\n| grep | 2 matches |";
        let out = text(&render(src, 121, &theme(), &opts));
        // natural widths 4 and 9 over 118 cells: 105 spare splits 52/52 and the odd cell goes to
        // column 0, so 57/61
        assert_eq!(out.len(), 7);
        assert!(out.iter().all(|l| display_width(l) == 121), "{out:#?}");
        assert_eq!(out[1].chars().take(6).collect::<String>(), "│Call ");
        assert_eq!(out[0].chars().position(|c| c == '┬'), Some(58));
        assert!(out[2].starts_with("├") && out[4].starts_with("├"));
        let theme = theme();
        let lines = render(src, 121, &theme, &opts);
        let head = find_span(&lines, "Call");
        assert_eq!(head.style.fg, Some(theme.markdown_heading));
        assert!(head.style.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn wide_tables_shrink_and_wrap_cells_within_width() {
        let src = "| name | description |\n|---|---|\n| x | this is a fairly long description that must wrap |";
        for w in [20u16, 30, 40] {
            let out = md(src, w);
            for l in &out {
                assert!(display_width(l) <= w as usize, "{l:?} > {w}");
            }
            assert!(out.len() > 5, "cell should have wrapped at {w}: {out:#?}");
        }
    }

    #[test]
    fn grid_tables_ignore_column_alignment_like_opencode() {
        let opts = MarkdownOptions {
            grid_tables: true,
            ..Default::default()
        };
        let out = text(&render(
            "| left | right | center |\n|:--|--:|:-:|\n| a | b | c |",
            60,
            &theme(),
            &opts,
        ));
        assert!(
            out[3].starts_with("│a ") && out[3].contains("│b ") && out[3].contains("│c "),
            "{out:#?}"
        );
    }

    #[test]
    fn grid_widths_share_by_square_root_when_content_overflows() {
        // measured in opencode 1.18.34 at 150 columns with the sidebar: 2/53/42
        assert_eq!(grid_widths(&[2, 132, 84], 97), vec![2, 53, 42]);
        // fits: natural plus an even split, remainder to the left
        assert_eq!(grid_widths(&[4, 9], 118), vec![57, 61]);
        assert_eq!(grid_widths(&[1, 3], 108), vec![53, 55]);
        for b in [3usize, 10, 40, 97] {
            let w = grid_widths(&[2, 132, 84], b);
            assert_eq!(w.iter().sum::<usize>(), b.max(3), "{w:?}");
        }
    }

    #[test]
    fn grid_tables_wrap_inside_a_narrow_width() {
        let opts = MarkdownOptions {
            grid_tables: true,
            ..Default::default()
        };
        let src = "| name | description |\n|---|---|\n| x | this is a fairly long description that must wrap |";
        for w in [12u16, 20, 30, 45] {
            let out = text(&render(src, w, &theme(), &opts));
            assert!(
                out.iter().all(|l| display_width(l) <= w as usize),
                "{w}: {out:#?}"
            );
            assert!(out.len() >= 5);
        }
    }

    #[test]
    fn opencode_inline_quirks() {
        let opts = MarkdownOptions {
            opencode_inline: true,
            ..Default::default()
        };
        let theme = theme();
        let lines = render("a ~~gone~~ b\n\n- [ ] todo\n- [x] done", 30, &theme, &opts);
        assert_eq!(text(&lines), vec!["a gone b", "", "- todo", "- done"]);
        let s = find_span(&lines, "gone");
        assert_eq!(s.style.fg, Some(theme.text_muted));
        assert!(!s.style.add_modifier.contains(Modifier::CROSSED_OUT));
    }

    fn oc(src: &str) -> Vec<String> {
        let opts = MarkdownOptions {
            opencode_blocks: true,
            opencode_inline: true,
            soft_break_newline: true,
            ..Default::default()
        };
        text(&render(src, 80, &theme(), &opts))
    }

    // The expectations below are what opencode 1.18.34 printed for the same markdown.
    #[test]
    fn opencode_blank_rows_inside_containers_follow_the_source() {
        assert_eq!(
            oc("- item with paragraph\n\n  second paragraph in item\n- [ ] task\n\n> quote with **bold**\n> - list in quote"),
            vec![
                "- item with paragraph",
                "",
                "  second paragraph in item",
                "-   task",
                "",
                "│ quote with bold",
                "│ - list in quote",
            ]
        );
        assert_eq!(
            oc("> para one\n>\n> para two\n>\n> - list in quote\n>\n> final para"),
            vec![
                "│ para one",
                "│",
                "│ para two",
                "│",
                "│ - list in quote",
                "│",
                "│ final para",
            ]
        );
        assert_eq!(
            oc("1. item one\n\n   para after blank\n2. item two\n   > quote in list"),
            vec![
                "1. item one",
                "",
                "   para after blank",
                "2. item two",
                "   │ quote in list",
            ]
        );
    }

    #[test]
    fn opencode_loose_lists_keep_their_blank_rows_and_nested_lists_sit_tight() {
        assert_eq!(oc("- a\n\n- b\n\n- c"), vec!["- a", "", "- b", "", "- c"]);
        assert_eq!(
            oc("- a\n- b\n\n  - nested after para\n- c"),
            vec!["- a", "- b", "  - nested after para", "- c"]
        );
        // one list: numbers keep counting across the blank lines
        assert_eq!(
            oc("3. three\n4. four\n\n1. a\n\n2. b"),
            vec!["3. three", "4. four", "", "5. a", "", "6. b"]
        );
    }

    #[test]
    fn opencode_top_level_blocks_always_have_a_blank_row() {
        assert_eq!(
            oc("Paragraph:\n- x\n- y\n\n# H1\ntext\n## H2\n- l\n* star"),
            vec![
                "Paragraph:",
                "",
                "- x",
                "- y",
                "",
                "H1",
                "",
                "text",
                "",
                "H2",
                "",
                "- l",
                "",
                "- star"
            ]
        );
    }

    #[test]
    fn opencode_task_items_show_the_character_without_brackets() {
        let opts = MarkdownOptions {
            opencode_blocks: true,
            opencode_inline: true,
            conceal: false,
            ..Default::default()
        };
        let theme = theme();
        let lines = render("- [ ] todo one\n- [x] done one", 40, &theme, &opts);
        assert_eq!(text(&lines), vec!["-   todo one", "- x done one"]);
        assert_eq!(
            find_span(&lines, "x").style.fg,
            Some(theme.markdown_list_enumeration)
        );
        // concealed (the default in a session) opencode draws a tight list's task as a plain
        // bullet, and the character only in a loose list (`tools/critic/fid/f03_md.py`, mdtask2)
        let opts = MarkdownOptions {
            conceal: true,
            ..opts
        };
        let lines = render("- [ ] todo one\n- [x] done one", 40, &theme, &opts);
        assert_eq!(text(&lines), vec!["- todo one", "- done one"]);
        let lines = render("- [ ] a\n- [x] b\n\n- [ ] c", 40, &theme, &opts);
        assert_eq!(text(&lines), vec!["-   a", "- x b", "", "-   c"]);
        let lines = render("1. [x] one\n2. [ ] two", 40, &theme, &opts);
        assert_eq!(text(&lines), vec!["1. one", "2. two"]);
        // ordered numbers use the bullet color
        let lines = render("1. one", 40, &theme, &opts);
        assert_eq!(
            find_span(&lines, "1.").style.fg,
            Some(theme.markdown_list_item)
        );
        // images are their alt text, as link text
        let lines = render("an ![alt text](http://x/y.png) here", 40, &theme, &opts);
        assert_eq!(text(&lines), vec!["an alt text here"]);
        assert_eq!(
            find_span(&lines, "alt text").style.fg,
            Some(theme.markdown_link_text)
        );
    }

    #[test]
    fn soft_breaks_can_stay_line_breaks() {
        let opts = MarkdownOptions {
            soft_break_newline: true,
            ..Default::default()
        };
        let out = text(&render("line 1\nline 2\n\n- a\n  b", 30, &theme(), &opts));
        assert_eq!(out, vec!["line 1", "line 2", "", "- a", "  b"]);
    }

    #[test]
    fn hard_and_soft_breaks() {
        assert_eq!(md("a\nb", 20), vec!["a b"]);
        assert_eq!(md("a  \nb", 20), vec!["a", "b"]);
    }

    #[test]
    fn wide_chars_and_combining_marks_wrap_by_display_width() {
        let out = md("日本語のテキスト e\u{301}e\u{301}e\u{301}", 8);
        for l in &out {
            assert!(display_width(l) <= 8, "{l:?}");
        }
        assert!(out.len() >= 2);
    }

    #[test]
    fn ansi_and_control_bytes_in_source_are_dropped() {
        assert_eq!(
            md("\x1b[31mred\x1b[0m\tx\r\nnext\x07", 40),
            vec!["red    x next"]
        );
    }

    #[test]
    fn zero_and_tiny_widths_do_not_panic_or_hang() {
        let src = "# h\n\n- a\n  - b\n\n> q\n\n| a | b |\n|-|-|\n| 1 | 2 |\n\n```\ncode\n```\n\n---\n\n日本語";
        assert!(render(src, 0, &theme(), &MarkdownOptions::default()).is_empty());
        for w in 1..6 {
            let _ = render(src, w, &theme(), &MarkdownOptions::default());
        }
    }

    #[test]
    fn empty_and_whitespace_only_documents_render_nothing() {
        assert!(md("", 20).is_empty());
        assert!(md("   \n\n  ", 20).is_empty());
    }

    #[test]
    fn html_blocks_survive_as_muted_text() {
        let out = md("<div>\nhi\n</div>\n\nafter", 30);
        assert_eq!(out, vec!["<div>", "hi", "</div>", "", "after"]);
    }

    /// A task item after a blank line belongs to the same loose list, so the streaming split
    /// must not cut there (the tail would be a tight list of one and lose its `-   `).
    #[test]
    fn a_loose_list_renders_the_same_streamed_as_whole() {
        let t = theme();
        let o = MarkdownOptions {
            opencode_blocks: true,
            opencode_inline: true,
            soft_break_newline: true,
            ..Default::default()
        };
        let src = "- [ ] a\n- [x] b\n\n- [ ] c\n\n- [ ] d\n";
        let whole = text(&render(src, 40, &t, &o));
        assert_eq!(whole[0], "-   a");
        assert_eq!(whole.last().unwrap(), "-   d");
        let mut s = StreamingMarkdown::new();
        let sp = s.update_split(src, 40, &t, &o);
        let streamed: Vec<String> = text(sp.stable).into_iter().chain(text(sp.tail)).collect();
        assert_eq!(streamed, whole);
        // typed one character at a time, a second list after a first one keeps its looseness
        let src = "* [ ] star\n\n- [ ] loose open\n\n- [x] loose done\n\n- plain\n";
        let whole = text(&render(src, 40, &t, &o));
        let mut s = StreamingMarkdown::new();
        let mut last = Vec::new();
        for end in src.char_indices().map(|(i, _)| i).chain([src.len()]) {
            let sp = s.update_split(&src[..end], 40, &t, &o);
            last = text(sp.stable).into_iter().chain(text(sp.tail)).collect();
        }
        assert_eq!(last, whole);
        assert!(whole.iter().any(|l| l == "-   loose open"), "{whole:?}");
    }

    #[test]
    fn stable_boundary_ignores_blank_lines_inside_fences_and_continuations() {
        let stable_boundary = |s: &str| stable_boundary_in(s, usize::MAX);
        let src = "a\n\n```\nx\n\ny\n```\n\nb\n\n    indented\n";
        let b = stable_boundary(src).unwrap();
        // last cut is after the blank before "b", not the one inside the fence or before the
        // indented continuation
        assert_eq!(&src[b..], "b\n\n    indented\n");
        // a list item after a blank line continues the list (it turns loose), so no cut there
        let list = "para\n\n- one\n\n- [ ] two\n";
        let b = stable_boundary(list).unwrap();
        assert_eq!(&list[b..], "- one\n\n- [ ] two\n");
        // while the first line after a blank is still being typed, nothing is cut before it
        assert_eq!(stable_boundary("para\n\n-"), None);
        assert_eq!(stable_boundary("one block\n"), None);
        assert_eq!(stable_boundary("a\n\n"), None);
    }

    /// A message with no blank line in it used to be one tail, re-rendered whole on every frame.
    #[test]
    fn a_long_run_without_a_blank_line_is_cut_into_stable_chunks() {
        let t = theme();
        let o = MarkdownOptions::default();
        let mut s = StreamingMarkdown::new();
        let src = "a line of plain words here\n".repeat(20_000);
        let sp = s.update_split(&src, 60, &t, &o);
        assert!(!sp.stable.is_empty(), "nothing became stable");
        // what stays live is a small part of the 540 KB
        assert!(
            sp.tail.len() * 4 < sp.stable.len(),
            "tail {} rows, stable {}",
            sp.tail.len(),
            sp.stable.len()
        );
        let epoch = sp.epoch;
        // growing it keeps the same stable head
        let longer = format!("{src}one more line\n");
        assert_eq!(s.update_split(&longer, 60, &t, &o).epoch, epoch);
        // a fence is never cut, however long
        let fenced = format!("```\n{}```\n", "code line here\n".repeat(20_000));
        let mut s2 = StreamingMarkdown::new();
        let sp = s2.update_split(&fenced, 60, &t, &o);
        assert!(sp.stable.is_empty(), "a code fence was split");
    }

    #[test]
    fn the_split_only_grows_between_epochs() {
        let t = theme();
        let o = MarkdownOptions::default();
        let mut s = StreamingMarkdown::new();
        let doc = "one\n\ntwo\n\nthree\n\nfour tail";
        let mut seen: Option<(u64, Vec<String>)> = None;
        for end in doc.char_indices().map(|(i, _)| i).chain([doc.len()]) {
            let sp = s.update_split(&doc[..end], 30, &t, &o);
            let stable = text(sp.stable);
            if let Some((e, prev)) = &seen {
                if *e == sp.epoch {
                    assert!(
                        stable.starts_with(prev),
                        "stable rows changed within an epoch"
                    );
                }
            }
            seen = Some((sp.epoch, stable));
            // stable plus tail is what a full render gives
            let mut all = text(sp.stable);
            all.extend(text(sp.tail));
            assert_eq!(all, text(&render(&doc[..end], 30, &t, &o)), "prefix {end}");
        }
        let (e0, n) = seen.unwrap();
        assert!(!n.is_empty());
        // a new width starts a new epoch
        assert!(s.update_split(doc, 12, &t, &o).epoch > e0);
    }

    #[test]
    fn streaming_matches_a_full_render_at_every_prefix() {
        let t = theme();
        let o = MarkdownOptions::default();
        let doc = "# Title\n\nPara one with `code` and more words to wrap around.\n\n- a\n- b\n\n```rust\nfn x() {\n\n    1\n}\n```\n\n| a | b |\n|---|---|\n| 1 | 2 |\n\nfinal *line*\n";
        let mut s = StreamingMarkdown::new();
        let mut end = 0;
        for (i, _) in doc.char_indices().chain(std::iter::once((doc.len(), ' '))) {
            end = i;
            let got = text(s.update(&doc[..end], 30, &t, &o));
            let want = text(&render(&doc[..end], 30, &t, &o));
            assert_eq!(got, want, "prefix {:?}", &doc[..end]);
        }
        assert_eq!(end, doc.len());
        // a width change or a different document resets the cache
        let got = text(s.update("other\n\ntext", 10, &t, &o));
        assert_eq!(got, vec!["other", "", "text"]);
    }

    #[test]
    fn hundred_kb_document_renders() {
        let t = theme();
        let mut doc = String::new();
        let mut i = 0;
        while doc.len() < 100 * 1024 {
            doc.push_str(&format!(
                "## Section {i}\n\nSome *prose* with `code`, a [link](http://x.y/{i}) and enough words to wrap a few times over.\n\n- item\n- item two\n\n```rust\nfn f{i}() -> u32 {{ {i} }}\n```\n\n"
            ));
            i += 1;
        }
        let t0 = std::time::Instant::now();
        let lines = render(&doc, 100, &t, &MarkdownOptions::default());
        eprintln!(
            "100KB full render: {:?} ({} lines, debug build)",
            t0.elapsed(),
            lines.len()
        );
        assert!(lines.len() > 1000);
        let mut s = StreamingMarkdown::new();
        let first = s.update(&doc, 100, &t, &MarkdownOptions::default()).len();
        // appending one token re-renders only the tail
        doc.push_str("more");
        let t1 = std::time::Instant::now();
        let second = s.update(&doc, 100, &t, &MarkdownOptions::default()).len();
        eprintln!(
            "streaming append of one token: {:?} (debug build)",
            t1.elapsed()
        );
        assert!(second >= first);
    }

    #[test]
    fn colours_come_from_the_theme_not_hard_coded() {
        let mut t = theme();
        t.markdown_text = Color::Rgb(1, 2, 3);
        let lines = render("plain", 20, &t, &MarkdownOptions::default());
        assert_eq!(lines[0].spans[0].style.fg, Some(Color::Rgb(1, 2, 3)));
    }
}
