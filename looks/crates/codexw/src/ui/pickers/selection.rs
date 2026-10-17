//! The selection list view every Codex popup is built from (spec C.7.1, C.7.2), ported from
//! `bottom_pane/list_selection_view.rs` and `selection_popup_common.rs`.
//!
//! Rows are `› N. name (current)   description`, the description column is placed from the
//! widest visible name, a wrapped row continues under the description column, and the selected
//! row is one accent run. The view paints a tinted band (header, rows, one pad row each side) and
//! puts the hint outside it. Replaces the composer in the bottom pane; Enter either closes with
//! an [`AppAction`] or swaps the view for the next step (the effort list after a model).

use std::fmt;
use std::sync::Arc;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Style, Stylize};
use ratatui::text::{Line, Span};
use unicode_segmentation::UnicodeSegmentation;

use crate::style::palette;
use crate::ui::{AppAction, BottomView, ViewResult};
use crate::wrap::{WrapOpts, width_of, word_wrap_line};

/// Rows shown at once; a wrapped row counts as one item (`popup_consts.rs`).
pub const MAX_POPUP_ROWS: usize = 8;

/// Where the description column starts (spec C.7.1 `desc_col`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ColumnWidthMode {
    /// Widest visible name plus the gap, capped at 70 percent of the width.
    #[default]
    AutoVisible,
    /// Same, over all rows, so the column does not move while scrolling.
    AutoAllRows,
    /// 30 percent of the width.
    Fixed,
}

/// Selected index and the first visible item (`scroll_state.rs`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ScrollState {
    pub selected_idx: Option<usize>,
    pub scroll_top: usize,
}

impl ScrollState {
    pub fn clamp_selection(&mut self, len: usize) {
        if self.clear_if_empty(len) {
            return;
        }
        self.selected_idx = Some(self.selected_idx.unwrap_or(0).min(len - 1));
    }

    pub fn move_up_wrap(&mut self, len: usize) {
        if self.clear_if_empty(len) {
            return;
        }
        self.selected_idx = Some(match self.selected_idx {
            Some(i) if i > 0 => i - 1,
            Some(_) => len - 1,
            None => 0,
        });
    }

    pub fn move_down_wrap(&mut self, len: usize) {
        if self.clear_if_empty(len) {
            return;
        }
        self.selected_idx = Some(match self.selected_idx {
            Some(i) if i + 1 < len => i + 1,
            _ => 0,
        });
    }

    pub fn page_up_clamped(&mut self, len: usize, visible: usize) {
        if self.clear_if_empty(len) {
            return;
        }
        let cur = self.selected_idx.unwrap_or(0).min(len - 1);
        self.selected_idx = Some(cur.saturating_sub(visible.max(1)));
        self.ensure_visible(len, visible);
    }

    pub fn page_down_clamped(&mut self, len: usize, visible: usize) {
        if self.clear_if_empty(len) {
            return;
        }
        let cur = self.selected_idx.unwrap_or(0).min(len - 1);
        self.selected_idx = Some(cur.saturating_add(visible.max(1)).min(len - 1));
        self.ensure_visible(len, visible);
    }

    pub fn jump_top(&mut self, len: usize, visible: usize) {
        if self.clear_if_empty(len) {
            return;
        }
        self.selected_idx = Some(0);
        self.ensure_visible(len, visible);
    }

    pub fn jump_bottom(&mut self, len: usize, visible: usize) {
        if self.clear_if_empty(len) {
            return;
        }
        self.selected_idx = Some(len - 1);
        self.ensure_visible(len, visible);
    }

    fn clear_if_empty(&mut self, len: usize) -> bool {
        if len != 0 {
            return false;
        }
        self.selected_idx = None;
        self.scroll_top = 0;
        true
    }

    pub fn ensure_visible(&mut self, len: usize, visible: usize) {
        if len == 0 || visible == 0 {
            self.scroll_top = 0;
            return;
        }
        match self.selected_idx {
            Some(sel) if sel < self.scroll_top => self.scroll_top = sel,
            Some(sel) => {
                let bottom = self.scroll_top + visible - 1;
                if sel > bottom {
                    self.scroll_top = sel + 1 - visible;
                }
            }
            None => self.scroll_top = 0,
        }
    }
}

/// What Enter does on a row.
#[derive(Clone)]
pub enum ItemAction {
    /// Close the view and do nothing.
    None,
    /// Close the view and ask the app to do this.
    Close(AppAction),
    /// Swap this view for the next step, built on demand.
    Child(Arc<dyn Fn() -> SelectionParams + Send + Sync>),
}

/// One row of the list before filtering and formatting.
#[derive(Clone)]
pub struct SelectionItem {
    pub name: String,
    pub description: Option<String>,
    /// Shown instead of `description` while the row is highlighted.
    pub selected_description: Option<String>,
    pub is_current: bool,
    pub is_default: bool,
    pub is_disabled: bool,
    pub disabled_reason: Option<String>,
    /// What the search matches against; rows without one vanish as soon as a query is typed.
    pub search_value: Option<String>,
    pub action: ItemAction,
}

impl SelectionItem {
    pub fn new(name: impl Into<String>, action: ItemAction) -> Self {
        Self {
            name: name.into(),
            description: None,
            selected_description: None,
            is_current: false,
            is_default: false,
            is_disabled: false,
            disabled_reason: None,
            search_value: None,
            action,
        }
    }

    pub fn described(mut self, d: impl Into<String>) -> Self {
        self.description = Some(d.into());
        self
    }

    fn enabled(&self) -> bool {
        !self.is_disabled && self.disabled_reason.is_none()
    }
}

/// Something drawn beside the list (or under it when the terminal is too narrow): the theme
/// preview.
pub trait SideContent: Send + Sync {
    fn render(&self, area: Rect, buf: &mut Buffer);
    fn desired_height(&self, width: u16) -> u16;
}

