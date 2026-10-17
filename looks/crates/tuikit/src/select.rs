//! Filterable list model for dialogs: fuzzy filter with highlight indices, grouped sections,
//! disabled rows, per-row right-aligned hints, scroll offset that keeps the selection visible,
//! paging and mouse wheel. Layout numbers follow opencode's `DialogSelect`.

use crate::editor::{Editor, EditorStyle, KeyOutcome};
use crate::fuzzysort;
use crate::paint::{fill_bg, put_str};
use crate::theme::Theme;
use crate::width::{display_width, truncate};
use crossterm::event::{
    KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use unicode_segmentation::UnicodeSegmentation;

const PAGE: usize = 10;
const TITLE_MAX: usize = 61;
const WHEEL_ROWS: usize = 3;

#[derive(Clone, Debug)]
pub struct SelectItem<T> {
    pub value: T,
    pub title: String,
    pub description: Option<String>,
    /// Right-aligned text, usually a keybind.
    pub hint: Option<String>,
    pub group: Option<String>,
    pub disabled: bool,
    /// Marks the active choice with a bullet (the model or theme in use).
    pub current: bool,
    /// Row background when selected, instead of `primary` (the red delete-confirm row).
    pub bg: Option<Color>,
    /// Selected row is drawn in the theme's `error` color (a pending delete).
    pub danger: bool,
    /// Draw the right-hand text in `success`, bold, even on the selected row (`✓ Enabled`).
    pub hint_success: bool,
    /// A cell in front of the title that replaces the bullet (pin slot digit, busy marker).
    pub gutter: Option<Gutter>,
}

/// What a row shows where the `●` would go.
#[derive(Clone, Debug)]
pub struct Gutter {
    pub text: String,
    pub muted: bool,
}

impl<T> SelectItem<T> {
    pub fn new(value: T, title: impl Into<String>) -> Self {
        Self {
            value,
            title: title.into(),
            description: None,
            hint: None,
            group: None,
            disabled: false,
            current: false,
            bg: None,
            danger: false,
            hint_success: false,
            gutter: None,
        }
    }

    pub fn description(mut self, d: impl Into<String>) -> Self {
        self.description = Some(d.into());
        self
    }

    pub fn hint(mut self, h: impl Into<String>) -> Self {
        self.hint = Some(h.into());
        self
    }

    pub fn group(mut self, g: impl Into<String>) -> Self {
        self.group = Some(g.into());
        self
    }

    pub fn disabled(mut self, d: bool) -> Self {
        self.disabled = d;
        self
    }

    pub fn current(mut self, c: bool) -> Self {
        self.current = c;
        self
    }

    pub fn bg(mut self, c: Color) -> Self {
        self.bg = Some(c);
        self
    }

    pub fn hint_success(mut self, on: bool) -> Self {
        self.hint_success = on;
        self
    }

    pub fn danger(mut self, d: bool) -> Self {
        self.danger = d;
        self
    }

    /// A one-cell marker in `accent` (or `textMuted` when `muted`) in front of the title.
    pub fn gutter(mut self, text: impl Into<String>, muted: bool) -> Self {
        self.gutter = Some(Gutter {
            text: text.into(),
            muted,
        });
        self
    }
}

/// What a key or mouse event did to the list.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SelectEvent {
    /// Not for the list; the host may handle it.
    Ignored,
    /// Query, selection or scroll changed.
    Changed,
    /// Enter or click on the item with this index into the original items.
    Submit(usize),
    Cancel,
}

#[derive(Clone, Debug)]
struct Visible {
    item: usize,
    /// Char indices into the title that matched the query.
    indices: Vec<usize>,
}

#[derive(Clone, Debug)]
enum RowKind {
    Header { group: String, gap_before: bool },
    Item(usize),
}

pub struct SelectState<T> {
    items: Vec<SelectItem<T>>,
    filter: Editor,
    visible: Vec<Visible>,
    rows: Vec<RowKind>,
    row_of: Vec<usize>,
    /// First row to scroll to so that an item's section header comes into view with it.
    top_of: Vec<usize>,
    selected: usize,
    offset: usize,
    viewport: usize,
    flat: bool,
    /// The host filters: every item is a match and keeps its order (`skipFilter`).
    skip_filter: bool,
    /// opencode's dialogs keep the selection on the middle row after every keyboard move.
    center: bool,
    /// Center once the real viewport height is known (the list has not been drawn yet).
    recenter: bool,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct SelectStyle {
    /// Emphasise the characters the query matched (opencode does not).
    pub highlight_matches: bool,
    /// A footer action has focus: the selected row drops to `backgroundElement` and its text
    /// to `textMuted`, as `DialogSelect` does while tab moves through the actions.
    pub action_focused: bool,
    /// Draw this in `accent` at the front of the selected row (a glyph, so the selection does
    /// not ride on the background step alone). Rows with their own gutter or the current-item
    /// dot keep those. None keeps opencode's look.
    pub marker: Option<&'static str>,
}

impl<T> SelectState<T> {
    pub fn new(items: Vec<SelectItem<T>>) -> Self {
        let mut s = Self {
            items,
            filter: Editor::new(),
            visible: Vec::new(),
            rows: Vec::new(),
            row_of: Vec::new(),
            top_of: Vec::new(),
            selected: 0,
            offset: 0,
            viewport: 10,
            flat: false,
            skip_filter: false,
            center: false,
            recenter: false,
        };
        s.refilter();
        s.select_current();
        s
    }

