//! One widget over every text run in the transcript.
//!
//! This is the thing the whole workstream turned on: a user has to be able to
//! drag from a prose paragraph, through a code block, into a tool row, and press
//! Ctrl+C. Stock iced 0.14 cannot do it — `Selection` exists only inside
//! `text_editor` and `text_input`, both single-buffer — and a synthesized drag
//! across three stock widgets is ignored by all three, because each of them sees
//! half a gesture over text it does not own.
//!
//! So the runs stop being widgets. [`Selectable`] takes a `Vec<Block>` and lays
//! them out itself: it owns the paragraphs, the y offsets, the hit testing, the
//! highlight and the clipboard write. There is exactly one widget, so there is
//! exactly one gesture, and a range that spans three kinds of block is not a
//! special case — it is the ordinary case, because from in here they are all
//! just paragraphs.
//!
//! # The anatomy of a selection
//!
//! An [`Anchor`] is `(block, offset)`: which paragraph, and how many bytes into
//! its plain text. It is `Ord`, so a drag upward is the same range as a drag
//! downward with the ends swapped, and no code below has to know which way the
//! mouse went. `start` is where the button went down and `focus` is where the
//! cursor is now; the *range* is the two of them sorted, and keeping them
//! unsorted is what lets a drag reverse across its own origin without the
//! selection sticking.
//!
//! # Bounds are the caller's job
//!
//! `cosmic_text::Buffer::hit` clamps: it answers `Some` for a point far outside
//! the paragraph. Stacking N paragraphs and asking each one whether it was hit
//! therefore resolves every click to block 0. [`Selectable::anchor_at`] does its
//! own vertical band dispatch first, using the offsets it computed during
//! layout, and only then asks the paragraph — where the clamping becomes
//! desirable, because a drag that strays into the left margin should still
//! select to the start of the line rather than stopping dead.
//!
//! # What is not here
//!
//! **Autoscroll.** A drag that reaches the bottom edge does not scroll the
//! transcript, because scrolling belongs to the `scrollable` above this widget
//! and iced 0.14 gives a child no way to ask its ancestor to move. Doing it
//! properly means either owning the scroll offset in here or driving an
//! `Operation` from `update`, and both are Phase 2.
//!
//! **Images and folds.** A block is text. Images and collapsible tool rows are
//! not blocks and cannot be selected across; see `docs/native-gui.md`.

use iced::advanced::graphics::text::Paragraph as GraphicsParagraph;
use iced::advanced::text::{self, Paragraph as _};
use iced::advanced::widget::{Tree, Widget, tree};
use iced::advanced::{Clipboard, Layout, Shell, clipboard, layout, mouse, renderer};
use iced::{Color, Element, Event, Font, Length, Point, Rectangle, Size, keyboard};

use super::block::{Block, floor_boundary, line_at, word_at};
use super::cache::ParagraphCache;
use super::geometry::{self, Lines};

/// A position in the transcript: which block, and which byte offset in it.
///
/// `Ord` derives from the field order, which is the reading order, so comparing
/// two anchors is comparing two points in the conversation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Anchor {
    pub block: usize,
    pub offset: usize,
}

/// How much a gesture selects at a time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum Granularity {
    #[default]
    Character,
    Word,
    Line,
}

/// One shaped block, plus what the selection layer needs to reason about it.
struct Shaped {
    paragraph: GraphicsParagraph,
    /// The logical-line map, derived once per shape rather than per hit test.
    lines: Lines,
    /// Fingerprint under which this paragraph goes back into the cache.
    key: u64,
    /// Top of the block, relative to the widget's own origin.
    top: f32,
    height: f32,
}

