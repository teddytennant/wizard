//! What the selection layer selects *over*.
//!
//! A [`Block`] is one run of styled text that lays out as a single paragraph:
//! one markdown paragraph, one fenced code block, one tool row's header, one
//! tool row's output. The transcript is a vertical stack of them, and a
//! selection is a range from a byte offset in one block to a byte offset in
//! another.
//!
//! # Why a block, and not a widget
//!
//! The obvious shape — a `column!` of `text`, `container` and `row` widgets —
//! is the one the spike proved cannot work: a drag that starts in one stock
//! widget and ends in another is two widgets each seeing half a gesture, and
//! neither of them owns a range that spans the gap. Selection has to be owned by
//! something that can see every run at once, so the runs are data rather than
//! children, and the one widget above them ([`super::widget::Selectable`]) lays
//! them out itself.
//!
//! The cost of that is the styling a block can carry: it is whatever a single
//! `Paragraph` can express (per-span font, size, colour) plus the small amount
//! of chrome this module adds around it (an indent, a fill, a gutter rule).
//! That is enough for prose, code and tool rows, which is the acceptance bar.
//! Images and collapsible rows are not blocks and are Phase 2.
//!
//! # The plain text is the source of truth
//!
//! Every offset in this layer — a drag anchor, a highlight boundary, a copied
//! range — is a byte offset into [`Block::plain`], the concatenation of the
//! block's span texts. It is computed once at construction rather than derived
//! from the shaped paragraph, because the shaped paragraph is a cache that can
//! be rebuilt at any moment and an offset that changed meaning when a window was
//! resized would be a selection that moved on its own.

use std::hash::{Hash, Hasher};

use iced::advanced::text::Span;
use iced::{Color, Font, Pixels};

/// One run of styled text that lays out as a single paragraph.
#[derive(Debug, Clone)]
pub struct Block {
    /// The styled runs, in order. Their concatenated text is [`Block::plain`].
    pub spans: Vec<Span<'static, ()>>,
    /// The block's own text, which is what offsets index and what a copy reads.
    plain: String,
    /// Base text size; a span may override it.
    pub size: Pixels,
    /// Base font; a span may override it.
    pub font: Font,
    /// Left inset, in pixels. Tool output and quotes are indented under the
    /// thing they belong to.
    pub indent: f32,
    /// Space below this block.
    pub gap: f32,
    /// A fill drawn behind the block's full width, inset by [`Block::indent`].
    /// Code blocks and tool bodies sit on a surface; prose does not.
    pub fill: Option<Color>,
    /// A 2px rule drawn down the left of the block. Block quotes and the
    /// user's own messages are marked this way rather than boxed.
    pub rule: Option<Color>,
}

impl Block {
    /// A block from its spans. The plain text is derived here, once.
    pub fn new(spans: Vec<Span<'static, ()>>, size: f32, font: Font) -> Self {
        let plain = spans.iter().map(|span| span.text.as_ref()).collect();
        Self {
            spans,
            plain,
            size: Pixels(size),
            font,
            indent: 0.0,
            gap: 10.0,
            fill: None,
            rule: None,
        }
    }

    /// A block of one unstyled run.
    pub fn plain_text(text: impl Into<String>, size: f32, font: Font, color: Color) -> Self {
        Self::new(vec![Span::new(text.into()).color(color)], size, font)
    }

    pub fn indent(mut self, indent: f32) -> Self {
        self.indent = indent;
        self
    }

    pub fn gap(mut self, gap: f32) -> Self {
        self.gap = gap;
        self
    }

    pub fn fill(mut self, fill: Color) -> Self {
        self.fill = Some(fill);
        self
    }

    pub fn rule(mut self, rule: Color) -> Self {
        self.rule = Some(rule);
        self
    }

    /// The block's text: what a selection over it copies.
    pub fn plain(&self) -> &str {
        &self.plain
    }