    /// Keep the selection on the middle row after keyboard moves and when the list opens on the
    /// current item, like opencode's `DialogSelect` (`scrollBy(y - floor(height / 2))`). Off by
    /// default: it scrolls the least that keeps the selection visible.
    pub fn set_centered(&mut self, on: bool) {
        self.center = on;
        if on {
            self.recenter = true;
            self.center_selected();
        }
    }

    fn center_selected(&mut self) {
        if self.visible.is_empty() {
            return;
        }
        let row = self.row_of[self.selected];
        self.offset = row.saturating_sub(self.viewport / 2).min(self.max_offset());
    }

    /// The items already reflect the query (the host filters, as opencode's session and model
    /// lists do): nothing is dropped or reordered here, the query is only shown.
    pub fn skip_filter(mut self, skip: bool) -> Self {
        self.skip_filter = skip;
        self.refilter();
        self
    }

    /// With `flat`, a non-empty query drops the group headers and lists matches by score.
    pub fn flat(mut self, flat: bool) -> Self {
        self.flat = flat;
        self.refilter();
        self
    }

    pub fn items(&self) -> &[SelectItem<T>] {
        &self.items
    }

    /// Replace the items, keeping the query. Selection returns to the first enabled row.
    pub fn set_items(&mut self, items: Vec<SelectItem<T>>) {
        self.items = items;
        self.refilter();
    }

    pub fn query(&self) -> &str {
        self.filter.text()
    }

    pub fn filter_editor(&mut self) -> &mut Editor {
        &mut self.filter
    }

    pub fn set_query(&mut self, q: &str) {
        let q = q.replace(['\n', '\r'], " ");
        self.filter.set_text(&q);
        self.refilter();
    }

    /// Paste into the filter; newlines become spaces.
    pub fn paste_query(&mut self, s: &str) {
        self.filter.paste(&s.replace(['\n', '\r'], " "));
        self.refilter();
    }

    pub fn len(&self) -> usize {
        self.visible.len()
    }

    pub fn is_empty(&self) -> bool {
        self.visible.is_empty()
    }

    /// Rows (headers, gaps and items) the visible list occupies.
    pub fn row_count(&self) -> usize {
        self.rows
            .iter()
            .map(|r| match r {
                RowKind::Header { gap_before, .. } => 1 + *gap_before as usize,
                RowKind::Item(_) => 1,
            })
            .sum()
    }

    pub fn viewport(&self) -> usize {
        self.viewport
    }

    pub fn offset(&self) -> usize {
        self.offset
    }

    /// Index into the original items of the selected row.
    pub fn selected_index(&self) -> Option<usize> {
        self.visible
            .get(self.selected)
            .map(|v| v.item)
            .filter(|&i| !self.items[i].disabled)
    }

    pub fn selected(&self) -> Option<&SelectItem<T>> {
        self.selected_index().map(|i| &self.items[i])
    }

    /// Position of the selected row among the visible items, counting from 0.
    pub fn position(&self) -> usize {
        self.selected
    }

    /// Visible items in display order as indices into `items()`.
    pub fn visible_indices(&self) -> Vec<usize> {
        self.visible.iter().map(|v| v.item).collect()
    }

    pub fn match_indices(&self, visible_pos: usize) -> &[usize] {
        self.visible.get(visible_pos).map_or(&[], |v| &v.indices)
    }