#[derive(Clone)]
pub struct SideParams {
    pub wide: Arc<dyn SideContent>,
    /// Shown below the list when the wide layout does not fit; `wide` again when `None`.
    pub stacked: Option<Arc<dyn SideContent>>,
    /// Narrowest side panel that is still drawn beside the list.
    pub min_width: u16,
    /// Keep the colours the content draws instead of forcing the terminal background.
    pub preserve_bg: bool,
}

/// Rows left between the list and the side panel.
const SIDE_CONTENT_GAP: u16 = 2;
/// The list is never narrower than this next to a side panel.
const MIN_LIST_WIDTH_FOR_SIDE: u16 = 40;

/// `(list, side)` widths when the side panel fits beside the list: half the content width (less
/// the gap) must reach `min_width` and leave the list `MIN_LIST_WIDTH_FOR_SIDE`.
pub fn side_by_side_layout_widths(content_width: u16, min_width: u16) -> Option<(u16, u16)> {
    let side = content_width.saturating_sub(SIDE_CONTENT_GAP) / 2;
    if side < min_width {
        return None;
    }
    let list = content_width.saturating_sub(SIDE_CONTENT_GAP + side);
    (list >= MIN_LIST_WIDTH_FOR_SIDE).then_some((list, side))
}

/// Called with the index (into `items`) of the row the highlight lands on.
pub type OnSelectionChanged = Arc<dyn Fn(usize) + Send + Sync>;
/// Called when the view is dismissed with Esc or Ctrl+C.
pub type OnDismiss = Arc<dyn Fn() + Send + Sync>;

/// Construction-time configuration (`SelectionViewParams`).
#[derive(Clone, Default)]
pub struct SelectionParams {
    pub side: Option<SideParams>,
    pub on_selection_changed: Option<OnSelectionChanged>,
    pub on_dismiss: Option<OnDismiss>,
    pub title: Option<String>,
    pub subtitle: Option<String>,
    /// Header rows after the title and subtitle (a coloured warning line).
    pub extra_header: Vec<Line<'static>>,
    pub footer_note: Option<Line<'static>>,
    /// `None` means the standard `Press enter to confirm or esc to go back`.
    pub footer_hint: Option<String>,
    pub items: Vec<SelectionItem>,
    pub is_searchable: bool,
    pub search_placeholder: Option<String>,
    pub col_width_mode: ColumnWidthMode,
    pub initial_selected_idx: Option<usize>,
    /// Asked for when the view is dismissed with Esc.
    pub on_cancel: Option<AppAction>,
}

pub const STANDARD_HINT: &str = "Press enter to confirm or esc to go back";

pub struct ListSelectionView {
    side: Option<SideParams>,
    on_selection_changed: Option<OnSelectionChanged>,
    on_dismiss: Option<OnDismiss>,
    /// The item the selection callback last heard about.
    notified: Option<usize>,
    header: Vec<Line<'static>>,
    footer_note: Option<Line<'static>>,
    footer_hint: String,
    items: Vec<SelectionItem>,
    state: ScrollState,
    is_searchable: bool,
    search_query: String,
    search_placeholder: Option<String>,
    col_width_mode: ColumnWidthMode,
    filtered: Vec<usize>,
    initial_selected_idx: Option<usize>,
    on_cancel: Option<AppAction>,
}

impl fmt::Debug for ListSelectionView {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ListSelectionView")
            .field("items", &self.items.len())
            .field("state", &self.state)
            .field("query", &self.search_query)
            .finish()
    }
}

/// Render-ready row.
struct Row {
    /// Gutter: `› `, then `N. ` on a non-searchable list.
    prefix: String,
    name: String,
    description: Option<String>,
    /// Continuation indent when there is no description (the label wraps under itself).
    wrap_indent: Option<usize>,
    is_disabled: bool,
    disabled_reason: Option<String>,
}

fn accent() -> Style {
    palette().accent()
}

impl ListSelectionView {
    pub fn new(p: SelectionParams) -> Self {
        let mut header = Vec::new();
        if let Some(t) = p.title {
            header.push(Line::from(t.bold()));
        }
        if let Some(s) = p.subtitle {
            header.push(Line::from(s.dim()));
        }
        header.extend(p.extra_header);
        let mut v = Self {
            side: p.side,
            on_selection_changed: p.on_selection_changed,
            on_dismiss: p.on_dismiss,
            notified: None,
            header,
            footer_note: p.footer_note,
            footer_hint: p.footer_hint.unwrap_or_else(|| STANDARD_HINT.to_string()),
            items: p.items,
            state: ScrollState::default(),
            is_searchable: p.is_searchable,
            search_query: String::new(),
            search_placeholder: if p.is_searchable {
                p.search_placeholder
            } else {
                None
            },
            col_width_mode: p.col_width_mode,
            filtered: Vec::new(),
            initial_selected_idx: p.initial_selected_idx,
            on_cancel: p.on_cancel,
        };
        v.apply_filter();
        v.notified = v.selected_actual();
        v
    }

    /// Tell the selection callback when the highlight moved to another item.
    fn notify_selection(&mut self) {
        let now = self.selected_actual();
        if now != self.notified {
            self.notified = now;
            if let (Some(i), Some(cb)) = (now, self.on_selection_changed.clone()) {
                cb(i);
            }
        }
    }

    /// Width the side panel gets beside the list, `None` when it stacks below it.
    fn side_width(&self, inner_width: u16) -> Option<u16> {
        let side = self.side.as_ref()?;
        side_by_side_layout_widths(inner_width, side.min_width).map(|(_, s)| s)
    }

    fn visible_len(&self) -> usize {
        self.filtered.len()
    }

    fn max_visible_rows(len: usize) -> usize {
        MAX_POPUP_ROWS.min(len.max(1))
    }

