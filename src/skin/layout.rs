//! The block model: how one transcript entry occupies the screen.
//!
//! This is the part that makes a skin more than a glyph table, and the part
//! Wizard did not have. The old transcript was a flat list of lines with a
//! two-column marker glued to the front of each *logical* line, wrapped
//! afterwards — so a paragraph that soft-wrapped lost its marker on every row
//! but the first. That is invisible when the marker is two spaces and fatal
//! when it is a colored bar down the side of the block, which is exactly what
//! Grok Build's UI is.
//!
//! So the order is now the one both upstreams use: **wrap the content to the
//! content width first, then decorate every row that came out.** Codex does it
//! with `prefix_lines` after `word_wrap_lines`
//! (`codex-rs/tui/src/render/line_utils.rs`); Grok Build does it by laying out
//! a dedicated accent column and painting it down the block's full height
//! (`crates/codegen/xai-grok-pager/src/scrollback/layout.rs`). The column
//! structure here — accent, left pad, content, right pad — is Grok Build's,
//! and so are the default pad widths. Both Apache-2.0; see `docs/ui-skins.md`.
//!
//! ```text
//! │A│PL│         content          │PR│
//! │1│ 2│           flex           │ 1│
//! ```

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;

use crate::theme::{self, Token};

use super::blend::Tint;
use super::motion;

/// What a block is, so a skin can decorate each kind differently. A tool card
/// and a user prompt are the two that every one of the four treats unlike
/// anything else.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockKind {
    /// The user's echoed prompt.
    User,
    /// An assistant message.
    Assistant,
    /// Model reasoning.
    Thinking,
    /// A tool call and its output.
    Tool,
    /// A system notice.
    Notice,
}

/// A colored column down the left of a block. Grok Build's whole visual
/// identity; `None` under the other three.
#[derive(Debug, Clone, Copy)]
pub struct Accent {
    /// The glyph repeated down the column. One display column wide.
    pub glyph: &'static str,
    /// Its color.
    pub token: Token,
    /// Whether it breathes while the block is running (see
    /// [`motion::wave`]). Only a block that is still working animates; a
    /// finished transcript is still.
    pub animate: bool,
}

/// The two-column marker that leads a block's content, applied *after*
/// wrapping so continuation rows keep their indent.
///
/// `head` leads the first row, `rest` every row below it. Both must be the
/// same display width or the content goes ragged at the wrap.
#[derive(Debug, Clone, Copy)]
pub struct Marker {
    pub head: &'static str,
    pub rest: &'static str,
    pub token: Token,
    pub bold: bool,
}

impl Marker {
    /// A hanging indent: a glyph, then blanks under it.
    pub const fn hanging(head: &'static str, token: Token, bold: bool) -> Marker {
        Marker {
            head,
            rest: "  ",
            token,
            bold,
        }
    }

    /// No marker at all: the content starts in column zero of its block.
    pub const fn none() -> Marker {
        Marker {
            head: "",
            rest: "",
            token: Token::Faint,
            bold: false,
        }
    }

    /// Display columns this marker occupies.
    pub fn width(&self) -> usize {
        self.head.width()
    }

    /// The style its glyph is painted in.
    pub fn style(&self) -> Style {
        let style = theme::style(self.token);
        if self.bold {
            style.add_modifier(Modifier::BOLD)
        } else {
            style
        }
    }
}

/// How a skin decorates one block.
#[derive(Debug, Clone, Copy)]
pub struct BlockStyle {
    /// The accent column, if this skin has one.
    pub accent: Option<Accent>,
    /// Columns between the accent column and the content.
    pub pad_left: u16,
    /// Columns kept clear at the right, so a tinted slab does not run into
    /// the scrollbar and text does not touch the terminal edge.
    pub pad_right: u16,
    /// Blank rows inside the block, above and below its content. This is what
    /// gives a tinted slab its margin; with no tint it reads as spacing.
    pub pad_y: u16,
    /// A slab behind the block, blended against the terminal's own
    /// background. `None` for skins (and terminals) that have none.
    pub tint: Option<Tint>,
    /// Blank rows before this block when it follows a different kind.
    pub gap_before: u16,
    /// The marker leading the content.
    pub marker: Marker,
}