/// Per-widget state, kept across rebuilds of the element tree by iced.
#[derive(Default)]
struct State {
    shaped: Vec<Shaped>,
    cache: ParagraphCache<GraphicsParagraph>,
    /// Where the button went down.
    start: Option<Anchor>,
    /// Where the cursor is now. Unsorted against `start` on purpose.
    focus: Option<Anchor>,
    dragging: bool,
    granularity: Granularity,
    /// The range the initial double or triple click established, so that
    /// dragging afterwards extends the selection by whole words or lines
    /// without ever shrinking below the one that was clicked.
    seed: Option<(Anchor, Anchor)>,
    last_click: Option<mouse::Click>,
}

impl State {
    /// The current selection, low end first, or `None` when it is empty.
    fn range(&self) -> Option<(Anchor, Anchor)> {
        let (a, b) = (self.start?, self.focus?);
        let (low, high) = if a <= b { (a, b) } else { (b, a) };
        (low != high).then_some((low, high))
    }

    fn clear(&mut self) {
        self.start = None;
        self.focus = None;
        self.seed = None;
        self.granularity = Granularity::Character;
    }
}

/// The transcript's text layer: every run, one gesture, one clipboard.
pub struct Selectable<'a> {
    blocks: &'a [Block],
    selection: Color,
    /// Fallback colour for a span that names none. `None` defers to whatever
    /// the renderer's ambient style says, which is what a widget with no
    /// opinion should do — a sentinel colour standing for "no opinion" would be
    /// a colour a caller could legitimately pass.
    text: Option<Color>,
    padding: f32,
}

impl<'a> Selectable<'a> {
    pub fn new(blocks: &'a [Block]) -> Self {
        Self {
            blocks,
            selection: Color::from_rgba(0.42, 0.55, 0.78, 0.34),
            text: None,
            padding: 0.0,
        }
    }

    pub fn selection_color(mut self, color: Color) -> Self {
        self.selection = color;
        self
    }

    pub fn text_color(mut self, color: Color) -> Self {
        self.text = Some(color);
        self
    }

    pub fn padding(mut self, padding: f32) -> Self {
        self.padding = padding;
        self
    }

    /// The text a selection over `range` copies.
    ///
    /// Blocks are joined with a newline, because on screen they are separate
    /// lines separated by whitespace and a paste that ran them together would
    /// not be what was on screen. The partial ends are sliced from the *plain*
    /// text, so the copy carries what the user could read and not the markdown
    /// it was rendered from — selecting a bolded word yields the word, not
    /// `**word**`.
    pub fn selected_text(&self, (low, high): (Anchor, Anchor)) -> String {
        if self.blocks.is_empty() {
            return String::new();
        }
        let last = self.blocks.len() - 1;
        let mut out = String::new();
        for index in low.block.min(last)..=high.block.min(last) {
            let plain = self.blocks[index].plain();
            let from = if index == low.block { low.offset } else { 0 };
            let to = if index == high.block {
                high.offset
            } else {
                plain.len()
            };
            let from = floor_boundary(plain, from);
            let to = floor_boundary(plain, to);
            if from < to {
                out.push_str(&plain[from..to]);
            }
            if index < high.block.min(last) {
                out.push('\n');
            }
        }
        out
    }

    /// The anchor at a point in the widget's own coordinate space.
    ///
    /// The vertical band dispatch the module header explains. A point between
    /// two blocks (in the gap) belongs to the end of the block above it, so a
    /// drag through the whitespace between two rows keeps selecting rather than
    /// stalling at the last glyph it was over.
    fn anchor_at(&self, state: &State, local: Point) -> Option<Anchor> {
        let mut fallback = None;
        for (index, shaped) in state.shaped.iter().enumerate() {
            if local.y >= shaped.top && local.y < shaped.top + shaped.height {
                let inside = Point::new(local.x - self.indent(index), local.y - shaped.top);
                let offset = geometry::offset_at(&shaped.paragraph, &shaped.lines, inside)?;
                return Some(Anchor {
                    block: index,
                    offset,
                });
            }
            if local.y >= shaped.top {
                // `.get`, not `[..]`: `shaped` is built from `blocks` in
                // `layout` and the two cannot disagree, but a panic in here
                // takes the window down, and there is a correct answer for the
                // impossible case.
                fallback = Some(Anchor {
                    block: index,
                    offset: self
                        .blocks
                        .get(index)
                        .map_or(0, |block| block.plain().len()),
                });
            }
        }
        // Above the first block: the very beginning.
        Some(fallback.unwrap_or(Anchor {
            block: 0,
            offset: 0,
        }))
    }