    /// A fingerprint identifying this block's *shaped* form.
    ///
    /// Two blocks with the same fingerprint at the same width shape to the same
    /// paragraph, so one can stand in for the other — which is what
    /// [`super::cache::ParagraphCache`] trades on. Everything that reaches
    /// cosmic-text is in it and nothing else is: the fill and the rule are drawn
    /// by this module after the fact and change no glyph, so folding them in
    /// would throw away a perfectly good paragraph every time a tool row went
    /// from running to done.
    pub fn fingerprint(&self, width: f32) -> u64 {
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        self.plain.hash(&mut hasher);
        self.size.0.to_bits().hash(&mut hasher);
        self.indent.to_bits().hash(&mut hasher);
        // A window resize invalidates every paragraph, because wrapping is a
        // function of the width. Quantized to whole pixels so a fractional
        // scale factor that jitters in the last bits does not reshape the
        // transcript on every frame.
        (width.max(0.0) as u32).hash(&mut hasher);
        // Fonts and per-span colours are not `Hash`, so they go in by their
        // parts. Colour matters because it is baked into the shaped paragraph's
        // spans, and a tool row that turns red on failure must not keep the
        // paragraph it had while it was running.
        hash_font(self.font, &mut hasher);
        for span in &self.spans {
            span.text.as_ref().hash(&mut hasher);
            span.size.map(|size| size.0.to_bits()).hash(&mut hasher);
            span.underline.hash(&mut hasher);
            span.strikethrough.hash(&mut hasher);
            if let Some(font) = span.font {
                hash_font(font, &mut hasher);
            }
            if let Some(color) = span.color {
                for channel in [color.r, color.g, color.b, color.a] {
                    channel.to_bits().hash(&mut hasher);
                }
            }
        }
        hasher.finish()
    }
}

/// Fold a [`Font`] into a hasher. It is `Copy + PartialEq` but not `Hash`, and
/// its family is a `&'static str` or a static name either way.
fn hash_font(font: Font, hasher: &mut impl Hasher) {
    match font.family {
        iced::font::Family::Name(name) => name.hash(hasher),
        other => std::mem::discriminant(&other).hash(hasher),
    }
    font.weight.hash(hasher);
    font.stretch.hash(hasher);
    font.style.hash(hasher);
}

/// The byte range of the word containing `at`, for a double click.
///
/// "Word" is a run of alphanumerics, `_`, `-` and `.`, which is a deliberate
/// widening of the usual rule: what a person double-clicks in this transcript is
/// most often `src/native/select/block.rs` or `--features`, and a definition
/// that stops at the first punctuation hands them `src` and makes them do it
/// again. A double click on whitespace selects the whitespace run, so the
/// gesture always selects *something* and a subsequent drag has an anchor.
pub fn word_at(text: &str, at: usize) -> (usize, usize) {
    if text.is_empty() {
        return (0, 0);
    }
    let at = floor_boundary(text, at.min(text.len()));
    let here = text[at..].chars().next();
    let before = text[..at].chars().next_back();
    // Which character the click belongs to, when it landed exactly between two.
    //
    // Two adjustments, both about the boundary case. A click past the last
    // character belongs to the last character. And a click on the boundary
    // between a word and the whitespace after it belongs to the *word*:
    // cosmic-text puts the cursor after a glyph when the click was on its right
    // half, so without this, double-clicking the second half of the last letter
    // of a word selects the space beside it.
    let seed = match (here, before) {
        (Some(here), Some(before)) if classify(here) == 1 && classify(before) != 1 => before,
        (Some(here), _) => here,
        (None, Some(before)) => before,
        (None, None) => return (at, at),
    };
    let wanted = classify(seed);
    let start = text[..at]
        .char_indices()
        .rev()
        .take_while(|(_, ch)| classify(*ch) == wanted)
        .map(|(index, _)| index)
        .last()
        .unwrap_or(at);
    let end = text[at..]
        .char_indices()
        .take_while(|(_, ch)| classify(*ch) == wanted)
        .map(|(index, ch)| at + index + ch.len_utf8())
        .last()
        .unwrap_or(at);
    (start, end)
}

/// The byte range of the line containing `at`, for a triple click. The
/// terminating newline is excluded: a triple click that copied an invisible
/// trailing newline would paste a line break the user cannot see they selected.
pub fn line_at(text: &str, at: usize) -> (usize, usize) {
    let at = floor_boundary(text, at.min(text.len()));
    let start = text[..at].rfind('\n').map_or(0, |index| index + 1);
    let end = text[at..].find('\n').map_or(text.len(), |index| at + index);
    (start, end)
}

/// Which of the three character classes `ch` is in, for [`word_at`].
fn classify(ch: char) -> u8 {
    if ch.is_alphanumeric() || matches!(ch, '_' | '-' | '.' | '/') {
        0
    } else if ch.is_whitespace() {
        1
    } else {
        2
    }
}