    fn refilter(&mut self) {
        let query = self.filter.text().trim().to_string();
        let mut matched: Vec<(f64, Visible)> = Vec::new();
        for (i, it) in self.items.iter().enumerate() {
            if query.is_empty() || self.skip_filter {
                matched.push((
                    0.0,
                    Visible {
                        item: i,
                        indices: Vec::new(),
                    },
                ));
                continue;
            }
            // `DialogSelect` runs fuzzysort over `title` and `category`, the title counting
            // double; the description is not searched.
            let ts = fuzzysort::go(&query, &it.title);
            let gs = it.group.as_deref().and_then(|g| fuzzysort::go(&query, g));
            if ts.is_none() && gs.is_none() {
                continue;
            }
            let total = ts.as_ref().map_or(0.0, |m| m.score) * 2.0 + gs.map_or(0.0, |m| m.score);
            matched.push((
                total,
                Visible {
                    item: i,
                    indices: ts.map(|m| m.indexes).unwrap_or_default(),
                },
            ));
        }
        if !query.is_empty() && !self.skip_filter {
            // fuzzysort's result order, ties included
            let order = fuzzysort::rank(&matched.iter().map(|m| m.0).collect::<Vec<_>>());
            let mut slots: Vec<Option<(f64, Visible)>> = matched.into_iter().map(Some).collect();
            matched = order.into_iter().filter_map(|i| slots[i].take()).collect();
        }
        // Group by section in order of first appearance.
        let grouped = !self.flat || query.is_empty();
        let mut order: Vec<String> = Vec::new();
        let mut buckets: Vec<Vec<Visible>> = Vec::new();
        for (_, v) in matched {
            let g = if grouped {
                self.items[v.item].group.clone().unwrap_or_default()
            } else {
                String::new()
            };
            match order.iter().position(|x| *x == g) {
                Some(p) => buckets[p].push(v),
                None => {
                    order.push(g);
                    buckets.push(vec![v]);
                }
            }
        }
        self.visible.clear();
        self.rows.clear();
        self.row_of.clear();
        for (gi, (g, bucket)) in order.into_iter().zip(buckets).enumerate() {
            if !g.is_empty() {
                self.rows.push(RowKind::Header {
                    group: g,
                    gap_before: gi > 0,
                });
            }
            for v in bucket {
                self.rows.push(RowKind::Item(self.visible.len()));
                self.visible.push(v);
            }
        }
        let mut row = 0;
        let mut pending_top: Option<usize> = None;
        self.row_of = vec![0; self.visible.len()];
        self.top_of = vec![0; self.visible.len()];
        for r in &self.rows {
            match r {
                RowKind::Header { gap_before, .. } => {
                    pending_top = Some(row);
                    row += 1 + *gap_before as usize;
                }
                RowKind::Item(v) => {
                    self.row_of[*v] = row;
                    self.top_of[*v] = pending_top.take().unwrap_or(row);
                    row += 1;
                }
            }
        }
        self.selected = self.first_enabled().unwrap_or(0);
        self.offset = 0;
        self.ensure_visible();
    }

    fn enabled(&self, vis: usize) -> bool {
        self.visible
            .get(vis)
            .is_some_and(|v| !self.items[v.item].disabled)
    }

    fn first_enabled(&self) -> Option<usize> {
        (0..self.visible.len()).find(|&i| self.enabled(i))
    }

    /// Put the selection on the item flagged `current`, if it is visible.
    pub fn select_current(&mut self) {
        if let Some(p) = self
            .visible
            .iter()
            .position(|v| self.items[v.item].current && !self.items[v.item].disabled)
        {
            self.selected = p;
            self.ensure_visible();
            if self.center {
                self.recenter = true;
                self.center_selected();
            }
        }
    }

    /// Select the first item matching `pred`.
    pub fn select_where(&mut self, pred: impl Fn(&T) -> bool) -> bool {
        let found = self
            .visible
            .iter()
            .position(|v| !self.items[v.item].disabled && pred(&self.items[v.item].value));
        if let Some(p) = found {
            self.selected = p;
            self.ensure_visible();
        }
        found.is_some()
    }

    pub fn set_viewport(&mut self, rows: usize) {
        self.viewport = rows.max(1);
        self.ensure_visible();
        if self.center && self.recenter {
            self.recenter = false;
            self.center_selected();
        }
    }

    fn max_offset(&self) -> usize {
        self.row_count().saturating_sub(self.viewport)
    }

    fn ensure_visible(&mut self) {
        if self.visible.is_empty() {
            self.offset = 0;
            return;
        }
        let (top, row) = (self.top_of[self.selected], self.row_of[self.selected]);
        if top < self.offset {
            self.offset = top;
        } else if row >= self.offset + self.viewport {
            self.offset = row + 1 - self.viewport;
        }
        // Nothing selectable above: show the very top, headers included.
        let nothing_above = self.visible[..self.selected]
            .iter()
            .all(|v| self.items[v.item].disabled);
        if nothing_above && row < self.viewport {
            self.offset = 0;
        }
        self.offset = self.offset.min(self.max_offset());
    }

    fn step(&mut self, dir: isize, wrap: bool) {
        let n = self.visible.len();
        if n == 0 || self.first_enabled().is_none() {
            return;
        }
        let mut i = self.selected as isize;
        for _ in 0..n {
            i += dir;
            if i < 0 {
                if !wrap {
                    return;
                }
                i = n as isize - 1;
            } else if i >= n as isize {
                if !wrap {
                    return;
                }
                i = 0;
            }
            if self.enabled(i as usize) {
                self.selected = i as usize;
                break;
            }
        }
        self.ensure_visible();
    }

    pub fn move_by(&mut self, delta: isize) {
        // Single steps wrap around like opencode; paging stops at the ends.
        let wrap = delta.abs() == 1;
        for _ in 0..delta.unsigned_abs() {
            let prev = self.selected;
            self.step(delta.signum(), wrap);
            if self.selected == prev {
                break;
            }
        }
        self.ensure_visible();
        if self.center {
            self.center_selected();
        }
    }

    pub fn page_up(&mut self) {
        self.move_by(-(PAGE as isize));
    }

    pub fn page_down(&mut self) {
        self.move_by(PAGE as isize);
    }

    pub fn select_first(&mut self) {
        if let Some(f) = self.first_enabled() {
            self.selected = f;
            self.ensure_visible();
        }
    }

    pub fn select_last(&mut self) {
        if let Some(l) = (0..self.visible.len()).rev().find(|&i| self.enabled(i)) {
            self.selected = l;
            self.ensure_visible();
        }
    }