    fn selected_actual(&self) -> Option<usize> {
        self.state
            .selected_idx
            .and_then(|v| self.filtered.get(v).copied())
    }

    fn enabled_actual(&self, actual: usize) -> Option<usize> {
        self.items
            .get(actual)
            .is_some_and(SelectionItem::enabled)
            .then_some(actual)
    }

    fn apply_filter(&mut self) {
        let previously_selected = self
            .selected_actual()
            .filter(|a| self.enabled_actual(*a).is_some())
            .or_else(|| {
                self.initial_selected_idx
                    .take()
                    .filter(|a| self.enabled_actual(*a).is_some())
            })
            .or_else(|| {
                (!self.is_searchable)
                    .then(|| self.items.iter().position(|i| i.is_current && i.enabled()))
                    .flatten()
            });

        if self.is_searchable && !self.search_query.is_empty() {
            let q = self.search_query.to_lowercase();
            self.filtered = self
                .items
                .iter()
                .enumerate()
                .filter(|(_, i)| {
                    i.search_value
                        .as_ref()
                        .is_some_and(|v| v.to_lowercase().contains(&q))
                })
                .map(|(n, _)| n)
                .collect();
        } else {
            self.filtered = (0..self.items.len()).collect();
        }

        let len = self.filtered.len();
        let still = self.state.selected_idx.and_then(|v| {
            self.filtered
                .get(v)
                .and_then(|idx| self.filtered.iter().position(|c| c == idx))
        });
        let selected = still.or_else(|| {
            previously_selected.and_then(|a| self.filtered.iter().position(|i| *i == a))
        });
        self.state.selected_idx = selected
            .filter(|v| {
                self.filtered
                    .get(*v)
                    .and_then(|a| self.items.get(*a))
                    .is_some_and(SelectionItem::enabled)
            })
            .or_else(|| self.first_enabled_visible())
            .or_else(|| (len > 0).then_some(0));
        self.state.clamp_selection(len);
        self.state.ensure_visible(len, Self::max_visible_rows(len));
    }

    fn first_enabled_visible(&self) -> Option<usize> {
        self.filtered
            .iter()
            .position(|a| self.items.get(*a).is_some_and(SelectionItem::enabled))
    }

    fn build_rows(&self) -> Vec<Row> {
        let number_width = self
            .filtered
            .iter()
            .filter(|a| self.items.get(**a).is_some_and(SelectionItem::enabled))
            .count()
            .max(1)
            .to_string()
            .len();
        let mut number = 0;
        self.filtered
            .iter()
            .enumerate()
            .filter_map(|(visible, actual)| {
                self.items.get(*actual).map(|item| {
                    let selected = self.state.selected_idx == Some(visible);
                    let arrow = if selected { '›' } else { ' ' };
                    let marker = if item.is_current {
                        " (current)"
                    } else if item.is_default {
                        " (default)"
                    } else {
                        ""
                    };
                    let disabled = !item.enabled();
                    let prefix = if self.is_searchable {
                        format!("{arrow} ")
                    } else if disabled {
                        format!("{arrow} {}", " ".repeat(number_width + 2))
                    } else {
                        number += 1;
                        format!("{arrow} {number}. ")
                    };
                    let description = selected
                        .then(|| item.selected_description.clone())
                        .flatten()
                        .or_else(|| item.description.clone());
                    let wrap_indent = description.is_none().then(|| width_of(&prefix));
                    Row {
                        name: format!("{}{marker}", item.name),
                        prefix,
                        description,
                        wrap_indent,
                        is_disabled: disabled,
                        disabled_reason: item.disabled_reason.clone(),
                    }
                })
            })
            .collect()
    }

    fn move_up(&mut self) {
        let len = self.visible_len();
        self.state.move_up_wrap(len);
        for _ in 0..len {
            if self.selected_disabled() {
                self.state.move_up_wrap(len);
            } else {
                break;
            }
        }
        self.state.ensure_visible(len, Self::max_visible_rows(len));
    }

    fn move_down(&mut self) {
        let len = self.visible_len();
        self.state.move_down_wrap(len);
        for _ in 0..len {
            if self.selected_disabled() {
                self.state.move_down_wrap(len);
            } else {
                break;
            }
        }
        self.state.ensure_visible(len, Self::max_visible_rows(len));
    }

    fn page(&mut self, down: bool) {
        let len = self.visible_len();
        let vis = Self::max_visible_rows(len);
        if down {
            self.state.page_down_clamped(len, vis);
        } else {
            self.state.page_up_clamped(len, vis);
        }
        self.settle_on_enabled(down);
    }

    fn jump(&mut self, top: bool) {
        let len = self.visible_len();
        let vis = Self::max_visible_rows(len);
        if top {
            self.state.jump_top(len, vis);
        } else {
            self.state.jump_bottom(len, vis);
        }
        self.settle_on_enabled(top);
    }

    /// After a clamped jump onto a disabled row, look for the nearest enabled one, forward first.
    fn settle_on_enabled(&mut self, forward: bool) {
        let len = self.visible_len();
        if let Some(start) = self.state.selected_idx {
            if self.visible_disabled(start) {
                let fwd = ((start + 1)..len).find(|i| !self.visible_disabled(*i));
                let back = (0..start).rev().find(|i| !self.visible_disabled(*i));
                self.state.selected_idx =
                    if forward { fwd.or(back) } else { back.or(fwd) }.or(Some(start));
            }
        }
        self.state.ensure_visible(len, Self::max_visible_rows(len));
    }

    fn visible_disabled(&self, v: usize) -> bool {
        self.filtered
            .get(v)
            .and_then(|a| self.items.get(*a))
            .is_some_and(|i| !i.enabled())
    }

    fn selected_disabled(&self) -> bool {
        self.state
            .selected_idx
            .is_some_and(|v| self.visible_disabled(v))
    }

