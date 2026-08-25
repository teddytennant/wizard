//! The escape hatch: exact text geometry, out of cosmic-text.
//!
//! iced's [`Paragraph`](iced::advanced::text::Paragraph) trait exposes hit
//! testing, but through a lossy type. `hit_test` returns
//! `Hit::CharOffset(usize)`, which is built from the underlying
//! `cosmic_text::Cursor`'s `index` field with its `line` field **discarded**.
//! For a paragraph holding one logical line that is fine: `index` is the whole
//! offset. For a paragraph holding a hard newline it is wrong in the worst way —
//! it is a plausible small number — because `index` is relative to the buffer
//! line the click landed on, so a click on line 3 of a code block reports an
//! offset near the start of the block. Every code block has newlines. So does
//! every tool row.
//!
//! The way out is that `iced_graphics::text::Paragraph::buffer()` is public and
//! `cosmic_text` is re-exported. That gives the full
//! `Cursor { line, index, affinity }` from [`cosmic_text::Buffer::hit`], and
//! per-visual-line geometry from `layout_runs()` — which the trait does not
//! expose at all, and which is what makes an *exact* partial highlight cheap
//! rather than a stack of guessed rectangles.
//!
//! # Two traps, both load-bearing
//!
//! **`hit` does not respect its own bounds.** It clamps: a point far above the
//! paragraph returns the first cursor, a point far below returns the last, and
//! an x outside the line returns its nearest end. So a widget stacking N
//! paragraphs cannot ask each one whether it was hit — every one of them says
//! yes, and the first to be asked wins, which resolves every click in the
//! transcript to block 0. The vertical band dispatch has to happen in the caller
//! ([`super::widget::Selectable`]), which knows the y offsets because it laid
//! them out. This module takes a point already known to be inside a paragraph,
//! and its clamping is then a feature: a drag that strays sideways out of the
//! text still selects to the end of the line.
//!
//! **Buffer lines are not visual lines.** `Cursor::line` indexes
//! [`cosmic_text::Buffer::lines`], which are the *logical* lines (split on hard
//! line endings). A soft-wrapped paragraph is one buffer line across several
//! `LayoutRun`s. [`Lines`] converts between the two coordinate systems once, at
//! the top of every operation, so nothing below it has to remember which one it
//! is holding.

use iced::advanced::graphics::text::{Paragraph, cosmic_text};
use iced::advanced::text::Paragraph as _;
use iced::{Point, Rectangle, Size};

/// Where a cosmic-text buffer's logical lines sit inside the block's plain text.
///
/// The two coordinate systems this layer straddles are "byte offset in the
/// block" (what a selection is made of, what a copy slices) and
/// `Cursor { line, index }` (what cosmic-text speaks). This is the map between
/// them, and it is derived from the buffer rather than from the block's text so
/// that cosmic-text's own idea of where the lines are is the one that wins — it
/// splits on `\r\n`, `\r` and `\n`, and a second implementation of that rule
/// here would be a second chance to get it wrong.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Lines {
    /// Byte offset at which each logical line's text begins.
    starts: Vec<usize>,
    /// Byte length of each logical line's text, excluding its line ending.
    lengths: Vec<usize>,
}

impl Lines {
    /// Map a shaped buffer's logical lines.
    pub fn of(buffer: &cosmic_text::Buffer) -> Self {
        let mut starts = Vec::with_capacity(buffer.lines.len());
        let mut lengths = Vec::with_capacity(buffer.lines.len());
        let mut at = 0;
        for line in &buffer.lines {
            starts.push(at);
            lengths.push(line.text().len());
            at += line.text().len() + line.ending().as_str().len();
        }
        Self { starts, lengths }
    }

    /// Whether the mapped text is empty.
    pub fn is_empty(&self) -> bool {
        self.starts.is_empty()
    }

    /// Total length of the mapped text, line endings included.
    pub fn len(&self) -> usize {
        match (self.starts.last(), self.lengths.last()) {
            (Some(start), Some(length)) => start + length,
            _ => 0,
        }
    }

    /// The block-relative byte offset a cursor names.
    pub fn offset(&self, cursor: cosmic_text::Cursor) -> usize {
        let start = self.starts.get(cursor.line).copied().unwrap_or(0);
        let length = self.lengths.get(cursor.line).copied().unwrap_or(0);
        start + cursor.index.min(length)
    }

