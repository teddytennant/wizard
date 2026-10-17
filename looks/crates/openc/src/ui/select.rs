// OWNER: transcript
//! Mouse selection over transcript rows. Positions are `(block, row in block, display column)`,
//! not screen cells, so a selection survives scrolling during a drag. A gesture covers a range
//! (a cell, a word or a whole block) and the selection is the union of the range where the
//! button went down and the range under the pointer now, which is how word and block drags
//! grow in whole words and blocks.

use tuikit::width::grapheme_width;
use unicode_segmentation::UnicodeSegmentation;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Pos {
    pub block: usize,
    pub row: usize,
    pub col: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Gran {
    Char,
    Word,
    Block,
}

/// `[start, end)`; the end column is exclusive.
pub type Range = (Pos, Pos);

#[derive(Clone, Debug)]
pub struct Sel {
    pub gran: Gran,
    pub anchor: Range,
    pub head: Range,
    /// The pointer moved while the button was down.
    pub dragged: bool,
}

impl Sel {
    pub fn bounds(&self) -> Range {
        (
            self.anchor.0.min(self.head.0),
            self.anchor.1.max(self.head.1),
        )
    }

    /// Something to paint or copy: a drag, or a double or triple click.
    pub fn active(&self) -> bool {
        self.dragged || self.gran != Gran::Char
    }

    /// Display columns `[c0, c1)` of row `(block, row)` that are selected, given the row's text
    /// width, or `None` when the row is outside the selection.
    pub fn cols(&self, block: usize, row: usize, text_w: usize) -> Option<(usize, usize)> {
        if !self.active() {
            return None;
        }
        let (s, e) = self.bounds();
        let here = (block, row);
        if here < (s.block, s.row) || here > (e.block, e.row) {
            return None;
        }
        let c0 = if here == (s.block, s.row) { s.col } else { 0 };
        let c1 = if here == (e.block, e.row) {
            e.col.min(text_w)
        } else {
            text_w
        };
        (c0 < c1).then_some((c0, c1))
    }
}

/// The word under display column `col` of `text`: `[start, end)` in display columns. Runs of
/// letters, digits and `_` are words; a run of anything else is a unit of its own, and blanks
/// select as one run.
pub fn word_at(text: &str, col: usize) -> (usize, usize) {
    let mut cells: Vec<(usize, usize, u8)> = Vec::new();
    let mut x = 0;
    for g in text.graphemes(true) {
        let w = grapheme_width(g);
        if w == 0 {
            continue;
        }
        let first = g.chars().next().unwrap_or(' ');
        let class = if first.is_alphanumeric() || first == '_' {
            0
        } else if first.is_whitespace() {
            1
        } else {
            2
        };
        cells.push((x, x + w, class));
        x += w;
    }
    let Some(i) = cells.iter().position(|(a, b, _)| *a <= col && col < *b) else {
        return (col, col + 1);
    };
    let class = cells[i].2;
    let mut lo = i;
    while lo > 0 && cells[lo - 1].2 == class {
        lo -= 1;
    }
    let mut hi = i;
    while hi + 1 < cells.len() && cells[hi + 1].2 == class {
        hi += 1;
    }
    (cells[lo].0, cells[hi].1)
}

/// Slice of `text` covering display columns `[c0, c1)`. A wide glyph that straddles an edge is
/// included whole.
pub fn slice_cols(text: &str, c0: usize, c1: usize) -> String {
    let mut out = String::new();
    let mut x = 0;
    for g in text.graphemes(true) {
        let w = grapheme_width(g);
        if x + w > c0 && x < c1 {
            out.push_str(g);
        }
        x += w;
        if x >= c1 {
            break;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(block: usize, row: usize, col: usize) -> Pos {
        Pos { block, row, col }
    }

    #[test]
    fn word_bounds() {
        let t = "  let name = foo_bar(1);";
        assert_eq!(word_at(t, 3), (2, 5));
        assert_eq!(word_at(t, 14), (13, 20));
        assert_eq!(word_at(t, 1), (0, 2));
        assert_eq!(word_at(t, 20), (20, 21));
        // Past the end selects the cell.
        assert_eq!(word_at(t, 40), (40, 41));
        // Wide glyphs are one unit.
        assert_eq!(word_at("a 日本語 b", 3), (2, 8));
    }

    #[test]
    fn slices_by_display_column() {
        assert_eq!(slice_cols("abcdef", 2, 4), "cd");
        assert_eq!(slice_cols("日本語", 2, 4), "本");
        assert_eq!(slice_cols("日本語", 1, 3), "日本");
        assert_eq!(slice_cols("ab", 5, 9), "");
    }

    #[test]
    fn union_of_gesture_ranges_in_either_direction() {
        let mut s = Sel {
            gran: Gran::Char,
            anchor: (p(1, 2, 5), p(1, 2, 6)),
            head: (p(1, 4, 3), p(1, 4, 4)),
            dragged: true,
        };
        assert_eq!(s.bounds(), (p(1, 2, 5), p(1, 4, 4)));
        // Dragging backwards gives the same kind of range.
        s.head = (p(1, 0, 1), p(1, 0, 2));
        assert_eq!(s.bounds(), (p(1, 0, 1), p(1, 2, 6)));
    }

    #[test]
    fn row_columns_follow_line_granularity_at_the_edges() {
        let s = Sel {
            gran: Gran::Char,
            anchor: (p(1, 2, 5), p(1, 2, 6)),
            head: (p(1, 4, 3), p(1, 4, 4)),
            dragged: true,
        };
        assert_eq!(s.cols(1, 1, 30), None);
        assert_eq!(s.cols(1, 2, 30), Some((5, 30)));
        assert_eq!(s.cols(1, 3, 30), Some((0, 30)));
        assert_eq!(s.cols(1, 4, 30), Some((0, 4)));
        assert_eq!(s.cols(1, 5, 30), None);
        // A blank row inside the selection has nothing to paint.
        assert_eq!(s.cols(1, 3, 0), None);
    }

    #[test]
    fn a_plain_click_selects_nothing() {
        let s = Sel {
            gran: Gran::Char,
            anchor: (p(1, 2, 5), p(1, 2, 6)),
            head: (p(1, 2, 5), p(1, 2, 6)),
            dragged: false,
        };
        assert!(!s.active());
        assert_eq!(s.cols(1, 2, 30), None);
    }
}
