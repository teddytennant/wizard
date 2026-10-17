// OWNER: bottom-pane (slash popup, @ mentions)
//! The command popup under the composer while the draft starts with `/` (spec C.3.1, C.6), and
//! the row layout it shares with the `@` popup: a name column sized to the widest name over all
//! rows, descriptions wrapped under it, the selected row in the accent style.
//!
//! Ported from Codex's `command_popup.rs`, `scroll_state.rs` and the standard-row path of
//! `selection_popup_common.rs`.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use unicode_segmentation::UnicodeSegmentation;

use std::borrow::Cow;

use crate::commands::{COMMANDS, HIDDEN};
use crate::style::palette;
use crate::wrap::{WrapOpts, width_of, word_wrap_line};

/// Rows of items shown at once; wrapped descriptions add lines on top (spec C.3).
pub const MAX_POPUP_ROWS: usize = 8;

// ---- scroll state ---------------------------------------------------------------------------

#[derive(Debug, Default, Clone, Copy)]
pub struct ScrollState {
    pub selected_idx: Option<usize>,
    pub scroll_top: usize,
}

impl ScrollState {
    pub fn reset(&mut self) {
        self.selected_idx = None;
        self.scroll_top = 0;
    }

    pub fn clamp_selection(&mut self, len: usize) {
        if len == 0 {
            self.reset();
            return;
        }
        self.selected_idx = Some(self.selected_idx.unwrap_or(0).min(len - 1));
    }

    pub fn move_up_wrap(&mut self, len: usize) {
        if len == 0 {
            self.reset();
            return;
        }
        self.selected_idx = Some(match self.selected_idx {
            Some(i) if i > 0 => i - 1,
            Some(_) => len - 1,
            None => 0,
        });
    }

    pub fn move_down_wrap(&mut self, len: usize) {
        if len == 0 {
            self.reset();
            return;
        }
        self.selected_idx = Some(match self.selected_idx {
            Some(i) if i + 1 < len => i + 1,
            _ => 0,
        });
    }

    pub fn ensure_visible(&mut self, len: usize, visible_rows: usize) {
        if len == 0 || visible_rows == 0 {
            self.scroll_top = 0;
            return;
        }
        match self.selected_idx {
            Some(sel) if sel < self.scroll_top => self.scroll_top = sel,
            Some(sel) => {
                let bottom = self.scroll_top + visible_rows - 1;
                if sel > bottom {
                    self.scroll_top = sel + 1 - visible_rows;
                }
            }
            None => self.scroll_top = 0,
        }
    }
}

// ---- row layout -----------------------------------------------------------------------------

/// One row: a name with optional bold match characters, and a description under it.
#[derive(Debug, Clone)]
pub struct PopupRow {
    pub name: String,
    /// Character offsets into `name` drawn bold.
    pub match_indices: Vec<usize>,
    pub description: String,
}

fn name_spans(row: &PopupRow, limit: usize) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    let mut used = 0usize;
    let mut truncated = false;
    let mut char_idx = 0usize;
    for g in row.name.graphemes(true) {
        let w = width_of(g);
        if used + w > limit {
            truncated = true;
            break;
        }
        used += w;
        let mut matched = false;
        for _ in g.chars() {
            matched |= row.match_indices.contains(&char_idx);
            char_idx += 1;
        }
        spans.push(if matched {
            Span::styled(g.to_string(), Style::default().bold())
        } else {
            Span::raw(g.to_string())
        });
    }
    if truncated {
        spans.push(Span::raw("…"));
    }
    spans
}

fn compute_desc_col(rows: &[PopupRow], content_width: u16) -> usize {
    if content_width <= 1 {
        return 0;
    }
    let max_desc_col = content_width as usize - 1;
    // Names get at most 70% of the width so descriptions keep 30%.
    let max_auto = max_desc_col.min(((content_width as usize * 7) / 10).max(1));
    let widest = rows.iter().map(|r| width_of(&r.name)).max().unwrap_or(0);
    (widest + 2).min(max_auto)
}

fn wrap_row(row: &PopupRow, desc_col: usize, width: u16) -> Vec<Line<'static>> {
    let name_limit = desc_col.saturating_sub(2);
    let ns = name_spans(row, name_limit);
    let name_w: usize = ns.iter().map(|s| width_of(&s.content)).sum();
    let mut spans = ns;
    let gap = desc_col.saturating_sub(name_w);
    if gap > 0 {
        spans.push(Span::raw(" ".repeat(gap)));
    }
    spans.push(Span::styled(
        row.description.clone(),
        Style::default().dim(),
    ));
    let full = Line::from(spans);
    let indent = desc_col.min(width.saturating_sub(1) as usize);
    let opts =
        WrapOpts::new(width.max(1) as usize).subsequent_indent(Line::from(" ".repeat(indent)));
    word_wrap_line(&full, &opts)
}