/// The largest char boundary at or below `at`.
///
/// Offsets arrive from cosmic-text, which indexes graphemes, and from
/// arithmetic in this module. Slicing a `str` on a non-boundary panics, and a
/// panic in a draw loop takes the window with it, so every slice in this layer
/// goes through here first.
pub fn floor_boundary(text: &str, mut at: usize) -> usize {
    at = at.min(text.len());
    while at > 0 && !text.is_char_boundary(at) {
        at -= 1;
    }
    at
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fingerprint that ignored the text would let one block's paragraph be
    /// reused for another's, which is a transcript that renders the wrong
    /// words. A fingerprint that ignored the width would keep a paragraph
    /// wrapped for the old window.
    #[test]
    fn a_fingerprint_covers_text_size_and_width() {
        let block = |text: &str, size: f32| {
            Block::plain_text(text.to_string(), size, Font::DEFAULT, Color::WHITE)
        };
        let base = block("hello", 14.0).fingerprint(400.0);
        assert_eq!(base, block("hello", 14.0).fingerprint(400.0));
        assert_ne!(base, block("hellp", 14.0).fingerprint(400.0));
        assert_ne!(base, block("hello", 15.0).fingerprint(400.0));
        assert_ne!(base, block("hello", 14.0).fingerprint(401.0));
    }

    /// The fill and the rule are drawn around the paragraph, not into it. A
    /// tool row that changes colour when it finishes must not pay for a
    /// reshape.
    #[test]
    fn chrome_does_not_change_a_fingerprint() {
        let base = Block::plain_text("ls -la", 13.0, Font::MONOSPACE, Color::WHITE);
        let dressed = base.clone().fill(Color::BLACK).rule(Color::WHITE).gap(4.0);
        assert_eq!(base.fingerprint(300.0), dressed.fingerprint(300.0));
    }

    /// But a span colour *is* baked into the shaped paragraph, so it must move
    /// the fingerprint or a failed tool call keeps the paragraph it had while
    /// it was running and never turns red.
    #[test]
    fn a_span_color_changes_a_fingerprint() {
        let one = Block::plain_text("✗ execute", 13.0, Font::MONOSPACE, Color::WHITE);
        let two = Block::plain_text("✗ execute", 13.0, Font::MONOSPACE, Color::BLACK);
        assert_ne!(one.fingerprint(300.0), two.fingerprint(300.0));
    }

    /// The widened word rule: a path is one word, because a path is what gets
    /// double-clicked in this transcript.
    #[test]
    fn a_double_click_takes_a_whole_path() {
        let text = "see src/native/select/block.rs for it";
        let (start, end) = word_at(text, 10);
        assert_eq!(&text[start..end], "src/native/select/block.rs");
    }

    /// Every offset in a word has to give the same word, including the two
    /// ends, or a double click that lands one pixel left of where the user
    /// aimed selects a different thing.
    #[test]
    fn a_word_is_the_same_from_any_offset_inside_it() {
        let text = "alpha beta gamma";
        let expected = (6, 10);
        for at in 6..=10 {
            assert_eq!(word_at(text, at), expected, "from offset {at}");
        }
        assert_eq!(&text[expected.0..expected.1], "beta");
    }

    /// Multi-byte text is where a naive `text[a..b]` panics. The word rule has
    /// to land on boundaries.
    #[test]
    fn word_and_line_ranges_land_on_char_boundaries() {
        let text = "héllo wörld\nzweite zeile";
        for at in 0..text.len() {
            let (start, end) = word_at(text, at);
            assert!(text.is_char_boundary(start) && text.is_char_boundary(end));
            let (start, end) = line_at(text, at);
            assert!(text.is_char_boundary(start) && text.is_char_boundary(end));
        }
    }

    /// A triple click takes the line and stops at the break: copying the
    /// newline would paste a line ending the user never saw themselves select.
    #[test]
    fn a_triple_click_takes_the_line_without_its_newline() {
        let text = "fn main() {\n    println!(\"hi\");\n}";
        let (start, end) = line_at(text, 20);
        assert_eq!(&text[start..end], "    println!(\"hi\");");
    }

    /// Whitespace is its own class, so a double click in the gutter of a code
    /// block selects the indent rather than jumping to the next identifier.
    #[test]
    fn a_double_click_on_whitespace_takes_the_whitespace() {
        let text = "    indented";
        let (start, end) = word_at(text, 2);
        assert_eq!(&text[start..end], "    ");
    }

    /// But the boundary between a word and the space after it belongs to the
    /// word. cosmic-text puts the cursor *after* a glyph when the click landed
    /// on its right half, so without the tie-break, double-clicking the second
    /// half of a word's last letter selects the space beside it.
    #[test]
    fn a_click_on_a_word_boundary_prefers_the_word() {
        let text = "alpha beta gamma";
        let (start, end) = word_at(text, 10);
        assert_eq!(&text[start..end], "beta");
    }
}
