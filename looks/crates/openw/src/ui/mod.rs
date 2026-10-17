// OWNER: shared (top-level draw; touch only to add a new overlay)
//! Screen composition: page (home or session), then the dialog over it, then the toast.

pub mod dialogs;
pub mod footer;
pub mod header;
pub mod home;
pub mod logo;
pub mod prompt;
pub mod session;
pub mod sidebar;
pub mod tools;

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::widgets::Widget;
use tuikit::paint::fill;
use tuikit::toast::ToastView;

use crate::app::{App, Route};

/// Paint the whole screen into `buf`. Returns where the terminal cursor belongs, if shown.
pub fn draw(buf: &mut Buffer, app: &mut App) -> Option<(u16, u16)> {
    let area = buf.area;
    app.size = (area.width, area.height);
    fill(
        buf,
        area,
        Style::new().bg(app.theme.background).fg(app.theme.text),
    );
    let mut cursor = match app.route {
        Route::Home => home::draw(buf, app),
        Route::Session => header::draw_session(buf, app),
    };
    if app.dialogs.is_open() {
        let theme = app.theme.clone();
        cursor = app.dialogs.draw(buf, area, &theme);
    }
    if let Some(t) = app.toasts.current() {
        ToastView {
            toast: t,
            theme: &app.theme,
        }
        .render(Rect::new(area.x, area.y, area.width, area.height), buf);
    }
    cursor
}