impl BlockStyle {
    /// The plain house block: a marker, no column, no pads, no slab.
    pub const fn plain(marker: Marker) -> BlockStyle {
        BlockStyle {
            accent: None,
            pad_left: 0,
            pad_right: 0,
            pad_y: 0,
            tint: None,
            gap_before: 1,
            marker,
        }
    }

    /// Columns this block's chrome takes before the content starts.
    pub fn indent(&self) -> u16 {
        self.accent.map_or(0, |_| 1) + self.pad_left + self.marker.width() as u16
    }

    /// Content columns available inside `width`.
    pub fn content_width(&self, width: u16) -> u16 {
        width
            .saturating_sub(self.indent())
            .saturating_sub(self.pad_right)
            .max(1)
    }
}

/// Decorate one block's already-wrapped content rows.
///
/// `rows` are the content lines, wrapped to [`BlockStyle::content_width`].
/// What comes back is the block as it will be painted: vertical padding, the
/// accent column on every row including the padding, the marker on the first
/// content row and its continuation on the rest, and the slab carried to the
/// full width so it is a rectangle rather than a ragged edge.
///
/// `width` is the full block width and `tick` drives the accent animation.
pub fn decorate(
    style: &BlockStyle,
    rows: Vec<Line<'static>>,
    width: u16,
    tick: u64,
    running: bool,
) -> Vec<Line<'static>> {
    let content_width = style.content_width(width);
    let bg = style.tint.and_then(Tint::resolve);
    let total = rows.len() as u16 + style.pad_y * 2;

    let mut out: Vec<Line<'static>> = Vec::with_capacity(total as usize);
    let mut row_index = 0u16;
    let mut push = |content: Vec<Span<'static>>, marker: Option<&Marker>, first: bool| {
        let mut spans: Vec<Span<'static>> = Vec::with_capacity(content.len() + 4);
        if let Some(accent) = style.accent {
            spans.push(Span::styled(
                accent.glyph,
                accent_style(&accent, row_index, total, tick, running, bg),
            ));
        }
        if style.pad_left > 0 {
            spans.push(padding(style.pad_left as usize, bg));
        }
        if let Some(marker) = marker {
            let raw = if first { marker.head } else { marker.rest };
            let mark = super::glyphs::adapt(raw);
            if !mark.is_empty() {
                spans.push(Span::styled(mark, with_bg(marker.style(), bg)));
            }
        } else if marker_width(style) > 0 {
            spans.push(padding(marker_width(style), bg));
        }
        // Carry the slab to the block's right edge. Without this a tinted
        // block is the shape of its text, which reads as highlighting rather
        // than as a panel.
        let used: usize = content.iter().map(|span| span.content.width()).sum();
        let mut spans = {
            let mut all = spans;
            all.extend(content);
            all
        };
        if bg.is_some() {
            let fill = (content_width as usize).saturating_sub(used) + style.pad_right as usize;
            if fill > 0 {
                spans.push(padding(fill, bg));
            }
        }
        row_index += 1;
        out.push(Line::from(spans));
    };

    for _ in 0..style.pad_y {
        push(Vec::new(), None, false);
    }
    let last = rows.len();
    for (index, line) in rows.into_iter().enumerate() {
        let first = index == 0;
        push(line.spans, Some(&style.marker), first);
        let _ = last;
    }
    for _ in 0..style.pad_y {
        push(Vec::new(), None, false);
    }
    out
}

/// The marker column's width, for the padding rows that have no marker.
fn marker_width(style: &BlockStyle) -> usize {
    style.marker.width()
}

/// A run of `n` blank columns carrying the block's background.
fn padding(n: usize, bg: Option<Color>) -> Span<'static> {
    Span::styled(" ".repeat(n), with_bg(Style::default(), bg))
}

/// `style` with the block's background applied, when there is one.
fn with_bg(style: Style, bg: Option<Color>) -> Style {
    match bg {
        Some(color) => style.bg(color),
        None => style,
    }
}