    /// The cursor for a block-relative byte offset.
    ///
    /// An offset that lands *on* a line ending belongs to the line it
    /// terminates, not to the next one: that is what makes a selection ending at
    /// the end of a line highlight to the right edge of that line rather than
    /// starting a zero-width highlight on the line below.
    pub fn cursor(&self, offset: usize) -> cosmic_text::Cursor {
        let line = match self.starts.binary_search(&offset) {
            Ok(line) => line,
            // `binary_search` on a miss returns where the value *would* be
            // inserted, so the line that contains it is the one before.
            Err(0) => 0,
            Err(next) => next - 1,
        };
        let start = self.starts.get(line).copied().unwrap_or(0);
        let length = self.lengths.get(line).copied().unwrap_or(0);
        cosmic_text::Cursor::new(line, offset.saturating_sub(start).min(length))
    }
}

/// The block-relative byte offset at `local`, a point in the paragraph's own
/// coordinate space (origin at its top left).
///
/// Clamps, deliberately — see the module header. `None` only when the paragraph
/// has no laid-out runs at all, which is an empty block.
pub fn offset_at(paragraph: &Paragraph, lines: &Lines, local: Point) -> Option<usize> {
    let cursor = paragraph.buffer().hit(local.x, local.y)?;
    Some(lines.offset(cursor))
}

/// The rectangles covering `[from, to)` inside this paragraph, one per visual
/// line, in the paragraph's own coordinate space.
///
/// Exactness is the whole point of reaching past the trait. `LayoutRun::highlight`
/// walks the run's glyphs and returns the pixel span of the intersection, which
/// handles soft wrapping, bidi and clusters that a per-span bounding box cannot:
/// `Paragraph::span_bounds` can only say "this whole span", so a selection
/// ending in the middle of a highlighted identifier would light the identifier
/// up to its end.
///
/// Two cases the glyph walk cannot answer on its own, both about *nothing*:
///
/// - A blank line inside the selection has no glyphs, so it would draw no
///   rectangle and a selected code block would appear to skip its empty lines.
/// - A selected line ending has no glyph either, so a selection running through
///   several lines would stop at the last character of each and read as a column
///   of separate selections rather than one continuous one.
///
/// Both are answered by [`NEWLINE_PAD`]: a stub of highlight past the end of a
/// line whose ending is inside the selection. It is drawn only on the last
/// visual run of a logical line, so a soft wrap — which is not a line ending and
/// which the user did not select — gets none.
pub fn highlight(paragraph: &Paragraph, lines: &Lines, from: usize, to: usize) -> Vec<Rectangle> {
    if from >= to {
        return Vec::new();
    }
    let start = lines.cursor(from);
    let end = lines.cursor(to);
    let pad = NEWLINE_PAD * paragraph.size().0;

    let mut rects = Vec::new();
    for run in paragraph.buffer().layout_runs() {
        let line_length = lines.lengths.get(run.line_i).copied().unwrap_or(0);
        let line_start = lines.starts.get(run.line_i).copied().unwrap_or(0);
        // The run is the tail of its logical line when it reaches that line's
        // last byte — the test that tells a hard line ending from a soft wrap.
        let is_tail = run
            .glyphs
            .last()
            .is_none_or(|glyph| glyph.end >= line_length);
        // The line ending after this run is inside the selection — which needs
        // both bounds, not just the upper one.
        //
        // With only `to >`, every blank line *above* the selection qualified
        // too: `to` is larger than any earlier line's ending by definition, so
        // selecting one word near the bottom of a code block painted a
        // selection-coloured stub on every empty line above it in the same
        // block. Visible as a column of small marks the user never selected,
        // never below the selection, and worse the further down the block you
        // clicked.
        let ending = line_start + line_length;
        let ending_selected = is_tail && to > ending && from <= ending;

        match run.highlight(start, end) {
            Some((x, width)) => {
                let width = if ending_selected { width + pad } else { width };
                rects.push(Rectangle {
                    x,
                    y: run.line_top,
                    width,
                    height: run.line_height,
                });
            }
            // No glyph intersected. A blank line whose ending is selected still
            // has to show as selected, or the selection appears to have a hole
            // in it.
            None if ending_selected && run.glyphs.is_empty() => rects.push(Rectangle {
                x: 0.0,
                y: run.line_top,
                width: pad,
                height: run.line_height,
            }),
            None => {}
        }
    }
    rects
}

