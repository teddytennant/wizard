//! The multi-select list `/statusline` and `/title` are built on (spec C.7.11), ported from
//! Codex's `multi_select_picker.rs`: type to search, Space toggles, Left and Right move the
//! highlighted item (only with an empty search), Enter confirms, Esc cancels, a live preview line
//! under the band and the hint under that.

use std::sync::Arc;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Style, Stylize};
use ratatui::text::{Line, Span};

use crate::style::palette;
use crate::ui::slash_popup::{MAX_POPUP_ROWS, ScrollState};
use crate::ui::status_indicator::truncate_with_ellipsis;
use crate::ui::{AppAction, BottomView, ViewResult};
use crate::wrap::{line_width, width_of};

/// Item names longer than this are cut with an ellipsis.
const NAME_TRUNCATE_LEN: usize = 21;
const SEARCH_PLACEHOLDER: &str = "Type to search";
const SECTION_BREAK_ROW: &str = "  ───────────────────────";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MultiItem {
    pub id: String,
    pub name: String,
    pub description: Option<String>,
    pub enabled: bool,
    pub orderable: bool,
    /// Draw a divider after this item when another visible one follows.
    pub section_break_after: bool,
}

impl MultiItem {
    pub fn new(id: &str, description: &str, enabled: bool) -> Self {
        Self {
            id: id.into(),
            name: id.into(),
            description: Some(description.into()),
            enabled,
            orderable: true,
            section_break_after: false,
        }
    }
}

pub type Preview = Arc<dyn Fn(&[MultiItem]) -> Option<Line<'static>> + Send + Sync>;
/// Called with the ids of the enabled items, in list order.
pub type Confirm = Arc<dyn Fn(&[String]) -> AppAction + Send + Sync>;

pub struct MultiSelectView {
    title: String,
    subtitle: String,
    items: Vec<MultiItem>,
    state: ScrollState,
    query: String,
    filtered: Vec<usize>,
    ordering: bool,
    footer_hint: String,
    preview: Option<Preview>,
    preview_line: Option<Line<'static>>,
    on_confirm: Confirm,
}

impl std::fmt::Debug for MultiSelectView {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MultiSelectView")
            .field("title", &self.title)
            .field("items", &self.items.len())
            .finish()
    }
}

impl MultiSelectView {
    pub fn new(
        title: &str,
        subtitle: &str,
        items: Vec<MultiItem>,
        ordering: bool,
        preview: Option<Preview>,
        on_confirm: Confirm,
    ) -> Self {
        let mut v = Self {
            title: title.into(),
            subtitle: subtitle.into(),
            items,
            state: ScrollState::default(),
            query: String::new(),
            filtered: Vec::new(),
            ordering,
            footer_hint: if ordering {
                "Press space to toggle; \u{2190}/\u{2192} to move; enter to confirm and close; esc to close"
            } else {
                "Press space to toggle; enter to confirm and close; esc to close"
            }
            .into(),
            preview,
            preview_line: None,
            on_confirm,
        };
        v.apply_filter();
        v.update_preview();
        v
    }

    pub fn items(&self) -> &[MultiItem] {
        &self.items
    }

    fn update_preview(&mut self) {
        self.preview_line = self.preview.as_ref().and_then(|p| p(&self.items));
    }

    fn apply_filter(&mut self) {
        let before = self
            .state
            .selected_idx
            .and_then(|v| self.filtered.get(v).copied());
        let q = self.query.trim();
        if q.is_empty() {
            self.filtered = (0..self.items.len()).collect();
        } else {
            let mut hits: Vec<(usize, i32)> = self
                .items
                .iter()
                .enumerate()
                .filter_map(|(i, it)| tuikit::fuzzy::score(q, &it.name).map(|m| (i, m.score)))
                .collect();
            hits.sort_by(|a, b| {
                b.1.cmp(&a.1)
                    .then_with(|| self.items[a.0].name.cmp(&self.items[b.0].name))
            });
            self.filtered = hits.into_iter().map(|(i, _)| i).collect();
        }
        let len = self.filtered.len();
        self.state.selected_idx = before
            .and_then(|a| self.filtered.iter().position(|i| *i == a))
            .or((len > 0).then_some(0));
        self.state.clamp_selection(len);
        self.state
            .ensure_visible(len, MAX_POPUP_ROWS.min(len.max(1)));
    }

