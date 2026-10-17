//! Bounds-checked drawing primitives. `Buffer` indexing panics outside its area, and the
//! toolkit has to survive zero-size and off-screen rects, so every widget draws through here.

use crate::width::{grapheme_width, TAB_WIDTH};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::Line;
use unicode_segmentation::UnicodeSegmentation;

/// Draw `s` at (`x`,`y`) clipped to `clip`. Returns the x after the last drawn cell.
/// Control characters are skipped; a wide grapheme that does not fit is dropped.
pub fn put_str(buf: &mut Buffer, x: u16, y: u16, s: &str, style: Style, clip: Rect) -> u16 {
    let clip = clip.intersection(buf.area);
    if clip.is_empty() || y < clip.top() || y >= clip.bottom() {
        return x;
    }
    let mut cx = x as u32;
    let right = clip.right() as u32;
    for g in s.graphemes(true) {
        // A tab is `TAB_WIDTH` blanks, the width `display_width` counts, so a row that holds
        // one still ends where the layout thinks it does.
        let w = if g == "\t" {
            TAB_WIDTH
        } else {
            grapheme_width(g)
        } as u32;
        if w == 0 {
            continue;
        }
        let draw = if g == "\t" { " " } else { g };
        if cx + w > right {
            break;
        }
        if cx < clip.left() as u32 {
            cx += w;
            continue;
        }
        if let Some(cell) = buf.cell_mut((cx as u16, y)) {
            cell.set_symbol(draw).set_style(style);
            // ratatui advances by the symbol's own width. Where this toolkit counts the cell
            // differently (per-code-point widths on a terminal without mode 2027), say how many
            // columns it takes, or the next cells are written at the wrong place.
            if unicode_width::UnicodeWidthStr::width(draw) != w as usize {
                if let Some(nz) = u16::try_from(w).ok().and_then(std::num::NonZeroU16::new) {
                    cell.diff_option = ratatui::buffer::CellDiffOption::ForcedWidth(nz);
                }
            }
        }
        // The cells a wide grapheme covers must not keep stale content.
        for k in 1..w {
            if let Some(cell) = buf.cell_mut(((cx + k) as u16, y)) {
                cell.reset();
                cell.set_style(style);
            }
        }
        cx += w;
    }
    cx.min(u16::MAX as u32) as u16
}

/// Draw a styled line. The line's own style is the base for each span.
pub fn put_line(buf: &mut Buffer, x: u16, y: u16, line: &Line<'_>, clip: Rect) -> u16 {
    let mut cx = x;
    for sp in &line.spans {
        cx = put_str(buf, cx, y, &sp.content, line.style.patch(sp.style), clip);
    }
    cx
}

/// Apply `style` to every cell of `area` (clipped to the buffer) without touching symbols.
pub fn set_style(buf: &mut Buffer, area: Rect, style: Style) {
    let area = area.intersection(buf.area);
    for y in area.top()..area.bottom() {
        for x in area.left()..area.right() {
            if let Some(c) = buf.cell_mut((x, y)) {
                c.set_style(style);
            }
        }
    }
}

/// Blank `area` with spaces in `style`.
pub fn fill(buf: &mut Buffer, area: Rect, style: Style) {
    let area = area.intersection(buf.area);
    for y in area.top()..area.bottom() {
        for x in area.left()..area.right() {
            if let Some(c) = buf.cell_mut((x, y)) {
                c.reset();
                c.set_style(style);
            }
        }
    }
}

/// Fill only the background colour, keeping symbols and foregrounds.
pub fn fill_bg(buf: &mut Buffer, area: Rect, bg: Color) {
    set_style(buf, area, Style::new().bg(bg));
}

/// Shrink a rect by padding; never underflows.
pub fn pad(area: Rect, left: u16, top: u16, right: u16, bottom: u16) -> Rect {
    let w = area.width.saturating_sub(left.saturating_add(right));
    let h = area.height.saturating_sub(top.saturating_add(bottom));
    Rect {
        x: area.x.saturating_add(left.min(area.width)),
        y: area.y.saturating_add(top.min(area.height)),
        width: w,
        height: h,
    }
}

/// Rect of `w` x `h` placed in `outer`, clamped to fit.
pub fn clamp_rect(outer: Rect, x: u16, y: u16, w: u16, h: u16) -> Rect {
    let x = x.max(outer.x).min(outer.right());
    let y = y.max(outer.y).min(outer.bottom());
    Rect {
        x,
        y,
        width: w.min(outer.right().saturating_sub(x)),
        height: h.min(outer.bottom().saturating_sub(y)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(buf: &Buffer, y: u16) -> String {
        (0..buf.area.width)
            .map(|x| buf[(x, y)].symbol().to_string())
            .collect()
    }

    #[test]
    fn put_str_clips_wide_chars_and_survives_empty_rects() {
        let mut b = Buffer::empty(Rect::new(0, 0, 6, 2));
        let a = b.area;
        let end = put_str(&mut b, 0, 0, "ab日本語", Style::default(), a);
        assert_eq!(end, 6);
        // the cell a wide grapheme covers holds a blank placeholder
        assert_eq!(row(&b, 0), "ab日 本 ");
        // zero rect and out-of-range rows are no-ops
        put_str(&mut b, 0, 5, "x", Style::default(), a);
        put_str(&mut b, 0, 0, "x", Style::default(), Rect::new(0, 0, 0, 0));
        let empty = Buffer::empty(Rect::new(0, 0, 0, 0));
        let mut empty = empty;
        let a = empty.area;
        put_str(&mut empty, 0, 0, "x", Style::default(), a);
        fill(&mut empty, a, Style::default());
    }

    #[test]
    fn put_str_skips_controls_and_clips_left() {
        let mut b = Buffer::empty(Rect::new(0, 0, 8, 1));
        let a = b.area;
        put_str(&mut b, 0, 0, "a\x1b[b\rc", Style::default(), a);
        assert_eq!(row(&b, 0).trim_end(), "a[bc");
        let mut b = Buffer::empty(Rect::new(0, 0, 8, 1));
        put_str(
            &mut b,
            0,
            0,
            "abcdef",
            Style::default(),
            Rect::new(2, 0, 3, 1),
        );
        assert_eq!(row(&b, 0).trim(), "cde");
    }

    #[test]
    fn pad_never_underflows() {
        assert_eq!(
            pad(Rect::new(1, 1, 3, 3), 2, 2, 2, 2),
            Rect::new(3, 3, 0, 0)
        );
        assert_eq!(
            pad(Rect::new(0, 0, 10, 10), 1, 2, 3, 4),
            Rect::new(1, 2, 6, 4)
        );
    }

    #[test]
    fn clamp_rect_stays_inside() {
        let o = Rect::new(0, 0, 10, 5);
        assert_eq!(clamp_rect(o, 8, 4, 10, 10), Rect::new(8, 4, 2, 1));
        assert_eq!(clamp_rect(o, 50, 50, 3, 3), Rect::new(10, 5, 0, 0));
    }

    #[test]
    fn a_tab_advances_by_the_width_the_layout_counts() {
        let mut buf = Buffer::empty(Rect::new(0, 0, 20, 1));
        let area = buf.area;
        let end = put_str(&mut buf, 0, 0, "a\tb", Style::default(), area);
        assert_eq!(end as usize, crate::width::display_width("a\tb"));
        assert_eq!(buf[(TAB_WIDTH as u16 + 1, 0)].symbol(), "b");
    }
}