/// How far past the end of a line a selected line ending is drawn, as a
/// fraction of the text size. Roughly the width of a space, which is what the
/// selection is standing in for.
const NEWLINE_PAD: f32 = 0.4;

/// The shaped size of a paragraph, as this layer measures blocks.
pub fn measure(paragraph: &Paragraph) -> Size {
    paragraph.min_bounds()
}

#[cfg(test)]
mod tests {
    use iced::advanced::text::{self, Span};
    use iced::{Font, Pixels};

    use super::*;

    /// Shape one block of text the way [`super::widget::Selectable`] does, on a
    /// headless renderer. No window, no compositor: `Paragraph` reaches the
    /// process-wide cosmic-text font system directly.
    fn shape(text: &str, width: f32, font: Font) -> Paragraph {
        let spans: Vec<Span<'_, ()>> = vec![Span::new(text).font(font)];
        Paragraph::with_spans(text::Text {
            content: &spans,
            bounds: Size::new(width, f32::INFINITY),
            size: Pixels(14.0),
            line_height: text::LineHeight::default(),
            font,
            align_x: text::Alignment::Left,
            align_y: iced::alignment::Vertical::Top,
            shaping: text::Shaping::Advanced,
            wrapping: text::Wrapping::Word,
        })
    }

    /// The trap the whole module exists for: `Cursor` carries the line and
    /// `Hit::CharOffset` does not, so a click on the fourth line of a code block
    /// has to come back as an offset near the end of the block and not near its
    /// start.
    #[test]
    fn offsets_on_a_multiline_block_span_the_whole_block() {
        let text = "AAAA\nBBBB\nCCCC\nDDDD";
        let paragraph = shape(text, 400.0, Font::MONOSPACE);
        let lines = Lines::of(paragraph.buffer());
        assert_eq!(lines.len(), text.len());

        let height = paragraph.min_bounds().height / 4.0;
        let offsets: Vec<usize> = (0..4)
            .map(|line| {
                let y = height * line as f32 + height / 2.0;
                offset_at(&paragraph, &lines, Point::new(300.0, y)).expect("a hit")
            })
            .collect();
        // Ends of "AAAA", "BBBB", "CCCC", "DDDD" in block coordinates.
        assert_eq!(offsets, vec![4, 9, 14, 19]);

        // And the lossy trait method is what it would have said instead: the
        // same small number four times, because it throws the line away.
        let lossy: Vec<usize> = (0..4)
            .map(|line| {
                let y = height * line as f32 + height / 2.0;
                paragraph
                    .hit_test(Point::new(300.0, y))
                    .expect("a hit")
                    .cursor()
            })
            .collect();
        assert_eq!(
            lossy,
            vec![4, 4, 4, 4],
            "this is the bug being routed around"
        );
    }

    /// Round-tripping every offset through the cursor form has to be the
    /// identity, or a selection would drift by a byte each time it was
    /// re-derived for a redraw.
    #[test]
    fn offsets_and_cursors_round_trip() {
        let text = "one\n\nthree\nfour";
        let paragraph = shape(text, 400.0, Font::MONOSPACE);
        let lines = Lines::of(paragraph.buffer());
        for offset in 0..=text.len() {
            assert_eq!(lines.offset(lines.cursor(offset)), offset, "at {offset}");
        }
    }

    /// An offset sitting on a line ending belongs to the line it ends. If it
    /// migrated to the next line, a selection stopping at end-of-line would
    /// draw a zero-width sliver on the line below instead of finishing the line
    /// above.
    #[test]
    fn an_offset_on_a_line_ending_stays_on_its_own_line() {
        let paragraph = shape("ab\ncd", 400.0, Font::MONOSPACE);
        let lines = Lines::of(paragraph.buffer());
        assert_eq!(lines.cursor(2), cosmic_text::Cursor::new(0, 2));
        assert_eq!(lines.cursor(3), cosmic_text::Cursor::new(1, 0));
    }