    /// Display rows (an item row, then a divider row where one is due) and the selection and
    /// scroll position translated into row indices.
    fn build_rows(&self) -> (Vec<(String, Option<String>, bool)>, ScrollState) {
        let mut rows = Vec::new();
        let mut visible_to_row = Vec::new();
        for (v, actual) in self.filtered.iter().enumerate() {
            let it = &self.items[*actual];
            visible_to_row.push(rows.len());
            let selected = self.state.selected_idx == Some(v);
            let mark = if it.enabled { 'x' } else { ' ' };
            let mut name: String = it.name.chars().take(NAME_TRUNCATE_LEN).collect();
            if it.name.chars().count() > NAME_TRUNCATE_LEN {
                name.pop();
                name.push('…');
            }
            rows.push((
                format!("{} [{mark}] {name}", if selected { '›' } else { ' ' }),
                it.description.clone(),
                false,
            ));
            if it.section_break_after && v + 1 < self.filtered.len() {
                rows.push((SECTION_BREAK_ROW.to_string(), None, true));
            }
        }
        let state = ScrollState {
            selected_idx: self
                .state
                .selected_idx
                .and_then(|v| visible_to_row.get(v).copied()),
            scroll_top: visible_to_row
                .get(self.state.scroll_top)
                .copied()
                .unwrap_or(0),
        };
        (rows, state)
    }

    fn rows_height(rows: &[(String, Option<String>, bool)]) -> u16 {
        rows.len().clamp(1, MAX_POPUP_ROWS) as u16
    }

    fn toggle(&mut self) {
        let Some(actual) = self
            .state
            .selected_idx
            .and_then(|v| self.filtered.get(v).copied())
        else {
            return;
        };
        self.items[actual].enabled = !self.items[actual].enabled;
        self.update_preview();
    }

    fn move_item(&mut self, down: bool) {
        if !self.ordering || !self.query.is_empty() {
            return;
        }
        let Some(actual) = self
            .state
            .selected_idx
            .and_then(|v| self.filtered.get(v).copied())
        else {
            return;
        };
        let to = if down {
            if actual + 1 >= self.items.len() {
                return;
            }
            actual + 1
        } else {
            if actual == 0 {
                return;
            }
            actual - 1
        };
        if !self.items[actual].orderable || !self.items[to].orderable {
            return;
        }
        self.items.swap(actual, to);
        self.update_preview();
        self.apply_filter();
        if let Some(v) = self.filtered.iter().position(|i| *i == to) {
            self.state.selected_idx = Some(v);
        }
    }

    fn nav(&mut self, f: impl FnOnce(&mut ScrollState, usize)) {
        let len = self.filtered.len();
        f(&mut self.state, len);
        self.state
            .ensure_visible(len, MAX_POPUP_ROWS.min(len.max(1)));
    }
}

impl BottomView for MultiSelectView {
    fn desired_height(&self, _width: u16) -> u16 {
        let (rows, _) = self.build_rows();
        // band pad, title, subtitle, blank, two search rows, rows, band pad, hint, preview
        1 + 2 + 1 + 2 + Self::rows_height(&rows) + 1 + 1 + u16::from(self.preview_line.is_some())
    }