    fn accept(&mut self) -> ViewResult {
        let Some(actual) = self.selected_actual() else {
            return ViewResult::Pending;
        };
        let Some(item) = self.items.get(actual) else {
            return ViewResult::Pending;
        };
        if !item.enabled() {
            return ViewResult::Pending;
        }
        match item.action.clone() {
            ItemAction::None => ViewResult::Close,
            ItemAction::Close(a) => ViewResult::CloseWith(a),
            ItemAction::Child(f) => {
                *self = ListSelectionView::new(f());
                ViewResult::Pending
            }
        }
    }

    fn cancel(&mut self) -> ViewResult {
        if let Some(cb) = self.on_dismiss.take() {
            cb();
        }
        match self.on_cancel.take() {
            Some(a) => ViewResult::CloseWith(a),
            None => ViewResult::Close,
        }
    }

    fn actual_for_number(&self, n: usize) -> Option<usize> {
        if n == 0 {
            return None;
        }
        self.items
            .iter()
            .enumerate()
            .filter(|(_, i)| i.enabled())
            .nth(n - 1)
            .map(|(i, _)| i)
    }

    fn hint_rows(&self) -> u16 {
        u16::from(!self.footer_hint.is_empty())
    }

    fn column_width(&self) -> ColumnWidthMode {
        self.col_width_mode
    }

    /// Rows area width for a given view width: the gutter sits in the band's left inset.
    fn rows_width(total: u16) -> u16 {
        total.saturating_sub(2)
    }

    fn rows_height(&self, rows: &[Row], width: u16) -> u16 {
        measure_rows_height(
            rows,
            &self.state,
            MAX_POPUP_ROWS,
            width.saturating_add(1),
            self.column_width(),
        )
    }

    fn footer_note_lines(&self, width: u16) -> Vec<Line<'static>> {
        match &self.footer_note {
            Some(n) => word_wrap_line(n, &WrapOpts::new(width.max(1) as usize)),
            None => Vec::new(),
        }
    }

    /// Number of selectable rows currently shown.
    pub fn len(&self) -> usize {
        self.visible_len()
    }

    pub fn is_empty(&self) -> bool {
        self.visible_len() == 0
    }

    pub fn selected_index(&self) -> Option<usize> {
        self.selected_actual()
    }

    pub fn query(&self) -> &str {
        &self.search_query
    }
}

impl BottomView for ListSelectionView {
    fn desired_height(&self, width: u16) -> u16 {
        let inner_width = width.saturating_sub(4);
        let side_w = self.side_width(inner_width);
        let rows = self.build_rows();
        let rows_w = match side_w {
            Some(sw) => Self::rows_width(width).saturating_sub(SIDE_CONTENT_GAP + sw),
            None => Self::rows_width(width),
        };
        let rows_h = self.rows_height(&rows, rows_w);
        let mut h = self.header.len() as u16 + rows_h + 3;
        if self.is_searchable {
            h += 1;
        }
        if side_w.is_none() {
            if let Some(side) = &self.side {
                let stacked = side.stacked.as_ref().unwrap_or(&side.wide);
                let sh = stacked.desired_height(inner_width);
                if sh > 0 {
                    h += 1 + sh;
                }
            }
        }
        h += self.footer_note_lines(width.saturating_sub(2)).len() as u16;
        h + self.hint_rows()
    }

    fn render(&self, area: Rect, buf: &mut Buffer) {
        if area.is_empty() {
            return;
        }
        let note = self.footer_note_lines(area.width.saturating_sub(2));
        let footer_rows = note.len() as u16 + self.hint_rows();
        let content_h = area.height.saturating_sub(footer_rows);
        let outer = Rect::new(area.x, area.y, area.width, content_h);
        tuikit::paint::set_style(buf, outer, palette().user_message_style());

        // The menu surface insets one row and two columns.
        let inner = Rect::new(
            outer.x + 2.min(outer.width),
            outer.y + 1.min(outer.height),
            outer.width.saturating_sub(4),
            outer.height.saturating_sub(2),
        );
        let mut y = inner.y;
        let bottom = inner.bottom();
        for l in &self.header {
            if y >= bottom {
                break;
            }
            tuikit::paint::put_line(buf, inner.x, y, l, inner);
            y += 1;
        }
        y += 1; // spacer
        if self.is_searchable && y < bottom {
            let span: Span<'static> = if self.search_query.is_empty() {
                self.search_placeholder
                    .clone()
                    .map(|p| p.dim())
                    .unwrap_or_else(|| "".into())
            } else {
                self.search_query.clone().into()
            };
            tuikit::paint::put_line(buf, inner.x, y, &Line::from(span), inner);
            y += 1;
        }
        let rows = self.build_rows();
        let side_w = self.side_width(inner.width);
        let full_rows_w = Self::rows_width(outer.width);
        let rows_w = match side_w {
            Some(sw) => full_rows_w.saturating_sub(SIDE_CONTENT_GAP + sw),
            None => full_rows_w,
        }
        .max(1);
        let rows_h = self.rows_height(&rows, rows_w.min(full_rows_w));
        let list = Rect::new(
            if rows.is_empty() {
                inner.x
            } else {
                inner.x.saturating_sub(2)
            },
            y,
            rows_w,
            rows_h.min(bottom.saturating_sub(y)),
        );
        if list.height > 0 {
            render_rows(
                list,
                buf,
                &rows,
                &self.state,
                list.height as usize,
                "no matches",
                self.column_width(),
            );
        }

