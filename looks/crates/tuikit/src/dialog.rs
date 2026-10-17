//! Modal dialog frame and the select dialog built on it, laid out like opencode's `dialog.tsx`
//! and `dialog-select.tsx`: the whole screen dims, a panel sits a quarter of the way down,
//! content has one row of top padding, the title row carries `esc` on the right.

use crate::editor::EditorStyle;
use crate::paint::{fill, pad, put_str};
use crate::select::{filter_style, SelectEvent, SelectItem, SelectState, SelectStyle};
use crate::theme::Theme;
use crate::width::display_width;
use crossterm::event::{KeyCode, KeyEvent, MouseEvent};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};

pub const MEDIUM: u16 = 60;
pub const LARGE: u16 = 88;
pub const XLARGE: u16 = 116;

/// Darken every cell the way the black overlay does (`theme.overlay_alpha`), foreground and
/// background both, so what is behind the dialog stays readable but recedes.
pub fn dim(buf: &mut Buffer, area: Rect, theme: &Theme) {
    let area = area.intersection(buf.area);
    for y in area.top()..area.bottom() {
        for x in area.left()..area.right() {
            if let Some(c) = buf.cell_mut((x, y)) {
                let fg = if c.fg == ratatui::style::Color::Reset {
                    theme.text
                } else {
                    c.fg
                };
                c.fg = theme.dim(fg);
                c.bg = theme.dim(c.bg);
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DialogLayout {
    /// The whole panel.
    pub outer: Rect,
    /// Inside the top padding row; this is where content starts.
    pub inner: Rect,
}

/// Panel placement: centred, `screen.height / 4` rows down (a .5 rounds up, as the reference
/// does at 42 rows: row 11), `width` wide but never more than
/// the screen minus 2. `content_h` is the height below the top padding row.
pub fn layout(screen: Rect, width: u16, content_h: u16) -> DialogLayout {
    let w = width
        .min(screen.width.saturating_sub(2))
        .max(screen.width.min(1));
    // Yoga rounds an odd leftover up, which is where opencode puts the panel.
    let x = screen.x + screen.width.saturating_sub(w).div_ceil(2);
    let y = screen.y + (screen.height + 2) / 4;
    let h = content_h
        .saturating_add(1)
        .min(screen.bottom().saturating_sub(y));
    let outer = Rect::new(x, y, w, h);
    DialogLayout {
        outer,
        inner: pad(outer, 0, 1, 0, 0),
    }
}

thread_local! {
    static LAST_OUTER: std::cell::Cell<Rect> = const { std::cell::Cell::new(Rect::new(0, 0, 0, 0)) };
}

/// The panel of the dialog drawn last, so the host can tell a click on the dim layer from one
/// on the dialog.
pub fn last_outer() -> Rect {
    LAST_OUTER.with(|c| c.get())
}

/// Dim the screen and paint the panel. Returns where to put content.
pub fn render_frame(
    buf: &mut Buffer,
    screen: Rect,
    theme: &Theme,
    width: u16,
    content_h: u16,
) -> DialogLayout {
    dim(buf, screen, theme);
    let l = layout(screen, width, content_h);
    LAST_OUTER.with(|c| c.set(l.outer));
    fill(
        buf,
        l.outer,
        Style::new().bg(theme.background_panel).fg(theme.text),
    );
    l
}

/// Title on the left, `esc` (or any hint) on the right, both inset 4 columns.
pub fn title_row(buf: &mut Buffer, row: Rect, theme: &Theme, title: &str, hint: &str) {
    let inner = pad(row, 4, 0, 4, 0);
    if inner.is_empty() {
        return;
    }
    let panel = theme.background_panel;
    put_str(
        buf,
        inner.x,
        inner.y,
        title,
        Style::new()
            .fg(theme.text)
            .bg(panel)
            .add_modifier(Modifier::BOLD),
        inner,
    );
    let hw = display_width(hint) as u16;
    if hw < inner.width {
        put_str(
            buf,
            inner.right() - hw,
            inner.y,
            hint,
            Style::new().fg(theme.text_muted).bg(panel),
            inner,
        );
    }
}

#[derive(Clone, Debug)]
pub struct FooterHint {
    pub title: String,
    pub key: String,
    pub right: bool,
    /// Tab can focus it and enter triggers it.
    pub action: bool,
    /// Drawn muted and skipped by tab.
    pub disabled: bool,
    /// Title in bold, as `DialogSelect` draws plain hints that are not actions.
    pub bold: bool,
}

impl FooterHint {
    pub fn new(title: impl Into<String>, key: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            key: key.into(),
            right: false,
            action: false,
            disabled: false,
            bold: false,
        }
    }

    pub fn right(mut self) -> Self {
        self.right = true;
        self
    }

    pub fn action(mut self) -> Self {
        self.action = true;
        self
    }

    pub fn bold(mut self) -> Self {
        self.bold = true;
        self
    }

    pub fn disabled(mut self, d: bool) -> Self {
        self.disabled = d;
        self
    }

    fn focusable(&self) -> bool {
        self.action && !self.disabled
    }
}

/// What a dialog frame needs from the host after drawing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DialogRender {
    pub outer: Rect,
    pub list: Rect,
    /// Where to put the terminal cursor (the filter input), if shown.
    pub cursor: Option<(u16, u16)>,
}

/// Filterable list in a dialog.
pub struct SelectDialog<T> {
    pub state: SelectState<T>,
    pub title: String,
    pub placeholder: String,
    pub width: u16,
    pub show_filter: bool,
    pub footer: Vec<FooterHint>,
    pub style: SelectStyle,
    /// What the title row shows on the right. opencode puts `esc` there.
    pub title_hint: String,
    /// Draw the footer as `key label` with the key in bold, the order openc uses everywhere.
    pub key_first: bool,
    /// A glyph drawn in `accent` in front of the filter input, so the row reads as a field.
    pub filter_bar: Option<&'static str>,
    /// Index into the focusable footer actions that tab moved to, if any.
    pub focus: Option<usize>,
    list_area: Rect,
}

impl<T> SelectDialog<T> {
    pub fn new(title: impl Into<String>, items: Vec<SelectItem<T>>) -> Self {
        Self {
            state: SelectState::new(items),
            title: title.into(),
            placeholder: "Search".into(),
            width: MEDIUM,
            show_filter: true,
            footer: Vec::new(),
            style: SelectStyle::default(),
            title_hint: "esc".into(),
            key_first: false,
            filter_bar: None,
            focus: None,
            list_area: Rect::default(),
        }
    }

    pub fn width(mut self, w: u16) -> Self {
        self.width = w;
        self
    }

    pub fn footer(mut self, hints: Vec<FooterHint>) -> Self {
        self.footer = hints;
        self
    }

    /// Move focus through the footer actions like `DialogSelect`: the first tab focuses the first
    /// action, going past either end gives focus back to the list.
    pub fn move_focus(&mut self, dir: isize) {
        let n = self.footer.iter().filter(|h| h.focusable()).count() as isize;
        if n == 0 {
            return;
        }
        self.focus = match self.focus {
            None => Some(if dir > 0 { 0 } else { (n - 1) as usize }),
            Some(i) => {
                let next = i as isize + dir;
                (0..n).contains(&next).then_some(next as usize)
            }
        };
    }

    /// The footer action tab has focused.
    pub fn focused_action(&self) -> Option<&FooterHint> {
        self.footer
            .iter()
            .filter(|h| h.focusable())
            .nth(self.focus?)
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> SelectEvent {
        // The filter row is hidden when there is nothing to type into.
        if !self.show_filter
            && !matches!(
                key.code,
                KeyCode::Up
                    | KeyCode::Down
                    | KeyCode::Enter
                    | KeyCode::Esc
                    | KeyCode::PageUp
                    | KeyCode::PageDown
            )
        {
            return SelectEvent::Ignored;
        }
        self.state.handle_key(key)
    }

    pub fn handle_mouse(&mut self, ev: MouseEvent) -> SelectEvent {
        self.state.handle_mouse(ev, self.list_area)
    }

    /// Rows the list wants: capped at half the screen minus 6, like opencode.
    fn list_height(&self, screen_h: u16) -> u16 {
        let cap = (screen_h / 2).saturating_sub(6).max(1);
        let want = if self.state.is_empty() {
            2
        } else {
            self.state.row_count().min(u16::MAX as usize) as u16
        };
        want.min(cap).max(1)
    }

    pub fn render(&mut self, buf: &mut Buffer, screen: Rect, theme: &Theme) -> DialogRender {
        let list_h = self.list_height(screen.height);
        let footer_h = u16::from(!self.footer.is_empty());
        let filter_h = if self.show_filter { 2 } else { 0 };
        // title + filter + gap + list + gap + footer + bottom padding
        let content_h = 1 + filter_h + 1 + list_h + 1 + footer_h + 1;
        let fr = render_frame(buf, screen, theme, self.width, content_h);
        let inner = fr.inner;

        title_row(
            buf,
            Rect { height: 1, ..inner },
            theme,
            &self.title,
            &self.title_hint,
        );
        let mut y = inner.y + 1;
        let mut cursor = None;
        if self.show_filter {
            let input = pad(Rect::new(inner.x, y + 1, inner.width, 1), 4, 0, 4, 0);
            let mut st: EditorStyle = filter_style(theme);
            st.placeholder = Some((self.placeholder.clone(), Style::new().fg(theme.text_muted)));
            st.text = st.text.bg(theme.background_panel);
            if let Some((_, s)) = st.placeholder.as_mut() {
                *s = s.bg(theme.background_panel);
            }
            if let (Some(bar), false) = (self.filter_bar, input.is_empty()) {
                put_str(
                    buf,
                    input.x.saturating_sub(2),
                    input.y,
                    bar,
                    Style::new().fg(theme.accent).bg(theme.background_panel),
                    fr.outer,
                );
            }
            if !input.is_empty() && input.bottom() <= fr.outer.bottom() {
                let info = self.state.filter_editor().render(input, buf, &st);
                cursor = info.cursor;
            }
            y += 2;
        }
        y += 1; // gap
        let list = Rect::new(inner.x, y, inner.width, list_h).intersection(fr.outer);
        self.list_area = list;
        let style = SelectStyle {
            action_focused: self.focus.is_some(),
            ..self.style
        };
        self.state.render_list(list, buf, theme, style);
        let fy = list.bottom() + 1;
        if footer_h == 1 && fy < fr.outer.bottom() {
            self.draw_footer(buf, Rect::new(inner.x, fy, inner.width, 1), theme);
        }
        DialogRender {
            outer: fr.outer,
            list,
            cursor,
        }
    }

    fn draw_footer(&self, buf: &mut Buffer, row: Rect, theme: &Theme) {
        let area = pad(row, 4, 0, 2, 0);
        let panel = theme.background_panel;
        let sel_fg = theme.selected_foreground(Some(theme.primary));
        // Index of the focused hint among all hints.
        let focused = self.focus.and_then(|f| {
            self.footer
                .iter()
                .enumerate()
                .filter(|(_, h)| h.focusable())
                .nth(f)
                .map(|(i, _)| i)
        });
        let draw = |buf: &mut Buffer, x: u16, i: usize, h: &FooterHint| -> u16 {
            let on = focused == Some(i);
            let bg = if on { theme.primary } else { panel };
            let title_fg = if on {
                sel_fg
            } else if h.disabled {
                theme.text_muted
            } else {
                theme.text
            };
            let key_fg = if on { sel_fg } else { theme.text_muted };
            let mut ts = Style::new().fg(title_fg).bg(bg);
            if on || h.bold {
                ts = ts.add_modifier(Modifier::BOLD);
            }
            if self.key_first {
                // `enter run`: the key is the thing to press, so it leads and is bold.
                let ks = Style::new()
                    .fg(if on { sel_fg } else { theme.text })
                    .bg(bg)
                    .add_modifier(Modifier::BOLD);
                let x = put_str(buf, x, area.y, &h.key, ks, area);
                return put_str(
                    buf,
                    x,
                    area.y,
                    &format!(" {}", h.title),
                    Style::new()
                        .fg(if on { sel_fg } else { theme.text_muted })
                        .bg(bg),
                    area,
                );
            }
            let x = put_str(buf, x, area.y, &h.title, ts, area);
            put_str(
                buf,
                x,
                area.y,
                &format!(" {}", h.key),
                Style::new().fg(key_fg).bg(bg),
                area,
            )
        };
        let mut x = area.x;
        for (i, h) in self.footer.iter().enumerate().filter(|(_, h)| !h.right) {
            x = draw(buf, x, i, h).saturating_add(if self.key_first { 3 } else { 2 });
        }
        let right: Vec<(usize, &FooterHint)> = self
            .footer
            .iter()
            .enumerate()
            .filter(|(_, h)| h.right)
            .collect();
        let total: u16 = right
            .iter()
            .map(|(_, h)| (display_width(&h.title) + 1 + display_width(&h.key)) as u16)
            .sum::<u16>()
            + 2 * right.len().saturating_sub(1) as u16;
        let mut rx = area.right().saturating_sub(total).max(x);
        for (i, h) in right {
            rx = draw(buf, rx, i, h).saturating_add(2);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::TestTerminal;
    use ratatui::style::Color;

    fn palette_items() -> Vec<SelectItem<&'static str>> {
        vec![
            SelectItem::new("a", "Switch session")
                .hint("ctrl+x l")
                .group("Suggested"),
            SelectItem::new("b", "Switch model")
                .hint("ctrl+x m")
                .group("Suggested"),
            SelectItem::new("c", "Connect provider").group("Suggested"),
            SelectItem::new("d", "Switch session")
                .hint("ctrl+x l")
                .group("Session"),
            SelectItem::new("e", "New session")
                .hint("ctrl+x n")
                .group("Session"),
            SelectItem::new("f", "Open editor")
                .hint("ctrl+x e")
                .group("Session"),
            SelectItem::new("g", "Move session")
                .description("Move to another project dir")
                .group("Session"),
            SelectItem::new("h", "Switch agent")
                .hint("ctrl+x a")
                .group("Agent"),
        ]
    }

    #[test]
    fn layout_matches_the_reference_geometry_at_120x36() {
        let l = layout(Rect::new(0, 0, 120, 36), MEDIUM, 17);
        // reference/120x36/06-palette.png: panel x=30..90, top row 9, 19 rows tall
        assert_eq!(l.outer, Rect::new(30, 9, 60, 18));
        // reference/150x42/06-palette: top row 11 (10.5 rounds up)
        assert_eq!(layout(Rect::new(0, 0, 150, 42), MEDIUM, 17).outer.y, 11);
        assert_eq!(layout(Rect::new(0, 0, 150, 44), MEDIUM, 17).outer.y, 11);
        let l = layout(Rect::new(0, 0, 120, 36), MEDIUM, 18);
        assert_eq!(l.outer, Rect::new(30, 9, 60, 19));
        assert_eq!(l.inner, Rect::new(30, 10, 60, 18));
    }

    #[test]
    fn narrow_and_tiny_screens_clamp() {
        assert_eq!(layout(Rect::new(0, 0, 40, 20), LARGE, 5).outer.width, 38);
        let l = layout(Rect::new(0, 0, 0, 0), MEDIUM, 10);
        assert!(l.outer.is_empty());
        let l = layout(Rect::new(0, 0, 10, 4), MEDIUM, 50);
        assert!(l.outer.bottom() <= 4);
    }

    #[test]
    fn dim_darkens_foreground_and_background_toward_black() {
        let theme = Theme::builtin("opencode").unwrap();
        let mut b = Buffer::empty(Rect::new(0, 0, 3, 1));
        b[(0, 0)]
            .set_char('x')
            .set_fg(Color::Rgb(238, 238, 238))
            .set_bg(Color::Rgb(30, 30, 30));
        let a = b.area;
        dim(&mut b, a, &theme);
        assert_eq!(b[(0, 0)].fg, Color::Rgb(98, 98, 98));
        assert_eq!(b[(0, 0)].bg, Color::Rgb(12, 12, 12));
        // default colours dim from the theme's text and background
        assert_eq!(b[(1, 0)].bg, theme.dim(Color::Reset));
    }

    #[test]
    fn select_dialog_reproduces_the_commands_palette_layout() {
        let theme = Theme::builtin("opencode").unwrap();
        let mut d = SelectDialog::new("Commands", palette_items());
        let mut t = TestTerminal::new(120, 36);
        let mut cursor = None;
        t.draw(|b, a| cursor = d.render(b, a, &theme).cursor);
        // title row 10, filter row 12, first header row 14, like the reference
        assert_eq!(t.cell(34, 10).unwrap().symbol(), "C");
        assert!(t.row(10).ends_with("esc"));
        assert_eq!(t.cell(83, 10).unwrap().symbol(), "e");
        assert_eq!(t.cell(34, 12).unwrap().symbol(), "S");
        assert!(t.row(12).contains("Search"));
        assert_eq!(cursor, Some((34, 12)));
        assert_eq!(t.row(14).trim(), "Suggested");
        let sel = t.row(15);
        assert!(
            sel.contains("Switch session") && sel.ends_with("ctrl+x l"),
            "{sel:?}"
        );
        // highlight spans x=31..89 on the selected row
        assert_eq!(t.cell(31, 15).unwrap().bg, theme.primary);
        assert_eq!(t.cell(88, 15).unwrap().bg, theme.primary);
        assert_eq!(t.cell(89, 15).unwrap().bg, theme.background_panel);
        // panel is 19 rows tall with the 12-row list capped; box ends at row 27
        assert_eq!(t.cell(30, 9).unwrap().bg, theme.background_panel);
        assert_eq!(t.cell(30, 27).unwrap().bg, theme.background_panel);
        assert_ne!(t.cell(30, 28).unwrap().bg, theme.background_panel);
        assert_ne!(t.cell(29, 15).unwrap().bg, theme.background_panel);
        // a header row below the cap is clipped, not drawn over the padding
        assert!(!t.row(26).contains("Agent"));
    }

    #[test]
    fn typing_filters_and_the_cursor_follows() {
        let theme = Theme::builtin("opencode").unwrap();
        let mut d = SelectDialog::new("Commands", palette_items());
        for c in "ag".chars() {
            d.handle_key(KeyEvent::new(
                KeyCode::Char(c),
                crossterm::event::KeyModifiers::NONE,
            ));
        }
        let mut t = TestTerminal::new(120, 36);
        let mut r = None;
        t.draw(|b, a| r = Some(d.render(b, a, &theme)));
        assert!(t.row(12).contains("ag"));
        assert_eq!(r.unwrap().cursor, Some((36, 12)));
        assert!(t.plain().contains("Switch agent"));
        assert!(!t.plain().contains("Connect provider"));
        // fewer rows: the panel shrinks
        assert!(r.unwrap().outer.height < 19);
    }

    #[test]
    fn footer_hints_render_left_and_right() {
        let theme = Theme::builtin("opencode").unwrap();
        let mut d = SelectDialog::new("Sessions", palette_items()).footer(vec![
            FooterHint::new("rename", "ctrl+r"),
            FooterHint::new("delete", "ctrl+d").right(),
        ]);
        let mut t = TestTerminal::new(120, 36);
        let mut r = None;
        t.draw(|b, a| r = Some(d.render(b, a, &theme)));
        let fy = r.unwrap().list.bottom() + 1;
        let row = t.row(fy);
        assert!(row.contains("rename ctrl+r"), "{row:?}");
        assert!(row.ends_with("delete ctrl+d"), "{row:?}");
    }

    #[test]
    fn empty_result_dialog_shows_the_placeholder_row() {
        let theme = Theme::builtin("opencode").unwrap();
        let mut d = SelectDialog::new("Commands", palette_items());
        d.state.set_query("zzzz");
        let mut t = TestTerminal::new(120, 36);
        t.draw(|b, a| {
            d.render(b, a, &theme);
        });
        assert!(t.plain().contains("No results found"));
    }

    #[test]
    fn mouse_events_use_the_last_drawn_list_area() {
        let theme = Theme::builtin("opencode").unwrap();
        let mut d = SelectDialog::new("Commands", palette_items());
        let mut t = TestTerminal::new(120, 36);
        let mut r = None;
        t.draw(|b, a| r = Some(d.render(b, a, &theme)));
        let list = r.unwrap().list;
        let click = |kind| MouseEvent {
            kind,
            column: list.x + 6,
            row: list.y + 3,
            modifiers: crossterm::event::KeyModifiers::NONE,
        };
        use crossterm::event::{MouseButton, MouseEventKind};
        d.handle_mouse(click(MouseEventKind::Down(MouseButton::Left)));
        assert_eq!(d.state.selected().unwrap().title, "Connect provider");
        assert_eq!(
            d.handle_mouse(click(MouseEventKind::Up(MouseButton::Left))),
            SelectEvent::Submit(2)
        );
    }

    #[test]
    fn tiny_screens_do_not_panic() {
        let theme = Theme::builtin("opencode").unwrap();
        let mut d =
            SelectDialog::new("Commands", palette_items()).footer(vec![FooterHint::new("a", "b")]);
        for (w, h) in [(0, 0), (1, 1), (10, 5), (30, 8), (200, 3)] {
            let mut t = TestTerminal::new(w, h);
            t.draw(|b, a| {
                d.render(b, a, &theme);
            });
        }
    }

    #[test]
    fn hidden_filter_ignores_typing() {
        let mut d = SelectDialog::new("Pick", palette_items());
        d.show_filter = false;
        assert_eq!(
            d.handle_key(KeyEvent::new(
                KeyCode::Char('x'),
                crossterm::event::KeyModifiers::NONE
            )),
            SelectEvent::Ignored
        );
        assert_eq!(d.state.query(), "");
        assert_eq!(
            d.handle_key(KeyEvent::new(
                KeyCode::Down,
                crossterm::event::KeyModifiers::NONE
            )),
            SelectEvent::Changed
        );
    }
}