fn window_start(rows: usize, state: &ScrollState, max_items: usize) -> usize {
    if rows == 0 || max_items == 0 {
        return 0;
    }
    let mut start = state.scroll_top.min(rows - 1);
    if let Some(sel) = state.selected_idx {
        if sel < start {
            start = sel;
        } else {
            let bottom = start + max_items - 1;
            if sel > bottom {
                start = sel + 1 - max_items;
            }
        }
    }
    start
}

/// Lines the popup needs at `width` (the composer's width, before the 2 column inset).
pub fn measure_rows_height(
    rows: &[PopupRow],
    state: &ScrollState,
    max_results: usize,
    width: u16,
) -> u16 {
    if rows.is_empty() {
        return 1;
    }
    let content_width = width.saturating_sub(1).max(1);
    let visible = max_results.min(rows.len());
    let start = window_start(rows.len(), state, visible);
    let desc_col = compute_desc_col(rows, content_width);
    rows.iter()
        .skip(start)
        .take(visible)
        .map(|r| wrap_row(r, desc_col, content_width).len() as u16)
        .sum()
}

fn selected_visible_in(
    rows: &[PopupRow],
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
    for (i, r) in rows.iter().enumerate().skip(start).take(max_items) {
        let n = wrap_row(r, desc_col, width).len().max(1);
        if used > 0 && used + n > height as usize {
            break;
        }
        if i == sel {
            return true;
        }
        used += n;
        if used >= height as usize {
            break;
        }
    }
    false
}

pub fn render_rows(
    area: Rect,
    buf: &mut Buffer,
    rows: &[PopupRow],
    state: &ScrollState,
    max_results: usize,
    empty_message: &str,
) -> u16 {
    if rows.is_empty() {
        if area.height > 0 {
            let l = Line::from(Span::styled(
                empty_message.to_string(),
                Style::default().dim().italic(),
            ));
            tuikit::paint::put_line(buf, area.x, area.y, &l, area);
        }
        return u16::from(area.height > 0);
    }
    let max_items = max_results.min(rows.len());
    let mut start = window_start(rows.len(), state, max_items);
    let desc_col = compute_desc_col(rows, area.width);
    if let Some(sel) = state.selected_idx {
        while start < sel
            && !selected_visible_in(
                rows,
                start,
                max_items,
                sel,
                desc_col,
                area.width,
                area.height,
            )
        {
            start += 1;
        }
    }
    let mut y = area.y;
    let mut drawn = 0u16;
    let accent = palette().accent();
    for (i, row) in rows.iter().enumerate().skip(start).take(max_items) {
        if y >= area.bottom() {
            break;
        }
        let mut lines = wrap_row(row, desc_col, area.width);
        if Some(i) == state.selected_idx {
            for l in &mut lines {
                for s in &mut l.spans {
                    s.style = accent;
                }
            }
        }
        for l in lines {
            if y >= area.bottom() {
                break;
            }
            tuikit::paint::put_line(buf, area.x, y, &l, Rect::new(area.x, y, area.width, 1));
            y += 1;
            drawn += 1;
        }
    }
    drawn
}

// ---- the command list -----------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlashItem {
    pub name: Cow<'static, str>,
    pub description: Cow<'static, str>,
    /// Typed names only: `/quit` for `/exit`, `/btw` for `/side`.
    pub alias: bool,
}

/// A command wizard advertised that codexw has no entry of its own for, listed after Codex's.
pub fn wizard_item(name: &str, description: &str) -> SlashItem {
    SlashItem {
        name: Cow::Owned(name.to_string()),
        description: Cow::Owned(description.to_string()),
        alias: false,
    }
}

/// Popup order (spec C.6.1) with the typed-only aliases next to the command they stand for.
pub fn all_items() -> Vec<SlashItem> {
    let mut out = Vec::new();
    for c in COMMANDS {
        if c.name == "exit" {
            if let Some((n, d)) = HIDDEN.iter().find(|(n, _)| *n == "quit") {
                out.push(SlashItem {
                    name: Cow::Borrowed(n),
                    description: Cow::Borrowed(d),
                    alias: true,
                });
            }
        }
        out.push(SlashItem {
            name: Cow::Borrowed(c.name),
            description: Cow::Borrowed(c.description),
            alias: false,
        });
        if c.name == "side" {
            if let Some((n, d)) = HIDDEN.iter().find(|(n, _)| *n == "btw") {
                out.push(SlashItem {
                    name: Cow::Borrowed(n),
                    description: Cow::Borrowed(d),
                    alias: true,
                });
            }
        }
    }
    out
}