        if let Some(side) = &self.side {
            match side_w {
                Some(sw) => {
                    // Beside the list: the right half, on the terminal's own background.
                    let side_x = inner.x + inner.width - sw;
                    let clear_x = side_x.saturating_sub(SIDE_CONTENT_GAP).max(outer.x);
                    let clear = Rect::new(clear_x, outer.y, outer.right() - clear_x, outer.height);
                    clear_to_terminal_bg(buf, clear);
                    side.wide
                        .render(Rect::new(side_x, inner.y, sw, inner.height), buf);
                    if !side.preserve_bg {
                        force_terminal_bg(buf, clear);
                    }
                }
                None => {
                    // Stacked: under the list, a gap row apart.
                    let stacked = side.stacked.as_ref().unwrap_or(&side.wide);
                    let sh = stacked.desired_height(inner.width);
                    let y0 = list.y + list.height + 1;
                    if sh > 0 && y0 < outer.bottom() {
                        let clear = Rect::new(outer.x, y0, outer.width, outer.bottom() - y0);
                        clear_to_terminal_bg(buf, clear);
                        stacked.render(
                            Rect::new(inner.x, y0, inner.width, sh.min(outer.bottom() - y0)),
                            buf,
                        );
                    }
                }
            }
        }

        let mut fy = outer.bottom();
        for l in &note {
            if fy >= area.bottom() {
                break;
            }
            let r = Rect::new(area.x + 2, fy, area.width.saturating_sub(2), 1);
            tuikit::paint::put_line(buf, r.x, fy, l, r);
            fy += 1;
        }
        if self.hint_rows() == 1 && fy < area.bottom() {
            let r = Rect::new(area.x + 2, fy, area.width.saturating_sub(2), 1);
            tuikit::paint::put_line(
                buf,
                r.x,
                fy,
                &Line::from(Span::from(self.footer_hint.clone()).dim()),
                r,
            );
        }
    }

    fn handle_key(&mut self, key: KeyEvent) -> ViewResult {
        let r = self.handle_key_inner(key);
        self.notify_selection();
        r
    }

    fn handle_paste(&mut self, text: &str) {
        if !self.is_searchable {
            return;
        }
        let t: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
        if t.is_empty() {
            return;
        }
        self.search_query.push_str(&t);
        self.apply_filter();
        self.notify_selection();
    }
}

impl ListSelectionView {
    fn handle_key_inner(&mut self, key: KeyEvent) -> ViewResult {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        let plain_char = matches!(key.code, KeyCode::Char(_)) && !ctrl && !alt;
        // Searchable lists keep printable characters for the query.
        let nav = !self.is_searchable || !plain_char;
        match key.code {
            KeyCode::Up if nav => self.move_up(),
            KeyCode::Down if nav => self.move_down(),
            KeyCode::Char('p' | 'k') if nav && (ctrl || (plain_char && !self.is_searchable)) => {
                self.move_up()
            }
            KeyCode::Char('n' | 'j') if nav && (ctrl || (plain_char && !self.is_searchable)) => {
                self.move_down()
            }
            KeyCode::PageUp if nav => self.page(false),
            KeyCode::PageDown if nav => self.page(true),
            KeyCode::Char('b') if ctrl => self.page(false),
            KeyCode::Char('f') if ctrl => self.page(true),
            KeyCode::Home if nav => self.jump(true),
            KeyCode::End if nav => self.jump(false),
            KeyCode::Backspace if self.is_searchable => {
                self.search_query.pop();
                self.apply_filter();
            }
            KeyCode::Esc => return self.cancel(),
            KeyCode::Char('c') if ctrl => return self.cancel(),
            KeyCode::Enter => return self.accept(),
            KeyCode::Char(c) if self.is_searchable && !ctrl && !alt && !c.is_ascii_control() => {
                self.search_query.push(c);
                self.apply_filter();
            }
            KeyCode::Char(c) if !self.is_searchable && !ctrl && !alt => {
                if let Some(n) = c.to_digit(10) {
                    if let Some(actual) = self.actual_for_number(n as usize) {
                        if let Some(v) = self.filtered.iter().position(|a| *a == actual) {
                            self.state.selected_idx = Some(v);
                            return self.accept();
                        }
                    }
                }
            }
            _ => {}
        }
        ViewResult::Pending
    }
}

// ---- row layout (selection_popup_common.rs, selection_row_layout.rs) ---------------------------

/// Column where descriptions start.
fn compute_desc_col(
    rows: &[Row],
    start: usize,
    visible: usize,
    content_width: u16,
    mode: ColumnWidthMode,
) -> usize {
    if content_width <= 1 {
        return 0;
    }
    let max_desc_col = content_width.saturating_sub(1) as usize;
    let max_auto = max_desc_col.min(((content_width as usize * 7) / 10).max(1));
    let name_w = |r: &Row| {
        let mut w = width_of(&r.prefix) + width_of(&r.name);
        if r.disabled_reason.is_some() {
            w += width_of(" (disabled)");
        }
        w
    };
    match mode {
        ColumnWidthMode::Fixed => ((content_width as usize * 3) / 10).clamp(1, max_desc_col),
        ColumnWidthMode::AutoVisible => rows
            .iter()
            .skip(start)
            .take(visible)
            .map(name_w)
            .max()
            .unwrap_or(0)
            .saturating_add(2)
            .min(max_auto),
        ColumnWidthMode::AutoAllRows => rows
            .iter()
            .map(name_w)
            .max()
            .unwrap_or(0)
            .saturating_add(2)
            .min(max_auto),
    }
}

fn combined_description(r: &Row) -> Option<String> {
    match (&r.description, &r.disabled_reason) {
        (Some(d), Some(why)) => Some(format!("{d} (disabled: {why})")),
        (Some(d), None) => Some(d.clone()),
        (None, Some(why)) => Some(format!("disabled: {why}")),
        (None, None) => None,
    }
}

/// The name cut to `limit` columns with an ellipsis, then ` (disabled)` when it has a reason.
fn name_spans(r: &Row, limit: usize) -> Vec<Span<'static>> {
    let mut out = String::new();
    let mut used = 0usize;
    let mut cut = false;
    for g in r.name.graphemes(true) {
        let w = width_of(g);
        if used + w > limit {
            cut = true;
            break;
        }
        used += w;
        out.push_str(g);
    }
    if cut {
        out.push('…');
    }
    let mut spans = vec![Span::from(out)];
    if r.disabled_reason.is_some() {
        spans.push(" (disabled)".dim());
    }
    spans
}

