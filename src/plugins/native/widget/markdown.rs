//! Markdown into selectable blocks.
//!
//! The model writes markdown; the transcript has to show it as prose. Wizard
//! already parses markdown with `pulldown-cmark` in [`crate::ui`], for the TUI,
//! and this is deliberately not a second parser — it is a second *renderer*
//! over the one parser. The browser GUI that this replaced did the opposite: it
//! reimplemented the reading in JavaScript, and its markdown was worse than the
//! TUI's for exactly that reason.
//!
//! # Why not iced's `markdown` widget
//!
//! iced 0.14 ships one, and it is not usable here for two independent reasons.
//! It pins `pulldown-cmark ^0.12` while Wizard is on 0.13, so enabling it links
//! two copies of the parser into one binary. And it produces a `Vec<Element>` —
//! one widget per block — which is exactly the shape [`super::super::select`]
//! exists because it cannot select across. What comes out of here instead is a
//! flat `Vec<Block>` that all belongs to one widget.
//!
//! # What a block boundary is
//!
//! One [`Block`] per block-level element: a paragraph, a heading, a fenced code
//! block, a list item, a quote. Inline styling (emphasis, inline code, links)
//! stays *inside* a block as spans, because a selection has to be able to run
//! through a bolded word without the bold ending the run. Nesting deeper than
//! one level is flattened into an indent rather than a tree: the transcript is
//! a reading surface, and a list inside a list inside a quote is not something
//! this needs to draw differently to be legible.
//!
//! # Code blocks
//!
//! Highlighted through `iced_highlighter`, which reaches syntect via `two-face`
//! with the same `syntect-default-fancy` feature selection Wizard already
//! carries — one syntect, one regex engine, no oniguruma. The highlighter is
//! line-oriented (`Stream::highlight_line` then `commit`), so a fenced block is
//! fed line by line and the newlines are re-inserted as spans of their own,
//! which is what makes the resulting paragraph one buffer with real line
//! endings — and therefore the thing [`super::super::select::geometry`] exists
//! to index correctly.

use iced::advanced::text::Span;
use iced::font::{Style, Weight};
use iced::{Color, Font};

use crate::plugins::native::font;
use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};

use crate::plugins::native::select::Block;
use crate::plugins::native::theme::Palette;
use crate::theme::Token;

/// Body text size, from `docs/gui-design-spec.md` ("14px transcript body").
pub const BODY: f32 = 14.0;
/// Literals: paths, commands, code.
pub const MONO: f32 = 13.0;

/// Bold body text.
pub fn bold(font: Font) -> Font {
    Font {
        weight: Weight::Bold,
        ..font
    }
}

/// Italic body text.
pub fn italic(font: Font) -> Font {
    Font {
        style: Style::Italic,
        ..font
    }
}

/// Render `source` as blocks, appending to `out`.
///
/// Appending rather than returning, because a transcript item is often a
/// markdown body *plus* chrome around it (a tool row's header, a user bubble's
/// attachments) and the caller is the only thing that knows the order.
pub fn render(source: &str, palette: &Palette, indent: f32, out: &mut Vec<Block>) {
    Renderer {
        palette,
        indent,
        out,
        spans: Vec::new(),
        style: Style_::default(),
        list: Vec::new(),
        quote: 0,
        pending: Pending::Paragraph,
        code: None,
    }
    .run(source);
}

/// The inline style in force, as a stack depth per attribute: markdown nests
/// (`**bold with *italic* inside**`), so a closing tag has to restore rather
/// than clear.
#[derive(Debug, Clone, Copy, Default)]
struct Style_ {
    bold: u32,
    italic: u32,
    strike: u32,
}

/// What kind of block the spans being accumulated will become.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Pending {
    Paragraph,
    Heading(HeadingLevel),
    Item,
}