    fn indent(&self, block: usize) -> f32 {
        self.padding + self.blocks.get(block).map_or(0.0, |block| block.indent)
    }

    /// Widen `anchor` to the granularity in force, as the two ends of a range.
    fn expand(&self, anchor: Anchor, granularity: Granularity) -> (Anchor, Anchor) {
        let Some(block) = self.blocks.get(anchor.block) else {
            return (anchor, anchor);
        };
        let (start, end) = match granularity {
            Granularity::Character => return (anchor, anchor),
            Granularity::Word => word_at(block.plain(), anchor.offset),
            Granularity::Line => line_at(block.plain(), anchor.offset),
        };
        (
            Anchor {
                block: anchor.block,
                offset: start,
            },
            Anchor {
                block: anchor.block,
                offset: end,
            },
        )
    }

    /// Where the whole transcript begins and ends, for select-all.
    fn everything(&self) -> Option<(Anchor, Anchor)> {
        let last = self.blocks.len().checked_sub(1)?;
        Some((
            Anchor {
                block: 0,
                offset: 0,
            },
            Anchor {
                block: last,
                offset: self.blocks[last].plain().len(),
            },
        ))
    }

    /// Write the current selection to `kind`, and say whether there was one.
    fn copy(&self, state: &State, clipboard: &mut dyn Clipboard, kind: clipboard::Kind) -> bool {
        let Some(range) = state.range() else {
            return false;
        };
        let text = self.selected_text(range);
        if text.is_empty() {
            return false;
        }
        clipboard.write(kind, text);
        true
    }
}