    /// A fully selected code block lights every visual line, blank ones
    /// included. The blank line is the case a glyph walk alone gets wrong.
    #[test]
    fn a_full_selection_covers_every_visual_line_including_blank_ones() {
        let text = "fn main() {\n\n    println!(\"hi\");\n}";
        let paragraph = shape(text, 600.0, Font::MONOSPACE);
        let lines = Lines::of(paragraph.buffer());
        let rects = highlight(&paragraph, &lines, 0, text.len());
        assert_eq!(rects.len(), 4, "one per line: {rects:?}");
        for rect in &rects {
            assert!(rect.width > 0.0, "a selected line with no width: {rect:?}");
            assert!(rect.height > 0.0);
        }
        // Top to bottom, no overlap.
        for pair in rects.windows(2) {
            assert!(pair[0].y < pair[1].y, "{rects:?}");
        }
    }

    /// A selection near the bottom of a block paints nothing above itself.
    ///
    /// The bug: `ending_selected` tested only `to > line_ending`, and `to` is
    /// greater than every earlier line's ending by definition — so every blank
    /// line *above* the selection got a line-ending stub. Selecting one word
    /// low in a code block drew a column of selection-coloured marks up the
    /// blank lines above it, never below, and worse the further down you
    /// clicked.
    #[test]
    fn a_selection_lights_no_blank_line_above_where_it_starts() {
        //        0          12 13         25 26         38 39
        let text = "alpha line

bravo line

charlie xyz
";
        let paragraph = shape(text, 600.0, Font::MONOSPACE);
        let lines = Lines::of(paragraph.buffer());

        // "charlie" on the last line — after both blank lines.
        let from = text.find("charlie").expect("the word is in the fixture");
        let rects = highlight(&paragraph, &lines, from, from + "charlie".len());

        assert!(!rects.is_empty(), "the word itself must light");
        let word_top = rects.iter().map(|r| r.y).fold(f32::INFINITY, f32::min);
        for rect in &rects {
            assert!(
                rect.y >= word_top - 0.5,
                "a rectangle above the selected word — that is the stub bug: \
                 {rect:?} against a word at y={word_top}, all of {rects:?}"
            );
        }
        assert_eq!(
            rects.len(),
            1,
            "one word on one line is one rectangle: {rects:?}"
        );
    }

    /// A partial selection stops where it was asked to. The exactness claim: a
    /// selection of the first two characters must be narrower than one of the
    /// first six, which a per-span bounding box could not express because both
    /// live in the same span.
    #[test]
    fn a_partial_selection_is_narrower_than_a_longer_one() {
        let paragraph = shape("abcdefghij", 600.0, Font::MONOSPACE);
        let lines = Lines::of(paragraph.buffer());
        let short = highlight(&paragraph, &lines, 0, 2);
        let long = highlight(&paragraph, &lines, 0, 6);
        assert_eq!(short.len(), 1);
        assert_eq!(long.len(), 1);
        assert!(
            short[0].width < long[0].width,
            "{:?} vs {:?}",
            short[0],
            long[0]
        );
    }

    /// An empty range highlights nothing. A click without a drag is not a
    /// selection, and drawing a caret-width sliver for one would look like a
    /// text cursor in a transcript that has none.
    #[test]
    fn an_empty_range_highlights_nothing() {
        let paragraph = shape("abcdef", 600.0, Font::MONOSPACE);
        let lines = Lines::of(paragraph.buffer());
        assert!(highlight(&paragraph, &lines, 3, 3).is_empty());
        assert!(highlight(&paragraph, &lines, 4, 2).is_empty());
    }

    /// Soft wrapping is not a line ending. A wrapped paragraph is one logical
    /// line, so it maps to one entry in `Lines` while producing several visual
    /// rectangles — and no newline pad, because the user selected no newline.
    #[test]
    fn soft_wrapping_makes_visual_lines_but_not_logical_ones() {
        let text = "the quick brown fox jumps over the lazy dog and keeps on going";
        let paragraph = shape(text, 120.0, Font::DEFAULT);
        let lines = Lines::of(paragraph.buffer());
        assert_eq!(lines.starts.len(), 1, "one logical line");
        let rects = highlight(&paragraph, &lines, 0, text.len());
        assert!(rects.len() > 1, "but several visual ones: {rects:?}");
    }
}