    /// Scroll the viewport without moving the selection (mouse wheel).
    pub fn scroll_by(&mut self, delta: isize) {
        let max = self.max_offset();
        self.offset = if delta < 0 {
            self.offset.saturating_sub(delta.unsigned_abs())
        } else {
            (self.offset + delta as usize).min(max)
        };
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> SelectEvent {
        if key.kind == KeyEventKind::Release {
            return SelectEvent::Ignored;
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Esc => return SelectEvent::Cancel,
            KeyCode::Enter => {
                return self
                    .selected_index()
                    .map_or(SelectEvent::Ignored, SelectEvent::Submit);
            }
            KeyCode::Up => self.move_by(-1),
            KeyCode::Down => self.move_by(1),
            KeyCode::Char('p') if ctrl => self.move_by(-1),
            KeyCode::Char('n') if ctrl => self.move_by(1),
            KeyCode::PageUp => self.page_up(),
            KeyCode::PageDown => self.page_down(),
            KeyCode::Home if ctrl => self.select_first(),
            KeyCode::End if ctrl => self.select_last(),
            _ => {
                let before = self.filter.text().to_string();
                return match self.filter.apply_key(key) {
                    KeyOutcome::Ignored => SelectEvent::Ignored,
                    _ => {
                        if self.filter.text() != before {
                            self.refilter();
                        }
                        SelectEvent::Changed
                    }
                };
            }
        }
        SelectEvent::Changed
    }

    /// `list` is the rect the list was last drawn in.
    pub fn handle_mouse(&mut self, ev: MouseEvent, list: Rect) -> SelectEvent {
        let inside = ev.column >= list.x
            && ev.column < list.right()
            && ev.row >= list.y
            && ev.row < list.bottom();
        match ev.kind {
            MouseEventKind::ScrollUp if inside => {
                self.scroll_by(-(WHEEL_ROWS as isize));
                SelectEvent::Changed
            }
            MouseEventKind::ScrollDown if inside => {
                self.scroll_by(WHEEL_ROWS as isize);
                SelectEvent::Changed
            }
            MouseEventKind::Down(MouseButton::Left) if inside => match self.item_at(list, ev.row) {
                Some(v) => {
                    self.selected = v;
                    SelectEvent::Changed
                }
                None => SelectEvent::Ignored,
            },
            MouseEventKind::Up(MouseButton::Left) if inside => match self.item_at(list, ev.row) {
                Some(v) if v == self.selected => SelectEvent::Submit(self.visible[v].item),
                _ => SelectEvent::Ignored,
            },
            MouseEventKind::Moved if inside => match self.item_at(list, ev.row) {
                Some(v) if v != self.selected => {
                    self.selected = v;
                    SelectEvent::Changed
                }
                _ => SelectEvent::Ignored,
            },
            _ => SelectEvent::Ignored,
        }
    }

    /// Visible position (not item index) of the enabled item drawn on screen row `y`.
    fn item_at(&self, list: Rect, y: u16) -> Option<usize> {
        let row = (y.checked_sub(list.y)? as usize) + self.offset;
        let v = self.row_of.iter().position(|&r| r == row)?;
        self.enabled(v).then_some(v)
    }

    /// Draw the list into `area` (opencode: 1 column of padding around each row, titles at +3,
    /// hints ending 3 columns from the right edge).
    pub fn render_list(&mut self, area: Rect, buf: &mut Buffer, theme: &Theme, style: SelectStyle) {
        self.set_viewport(area.height as usize);
        if area.is_empty() {
            return;
        }
        if self.visible.is_empty() {
            put_str(
                buf,
                area.x + 4,
                area.y + 1,
                "No results found",
                Style::new().fg(theme.text_muted),
                area,
            );
            return;
        }
        let sel_fg = theme.selected_foreground(Some(theme.primary));
        let mut y = 0usize; // row counter in list coordinates
        for r in &self.rows {
            match r {
                RowKind::Header { group, gap_before } => {
                    if *gap_before {
                        y += 1;
                    }
                    if y >= self.offset && y < self.offset + area.height as usize {
                        let sy = area.y + (y - self.offset) as u16;
                        put_str(
                            buf,
                            area.x + 4,
                            sy,
                            group,
                            Style::new().fg(theme.accent).add_modifier(Modifier::BOLD),
                            area,
                        );
                    }
                    y += 1;
                }
                RowKind::Item(v) => {
                    if y >= self.offset && y < self.offset + area.height as usize {
                        let sy = area.y + (y - self.offset) as u16;
                        let item = &self.items[self.visible[*v].item];
                        let active = *v == self.selected;
                        let hl = style
                            .highlight_matches
                            .then(|| self.visible[*v].indices.as_slice());
                        let footer = (self.flat && !self.filter.text().trim().is_empty())
                            .then_some(item.group.as_deref())
                            .flatten();
                        draw_item(
                            buf, area, sy, item, active, hl, theme, sel_fg, style, footer,
                        );
                    }
                    y += 1;
                }
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_item<T>(
    buf: &mut Buffer,
    area: Rect,
    y: u16,
    item: &SelectItem<T>,
    active: bool,
    highlight: Option<&[usize]>,
    theme: &Theme,
    sel_fg: Color,
    style: SelectStyle,
    footer: Option<&str>,
) {
    let hx0 = area.x + u16::from(area.width > 2);
    let hx1 = area.right().saturating_sub(u16::from(area.width > 2));
    let row_area = Rect::new(hx0, y, hx1.saturating_sub(hx0), 1);
    // A focused footer action takes the highlight; the row stays marked, but quiet.
    let lit = active && !style.action_focused;
    let bg = if active {
        Some(if style.action_focused {
            theme.background_element
        } else {
            item.bg.unwrap_or(if item.danger {
                theme.error
            } else {
                theme.primary
            })
        })
    } else {
        None
    };
    if let Some(b) = bg {
        fill_bg(buf, row_area, b);
    }
    let with_bg = |s: Style| if let Some(b) = bg { s.bg(b) } else { s };

    let title_color = if lit {
        sel_fg
    } else if item.disabled || (style.action_focused && (active || item.current)) {
        theme.text_muted
    } else if item.current {
        theme.primary
    } else {
        theme.text
    };
    let muted_color = if lit { sel_fg } else { theme.text_muted };

    if let Some(g) = &item.gutter {
        let fg = if g.muted {
            theme.text_muted
        } else {
            theme.accent
        };
        put_str(
            buf,
            hx0 + 1,
            y,
            &g.text,
            with_bg(Style::new().fg(fg)),
            row_area,
        );
    } else if item.current {
        put_str(
            buf,
            hx0 + 1,
            y,
            "●",
            with_bg(Style::new().fg(title_color)),
            row_area,
        );
    } else if let (Some(m), true) = (style.marker, active) {
        put_str(
            buf,
            hx0 + 1,
            y,
            m,
            with_bg(Style::new().fg(theme.accent)),
            row_area,
        );
    }
    let text_x = hx0 + 3;
    let right_edge = hx1.saturating_sub(3);
    let footer = footer.or(item.hint.as_deref());
    let hint_w = footer.map_or(0, display_width) as u16;
    let hint_x = right_edge.saturating_sub(hint_w).max(text_x);
    if let Some(h) = footer {
        if hint_w > 0 {
            let hs = if item.hint_success {
                Style::new().fg(theme.success).add_modifier(Modifier::BOLD)
            } else {
                Style::new().fg(muted_color)
            };
            put_str(buf, hint_x, y, h, with_bg(hs), row_area);
        }
    }
    let gap = u16::from(hint_w > 0);
    let avail = hint_x.saturating_sub(text_x).saturating_sub(gap) as usize;
    // opencode cuts every title at 61 cells, whatever room the dialog has.
    let title = truncate(&item.title, avail.min(TITLE_MAX));
    let mut tstyle = Style::new().fg(title_color);
    if lit {
        tstyle = tstyle.add_modifier(Modifier::BOLD);
    }
    let tw = display_width(&title);
    match highlight {
        Some(idx) if !idx.is_empty() => {
            // Draw grapheme by grapheme so matched chars can be emphasised.
            let mut x = text_x;
            let mut ci = 0usize;
            for g in title.graphemes(true) {
                let hit = (ci..ci + g.chars().count()).any(|c| idx.binary_search(&c).is_ok());
                ci += g.chars().count();
                let mut st = tstyle;
                if hit {
                    st = st.add_modifier(Modifier::UNDERLINED);
                    if !lit {
                        st = st.fg(theme.primary);
                    }
                }
                x = put_str(buf, x, y, g, with_bg(st), row_area);
            }
        }
        _ => {
            put_str(buf, text_x, y, &title, with_bg(tstyle), row_area);
        }
    }
    if let Some(d) = &item.description {
        let used = tw as u16 + 1;
        let room = avail.saturating_sub(used as usize);
        if room > 0 {
            let d = truncate(d, room);
            // The description lives in the title's text node, so it is bold with it.
            let mut ds = Style::new().fg(muted_color);
            if lit {
                ds = ds.add_modifier(Modifier::BOLD);
            }
            put_str(buf, text_x + used, y, &d, with_bg(ds), row_area);
        }
    }
}

/// Convenience for single-line input drawing in dialogs.
pub fn filter_style(theme: &Theme) -> EditorStyle {
    EditorStyle {
        text: Style::new().fg(theme.text_muted),
        placeholder: Some(("Search".into(), Style::new().fg(theme.text_muted))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::TestTerminal;
    use crossterm::event::KeyEventState;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent {
            code,
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        }
    }

    fn items() -> Vec<SelectItem<&'static str>> {
        vec![
            SelectItem::new("switch-session", "Switch session")
                .hint("ctrl+x l")
                .group("Suggested"),
            SelectItem::new("switch-model", "Switch model")
                .hint("ctrl+x m")
                .group("Suggested"),
            SelectItem::new("connect", "Connect provider").group("Suggested"),
            SelectItem::new("new", "New session")
                .hint("ctrl+x n")
                .group("Session"),
            SelectItem::new("editor", "Open editor").group("Session"),
            SelectItem::new("move", "Move session")
                .description("Move to another project dir")
                .group("Session"),
            SelectItem::new("agent", "Switch agent").group("Agent"),
        ]
    }

    fn titles(s: &SelectState<&'static str>) -> Vec<String> {
        s.visible_indices()
            .iter()
            .map(|&i| s.items()[i].title.clone())
            .collect()
    }

    fn draw(s: &mut SelectState<&'static str>, w: u16, h: u16) -> TestTerminal {
        let theme = Theme::builtin("opencode").unwrap();
        let mut t = TestTerminal::new(w, h);
        t.draw(|b, a| s.render_list(a, b, &theme, SelectStyle::default()));
        t
    }

    #[test]
    fn groups_keep_first_appearance_order() {
        let s = SelectState::new(items());
        assert_eq!(s.len(), 7);
        let t = draw(&mut SelectState::new(items()), 40, 12);
        let plain = t.plain();
        let lines: Vec<&str> = plain.lines().collect();
        assert_eq!(lines[0].trim(), "Suggested");
        assert_eq!(lines[4].trim(), "");
        assert_eq!(lines[5].trim(), "Session");
        assert_eq!(lines[10].trim(), "Agent");
        assert_eq!(s.row_count(), 1 + 3 + 2 + 3 + 2 + 1);
    }

    #[test]
    fn layout_matches_opencode_columns() {
        let mut s = SelectState::new(items());
        let t = draw(&mut s, 60, 12);
        // title at +4, hint ends 4 columns from the right edge (1 padding + 3)
        assert_eq!(
            t.row(1),
            format!(
                "    Switch session{}ctrl+x l",
                " ".repeat(60 - 4 - 14 - 8 - 4)
            )
        );
        assert_eq!(t.row(0), "    Suggested");
        let theme = Theme::builtin("opencode").unwrap();
        assert_eq!(
            t.cell(1, 1).unwrap().bg,
            theme.primary,
            "selected row highlight starts after 1 col of padding"
        );
        assert_eq!(t.cell(0, 1).unwrap().bg, ratatui::style::Color::Reset);
        assert_eq!(t.cell(58, 1).unwrap().bg, theme.primary);
        assert_eq!(t.cell(59, 1).unwrap().bg, ratatui::style::Color::Reset);
        assert_eq!(t.cell(4, 1).unwrap().fg, theme.selected_foreground(None));
        assert!(t.cell(4, 1).unwrap().modifier.contains(Modifier::BOLD));
        assert_eq!(t.cell(4, 2).unwrap().fg, theme.text);
        assert_eq!(t.cell(4, 0).unwrap().fg, theme.accent);
        // description follows the title in muted colour
        let moved = (0..12)
            .find(|&y| t.row(y).contains("Move session"))
            .unwrap();
        assert!(t
            .row(moved)
            .contains("Move session Move to another project dir"));
    }

    #[test]
    fn filtering_ranks_and_regroups() {
        let mut s = SelectState::new(items());
        s.set_query("sess");
        // Three title matches, plus "Open editor" which only matches through its section name,
        // ranked after them.
        let t = titles(&s);
        assert_eq!(t.len(), 4, "{t:?}");
        // the group-only match ranks below the title matches of its own group
        let pos = |n: &str| t.iter().position(|x| x == n).unwrap();
        assert!(pos("Open editor") > pos("New session"), "{t:?}");
        assert!(pos("Open editor") > pos("Move session"), "{t:?}");
        assert!(
            t.contains(&"Switch session".to_string()) && t.contains(&"New session".to_string())
        );
        assert_eq!(
            s.selected().unwrap().title,
            t[0],
            "selection resets to the first match"
        );
        s.set_query("zzz");
        assert!(s.is_empty());
        assert!(s.selected().is_none());
        s.set_query("");
        assert_eq!(s.len(), 7);
    }

    #[test]
    fn query_matches_title_and_group_but_not_the_description() {
        let mut s = SelectState::new(items());
        // `DialogSelect` searches title and category only
        s.set_query("project");
        assert!(s.is_empty());
        s.set_query("agent");
        // title "Switch agent" and group "Agent" both match
        assert_eq!(titles(&s), vec!["Switch agent"]);
        s.set_query("sw ses");
        assert_eq!(titles(&s), vec!["Switch session"]);
    }

    #[test]
    fn flat_mode_drops_headers_while_filtering() {
        let mut s = SelectState::new(items()).flat(true);
        assert!(s.row_count() > 7, "headers when the query is empty");
        s.set_query("s");
        assert_eq!(s.row_count(), s.len());
    }

    #[test]
    fn match_indices_are_available_for_highlighting() {
        let mut s = SelectState::new(items());
        s.set_query("sm");
        let top = s.visible_indices()[0];
        assert_eq!(s.items()[top].title, "Switch model");
        assert_eq!(s.match_indices(0), &[0, 7]);
    }

    #[test]
    fn arrows_wrap_and_skip_disabled_rows() {
        let mut v = items();
        v[1].disabled = true;
        let mut s = SelectState::new(v);
        assert_eq!(s.selected().unwrap().title, "Switch session");
        s.move_by(1);
        assert_eq!(
            s.selected().unwrap().title,
            "Connect provider",
            "disabled row skipped"
        );
        s.move_by(-1);
        s.move_by(-1);
        assert_eq!(
            s.selected().unwrap().title,
            "Switch agent",
            "wraps to the end"
        );
        s.move_by(1);
        assert_eq!(s.selected().unwrap().title, "Switch session");
    }

    #[test]
    fn all_disabled_or_empty_lists_do_not_loop_or_panic() {
        let mut s = SelectState::new(vec![
            SelectItem::new(1, "a").disabled(true),
            SelectItem::new(2, "b").disabled(true),
        ]);
        s.move_by(1);
        s.page_down();
        s.select_last();
        assert!(s.selected().is_none() || s.selected().unwrap().disabled);
        assert_eq!(
            s.handle_key(key(KeyCode::Enter)),
            SelectEvent::Ignored,
            "a disabled row cannot be submitted"
        );
        let mut e: SelectState<()> = SelectState::new(Vec::new());
        e.move_by(1);
        e.page_up();
        assert_eq!(e.handle_key(key(KeyCode::Enter)), SelectEvent::Ignored);
        let theme = Theme::builtin("opencode").unwrap();
        let mut t = TestTerminal::new(30, 4);
        t.draw(|b, a| e.render_list(a, b, &theme, SelectStyle::default()));
        assert_eq!(t.row(1), "    No results found");
    }

    #[test]
    fn paging_moves_ten_and_stops_at_the_ends() {
        let many: Vec<SelectItem<usize>> = (0..25)
            .map(|i| SelectItem::new(i, format!("item {i}")))
            .collect();
        let mut s = SelectState::new(many);
        s.page_down();
        assert_eq!(s.selected().unwrap().value, 10);
        s.page_down();
        s.page_down();
        assert_eq!(s.selected().unwrap().value, 24);
        s.page_up();
        assert_eq!(s.selected().unwrap().value, 14);
        s.select_first();
        s.page_up();
        assert_eq!(s.selected().unwrap().value, 0);
    }

    #[test]
    fn scroll_offset_keeps_selection_in_view() {
        let many: Vec<SelectItem<usize>> = (0..40)
            .map(|i| SelectItem::new(i, format!("item {i}")))
            .collect();
        let mut s = SelectState::new(many);
        s.set_viewport(5);
        for _ in 0..7 {
            s.move_by(1);
        }
        assert_eq!(s.selected().unwrap().value, 7);
        assert_eq!(s.offset(), 3);
        s.move_by(-1);
        s.move_by(-1);
        s.move_by(-1);
        s.move_by(-1);
        s.move_by(-1);
        assert_eq!(s.selected().unwrap().value, 2);
        assert_eq!(s.offset(), 2);
        s.select_last();
        assert_eq!(s.offset(), 35);
        s.move_by(1); // wraps to the top
        assert_eq!(s.offset(), 0);
    }

    #[test]
    fn scrolling_up_to_a_section_start_reveals_its_header() {
        let mut s = SelectState::new(items());
        s.set_viewport(4);
        s.select_last();
        assert!(s.offset() > 0);
        for _ in 0..3 {
            s.move_by(-1);
        }
        // selected is "New session" (first of Session); its header must be on screen
        assert_eq!(s.selected().unwrap().title, "New session");
        let t = draw(&mut s, 40, 4);
        assert!(t.plain().contains("Session"), "{}", t.plain());
    }

    #[test]
    fn centered_lists_keep_the_selection_on_the_middle_row() {
        let many: Vec<SelectItem<usize>> = (0..40)
            .map(|i| SelectItem::new(i, format!("item {i}")).current(i == 20))
            .collect();
        let mut s = SelectState::new(many);
        s.set_centered(true);
        s.set_viewport(10);
        // opens on the current item, in the middle of the 10 rows
        assert_eq!(s.selected().unwrap().value, 20);
        assert_eq!(s.offset(), 15);
        s.move_by(1);
        assert_eq!(s.offset(), 16);
        s.select_first();
        s.move_by(-1); // wraps to the end; clamped so the last row is the last item
        assert_eq!(s.selected().unwrap().value, 39);
        assert_eq!(s.offset(), 30);
        s.select_first();
        assert_eq!(s.offset(), 0, "the top clamps");
    }

    #[test]
    fn wheel_scrolls_viewport_without_moving_selection() {
        let many: Vec<SelectItem<usize>> = (0..40)
            .map(|i| SelectItem::new(i, format!("item {i}")))
            .collect();
        let mut s = SelectState::new(many);
        s.set_viewport(5);
        let list = Rect::new(0, 0, 40, 5);
        let wheel = |kind| MouseEvent {
            kind,
            column: 3,
            row: 2,
            modifiers: KeyModifiers::NONE,
        };
        assert_eq!(
            s.handle_mouse(wheel(MouseEventKind::ScrollDown), list),
            SelectEvent::Changed
        );
        assert_eq!(s.offset(), 3);
        assert_eq!(s.selected().unwrap().value, 0);
        for _ in 0..30 {
            s.handle_mouse(wheel(MouseEventKind::ScrollDown), list);
        }
        assert_eq!(s.offset(), 35, "clamped at the end");
        s.handle_mouse(wheel(MouseEventKind::ScrollUp), list);
        assert_eq!(s.offset(), 32);
        let outside = MouseEvent {
            kind: MouseEventKind::ScrollDown,
            column: 50,
            row: 2,
            modifiers: KeyModifiers::NONE,
        };
        assert_eq!(s.handle_mouse(outside, list), SelectEvent::Ignored);
    }

    #[test]
    fn click_selects_then_submits_and_hover_moves() {
        let mut s = SelectState::new(items());
        s.set_viewport(12);
        let list = Rect::new(10, 5, 50, 12);
        // row 0 is the "Suggested" header, row 1 the first item, row 3 "Connect provider"
        let ev = |kind, row| MouseEvent {
            kind,
            column: 20,
            row,
            modifiers: KeyModifiers::NONE,
        };
        assert_eq!(
            s.handle_mouse(ev(MouseEventKind::Down(MouseButton::Left), 5), list),
            SelectEvent::Ignored,
            "header"
        );
        assert_eq!(
            s.handle_mouse(ev(MouseEventKind::Down(MouseButton::Left), 8), list),
            SelectEvent::Changed
        );
        assert_eq!(s.selected().unwrap().title, "Connect provider");
        assert_eq!(
            s.handle_mouse(ev(MouseEventKind::Up(MouseButton::Left), 8), list),
            SelectEvent::Submit(2)
        );
        assert_eq!(
            s.handle_mouse(ev(MouseEventKind::Moved, 6), list),
            SelectEvent::Changed
        );
        assert_eq!(s.selected().unwrap().title, "Switch session");
    }

    #[test]
    fn keys_navigate_edit_the_query_and_submit() {
        let mut s = SelectState::new(items());
        assert_eq!(s.handle_key(key(KeyCode::Down)), SelectEvent::Changed);
        assert_eq!(s.handle_key(key(KeyCode::Enter)), SelectEvent::Submit(1));
        for c in "agent".chars() {
            s.handle_key(key(KeyCode::Char(c)));
        }
        assert_eq!(s.query(), "agent");
        assert_eq!(titles(&s), vec!["Switch agent"]);
        s.handle_key(key(KeyCode::Backspace));
        assert_eq!(s.query(), "agen");
        assert_eq!(s.handle_key(key(KeyCode::Esc)), SelectEvent::Cancel);
        assert_eq!(s.handle_key(key(KeyCode::F(5))), SelectEvent::Ignored);
        s.paste_query("x\ny");
        assert_eq!(s.query(), "agenx y");
    }

    #[test]
    fn current_item_gets_a_bullet_and_initial_selection() {
        let mut v = items();
        v[4].current = true;
        let mut s = SelectState::new(v);
        assert_eq!(s.selected().unwrap().title, "Open editor");
        s.move_by(1);
        let t = draw(&mut s, 40, 12);
        let row = (0..12).find(|&y| t.row(y).contains("Open editor")).unwrap();
        assert!(t.row(row).starts_with("  ● "), "{:?}", t.row(row));
        let theme = Theme::builtin("opencode").unwrap();
        assert_eq!(
            t.cell(5, row).unwrap().fg,
            theme.primary,
            "current but not selected is primary"
        );
    }

    #[test]
    fn highlight_option_underlines_matched_characters() {
        let theme = Theme::builtin("opencode").unwrap();
        let mut s = SelectState::new(items());
        s.set_query("cp");
        let mut t = TestTerminal::new(40, 4);
        t.draw(|b, a| {
            s.render_list(
                a,
                b,
                &theme,
                SelectStyle {
                    highlight_matches: true,
                    ..Default::default()
                },
            )
        });
        let row = (0..4)
            .find(|&y| t.row(y).contains("Connect provider"))
            .unwrap();
        assert!(t
            .cell(4, row)
            .unwrap()
            .modifier
            .contains(Modifier::UNDERLINED));
        assert!(!t
            .cell(5, row)
            .unwrap()
            .modifier
            .contains(Modifier::UNDERLINED));
    }

    #[test]
    fn long_titles_truncate_before_the_hint_and_tiny_areas_survive() {
        let theme = Theme::builtin("opencode").unwrap();
        let mut s = SelectState::new(vec![SelectItem::new(
            1,
            "A very long title that cannot fit",
        )
        .hint("ctrl+k")]);
        let mut t = TestTerminal::new(30, 2);
        t.draw(|b, a| s.render_list(a, b, &theme, SelectStyle::default()));
        let row = t.row(0);
        assert!(row.contains('…') && row.ends_with("ctrl+k"), "{row:?}");
        assert!(display_width(&row) <= 27);
        for (w, h) in [(0, 0), (1, 1), (3, 1), (8, 2)] {
            let mut tt = TestTerminal::new(w, h);
            tt.draw(|b, a| s.render_list(a, b, &theme, SelectStyle::default()));
        }
    }
}
