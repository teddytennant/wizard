//! A scrollable markdown page inside a modal: how-to guides, tutorial topics (spec 6.13).

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Style;
use tuikit::paint::put_str;

use super::widgets::{self, Hint};
use crate::app::App;
use crate::ui::markdown::{self, LineKind, MdLine};

/// Rows the page shows: from two under the top border to the footer block.
pub fn area(r: Rect, hints: &[Hint]) -> Rect {
    let f = widgets::foot_rows(r, hints).max(2);
    let top = r.y + 2;
    let bottom = (r.bottom() - 1).saturating_sub(f);
    Rect::new(
        r.x + 3,
        top,
        r.width.saturating_sub(6),
        bottom.saturating_sub(top),
    )
}

pub fn lines(app: &App, body: &str, width: u16) -> Vec<MdLine> {
    markdown::render(body, width as usize, width as usize, &app.theme, false)
}

pub fn draw(buf: &mut Buffer, app: &App, r: Rect, hints: &[Hint], body: &str, scroll: usize) {
    widgets::draw_hints(buf, app, r, hints);
    let a = area(r, hints);
    if a.width == 0 || a.height == 0 {
        return;
    }
    let md = lines(app, body, a.width);
    let skip = scroll.min(md.len().saturating_sub(a.height as usize));
    for (i, l) in md.iter().skip(skip).take(a.height as usize).enumerate() {
        let y = a.y + i as u16;
        if l.kind == LineKind::Code {
            crate::ui::fill(buf, Rect::new(a.x, y, a.width, 1), app.theme.md_code_bg);
        }
        let mut x = a.x;
        for sp in &l.spans {
            let mut st = Style::new().fg(app.theme.text_primary);
            st = st.patch(sp.style);
            if l.kind == LineKind::Code {
                st = st.bg(app.theme.md_code_bg);
            }
            x = put_str(buf, x, y, sp.content.as_ref(), st, a);
        }
    }
}

/// Scroll keys of a page; true when the key was one of them.
pub fn scroll_key(
    app: &App,
    r: Rect,
    hints: &[Hint],
    body: &str,
    scroll: &mut usize,
    key: KeyEvent,
) -> bool {
    let a = area(r, hints);
    let total = lines(app, body, a.width).len();
    let max = total.saturating_sub(a.height as usize);
    let page = (a.height as usize).saturating_sub(2).max(1);
    match key.code {
        KeyCode::Down | KeyCode::Char('j') => *scroll = (*scroll + 1).min(max),
        KeyCode::Up | KeyCode::Char('k') => *scroll = scroll.saturating_sub(1),
        KeyCode::PageDown | KeyCode::Char(' ') => *scroll = (*scroll + page).min(max),
        KeyCode::PageUp => *scroll = scroll.saturating_sub(page),
        KeyCode::Home | KeyCode::Char('g') => *scroll = 0,
        KeyCode::End | KeyCode::Char('G') => *scroll = max,
        _ => return false,
    }
    true
}