/// Commands matching `query` (the text after the slash), Codex's then `extra`: all of them in
/// presentation order when empty, otherwise exact matches then prefix matches, case-insensitive.
pub fn matching_items(query: &str, extra: &[SlashItem]) -> Vec<(SlashItem, usize)> {
    let mut items = all_items();
    items.extend(extra.iter().cloned());
    let q = query.trim().to_lowercase();
    if q.is_empty() {
        return items
            .into_iter()
            .filter(|i| !i.alias)
            .map(|i| (i, 0))
            .collect();
    }
    let n = q.chars().count();
    let mut exact = Vec::new();
    let mut prefix = Vec::new();
    for i in items {
        let name = i.name.to_lowercase();
        if name == q {
            exact.push((i, n));
        } else if name.starts_with(&q) {
            prefix.push((i, n));
        }
    }
    exact.extend(prefix);
    exact
}

/// Names in Codex's popup order filtered by `query` (text after the slash).
pub fn visible_commands(query: &str) -> Vec<String> {
    matching_items(query, &[])
        .into_iter()
        .map(|(i, _)| i.name.into_owned())
        .collect()
}

/// True when some command, alias or `extra` entry included, starts with `name`.
pub fn has_command_prefix(name: &str, extra: &[SlashItem]) -> bool {
    let n = name.to_lowercase();
    all_items()
        .iter()
        .chain(extra)
        .any(|i| i.name.starts_with(&n))
}

// ---- the popup ------------------------------------------------------------------------------

#[derive(Debug)]
pub struct SlashPopup {
    filter: String,
    state: ScrollState,
    /// wizard's commands, after Codex's.
    extra: Vec<SlashItem>,
}

impl SlashPopup {
    pub fn new(filter: &str) -> Self {
        Self::with_extra(filter, Vec::new())
    }

    pub fn with_extra(filter: &str, extra: Vec<SlashItem>) -> Self {
        let mut p = SlashPopup {
            filter: String::new(),
            state: ScrollState::default(),
            extra,
        };
        p.set_filter(filter);
        p
    }

    /// `filter` is the text after the slash; the selection resets when it changes.
    pub fn set_filter(&mut self, filter: &str) {
        let prev = std::mem::replace(&mut self.filter, filter.to_string());
        if prev != self.filter {
            self.state.reset();
        }
        let n = self.matches().len();
        self.state.clamp_selection(n);
        self.state.ensure_visible(n, MAX_POPUP_ROWS.min(n));
    }

    fn matches(&self) -> Vec<(SlashItem, usize)> {
        matching_items(&self.filter, &self.extra)
    }

    fn rows(&self) -> Vec<PopupRow> {
        self.matches()
            .into_iter()
            .map(|(i, hl)| PopupRow {
                name: format!("/{}", i.name),
                match_indices: (1..=hl).collect(),
                description: i.description.to_string(),
            })
            .collect()
    }

    pub fn move_up(&mut self) {
        let n = self.matches().len();
        self.state.move_up_wrap(n);
        self.state.ensure_visible(n, MAX_POPUP_ROWS.min(n));
    }

    pub fn move_down(&mut self) {
        let n = self.matches().len();
        self.state.move_down_wrap(n);
        self.state.ensure_visible(n, MAX_POPUP_ROWS.min(n));
    }

    pub fn selected(&self) -> Option<SlashItem> {
        let m = self.matches();
        self.state
            .selected_idx
            .and_then(|i| m.get(i).map(|(it, _)| it.clone()))
    }

    pub fn is_empty(&self) -> bool {
        self.matches().is_empty()
    }

    pub fn required_height(&self, width: u16) -> u16 {
        measure_rows_height(&self.rows(), &self.state, MAX_POPUP_ROWS, width)
    }

    pub fn render(&self, area: Rect, buf: &mut Buffer) {
        let inner = Rect::new(
            area.x + 2.min(area.width),
            area.y,
            area.width.saturating_sub(2),
            area.height,
        );
        render_rows(
            inner,
            buf,
            &self.rows(),
            &self.state,
            MAX_POPUP_ROWS,
            "no matches",
        );
    }