fn build_full_line(r: &Row, desc_col: usize) -> Line<'static> {
    let description = combined_description(r);
    let prefix_w = width_of(&r.prefix);
    let limit = description
        .as_ref()
        .map(|_| desc_col.saturating_sub(2).saturating_sub(prefix_w))
        .unwrap_or(usize::MAX);
    let names = name_spans(r, limit);
    let name_w = prefix_w + names.iter().map(|s| width_of(&s.content)).sum::<usize>();
    let mut spans = vec![Span::from(r.prefix.clone())];
    spans.extend(names);
    if let Some(d) = description {
        let gap = desc_col.saturating_sub(name_w);
        if gap > 0 {
            spans.push(" ".repeat(gap).into());
        }
        spans.push(d.dim());
    }
    Line::from(spans)
}

fn wrap_row_lines(r: &Row, desc_col: usize, width: u16) -> Vec<Line<'static>> {
    let full = build_full_line(r, desc_col);
    let max_indent = width.saturating_sub(1) as usize;
    let indent = r
        .wrap_indent
        .unwrap_or(if r.description.is_some() || r.disabled_reason.is_some() {
            desc_col
        } else {
            0
        })
        .min(max_indent);
    let opts =
        WrapOpts::new(width.max(1) as usize).subsequent_indent(Line::from(" ".repeat(indent)));
    word_wrap_line(&full, &opts)
}

fn item_window_start(rows: &[Row], state: &ScrollState, max_items: usize) -> usize {
    if rows.is_empty() || max_items == 0 {
        return 0;
    }
    let mut start = state.scroll_top.min(rows.len() - 1);
    if let Some(sel) = state.selected_idx {
        if sel < start {
            start = sel;
        } else {
            let bottom = start.saturating_add(max_items.saturating_sub(1));
            if sel > bottom {
                start = sel + 1 - max_items;
            }
        }
    }
    start
}

fn selected_visible_in_viewport(
    rows: &[Row],
    start: usize,
    max_items: usize,
    sel: usize,
    desc_col: usize,
    width: u16,
    height: u16,
) -> bool {
    if height == 0 {
        return false;
    }
    let mut used = 0usize;
    for (i, row) in rows.iter().enumerate().skip(start).take(max_items) {
        let lines = wrap_row_lines(row, desc_col, width).len().max(1);
        if used > 0 && used + lines > height as usize {
            break;
        }
        if i == sel {
            return true;
        }
        used += lines;
        if used >= height as usize {
            break;
        }
    }
    false
}

/// Wrapped rows can push the highlighted one out of a line-based viewport; advance the window.
fn adjust_start_for_wrapped_selection(
    rows: &[Row],
    state: &ScrollState,
    max_items: usize,
    measure_items: usize,
    width: u16,
    height: u16,
    mode: ColumnWidthMode,
) -> usize {
    let mut start = item_window_start(rows, state, max_items);
    let Some(sel) = state.selected_idx else {
        return start;
    };
    if height == 0 {
        return start;
    }
    while start < sel {
        let desc_col = compute_desc_col(rows, start, measure_items, width, mode);
        if selected_visible_in_viewport(rows, start, max_items, sel, desc_col, width, height) {
            break;
        }
        start += 1;
    }
    start
}

fn render_rows(
    area: Rect,
    buf: &mut Buffer,
    rows: &[Row],
    state: &ScrollState,
    max_results: usize,
    empty_message: &str,
    mode: ColumnWidthMode,
) -> u16 {
    if rows.is_empty() {
        if area.height > 0 {
            tuikit::paint::put_line(
                buf,
                area.x,
                area.y,
                &Line::from(Span::from(empty_message.to_string()).dim().italic()),
                area,
            );
        }
        return u16::from(area.height > 0);
    }
    let max_items = max_results.min(rows.len());
    if max_items == 0 {
        return 0;
    }
    let measure_items = max_items.min(area.height.max(1) as usize);
    let start = adjust_start_for_wrapped_selection(
        rows,
        state,
        max_items,
        measure_items,
        area.width,
        area.height,
        mode,
    );
    let desc_col = compute_desc_col(rows, start, measure_items, area.width, mode);
    let mut y = area.y;
    let mut drawn = 0u16;
    for (i, row) in rows.iter().enumerate().skip(start).take(max_items) {
        if y >= area.bottom() {
            break;
        }
        let mut lines = wrap_row_lines(row, desc_col, area.width);
        let selected = Some(i) == state.selected_idx && !row.is_disabled;
        for l in lines.iter_mut() {
            if selected {
                for s in l.spans.iter_mut() {
                    s.style = accent();
                }
            }
            if row.is_disabled {
                for s in l.spans.iter_mut() {
                    s.style = s.style.dim();
                }
            }
        }
        for l in lines {
            if y >= area.bottom() {
                break;
            }
            tuikit::paint::put_line(buf, area.x, y, &l, area);
            y += 1;
            drawn += 1;
        }
    }
    drawn
}

fn measure_rows_height(
    rows: &[Row],
    state: &ScrollState,
    max_results: usize,
    width: u16,
    mode: ColumnWidthMode,
) -> u16 {
    if rows.is_empty() {
        return 1;
    }
    let content_width = width.saturating_sub(1).max(1);
    let visible = max_results.min(rows.len());
    let start = item_window_start(rows, state, visible);
    let desc_col = compute_desc_col(rows, start, visible, content_width, mode);
    let mut total: u16 = 0;
    for r in rows.iter().skip(start).take(visible) {
        total = total.saturating_add(wrap_row_lines(r, desc_col, content_width).len() as u16);
    }
    total.max(1)
}