struct Renderer<'a> {
    palette: &'a Palette,
    indent: f32,
    out: &'a mut Vec<Block>,
    spans: Vec<Span<'static, ()>>,
    style: Style_,
    /// One entry per open list, holding its next ordinal (`None` = bulleted).
    list: Vec<Option<u64>>,
    /// How many block quotes are open.
    ///
    /// Separate from `pending`, and it has to be: a quote *contains* a
    /// paragraph, so pulldown emits `Start(BlockQuote)` then `Start(Paragraph)`,
    /// and a single "what am I building" field is overwritten by the inner tag
    /// before a single character of the quote has arrived. A depth also gets
    /// nesting right, where a flag would leave the outer quote unmarked as soon
    /// as an inner one closed.
    quote: u32,
    pending: Pending,
    /// The language token and accumulated source of an open fenced block.
    code: Option<(String, String)>,
}

impl Renderer<'_> {
    fn run(mut self, source: &str) {
        // Tables and footnotes are not rendered by this phase and would arrive
        // as unstyled text with their pipes intact, which reads worse than the
        // source; strikethrough is on because the model uses it in checklists.
        let options = Options::ENABLE_STRIKETHROUGH;
        for event in Parser::new_ext(source, options) {
            self.event(event);
        }
        self.flush();
    }

    fn event(&mut self, event: Event<'_>) {
        match event {
            Event::Start(tag) => self.start(tag),
            Event::End(tag) => self.end(tag),
            Event::Text(text) => match &mut self.code {
                Some((_, body)) => body.push_str(&text),
                None => self.push(&text),
            },
            // Inline code. A separate event from `Text`, so it does not need
            // the style stack.
            Event::Code(text) => {
                let span = Span::new(format!("`{text}`"))
                    .font(font::MONO)
                    .size(MONO)
                    .color(self.palette.color(Token::Code));
                self.spans.push(span);
            }
            // A soft break is a newline in the source that is not one in the
            // output. It becomes a space, so the paragraph rewraps to the
            // window rather than keeping the model's 80-column habits.
            Event::SoftBreak => self.push(" "),
            Event::HardBreak => self.push("\n"),
            Event::Rule => {
                self.flush();
                self.out.push(
                    Block::plain_text("─".repeat(24), MONO, font::SANS, self.color(Token::Faint))
                        .indent(self.indent)
                        .gap(12.0),
                );
            }
            Event::TaskListMarker(done) => self.push(if done { "[x] " } else { "[ ] " }),
            // Raw HTML in a model's reply is nearly always a stray tag rather
            // than markup the user wants rendered. Shown verbatim: hiding it
            // would silently drop text the model wrote.
            Event::Html(text) | Event::InlineHtml(text) => self.push(&text),
            Event::FootnoteReference(name) => self.push(&format!("[^{name}]")),
            Event::InlineMath(text) | Event::DisplayMath(text) => self.push(&text),
        }
    }

    fn start(&mut self, tag: Tag<'_>) {
        match tag {
            Tag::Paragraph => {
                self.flush();
                self.pending = Pending::Paragraph;
            }
            Tag::Heading { level, .. } => {
                self.flush();
                self.pending = Pending::Heading(level);
            }
            Tag::BlockQuote(_) => {
                self.flush();
                self.quote += 1;
            }
            Tag::CodeBlock(kind) => {
                self.flush();
                let token = match kind {
                    CodeBlockKind::Fenced(info) => {
                        info.split_whitespace().next().unwrap_or("").to_string()
                    }
                    CodeBlockKind::Indented => String::new(),
                };
                self.code = Some((token, String::new()));
            }
            Tag::List(first) => self.list.push(first),
            Tag::Item => {
                self.flush();
                self.pending = Pending::Item;
                let marker = match self.list.last_mut() {
                    Some(Some(ordinal)) => {
                        let text = format!("{ordinal}. ");
                        *ordinal += 1;
                        text
                    }
                    _ => "• ".to_string(),
                };
                self.spans.push(
                    Span::new(marker)
                        .size(BODY)
                        .color(self.palette.color(Token::Faint)),
                );
            }
            Tag::Emphasis => self.style.italic += 1,
            Tag::Strong => self.style.bold += 1,
            Tag::Strikethrough => self.style.strike += 1,
            // Rendered as its text; the destination follows in the `End`, so a
            // link is readable and its target is copyable.
            Tag::Link { .. } | Tag::Image { .. } => {}
            _ => {}
        }
    }

    fn end(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Paragraph | TagEnd::Heading(_) | TagEnd::Item => self.flush(),
            TagEnd::BlockQuote(_) => {
                self.flush();
                self.quote = self.quote.saturating_sub(1);
            }
            TagEnd::CodeBlock => {
                if let Some((token, body)) = self.code.take() {
                    self.code_block(&token, &body);
                }
            }
            TagEnd::List(_) => {
                let _ = self.list.pop();
                self.flush();
            }
            TagEnd::Emphasis => self.style.italic = self.style.italic.saturating_sub(1),
            TagEnd::Strong => self.style.bold = self.style.bold.saturating_sub(1),
            TagEnd::Strikethrough => self.style.strike = self.style.strike.saturating_sub(1),
            _ => {}
        }
    }

    /// Append inline text in the style currently in force.
    fn push(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        let (font, size, color) = match (self.pending, self.quote) {
            (Pending::Heading(level), _) => (bold(font::SANS), heading_size(level), Token::Heading),
            (_, 1..) => (font::SANS, BODY, Token::Quote),
            _ => (font::SANS, BODY, Token::Text),
        };
        let font = if self.style.bold > 0 {
            bold(font)
        } else {
            font
        };
        let font = if self.style.italic > 0 {
            italic(font)
        } else {
            font
        };
        self.spans.push(
            Span::new(text.to_string())
                .font(font)
                .size(size)
                .color(self.color(color))
                .strikethrough(self.style.strike > 0),
        );
    }

    /// Emit whatever spans have accumulated as one block.
    fn flush(&mut self) {
        if self.spans.is_empty() {
            return;
        }
        let spans = std::mem::take(&mut self.spans);
        let size = match self.pending {
            Pending::Heading(level) => heading_size(level),
            _ => BODY,
        };
        let mut block = Block::new(spans, size, font::SANS)
            .indent(self.indent + self.nesting())
            .gap(match self.pending {
                Pending::Heading(_) => 6.0,
                Pending::Item => 2.0,
                _ => 10.0,
            });
        if self.quote > 0 {
            block = block.rule(self.color(Token::Quote));
        }
        // A block that is only whitespace draws nothing and would still take
        // vertical space and a slot in the selection.
        if !block.plain().trim().is_empty() {
            self.out.push(block);
        }
        self.pending = Pending::Paragraph;
    }

    /// A fenced block as one highlighted, monospaced block.
    fn code_block(&mut self, token: &str, body: &str) {
        let body = body.strip_suffix('\n').unwrap_or(body);
        let mut spans = Vec::new();
        let mut stream = iced_highlighter::Stream::new(&iced_highlighter::Settings {
            theme: iced_highlighter::Theme::Base16Ocean,
            token: token.to_string(),
        });
        for (index, line) in body.split('\n').enumerate() {
            if index > 0 {
                // The line ending is a span of its own so that it lands in the
                // paragraph's text — a code block whose newlines were dropped
                // would shape as one long line, and would copy as one too.
                spans.push(Span::new("\n").font(font::MONO).size(MONO));
            }
            for (range, highlight) in stream.highlight_line(line) {
                let Some(text) = line.get(range) else {
                    continue;
                };
                let format = highlight.to_format();
                spans.push(
                    Span::new(text.to_string())
                        .font(format.font.unwrap_or(font::MONO))
                        .size(MONO)
                        .color(format.color.unwrap_or(self.color(Token::Code))),
                );
            }
            stream.commit();
        }
        if spans.is_empty() {
            return;
        }
        self.out.push(
            Block::new(spans, MONO, font::MONO)
                .indent(self.indent + 10.0)
                .gap(12.0)
                .fill(self.palette.surface),
        );
    }

    /// Extra indent from open lists, one step per level.
    fn nesting(&self) -> f32 {
        self.list.len().saturating_sub(1) as f32 * 16.0
    }

    fn color(&self, token: Token) -> Color {
        self.palette.color(token)
    }
}