    #[cfg(test)]
    pub fn selected_index(&self) -> Option<usize> {
        self.state.selected_idx
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::style::Color;

    fn dump(p: &SlashPopup, w: u16) -> Vec<String> {
        let h = p.required_height(w);
        let area = Rect::new(0, 0, w, h);
        let mut buf = Buffer::empty(area);
        p.render(area, &mut buf);
        (0..h)
            .map(|y| {
                (0..w)
                    .map(|x| buf[(x, y)].symbol().to_string())
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect()
    }

    #[test]
    fn empty_filter_lists_popup_order_without_aliases() {
        let names = visible_commands("");
        assert_eq!(names[0], "model");
        assert!(!names.iter().any(|n| n == "quit"));
        assert!(!names.iter().any(|n| n == "btw"));
        assert!(names.iter().any(|n| n == "exit"));
        let pos = |n: &str| names.iter().position(|x| *x == n).unwrap();
        assert!(pos("status") < pos("title"));
    }

    #[test]
    fn filter_is_prefix_with_exact_first() {
        assert_eq!(visible_commands("mo"), vec!["model"]);
        assert_eq!(visible_commands("st"), vec!["status", "statusline", "stop"]);
        assert_eq!(
            visible_commands("p"),
            vec![
                "permissions",
                "plan",
                "pets",
                "plugins",
                "ps",
                "personality"
            ]
        );
        assert_eq!(visible_commands("qu"), vec!["quit"]);
        assert_eq!(visible_commands("ar"), vec!["archive"]);
        assert_eq!(visible_commands("MO"), vec!["model"]);
        assert!(visible_commands("nonsense").is_empty());
        // Substring and fuzzy matches are not used.
        assert!(!visible_commands("ac").iter().any(|n| n == "compact"));
        // An exact match jumps ahead of longer prefix matches.
        assert_eq!(visible_commands("pet")[0], "pets");
        assert_eq!(visible_commands("status")[0], "status");
    }

    #[test]
    fn slash_st_snapshot() {
        // command_popup_filter_reset_after_scroll, narrowed to the three rows.
        let p = SlashPopup::new("st");
        let rows = dump(&p, 72);
        assert_eq!(
            rows,
            vec![
                "  /status      show current session configuration and token usage",
                "  /statusline  configure which items appear in the status line",
                "  /stop        stop all background terminals",
            ]
        );
    }

    #[test]
    fn first_row_is_one_accent_run_and_prefix_is_bold() {
        let p = SlashPopup::new("st");
        let area = Rect::new(0, 0, 72, 3);
        let mut buf = Buffer::empty(area);
        p.render(area, &mut buf);
        let accent = palette().accent();
        assert_eq!(buf[(2, 0)].fg, accent.fg.unwrap());
        assert!(
            buf[(2, 0)]
                .modifier
                .contains(ratatui::style::Modifier::BOLD)
        );
        assert_eq!(buf[(40, 0)].fg, accent.fg.unwrap());
        // Row 1: slash plain, `st` bold, rest plain, description dim.
        assert_eq!(buf[(2, 1)].fg, Color::Reset);
        assert!(
            buf[(3, 1)]
                .modifier
                .contains(ratatui::style::Modifier::BOLD)
        );
        assert!(
            !buf[(5, 1)]
                .modifier
                .contains(ratatui::style::Modifier::BOLD)
        );
        assert!(
            buf[(15, 1)]
                .modifier
                .contains(ratatui::style::Modifier::DIM)
        );
    }

    #[test]
    fn eight_items_at_120_and_selection_scrolls_one_row_at_a_time() {
        let mut p = SlashPopup::new("");
        assert_eq!(p.required_height(120), 8);
        let rows = dump(&p, 120);
        assert_eq!(
            rows[0],
            "  /model         choose what model and reasoning effort to use"
        );
        // The name column is the widest name over all rows, `/experimental`, plus two.
        assert!(rows[1].starts_with("  /fast          1.5x speed, increased usage"));
        assert!(rows[2].starts_with("  /ide           include current selection"));
        for _ in 0..12 {
            p.move_down();
        }
        let rows = dump(&p, 120);
        assert!(rows.last().unwrap().starts_with("  /review "), "{rows:?}");
        assert!(rows[0].starts_with("  /vim "), "{rows:?}");
    }

    #[test]
    fn up_from_the_top_wraps_to_the_last_row() {
        let mut p = SlashPopup::new("");
        p.move_up();
        assert_eq!(p.selected().unwrap().name, "ui");
        p.move_down();
        assert_eq!(p.selected().unwrap().name, "model");
    }

    #[test]
    fn long_descriptions_wrap_under_the_name_column() {
        let p = SlashPopup::new("");
        // At 80 columns `/ide` wraps to a second line (spec C.3: 9 lines).
        assert_eq!(p.required_height(80), 9);
        let rows = dump(&p, 80);
        assert!(rows[3].trim_start().starts_with("your IDE"), "{rows:?}");
    }

    #[test]
    fn no_rows_shows_a_dim_italic_placeholder() {
        let p = SlashPopup::new("zzz");
        assert_eq!(p.required_height(80), 1);
        assert_eq!(dump(&p, 80), vec!["  no matches"]);
    }

    #[test]
    fn filter_change_resets_selection() {
        let mut p = SlashPopup::new("");
        p.move_down();
        p.move_down();
        assert_eq!(p.selected_index(), Some(2));
        p.set_filter("s");
        assert_eq!(p.selected_index(), Some(0));
        p.set_filter("s");
        p.move_down();
        p.set_filter("s");
        assert_eq!(p.selected_index(), Some(1));
    }
}