/// Paint the cells of `area` as blanks with no style, so the terminal's own background shows.
fn clear_to_terminal_bg(buf: &mut Buffer, area: Rect) {
    for y in area.top()..area.bottom() {
        for x in area.left()..area.right() {
            if let Some(c) = buf.cell_mut((x, y)) {
                c.reset();
            }
        }
    }
}

/// Drop any background the side content set, keeping its glyphs and foregrounds.
fn force_terminal_bg(buf: &mut Buffer, area: Rect) {
    for y in area.top()..area.bottom() {
        for x in area.left()..area.right() {
            if let Some(c) = buf.cell_mut((x, y)) {
                c.bg = ratatui::style::Color::Reset;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::style::{ColorLevel, Palette, set_palette};

    fn plain_palette() {
        set_palette(Palette::new(None, None, ColorLevel::TrueColor));
    }

    fn render_to_rows(view: &ListSelectionView, width: u16) -> Vec<String> {
        let h = view.desired_height(width);
        let area = Rect::new(0, 0, width, h);
        let mut buf = Buffer::empty(area);
        view.render(area, &mut buf);
        (0..h)
            .map(|y| {
                (0..width)
                    .map(|x| buf[(x, y)].symbol().to_string())
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect()
    }

    fn item(name: &str, desc: &str) -> SelectionItem {
        SelectionItem::new(name, ItemAction::None).described(desc)
    }

    #[test]
    fn spacing_with_subtitle_snapshot() {
        plain_palette();
        let mut ro = item("Read Only", "Wizard can read files");
        ro.is_current = true;
        let v = ListSelectionView::new(SelectionParams {
            title: Some("Select Approval Mode".into()),
            subtitle: Some("Switch between Wizard approval presets".into()),
            items: vec![ro, item("Full Access", "Wizard can edit files")],
            ..Default::default()
        });
        let want = [
            "",
            "  Select Approval Mode",
            "  Switch between Wizard approval presets",
            "",
            "› 1. Read Only (current)  Wizard can read files",
            "  2. Full Access          Wizard can edit files",
            "",
            "  Press enter to confirm or esc to go back",
        ];
        assert_eq!(render_to_rows(&v, 50), want);
    }

    #[test]
    fn empty_searchable_snapshot() {
        plain_palette();
        let v = ListSelectionView::new(SelectionParams {
            title: Some("Select a base branch".into()),
            is_searchable: true,
            search_placeholder: Some("Type to search branches".into()),
            ..Default::default()
        });
        let want = [
            "",
            "  Select a base branch",
            "",
            "  Type to search branches",
            "  no matches",
            "",
            "  Press enter to confirm or esc to go back",
        ];
        assert_eq!(render_to_rows(&v, 48), want);
    }

    #[test]
    fn model_picker_width_80_wraps_under_description_column() {
        plain_palette();
        let mut a = item(
            "gpt-5.1-codex",
            "Optimized for Wizard. Balance of reasoning quality and coding ability.",
        );
        a.is_current = true;
        let b = item(
            "gpt-5.1-codex-mini",
            "Optimized for Wizard. Cheaper, faster, but less capable.",
        );
        let v = ListSelectionView::new(SelectionParams {
            title: Some("Select Model and Effort".into()),
            items: vec![a, b],
            ..Default::default()
        });
        let rows = render_to_rows(&v, 80);
        let want = [
            "",
            "  Select Model and Effort",
            "",
            "› 1. gpt-5.1-codex (current)  Optimized for Wizard. Balance of reasoning",
            "                              quality and coding ability.",
            "  2. gpt-5.1-codex-mini       Optimized for Wizard. Cheaper, faster, but less",
            "                              capable.",
            "",
            "  Press enter to confirm or esc to go back",
        ];
        assert_eq!(rows, want);
    }

    #[test]
    fn footer_note_wraps_at_width_40() {
        plain_palette();
        let mut ro = item("Read Only", "Wizard can read files");
        ro.is_current = true;
        let v = ListSelectionView::new(SelectionParams {
            title: Some("Select Approval Mode".into()),
            footer_note: Some(Line::from(
                "Note: Use /setup-default-sandbox to allow network access.",
            )),
            items: vec![ro],
            ..Default::default()
        });
        let want = [
            "",
            "  Select Approval Mode",
            "",
            "› 1. Read Only (current)  Wizard can",
            "                          read files",
            "",
            "  Note: Use /setup-default-sandbox to",
            "  allow network access.",
            "  Press enter to confirm or esc to go ba",
        ];
        // The hint is clipped by the width like the snapshot.
        assert_eq!(render_to_rows(&v, 40), want);
    }

    fn scroll_view(mode: ColumnWidthMode) -> ListSelectionView {
        let mut items: Vec<SelectionItem> = (1..=8)
            .map(|i| item(&format!("Item {i}"), &format!("desc {i}")))
            .collect();
        items.push(item(
            "Item 9 with an intentionally much longer name",
            "desc 9",
        ));
        ListSelectionView::new(SelectionParams {
            title: Some("Debug".into()),
            footer_hint: Some(String::new()),
            items,
            col_width_mode: mode,
            ..Default::default()
        })
    }

    fn body(rows: &[String]) -> Vec<String> {
        rows[3..rows.len() - 1].to_vec()
    }

    #[test]
    fn column_modes_before_and_after_scroll() {
        plain_palette();
        for (mode, col) in [
            (ColumnWidthMode::AutoAllRows, 52usize),
            (ColumnWidthMode::Fixed, 28),
        ] {
            let mut v = scroll_view(mode);
            let first = body(&render_to_rows(&v, 96));
            assert!(first[0].starts_with("› 1. Item 1"), "{first:?}");
            let at = first[0]
                .find("desc 1")
                .map(|b| first[0][..b].chars().count());
            assert_eq!(at, Some(col), "{mode:?}");
            for _ in 0..8 {
                v.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
            }
            let after = body(&render_to_rows(&v, 96));
            assert!(after[0].starts_with("  2. Item 2"), "{after:?}");
            assert!(
                after.last().unwrap().starts_with("› 9. Item 9"),
                "{after:?}"
            );
        }
    }

    #[test]
    fn auto_visible_keeps_the_column_tight_until_the_long_row_scrolls_in() {
        plain_palette();
        let mut v = scroll_view(ColumnWidthMode::AutoVisible);
        let first = body(&render_to_rows(&v, 96));
        assert_eq!(first[0], "› 1. Item 1  desc 1");
        for _ in 0..8 {
            v.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        }
        let after = body(&render_to_rows(&v, 96));
        assert_eq!(
            after.last().unwrap(),
            "› 9. Item 9 with an intentionally much longer name  desc 9"
        );
        assert_eq!(
            after[0],
            "  2. Item 2                                         desc 2"
        );
    }

    #[test]
    fn fixed_mode_cuts_a_long_name_with_an_ellipsis() {
        plain_palette();
        let mut v = scroll_view(ColumnWidthMode::Fixed);
        for _ in 0..8 {
            v.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        }
        let after = body(&render_to_rows(&v, 96));
        assert_eq!(after.last().unwrap(), "› 9. Item 9 with an intent… desc 9");
    }

    #[test]
    fn digits_accept_the_nth_enabled_row() {
        plain_palette();
        let mut v = ListSelectionView::new(SelectionParams {
            items: vec![
                SelectionItem::new("a", ItemAction::Close(AppAction::Info("a".into()))),
                SelectionItem::new("b", ItemAction::Close(AppAction::Info("b".into()))),
            ],
            ..Default::default()
        });
        let r = v.handle_key(KeyEvent::new(KeyCode::Char('2'), KeyModifiers::NONE));
        assert_eq!(r, ViewResult::CloseWith(AppAction::Info("b".into())));
    }

    #[test]
    fn highlight_starts_on_the_current_row_and_wraps() {
        plain_palette();
        let mut cur = item("two", "");
        cur.is_current = true;
        let mut v = ListSelectionView::new(SelectionParams {
            items: vec![item("one", ""), cur, item("three", "")],
            ..Default::default()
        });
        assert_eq!(v.selected_index(), Some(1));
        v.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        v.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(v.selected_index(), Some(0));
        v.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
        assert_eq!(v.selected_index(), Some(2));
    }

    #[test]
    fn disabled_rows_are_skipped_and_cannot_be_accepted() {
        plain_palette();
        let mut off = item("off", "");
        off.is_disabled = true;
        let mut v = ListSelectionView::new(SelectionParams {
            items: vec![
                SelectionItem::new("a", ItemAction::None),
                off,
                SelectionItem::new("c", ItemAction::None),
            ],
            ..Default::default()
        });
        v.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(v.selected_index(), Some(2));
    }

    #[test]
    fn search_filters_and_digits_become_text() {
        plain_palette();
        let mut a = SelectionItem::new("alpha", ItemAction::None);
        a.search_value = Some("alpha".into());
        let mut b = SelectionItem::new("beta", ItemAction::None);
        b.search_value = Some("beta".into());
        let mut v = ListSelectionView::new(SelectionParams {
            items: vec![a, b],
            is_searchable: true,
            ..Default::default()
        });
        for c in "be1".chars() {
            v.handle_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
        }
        assert_eq!(v.query(), "be1");
        assert!(v.is_empty());
        v.handle_key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE));
        assert_eq!(v.len(), 1);
    }

    #[test]
    fn selected_row_is_one_accent_run() {
        set_palette(Palette::new(
            Some((230, 230, 230)),
            Some((0, 0, 0)),
            ColorLevel::TrueColor,
        ));
        let mut cur = item("gpt-5.5", "Frontier model");
        cur.is_current = true;
        let v = ListSelectionView::new(SelectionParams {
            title: Some("T".into()),
            items: vec![cur],
            ..Default::default()
        });
        let h = v.desired_height(60);
        let area = Rect::new(0, 0, 60, h);
        let mut buf = Buffer::empty(area);
        v.render(area, &mut buf);
        // Row 3 is the first list row: the arrow, the name and the description share one style.
        let a = buf[(0, 3)].style();
        let d = buf[(30, 3)].style();
        assert_eq!(a.fg, d.fg);
        assert!(!d.add_modifier.contains(ratatui::style::Modifier::DIM));
        assert!(a.add_modifier.contains(ratatui::style::Modifier::BOLD));
        // The pad row above the header has the band background, the hint row does not.
        assert!(matches!(buf[(0, 0)].bg, ratatui::style::Color::Rgb(..)));
        assert_eq!(buf[(0, h - 1)].bg, ratatui::style::Color::Reset);
    }

    #[test]
    fn every_size_renders_without_panicking() {
        plain_palette();
        let mut cur = item(
            "current one",
            "with a long description that has to wrap somewhere",
        );
        cur.is_current = true;
        let mut off = item("off", "x");
        off.disabled_reason = Some("why".into());
        let v = ListSelectionView::new(SelectionParams {
            title: Some("Title".into()),
            subtitle: Some("Sub".into()),
            footer_note: Some(Line::from("a note that wraps on narrow widths")),
            items: vec![
                cur,
                off,
                item("a very long name indeed, longer than any column", "d"),
            ],
            ..Default::default()
        });
        for w in 0..=130u16 {
            for h in [0u16, 1, 2, 5, 12, 40] {
                let area = Rect::new(0, 0, w, h);
                let mut buf = Buffer::empty(area);
                v.render(area, &mut buf);
            }
            let _ = v.desired_height(w);
        }
    }
}
