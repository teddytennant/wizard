//! Pieces every modal shares: where its rows sit, the search row, the wrapped footer hints, the
//! scrollbar (spec 6.2.3 to 6.2.5).

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use tuikit::width::display_width;

use crate::app::App;
use crate::ui::{bold, put, st};

/// A key and what it does, as the footer prints them.
pub type Hint<'a> = (&'a str, &'a str);

/// Rows of a titled window with a search row: the list starts under the divider and ends above
/// the blank row that sits over the footer (and over the tip, when there is one).
#[derive(Clone, Copy, Debug)]
pub struct Body {
    pub search_y: u16,
    pub div_y: u16,
    pub list_y: u16,
    pub list_h: u16,
    pub tip_y: Option<u16>,
    /// First footer row.
    pub foot_y: u16,
}

/// `foot` footer rows, `tip` whether a tip row sits above them. The footer is a block of at
/// least two rows: a blank one over a single row of hints, no blank once the hints wrap.
pub fn body(r: Rect, foot: u16, tip: bool) -> Body {
    let last = r.bottom().saturating_sub(2);
    let foot = foot.max(1);
    let foot_y = (last + 1).saturating_sub(foot);
    let block_top = (last + 1).saturating_sub(foot.max(2));
    // the tip sits two rows above the first row of hints
    let tip_y = tip.then(|| foot_y.saturating_sub(2));
    let list_y = r.y + 4;
    let end = tip_y.map_or(block_top, |t| t.saturating_sub(1));
    Body {
        search_y: r.y + 2,
        div_y: r.y + 3,
        list_y,
        list_h: end.saturating_sub(list_y),
        tip_y,
        foot_y,
    }
}

/// Hints split into the rows they wrap onto. A row holds as many as fit in four cells less than
/// the inside of the window.
pub fn wrap_hints<'a>(hints: &[Hint<'a>], inner_w: u16) -> Vec<Vec<Hint<'a>>> {
    let max = inner_w.saturating_sub(4) as usize;
    let mut rows: Vec<Vec<Hint<'a>>> = vec![Vec::new()];
    let mut w = 0usize;
    for &(k, l) in hints {
        let hw = display_width(k) + 1 + display_width(l);
        let row = rows.last_mut().unwrap();
        if !row.is_empty() && w + 5 + hw > max {
            rows.push(vec![(k, l)]);
            w = hw;
        } else {
            w += if row.is_empty() { 0 } else { 5 } + hw;
            row.push((k, l));
        }
    }
    rows
}

/// Footer rows the hints need in window `r`.
pub fn foot_rows(r: Rect, hints: &[Hint]) -> u16 {
    wrap_hints(hints, r.width.saturating_sub(2)).len() as u16
}

/// Draw the hints centered, wrapped, ending on the row above the bottom border.
pub fn draw_hints(buf: &mut Buffer, app: &App, r: Rect, hints: &[Hint]) {
    let th = &app.theme;
    let inner_w = r.width.saturating_sub(2);
    let rows = wrap_hints(hints, inner_w);
    let top = (r.bottom().saturating_sub(1)).saturating_sub(rows.len() as u16);
    for (i, row) in rows.iter().enumerate() {
        let total: usize = row
            .iter()
            .map(|(k, l)| display_width(k) + 1 + display_width(l))
            .sum::<usize>()
            + row.len().saturating_sub(1) * 5;
        let mut x = r.x + 1 + (inner_w as usize).saturating_sub(total) as u16 / 2;
        let y = top + i as u16;
        for (j, (k, l)) in row.iter().enumerate() {
            if j > 0 {
                x = put(buf, x, y, "  |  ", st(th.gray_dim));
            }
            x = put(buf, x, y, k, bold(th.text_secondary));
            x = put(buf, x, y, &format!(" {l}"), st(th.gray));
        }
    }
}

/// ` / to search` when idle, ` search: query█` while typing.
pub fn draw_search(buf: &mut Buffer, app: &App, r: Rect, y: u16, query: &str, active: bool) {
    let th = &app.theme;
    let x = r.x + 3;
    if active {
        let nx = put(buf, x, y, " search: ", st(th.gray));
        let q = put(buf, nx, y, query, st(th.text_primary));
        put(
            buf,
            q,
            y,
            " ",
            Style::new().fg(th.bg_base).bg(th.text_primary),
        );
    } else {
        put(buf, x, y, " / to search", st(th.gray_dim));
    }
}

/// A full-width divider under the search row, `inset` cells short of each border.
pub fn draw_divider(buf: &mut Buffer, app: &App, r: Rect, y: u16, inset: u16) {
    let w = (r.width as usize).saturating_sub(2 + 2 * inset as usize);
    if inset == 0 {
        put(
            buf,
            r.x,
            y,
            &format!("│{}│", "─".repeat(w)),
            st(app.theme.gray_dim),
        );
    } else {
        put(
            buf,
            r.x + 1 + inset,
            y,
            &"─".repeat(w),
            st(app.theme.gray_dim),
        );
    }
}

/// The window's scrollbar: a thumb in the last inner column over `list_h` rows.
pub fn draw_scrollbar(
    buf: &mut Buffer,
    app: &App,
    r: Rect,
    y0: u16,
    h: u16,
    total: usize,
    top: usize,
) {
    if let Some((pos, size)) = crate::ui::thumb(total, h as usize, top, h as usize) {
        for i in pos..(pos + size).min(h as usize) {
            put(
                buf,
                r.right() - 2,
                y0 + i as u16,
                "█",
                st(app.theme.gray_dim).bg(app.theme.gray_dim),
            );
        }
    }
}

/// A one-row band from `x0` to `x1` (exclusive).
pub fn band(buf: &mut Buffer, app: &App, x0: u16, x1: u16, y: u16) {
    crate::ui::fill(
        buf,
        Rect::new(x0, y, x1.saturating_sub(x0), 1),
        app.theme.bg_visual,
    );
}

pub fn bolded(sel: bool) -> Modifier {
    if sel {
        Modifier::BOLD
    } else {
        Modifier::empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cheatsheet_footer_wraps_like_the_capture() {
        let h: Vec<Hint> = vec![
            ("↑/↓", "nav"),
            ("f", "filter"),
            ("e/Space/→", "expand"),
            ("←", "collapse"),
            ("Enter", "details"),
            ("/", "search"),
            ("Esc", "close"),
        ];
        // 80 wide: five hints on the first row, two on the second
        let rows = wrap_hints(&h, 78);
        assert_eq!(rows.iter().map(Vec::len).collect::<Vec<_>>(), vec![5, 2]);
        // 56 wide: three rows
        let rows = wrap_hints(&h, 54);
        assert_eq!(rows.iter().map(Vec::len).collect::<Vec<_>>(), vec![3, 3, 1]);
    }
}