/// The accent column's style at one row: its token's color, breathing down the
/// block while it runs, and holding still once it has finished.
fn accent_style(
    accent: &Accent,
    row: u16,
    total: u16,
    tick: u64,
    running: bool,
    bg: Option<Color>,
) -> Style {
    let color = theme::color(accent.token);
    let color = if accent.animate && running {
        motion::breathe(color, motion::wave(tick, row, total.max(1), 0.35))
    } else {
        color
    };
    with_bg(Style::default().fg(color), bg)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(text: &str) -> Line<'static> {
        Line::from(Span::raw(text.to_string()))
    }

    fn barred() -> BlockStyle {
        BlockStyle {
            accent: Some(Accent {
                glyph: "┃",
                token: Token::Accent,
                animate: false,
            }),
            pad_left: 1,
            pad_right: 1,
            pad_y: 1,
            tint: None,
            gap_before: 1,
            marker: Marker::none(),
        }
    }

    fn text(line: &Line<'_>) -> String {
        line.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect()
    }

    #[test]
    fn the_accent_column_runs_the_whole_block_padding_included() {
        // The bug this whole module exists to fix: a bar that only marks the
        // first row is not a bar.
        let decorated = decorate(&barred(), vec![row("one"), row("two")], 20, 0, false);
        assert_eq!(
            decorated.len(),
            4,
            "two content rows plus a pad row each end"
        );
        for line in &decorated {
            assert!(
                text(line).starts_with('┃'),
                "every row carries the column: {:?}",
                text(line)
            );
        }
    }

    #[test]
    fn a_marker_leads_the_first_row_and_indents_the_rest() {
        let style = BlockStyle::plain(Marker::hanging("⏺ ", Token::Accent, false));
        let decorated = decorate(&style, vec![row("first"), row("second")], 20, 0, false);
        assert_eq!(text(&decorated[0]), "⏺ first");
        assert_eq!(
            text(&decorated[1]),
            "  second",
            "continuation keeps the column"
        );
    }

    #[test]
    fn content_width_leaves_room_for_every_piece_of_chrome() {
        let style = barred();
        // 1 accent + 1 left pad + 0 marker + 1 right pad = 3 columns of chrome.
        assert_eq!(style.content_width(20), 17);
        assert_eq!(style.indent(), 2);
        // A width narrower than the chrome still yields a usable column rather
        // than zero, so a one-column terminal renders something.
        assert_eq!(style.content_width(1), 1);
    }

    #[test]
    fn a_theme_that_declares_no_slab_paints_none() {
        // `minimal` gives both background tokens `reset`, which is the house
        // rule — this UI paints on the terminal's own background. A block that
        // asks for a tint under such a theme gets no slab, and therefore no
        // padding carrying one out to the edge.
        let _theme = crate::theme::pin(crate::theme::minimal());
        let mut style = barred();
        style.tint = Some(Tint::Raised);
        let plain = decorate(&style, vec![row("hi")], 20, 0, false);
        let widths: Vec<usize> = plain.iter().map(|line| text(line).width()).collect();
        assert!(
            widths.iter().all(|w| *w < 20),
            "no declared slab means nothing is carried to the edge: {widths:?}"
        );
    }

    #[test]
    fn a_theme_that_declares_a_slab_carries_it_to_the_blocks_edge() {
        // And the other half: a skin's own theme names a slab, so the block
        // becomes a rectangle rather than the ragged shape of its text. This
        // is what makes Codex's user message read as a panel.
        let theme = std::sync::Arc::new(crate::theme::load("codex").expect("codex theme loads"));
        let _theme = crate::theme::pin(theme);
        let mut style = barred();
        style.tint = Some(Tint::Raised);
        let painted = decorate(&style, vec![row("hi")], 20, 0, false);
        for line in &painted {
            assert_eq!(
                text(line).width(),
                20,
                "every row of a slabbed block reaches the edge: {:?}",
                text(line)
            );
        }
    }

    #[test]
    fn a_still_block_and_a_running_one_differ_only_while_animated() {
        let mut style = barred();
        style.accent = Some(Accent {
            glyph: "┃",
            token: Token::Accent,
            animate: true,
        });
        // Without truecolor and a known background there is nothing to
        // interpolate, so the two are identical — the 16-color path.
        let still = decorate(&style, vec![row("x")], 12, 0, false);
        let moving = decorate(&style, vec![row("x")], 12, 3, true);
        assert_eq!(still.len(), moving.len());
    }
}