impl<Message, Theme, Renderer> Widget<Message, Theme, Renderer> for Selectable<'_>
where
    Renderer: text::Renderer<Font = Font, Paragraph = GraphicsParagraph>,
{
    fn tag(&self) -> tree::Tag {
        tree::Tag::of::<State>()
    }

    fn state(&self) -> tree::State {
        tree::State::new(State::default())
    }

    fn size(&self) -> Size<Length> {
        Size::new(Length::Fill, Length::Shrink)
    }

    fn layout(
        &mut self,
        tree: &mut Tree,
        _renderer: &Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        let state = tree.state.downcast_mut::<State>();
        let width = limits.max().width;

        // Last pass's paragraphs become this pass's pool. Keyed by content, so
        // a row spliced into the middle costs one reshape rather than N — see
        // `super::cache`.
        let previous: Vec<(u64, GraphicsParagraph)> = std::mem::take(&mut state.shaped)
            .into_iter()
            .map(|shaped| (shaped.key, shaped.paragraph))
            .collect();
        state.cache.begin(previous);

        let mut top = 0.0;
        for (index, block) in self.blocks.iter().enumerate() {
            let available = (width - self.indent(index)).max(1.0);
            let key = block.fingerprint(available);
            let paragraph = state.cache.take(key, || {
                GraphicsParagraph::with_spans(text::Text {
                    content: &block.spans,
                    bounds: Size::new(available, f32::INFINITY),
                    size: block.size,
                    line_height: text::LineHeight::default(),
                    font: block.font,
                    align_x: text::Alignment::Left,
                    align_y: iced::alignment::Vertical::Top,
                    shaping: text::Shaping::Advanced,
                    wrapping: text::Wrapping::Word,
                })
            });
            let lines = Lines::of(paragraph.buffer());
            let height = geometry::measure(&paragraph).height;
            state.shaped.push(Shaped {
                paragraph,
                lines,
                key,
                top,
                height,
            });
            top += height + block.gap;
        }

        layout::Node::new(Size::new(width, top.max(0.0)))
    }

    fn update(
        &mut self,
        tree: &mut Tree,
        event: &Event,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        _renderer: &Renderer,
        clipboard: &mut dyn Clipboard,
        shell: &mut Shell<'_, Message>,
        _viewport: &Rectangle,
    ) {
        let bounds = layout.bounds();
        let state = tree.state.downcast_mut::<State>();

        match event {
            Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)) => {
                let Some(position) = cursor.position_over(bounds) else {
                    // A click anywhere else dismisses the selection, the way
                    // clicking off a selection does in every other text surface.
                    if state.range().is_some() {
                        state.clear();
                        shell.request_redraw();
                    }
                    return;
                };
                let local = Point::new(position.x - bounds.x, position.y - bounds.y);
                let Some(anchor) = self.anchor_at(state, local) else {
                    return;
                };
                let click = mouse::Click::new(position, mouse::Button::Left, state.last_click);
                state.granularity = match click.kind() {
                    mouse::click::Kind::Single => Granularity::Character,
                    mouse::click::Kind::Double => Granularity::Word,
                    mouse::click::Kind::Triple => Granularity::Line,
                };
                state.last_click = Some(click);
                let (low, high) = self.expand(anchor, state.granularity);
                state.seed = Some((low, high));
                state.start = Some(low);
                state.focus = Some(high);
                state.dragging = true;
                shell.capture_event();
                shell.request_redraw();
            }
            Event::Mouse(mouse::Event::CursorMoved { .. }) if state.dragging => {
                // `cursor`, not the event's own `position`. Inside a
                // `scrollable` the two are different: the scrollable
                // translates the `Cursor` it hands down by the scroll offset
                // and forwards the `Event` unmodified. The press arm above
                // reads `cursor` and was right; this arm read the payload and
                // was short by exactly the offset, so once the transcript had
                // been scrolled a screenful, dragging highlighted text several
                // hundred pixels above the pointer and never the line under
                // it. Double-click and triple-click were unaffected, because
                // they only ever go through the press arm — which is why this
                // survived: the selection looks fine until you drag.
                //
                // `position()` rather than `position_over(bounds)`: a drag is
                // allowed to leave the widget, and `anchor_at` clamps.
                let Some(position) = cursor.position() else {
                    return;
                };
                let local = Point::new(position.x - bounds.x, position.y - bounds.y);
                let Some(anchor) = self.anchor_at(state, local) else {
                    return;
                };
                // Dragging after a double or triple click extends by the same
                // unit, and never shrinks inside the word or line that started
                // it: that is what makes "double click, drag" select whole
                // words in both directions.
                let (low, high) = self.expand(anchor, state.granularity);
                let seed = state.seed.unwrap_or((low, high));
                if high > seed.1 {
                    state.start = Some(seed.0);
                    state.focus = Some(high);
                } else {
                    state.start = Some(seed.1);
                    state.focus = Some(low);
                }
                shell.capture_event();
                shell.request_redraw();
            }
            Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)) if state.dragging => {
                state.dragging = false;
                // X11's primary selection: a selection made with the mouse is
                // pasteable with the middle button, without a copy step. It is
                // what every other text surface on the platform does, and iced
                // models it as a separate clipboard rather than as a mode.
                let _ = self.copy(state, clipboard, clipboard::Kind::Primary);
                shell.capture_event();
            }
            Event::Keyboard(keyboard::Event::KeyPressed { key, modifiers, .. })
                if modifiers.command() =>
            {
                match key.as_ref() {
                    keyboard::Key::Character("c")
                        if self.copy(state, clipboard, clipboard::Kind::Standard) =>
                    {
                        shell.capture_event();
                    }
                    keyboard::Key::Character("a") => {
                        // Only when there is already a selection here. The
                        // composer is the other thing on screen that answers to
                        // Ctrl+A, and stealing it from a focused text field
                        // would make the composer's own select-all unreachable.
                        if state.range().is_some()
                            && let Some((low, high)) = self.everything()
                        {
                            state.start = Some(low);
                            state.focus = Some(high);
                            state.granularity = Granularity::Character;
                            state.seed = Some((low, high));
                            shell.capture_event();
                            shell.request_redraw();
                        }
                    }
                    _ => {}
                }
            }
            Event::Keyboard(keyboard::Event::KeyPressed {
                key: keyboard::Key::Named(keyboard::key::Named::Escape),
                ..
            }) if state.range().is_some() => {
                state.clear();
                shell.capture_event();
                shell.request_redraw();
            }
            _ => {}
        }
    }

    fn mouse_interaction(
        &self,
        _tree: &Tree,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        _viewport: &Rectangle,
        _renderer: &Renderer,
    ) -> mouse::Interaction {
        if cursor.is_over(layout.bounds()) {
            mouse::Interaction::Text
        } else {
            mouse::Interaction::None
        }
    }

    fn draw(
        &self,
        tree: &Tree,
        renderer: &mut Renderer,
        _theme: &Theme,
        style: &renderer::Style,
        layout: Layout<'_>,
        _cursor: mouse::Cursor,
        viewport: &Rectangle,
    ) {
        let state = tree.state.downcast_ref::<State>();
        let bounds = layout.bounds();
        let range = state.range();

        for (index, shaped) in state.shaped.iter().enumerate() {
            let Some(block) = self.blocks.get(index) else {
                continue;
            };
            let origin = Point::new(bounds.x + self.indent(index), bounds.y + shaped.top);
            let body = Rectangle {
                x: origin.x,
                y: origin.y,
                width: (bounds.width - self.indent(index)).max(0.0),
                height: shaped.height,
            };
            // Rows below the viewport are laid out but never rasterized: the
            // transcript is the whole conversation and most of it is off screen.
            if !body.intersects(viewport) {
                continue;
            }

            if let Some(fill) = block.fill {
                renderer.fill_quad(
                    renderer::Quad {
                        bounds: body.expand(4.0),
                        border: iced::Border::default().rounded(6.0),
                        ..Default::default()
                    },
                    fill,
                );
            }
            if let Some(rule) = block.rule {
                renderer.fill_quad(
                    renderer::Quad {
                        bounds: Rectangle {
                            x: origin.x - 10.0,
                            y: origin.y,
                            width: 2.0,
                            height: shaped.height,
                        },
                        ..Default::default()
                    },
                    rule,
                );
            }

            // Under the glyphs, so a translucent wash tints the text rather
            // than covering it.
            if let Some((low, high)) = range
                && index >= low.block
                && index <= high.block
            {
                let plain = block.plain();
                let from = if index == low.block { low.offset } else { 0 };
                let to = if index == high.block {
                    high.offset
                } else {
                    // Past the last byte, so the trailing line ending is part of
                    // the selection and the highlight runs to the line's end.
                    plain.len() + 1
                };
                for rect in geometry::highlight(&shaped.paragraph, &shaped.lines, from, to) {
                    renderer.fill_quad(
                        renderer::Quad {
                            bounds: Rectangle {
                                x: origin.x + rect.x,
                                y: origin.y + rect.y,
                                ..rect
                            },
                            ..Default::default()
                        },
                        self.selection,
                    );
                }
            }

            renderer.fill_paragraph(
                &shaped.paragraph,
                origin,
                self.text.unwrap_or(style.text_color),
                *viewport,
            );
        }
    }
}