    fn render(&self, area: Rect, buf: &mut Buffer) {
        if area.height < 4 || area.width < 4 {
            return;
        }
        let footer_h = 1 + u16::from(self.preview_line.is_some());
        let band = Rect::new(
            area.x,
            area.y,
            area.width,
            area.height.saturating_sub(footer_h),
        );
        tuikit::paint::set_style(buf, band, palette().user_message_style());
        let inner = Rect::new(
            band.x + 2,
            band.y + 1,
            band.width.saturating_sub(4),
            band.height.saturating_sub(2),
        );
        let put = |buf: &mut Buffer, y: u16, line: Line<'static>| {
            if y < inner.bottom() {
                let r = Rect::new(inner.x, y, inner.width, 1);
                tuikit::paint::put_line(buf, r.x, y, &line, r);
            }
        };
        let mut y = inner.y;
        put(buf, y, Line::from(self.title.clone().bold()));
        y += 1;
        put(buf, y, Line::from(self.subtitle.clone().dim()));
        y += 2;
        put(buf, y, Line::from(SEARCH_PLACEHOLDER.dim()));
        y += 1;
        let mut search = vec![Span::from("> ").dim()];
        if !self.query.is_empty() {
            search.push(Span::from(self.query.clone()));
        }
        put(buf, y, Line::from(search));
        y += 1;
        let (rows, state) = self.build_rows();
        let list = Rect::new(
            band.x,
            y,
            band.width.saturating_sub(2).max(1),
            Self::rows_height(&rows).min(inner.bottom().saturating_sub(y)),
        );
        render_single_line_rows(list, buf, &rows, &state);
        let mut fy = band.bottom();
        if let Some(p) = &self.preview_line {
            let r = Rect::new(area.x + 2, fy, area.width.saturating_sub(2), 1);
            let line = truncate_with_ellipsis(p.clone(), r.width.saturating_sub(2) as usize);
            tuikit::paint::put_line(buf, r.x, fy, &line, r);
            fy += 1;
        }
        if fy < area.bottom() {
            let r = Rect::new(area.x + 2, fy, area.width.saturating_sub(2), 1);
            tuikit::paint::put_line(buf, r.x, fy, &Line::from(self.footer_hint.clone().dim()), r);
        }
    }

    fn handle_key(&mut self, key: KeyEvent) -> ViewResult {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        let plain = key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT;
        match key.code {
            KeyCode::Left if plain => self.move_item(false),
            KeyCode::Right if plain => self.move_item(true),
            KeyCode::Up => self.nav(|s, n| s.move_up_wrap(n)),
            KeyCode::Down => self.nav(|s, n| s.move_down_wrap(n)),
            KeyCode::Char('p') if ctrl => self.nav(|s, n| s.move_up_wrap(n)),
            KeyCode::Char('n') if ctrl => self.nav(|s, n| s.move_down_wrap(n)),
            KeyCode::Backspace => {
                self.query.pop();
                self.apply_filter();
            }
            KeyCode::Char(' ') if key.modifiers == KeyModifiers::NONE => self.toggle(),
            KeyCode::Enter => {
                let ids: Vec<String> = self
                    .items
                    .iter()
                    .filter(|i| i.enabled)
                    .map(|i| i.id.clone())
                    .collect();
                return ViewResult::CloseWith((self.on_confirm)(&ids));
            }
            KeyCode::Esc => return ViewResult::Close,
            KeyCode::Char('c') if ctrl => return ViewResult::Close,
            KeyCode::Char(c) if !ctrl && !alt => {
                self.query.push(c);
                self.apply_filter();
            }
            _ => {}
        }
        ViewResult::Pending
    }

    fn handle_paste(&mut self, text: &str) {
        let t: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
        if !t.is_empty() {
            self.query.push_str(&t);
            self.apply_filter();
        }
    }
}

/// Codex `render_rows_single_line`: names in a column sized by the widest visible one (at most
/// 70% of the width), descriptions dim after it, one row each, cut with an ellipsis; the selected
/// row is one accent run, dividers dim.
fn render_single_line_rows(
    area: Rect,
    buf: &mut Buffer,
    rows: &[(String, Option<String>, bool)],
    state: &ScrollState,
) {
    if area.height == 0 {
        return;
    }
    if rows.is_empty() {
        let l = Line::from(Span::styled("no matches", Style::default().dim().italic()));
        tuikit::paint::put_line(buf, area.x, area.y, &l, area);
        return;
    }
    let visible = MAX_POPUP_ROWS.min(rows.len()).min(area.height as usize);
    let mut start = state.scroll_top.min(rows.len() - 1);
    if let Some(sel) = state.selected_idx {
        if sel < start {
            start = sel;
        } else if visible > 0 && sel > start + visible - 1 {
            start = sel + 1 - visible;
        }
    }
    let max_auto = (area.width as usize)
        .saturating_sub(1)
        .min(((area.width as usize * 7) / 10).max(1));
    let desc_col = rows
        .iter()
        .skip(start)
        .take(visible)
        .map(|(n, _, _)| width_of(n))
        .max()
        .unwrap_or(0)
        .saturating_add(2)
        .min(max_auto);
    for (i, (name, desc, divider)) in rows.iter().enumerate().skip(start).take(visible) {
        let y = area.y + (i - start) as u16;
        let mut spans = vec![Span::raw(name.clone())];
        if let Some(d) = desc {
            let gap = desc_col.saturating_sub(width_of(name));
            spans.push(Span::raw(" ".repeat(gap)));
            spans.push(Span::styled(d.clone(), Style::default().dim()));
        }
        let mut line = Line::from(spans);
        if Some(i) == state.selected_idx && !divider {
            line.spans
                .iter_mut()
                .for_each(|s| s.style = palette().accent());
        }
        if *divider {
            line.spans.iter_mut().for_each(|s| s.style = s.style.dim());
        }
        let line = truncate_with_ellipsis(line, area.width as usize);
        debug_assert!(line_width(&line) <= area.width as usize);
        tuikit::paint::put_line(buf, area.x, y, &line, Rect::new(area.x, y, area.width, 1));
    }
}