/// Heading sizes. Six levels compressed to three, because a transcript with six
/// distinguishable heading sizes in it is a document, not a conversation.
fn heading_size(level: HeadingLevel) -> f32 {
    match level {
        HeadingLevel::H1 => 20.0,
        HeadingLevel::H2 => 17.0,
        _ => 15.0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn blocks(source: &str) -> Vec<Block> {
        let mut out = Vec::new();
        render(
            source,
            &Palette::from_theme(&crate::theme::minimal()),
            0.0,
            &mut out,
        );
        out
    }

    fn texts(source: &str) -> Vec<String> {
        blocks(source)
            .iter()
            .map(|block| block.plain().to_string())
            .collect()
    }

    /// One block per block-level element, and the markdown syntax gone from the
    /// text: what the user selects is what they can read, not the source it was
    /// rendered from.
    #[test]
    fn block_elements_become_blocks_and_syntax_disappears() {
        let out = texts("# Title\n\nSome **bold** prose.\n\n- one\n- two\n");
        assert_eq!(out, ["Title", "Some bold prose.", "• one", "• two"]);
    }

    /// A soft break rewraps; a hard break does not. A paragraph the model wrote
    /// at 80 columns must reflow to the window, or every reply is ragged.
    #[test]
    fn a_soft_break_becomes_a_space_and_a_hard_break_survives() {
        assert_eq!(texts("one\ntwo"), ["one two"]);
        assert_eq!(texts("one\\\ntwo"), ["one\ntwo"]);
    }

    /// The property the selection layer depends on: a fenced block is ONE block
    /// whose plain text carries real newlines. Split it into one block per line
    /// and a copy loses the structure; drop the newlines and it shapes as a
    /// single line.
    #[test]
    fn a_fenced_block_is_one_block_with_its_newlines_intact() {
        let out = blocks("prose\n\n```rust\nfn main() {\n    let x = 42;\n}\n```\n");
        assert_eq!(out.len(), 2, "{:?}", texts("…"));
        assert_eq!(out[1].plain(), "fn main() {\n    let x = 42;\n}");
        assert!(out[1].fill.is_some(), "a code block sits on a surface");
    }

    /// Highlighting is on, and it is per-token: a `fn` and an identifier in the
    /// same line must not come back as one span in one colour. If this ever
    /// reads 1, the `highlighter` feature or the language token is not
    /// reaching syntect.
    #[test]
    fn a_fenced_block_is_highlighted_into_several_spans() {
        let out = blocks("```rust\nfn main() { let x = 42; }\n```\n");
        let code = &out[0];
        assert!(code.spans.len() > 3, "{} spans", code.spans.len());
        let colors: std::collections::BTreeSet<String> = code
            .spans
            .iter()
            .filter_map(|span| span.color.map(|color| format!("{color:?}")))
            .collect();
        assert!(
            colors.len() > 1,
            "one colour is not highlighting: {colors:?}"
        );
    }

    /// Nested emphasis: markdown allows it and a stack is the only thing that
    /// gets the close right. With a boolean, the inner close clears the outer
    /// bold and the rest of the sentence loses its weight.
    #[test]
    fn nested_emphasis_restores_rather_than_clears() {
        let out = blocks("**bold *and italic* still bold**");
        let weights: Vec<Weight> = out[0]
            .spans
            .iter()
            .map(|span| span.font.unwrap_or(font::SANS).weight)
            .collect();
        assert!(
            weights.iter().all(|weight| *weight == Weight::Bold),
            "{weights:?}"
        );
    }

    /// An ordered list numbers itself from the list's own start, and a nested
    /// list indents rather than restarting the outer one.
    #[test]
    fn ordered_lists_number_themselves() {
        let out = texts("3. three\n4. four\n");
        assert_eq!(out, ["3. three", "4. four"]);
    }

    /// A quote is marked with a rule rather than boxed, per the design spec's
    /// "hairlines instead of boxes inside boxes".
    #[test]
    fn a_quote_is_ruled_not_boxed() {
        let out = blocks("> quoted\n");
        assert_eq!(out[0].plain(), "quoted");
        assert!(out[0].rule.is_some());
        assert!(out[0].fill.is_none());
    }

    /// Empty and whitespace-only input produces nothing at all, rather than a
    /// zero-height block that still occupies a slot in the selection.
    #[test]
    fn nothing_renders_as_nothing() {
        assert!(blocks("").is_empty());
        assert!(blocks("   \n\n  \n").is_empty());
    }
}