impl<'a, Message, Theme, Renderer> From<Selectable<'a>> for Element<'a, Message, Theme, Renderer>
where
    Message: 'a,
    Theme: 'a,
    Renderer: text::Renderer<Font = Font, Paragraph = GraphicsParagraph> + 'a,
{
    fn from(selectable: Selectable<'a>) -> Self {
        Element::new(selectable)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn block(text: &str) -> Block {
        Block::plain_text(text.to_string(), 14.0, Font::MONOSPACE, Color::WHITE)
    }

    /// The copy rule, exercised on the range shape that matters: partial at
    /// both ends, whole blocks in between, joined by newlines.
    #[test]
    fn a_copy_slices_the_ends_and_takes_the_middle_whole() {
        let blocks = [
            block("prose here"),
            block("fn main() {}"),
            block("tool row"),
        ];
        let widget = Selectable::new(&blocks);
        let text = widget.selected_text((
            Anchor {
                block: 0,
                offset: 6,
            },
            Anchor {
                block: 2,
                offset: 4,
            },
        ));
        assert_eq!(text, "here\nfn main() {}\ntool");
    }

    /// A range inside one block copies exactly that, with no stray newline.
    #[test]
    fn a_single_block_copy_carries_no_separator() {
        let blocks = [block("alpha beta gamma")];
        let widget = Selectable::new(&blocks);
        let text = widget.selected_text((
            Anchor {
                block: 0,
                offset: 6,
            },
            Anchor {
                block: 0,
                offset: 10,
            },
        ));
        assert_eq!(text, "beta");
    }

    /// A drag upward is the same selection as the drag downward it reverses.
    /// The state keeps its ends unsorted so the gesture can cross its own
    /// origin; `range` is what makes that invisible to everything else.
    #[test]
    fn a_range_is_the_same_in_either_direction() {
        let low = Anchor {
            block: 0,
            offset: 2,
        };
        let high = Anchor {
            block: 3,
            offset: 1,
        };
        let mut state = State {
            start: Some(low),
            focus: Some(high),
            ..State::default()
        };
        let forward = state.range();
        state.start = Some(high);
        state.focus = Some(low);
        assert_eq!(forward, state.range());
        assert_eq!(forward, Some((low, high)));
    }

    /// A click with no drag is not a selection. Reporting one would put a
    /// zero-width wash on screen and make Ctrl+C copy an empty string over
    /// whatever the user had on their clipboard.
    #[test]
    fn a_click_without_a_drag_selects_nothing() {
        let at = Anchor {
            block: 1,
            offset: 5,
        };
        let state = State {
            start: Some(at),
            focus: Some(at),
            ..State::default()
        };
        assert_eq!(state.range(), None);
    }

    /// Copying out of an offset that is not a char boundary must not panic —
    /// a panic in here takes the window down. Offsets arrive from cosmic-text,
    /// which indexes graphemes.
    #[test]
    fn a_copy_survives_offsets_inside_a_multibyte_character() {
        let blocks = [block("héllo wörld")];
        let widget = Selectable::new(&blocks);
        for offset in 0..=blocks[0].plain().len() + 4 {
            let text = widget.selected_text((
                Anchor {
                    block: 0,
                    offset: 0,
                },
                Anchor { block: 0, offset },
            ));
            assert!(blocks[0].plain().starts_with(&text), "{text:?}");
        }
    }

    /// Select-all spans the whole conversation, and an empty one has nothing
    /// to span.
    #[test]
    fn select_all_covers_every_block() {
        let blocks = [block("one"), block("two"), block("three")];
        let widget = Selectable::new(&blocks);
        let range = widget.everything().expect("three blocks");
        assert_eq!(widget.selected_text(range), "one\ntwo\nthree");

        let empty: [Block; 0] = [];
        assert_eq!(Selectable::new(&empty).everything(), None);
    }

    /// Granularity widens an anchor into a range. Character granularity must
    /// widen it into nothing at all, or a single click would select a word.
    #[test]
    fn granularity_widens_an_anchor() {
        let blocks = [block("alpha beta\ngamma delta")];
        let widget = Selectable::new(&blocks);
        let at = Anchor {
            block: 0,
            offset: 7,
        };
        assert_eq!(widget.expand(at, Granularity::Character), (at, at));

        let (low, high) = widget.expand(at, Granularity::Word);
        assert_eq!(&blocks[0].plain()[low.offset..high.offset], "beta");

        let (low, high) = widget.expand(at, Granularity::Line);
        assert_eq!(&blocks[0].plain()[low.offset..high.offset], "alpha beta");
    }
}
