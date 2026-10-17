//! The resume picker: a full-screen list of saved sessions on the alternate screen (spec A.15),
//! ported from Codex's `resume_picker.rs`.
//!
//! Wizard lists only the first prompt, the working directory and the update time, so the branch
//! column reads `no branch` and a created time exists only when the session file's first line has
//! one. Everything is loaded at once, so there is no paging state: the `Loading…` states of the
//! Codex picker never show.

use std::fmt;
use std::path::PathBuf;
use std::time::Duration;

use agent_core::{HistoryItem, SessionInfo};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style, Stylize};
use ratatui::text::{Line, Span};
use unicode_segmentation::UnicodeSegmentation;

use crate::style::palette;
use crate::ui::pager_overlay::TranscriptOverlay;
use crate::ui::{AppAction, BoxedCell, Overlay, OverlayResult};
use crate::wrap::{WrapOpts, adaptive_wrap_line, line_width, width_of};

const DATE_WIDTH: usize = 12;
const META_INDENT: usize = 2;
const FIELD_GAP: usize = 2;
const MIN_CWD_WIDTH: usize = 30;
const MAX_CWD_WIDTH: usize = 72;
const BRANCH_ICON: &str = "\u{e0a0}";
const CWD_ICON: &str = "⌁";
const FOOTER_COMPACT_BREAKPOINT: u16 = 120;
const FOOTER_HINT_GAP: usize = 3;
const CHROME_HEIGHT: u16 = 8;
const LIST_INSET: u16 = 4;
/// The note wizard's data forces on the picker (spec Part E, row 34).
pub const LIST_NOTE: &str = "Resume list shows the first prompt, working directory and time; wizard does not keep branch or thread names.";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FilterMode {
    Cwd,
    All,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SortKey {
    Updated,
    Created,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Focus {
    Filter,
    Sort,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Density {
    Dense,
    Comfortable,
}

impl Density {
    fn toggle(self) -> Self {
        match self {
            Density::Dense => Density::Comfortable,
            Density::Comfortable => Density::Dense,
        }
    }
}

#[derive(Clone, Debug)]
pub struct SessionRow {
    pub id: String,
    /// The first prompt.
    pub title: String,
    /// A name given with `/rename`.
    pub name: Option<String>,
    pub cwd: String,
    pub created: Option<i64>,
    pub updated: Option<i64>,
}

impl SessionRow {
    fn display_preview(&self) -> &str {
        if let Some(n) = &self.name {
            n
        } else if self.title.trim().is_empty() {
            "(no message yet)"
        } else {
            &self.title
        }
    }

    fn matches_query(&self, q: &str) -> bool {
        self.title.to_lowercase().contains(q)
            || self
                .name
                .as_ref()
                .is_some_and(|n| n.to_lowercase().contains(q))
            || self.id.to_lowercase().contains(q)
            || self.cwd.to_lowercase().contains(q)
    }
}

pub struct ResumeOpts {
    /// Startup picker (`codexw resume`): Esc starts fresh, Ctrl+C quits.
    pub startup: bool,
    /// The directory the `Cwd` filter keeps.
    pub cwd: String,
    /// Unix seconds that relative times count from.
    pub now: i64,
    /// `~/.wizard/sessions`, for created times and transcript previews.
    pub sessions_dir: Option<PathBuf>,
    pub density: Density,
    /// Session id to name, from `/rename`.
    pub names: std::collections::BTreeMap<String, String>,
    /// Where Ctrl+O saves the density.
    pub config_path: Option<PathBuf>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Speaker {
    User,
    Assistant,
}

struct PreviewLine {
    speaker: Speaker,
    text: String,
}

/// The full transcript of one session in the shared pager (Ctrl+T).
struct OpenTranscript {
    overlay: TranscriptOverlay,
    cells: Vec<BoxedCell>,
}

pub struct ResumePicker {
    all: Vec<SessionRow>,
    rows: Vec<SessionRow>,
    selected: usize,
    scroll_top: usize,
    query: String,
    filter_mode: FilterMode,
    filter_cwd: Option<String>,
    focus: Focus,
    sort: SortKey,
    density: Density,
    startup: bool,
    now: i64,
    sessions_dir: Option<PathBuf>,
    config_path: Option<PathBuf>,
    inline_error: Option<String>,
    expanded: Option<String>,
    view_rows: Option<usize>,
    view_width: Option<u16>,
    pager: Option<OpenTranscript>,
}

impl fmt::Debug for ResumePicker {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ResumePicker")
            .field("rows", &self.rows.len())
            .field("selected", &self.selected)
            .finish()
    }
}

impl ResumePicker {
    pub fn new(sessions: &[SessionInfo], o: ResumeOpts) -> Self {
        let all = sessions
            .iter()
            .map(|s| SessionRow {
                id: s.id.clone(),
                title: s.title.clone(),
                name: o.names.get(&s.id).cloned(),
                cwd: s.cwd.clone(),
                created: o
                    .sessions_dir
                    .as_ref()
                    .and_then(|d| read_created(&d.join(format!("{}.jsonl", s.id)))),
                updated: (s.updated > 0).then_some(s.updated),
            })
            .collect();
        let mut p = Self {
            all,
            rows: Vec::new(),
            selected: 0,
            scroll_top: 0,
            query: String::new(),
            filter_mode: FilterMode::Cwd,
            filter_cwd: Some(o.cwd),
            focus: Focus::Filter,
            sort: SortKey::Updated,
            density: o.density,
            startup: o.startup,
            now: o.now,
            sessions_dir: o.sessions_dir,
            config_path: o.config_path,
            inline_error: None,
            expanded: None,
            view_rows: None,
            view_width: None,
            pager: None,
        };
        p.resort();
        p.apply_filter();
        p
    }

    fn resort(&mut self) {
        // Wizard has one timestamp that always exists; Created falls back to it for the order.
        let key = |r: &SessionRow| match self.sort {
            SortKey::Updated => r.updated.unwrap_or(0),
            SortKey::Created => r.created.or(r.updated).unwrap_or(0),
        };
        let mut all = std::mem::take(&mut self.all);
        all.sort_by_key(|r| std::cmp::Reverse(key(r)));
        self.all = all;
    }

    fn row_matches_filter(&self, r: &SessionRow) -> bool {
        match (&self.filter_mode, &self.filter_cwd) {
            (FilterMode::Cwd, Some(cwd)) => same_path(&r.cwd, cwd),
            _ => true,
        }
    }

    fn apply_filter(&mut self) {
        let q = self.query.to_lowercase();
        self.rows = self
            .all
            .iter()
            .filter(|r| self.row_matches_filter(r))
            .filter(|r| q.is_empty() || r.matches_query(&q))
            .cloned()
            .collect();
        if self.selected >= self.rows.len() {
            self.selected = self.rows.len().saturating_sub(1);
        }
        if self.rows.is_empty() {
            self.scroll_top = 0;
        }
        self.ensure_selected_visible();
    }

    fn set_query(&mut self, q: String) {
        if q == self.query {
            return;
        }
        self.query = q;
        self.selected = 0;
        self.apply_filter();
    }

    fn clear_query_preserving_selection(&mut self) {
        let keep = self.rows.get(self.selected).map(|r| r.id.clone());
        self.query.clear();
        self.apply_filter();
        if let Some(id) = keep {
            if let Some(i) = self.rows.iter().position(|r| r.id == id) {
                self.selected = i;
                self.ensure_selected_visible();
            }
        }
    }

    pub fn selected_id(&self) -> Option<&str> {
        self.rows.get(self.selected).map(|r| r.id.as_str())
    }

    pub fn row_count(&self) -> usize {
        self.rows.len()
    }

    // ---- scrolling ------------------------------------------------------------------------

    fn has_more_above(&self) -> bool {
        self.scroll_top > 0
    }

    fn available_content_rows(&self, viewport: usize) -> usize {
        viewport
            .saturating_sub(usize::from(self.has_more_above()))
            .saturating_sub(usize::from(self.selected + 1 < self.rows.len()))
            .max(1)
    }

    fn row_separator_height(&self) -> usize {
        match self.density {
            Density::Comfortable => 1,
            Density::Dense => 0,
        }
    }

    fn is_expanded(&self, idx: usize) -> bool {
        idx == self.selected
            && self
                .rows
                .get(idx)
                .is_some_and(|r| self.expanded.as_deref() == Some(&r.id))
    }

    fn rendered_height_between(&self, start: usize, end: usize) -> usize {
        let Some(slice) = self.rows.get(start..=end) else {
            return 0;
        };
        slice
            .iter()
            .enumerate()
            .map(|(off, row)| {
                let idx = start + off;
                self.session_lines(
                    row,
                    idx == self.selected,
                    self.is_expanded(idx),
                    false,
                    self.view_width.unwrap_or(u16::MAX),
                )
                .len()
            })
            .sum::<usize>()
            + self.row_separator_height() * end.saturating_sub(start)
    }

    fn ensure_selected_visible(&mut self) {
        if self.rows.is_empty() {
            self.scroll_top = 0;
            return;
        }
        let viewport = self.view_rows.unwrap_or(usize::MAX).max(1);
        if self.selected < self.scroll_top {
            self.scroll_top = self.selected;
        }
        while self.rendered_height_between(self.scroll_top, self.selected)
            > self.available_content_rows(viewport)
            && self.scroll_top < self.selected
        {
            self.scroll_top += 1;
        }
    }

    fn has_more_below(&self, viewport: usize) -> bool {
        if self.rows.is_empty() {
            return false;
        }
        let capacity = self.available_content_rows(viewport);
        let mut used = 0usize;
        for (off, row) in self.rows[self.scroll_top..].iter().enumerate() {
            let idx = self.scroll_top + off;
            let h = self
                .session_lines(
                    row,
                    idx == self.selected,
                    self.is_expanded(idx),
                    false,
                    self.view_width.unwrap_or(u16::MAX),
                )
                .len();
            let sep = usize::from(off > 0) * self.row_separator_height();
            if used + sep + h > capacity {
                return true;
            }
            used += sep + h;
        }
        false
    }

    fn progress_label(&self, list_height: u16, width: u16) -> String {
        let pos = if self.rows.is_empty() {
            0
        } else {
            self.selected + 1
        };
        let total = self.rows.len();
        let pct = self.scroll_percent(list_height);
        [
            format!(" {pos} / {total} · {pct}% "),
            format!(" {pos}/{total} · {pct}% "),
            format!(" {pct}% "),
        ]
        .into_iter()
        .find(|l| width_of(l) < width as usize)
        .unwrap_or_default()
    }

    fn scroll_percent(&self, list_height: u16) -> u8 {
        if self.rows.is_empty() {
            return 100;
        }
        let content = self.available_content_rows(list_height as usize);
        let total = self.rendered_height_between(0, self.rows.len() - 1);
        let max_scroll = total.saturating_sub(content);
        if max_scroll == 0 {
            return 100;
        }
        let remaining = self.rendered_height_between(self.scroll_top, self.rows.len() - 1);
        if remaining <= content {
            return 100;
        }
        let skipped = if self.scroll_top == 0 {
            0
        } else {
            self.rendered_height_between(0, self.scroll_top - 1)
        };
        (((skipped.min(max_scroll)) as f32 / max_scroll as f32) * 100.0).round() as u8
    }

    // ---- toolbar actions ------------------------------------------------------------------

    fn toggle_filter(&mut self) {
        if self.filter_cwd.is_none() {
            return;
        }
        self.filter_mode = match self.filter_mode {
            FilterMode::Cwd => FilterMode::All,
            FilterMode::All => FilterMode::Cwd,
        };
        self.selected = 0;
        self.scroll_top = 0;
        self.apply_filter();
    }

    fn toggle_sort(&mut self) {
        self.sort = match self.sort {
            SortKey::Updated => SortKey::Created,
            SortKey::Created => SortKey::Updated,
        };
        self.selected = 0;
        self.scroll_top = 0;
        self.resort();
        self.apply_filter();
    }

    fn toggle_density(&mut self) {
        self.density = self.density.toggle();
        self.ensure_selected_visible();
        if let Err(e) = self.save_density() {
            self.inline_error = Some(format!("Failed to save view mode: {e}"));
        }
    }

    fn save_density(&self) -> std::io::Result<()> {
        let Some(path) = &self.config_path else {
            return Ok(());
        };
        let v = match self.density {
            Density::Dense => "dense",
            Density::Comfortable => "comfortable",
        };
        crate::config::set(path, "session_picker_view", Some(v))
    }

    fn toggle_expansion(&mut self) {
        let Some(row) = self.rows.get(self.selected) else {
            return;
        };
        if self.expanded.as_deref() == Some(&row.id) {
            self.expanded = None;
        } else {
            self.expanded = Some(row.id.clone());
        }
        self.ensure_selected_visible();
    }

    fn load_items(&self, id: &str) -> Option<Vec<HistoryItem>> {
        let dir = self.sessions_dir.as_ref()?;
        read_history(&dir.join(format!("{id}.jsonl")))
    }

    fn open_transcript(&mut self) {
        let Some(row) = self.rows.get(self.selected) else {
            return;
        };
        let id = row.id.clone();
        match self.load_items(&id) {
            Some(items) if !items.is_empty() => {
                self.pager = Some(OpenTranscript {
                    overlay: TranscriptOverlay::new(),
                    cells: crate::app::replay_cells(&items),
                });
            }
            Some(_) => self.inline_error = Some("No transcript available for this session".into()),
            None => self.inline_error = Some("Could not load transcript preview".into()),
        }
    }

    // ---- rows -----------------------------------------------------------------------------

    fn session_lines(
        &self,
        row: &SessionRow,
        selected: bool,
        expanded: bool,
        zebra: bool,
        width: u16,
    ) -> Vec<Line<'static>> {
        match self.density {
            Density::Comfortable => self.comfortable_lines(row, selected, expanded, zebra, width),
            Density::Dense => self.dense_lines(row, selected, expanded, zebra, width),
        }
    }

    fn date_text(&self, row: &SessionRow) -> String {
        match self.sort {
            SortKey::Created => relative_time(self.now, row.created),
            SortKey::Updated => relative_time(self.now, row.updated.or(row.created)),
        }
    }

    fn dense_lines(
        &self,
        row: &SessionRow,
        selected: bool,
        expanded: bool,
        zebra: bool,
        width: u16,
    ) -> Vec<Line<'static>> {
        let marker = selection_marker(selected, expanded);
        let avail = (width as usize).saturating_sub(marker.width());
        let title_w = avail.saturating_sub(DATE_WIDTH);
        let title_text = dense_column_text(row.display_preview(), title_w);
        let title: Span<'static> = if selected {
            Span::styled(title_text, selected_session_style())
        } else {
            title_text.into()
        };
        let mut line = Line::from(vec![
            marker,
            dense_column_text(&self.date_text(row), DATE_WIDTH).dim(),
            title,
        ]);
        let style = if selected {
            Some(dense_selected_style())
        } else if zebra {
            Some(dense_row_background_style(false))
        } else {
            None
        };
        if let Some(st) = style {
            line = fill_line(line, st, width);
        }
        let mut out = vec![line];
        if expanded {
            out.extend(self.preview_lines(row, width));
        }
        out
    }

    fn comfortable_lines(
        &self,
        row: &SessionRow,
        selected: bool,
        expanded: bool,
        zebra: bool,
        width: u16,
    ) -> Vec<Line<'static>> {
        let marker = selection_marker(selected, expanded);
        let title = truncate_text(row.display_preview(), width.saturating_sub(2) as usize);
        let title: Span<'static> = if selected {
            Span::styled(title, selected_session_style())
        } else {
            title.into()
        };
        let mut lines = vec![Line::from(vec![marker, title])];
        let style = if selected {
            Some(dense_selected_style())
        } else if zebra {
            Some(dense_row_background_style(false))
        } else {
            None
        };
        if let Some(st) = style {
            lines = lines.into_iter().map(|l| fill_line(l, st, width)).collect();
        }
        if expanded {
            lines.extend(self.preview_lines(row, width));
            return lines;
        }
        let date = self.date_text(row);
        let mut parts = vec![FooterPart::Date(date)];
        if self.filter_mode == FilterMode::All {
            parts.push(FooterPart::Cwd(
                (!row.cwd.is_empty()).then(|| row.cwd.clone()),
            ));
        }
        parts.push(FooterPart::Branch(None));
        let meta = pack_footer_parts(parts, width);
        match style {
            Some(st) => lines.extend(meta.into_iter().map(|l| fill_line(l, st, width))),
            None => lines.extend(meta),
        }
        lines
    }

    fn preview_lines(&self, row: &SessionRow, width: u16) -> Vec<Line<'static>> {
        let mut out = self.expanded_details(row, width);
        let preview = self
            .load_items(&row.id)
            .map(|items| preview_from_items(&items));
        out.extend(match preview {
            None => vec![Line::from(vec![
                "  │ ".dim(),
                Span::from("Could not load transcript preview")
                    .italic()
                    .red(),
            ])],
            Some(lines) => conversation_preview_lines(&lines, width),
        });
        out
    }

    fn expanded_details(&self, row: &SessionRow, width: u16) -> Vec<Line<'static>> {
        let dir = if row.cwd.is_empty() {
            "-".to_string()
        } else {
            row.cwd.clone()
        };
        let session = match &row.name {
            Some(n) => format!("{n} ({})", row.id),
            None => row.id.clone(),
        };
        vec![
            expanded_detail_line("Session:", &session, width),
            expanded_time_detail_line("Created:", self.now, row.created, width),
            expanded_time_detail_line("Updated:", self.now, row.updated.or(row.created), width),
            expanded_detail_line("Directory:", &dir, width),
            expanded_detail_line("Branch:", &format!("{BRANCH_ICON} no branch"), width),
            Line::from("  │".dim()),
            Line::from(vec!["  │ ".dim(), "Conversation:".dim()]),
        ]
    }

    // ---- chrome ---------------------------------------------------------------------------

    fn search_line(&self, width: u16) -> Line<'static> {
        if let Some(e) = &self.inline_error {
            return Line::from(Span::from(e.clone()).red());
        }
        let search: Span<'static> = if self.query.is_empty() {
            "Type to search".dim()
        } else {
            format!("Search: {}", self.query).into()
        };
        let mut toolbar = self.toolbar_line(false);
        if line_width(&toolbar) as u16 > width.saturating_sub(2) {
            toolbar = self.toolbar_line(true);
        }
        let sw = width_of(&search.content);
        let tw = line_width(&toolbar);
        let spacer = width.saturating_sub((sw + tw) as u16).max(2) as usize;
        let avail = width
            .saturating_sub(tw as u16)
            .saturating_sub(spacer as u16) as usize;
        let search = if sw > avail {
            let t = truncate_text(&search.content, avail);
            if self.query.is_empty() {
                t.dim()
            } else {
                t.into()
            }
        } else {
            search
        };
        let mut spans = vec![search, " ".repeat(spacer).into()];
        spans.extend(toolbar.spans);
        Line::from(spans)
    }

    fn toolbar_line(&self, compact: bool) -> Line<'static> {
        let mut spans = self.filter_spans(compact);
        spans.push("   ".dim());
        spans.extend(self.sort_spans(compact));
        Line::from(spans)
    }

    fn sort_spans(&self, compact: bool) -> Vec<Span<'static>> {
        let focused = self.focus == Focus::Sort;
        if compact {
            let label = match self.sort {
                SortKey::Updated => "Updated",
                SortKey::Created => "Created",
            };
            return vec!["Sort:".dim(), toolbar_value(label, true, focused)];
        }
        vec![
            "Sort: ".dim(),
            toolbar_value("Updated", self.sort == SortKey::Updated, focused),
            toolbar_value("Created", self.sort == SortKey::Created, focused),
        ]
    }

    fn filter_spans(&self, compact: bool) -> Vec<Span<'static>> {
        let focused = self.focus == Focus::Filter;
        if compact || self.filter_cwd.is_none() {
            let label = match self.filter_mode {
                FilterMode::Cwd => "Cwd",
                FilterMode::All => "All",
            };
            return vec!["Filter:".dim(), toolbar_value(label, true, focused)];
        }
        vec![
            "Filter: ".dim(),
            toolbar_value("Cwd", self.filter_mode == FilterMode::Cwd, focused),
            toolbar_value("All", self.filter_mode == FilterMode::All, focused),
        ]
    }

    fn empty_state_line(&self) -> Line<'static> {
        if !self.query.is_empty() {
            return Line::from(Span::from("No results for your search").italic().dim());
        }
        Line::from(Span::from("No sessions yet").italic().dim())
    }

    fn render_list(&self, area: Rect, buf: &mut Buffer) {
        if area.height == 0 {
            return;
        }
        if self.rows.is_empty() {
            put(buf, area, area.y, &self.empty_state_line());
            return;
        }
        let above = self.has_more_above();
        let below = self.has_more_below(area.height as usize);
        let top = area.y + u16::from(above);
        let bottom = area.bottom() - u16::from(below);
        if above {
            put(buf, area, area.y, &Line::from("↑ more".dim()));
        }
        let start = self.scroll_top.min(self.rows.len() - 1);
        let mut y = top;
        for (off, row) in self.rows[start..].iter().enumerate() {
            if y >= bottom {
                break;
            }
            let idx = start + off;
            let lines = self.session_lines(
                row,
                idx == self.selected,
                self.is_expanded(idx),
                idx.is_multiple_of(2),
                area.width,
            );
            for l in lines {
                if y >= bottom {
                    break;
                }
                put(buf, area, y, &l);
                y += 1;
            }
            if self.density == Density::Comfortable && y < bottom && idx + 1 < self.rows.len() {
                y += 1;
            }
        }
        if below {
            put(buf, area, area.bottom() - 1, &Line::from("↓ more".dim()));
        }
    }

    fn render_footer(&self, area: Rect, buf: &mut Buffer, list_height: u16) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        put(
            buf,
            area,
            area.y,
            &Line::from("─".repeat(area.width as usize).dim()),
        );
        let label = self.progress_label(list_height, area.width);
        let lw = width_of(&label) as u16;
        if lw < area.width {
            let r = Rect::new(area.x + area.width - lw - 1, area.y, lw, 1);
            put(buf, r, area.y, &Line::from(label.dim()));
        }
        let lines = self.footer_hint_lines(area.width);
        for (i, l) in lines.into_iter().enumerate() {
            let y = area.y + 1 + i as u16;
            if y >= area.bottom() {
                break;
            }
            put(buf, area, y, &l);
        }
        let y = area.y + 3;
        if y < area.bottom() {
            let t = truncate_text(LIST_NOTE, (area.width as usize).saturating_sub(1));
            put(buf, area, y, &Line::from(vec![" ".dim(), t.dim()]));
        }
    }

    fn footer_hint_lines(&self, width: u16) -> Vec<Line<'static>> {
        let esc = if self.query.is_empty() {
            if self.startup {
                ("start new", "new")
            } else {
                ("exit", "exit")
            }
        } else {
            ("clear search", "clear")
        };
        let ctrl_c = if self.startup { "quit" } else { "exit" };
        let (dl, dc) = match self.density {
            Density::Comfortable => ("dense view", "dense"),
            Density::Dense => ("comfortable view", "comfy"),
        };
        let h = |k: &str, w: &str, c: &str, p: u8| FooterHint {
            key: k.into(),
            wide: w.into(),
            compact: c.into(),
            priority: p,
        };
        let first = [
            h("enter", "resume", "resume", 0),
            h("esc", esc.0, esc.1, 1),
            h("ctrl+c", ctrl_c, ctrl_c, 2),
            h("tab", "focus sort/filter", "focus", 7),
            h("←/→", "change option", "option", 8),
        ];
        let second = [
            h("ctrl+o", dl, dc, 3),
            h("ctrl+t", "transcript", "preview", 4),
            h("ctrl+e", "expand", "exp", 6),
            h("↑/↓", "browse", "browse", 5),
        ];
        vec![
            hint_line_for_row(&first, width),
            hint_line_for_row(&second, width),
        ]
    }

    // ---- keys -----------------------------------------------------------------------------

    fn page_rows(&self) -> usize {
        self.view_rows.unwrap_or(10).max(1)
    }

    fn key_list(&mut self, key: KeyEvent) -> OverlayResult {
        self.inline_error = None;
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        let plain = matches!(key.code, KeyCode::Char(_)) && !ctrl && !alt;
        match key.code {
            KeyCode::Char('c') if ctrl => {
                return if self.startup {
                    OverlayResult::CloseWith(AppAction::Quit)
                } else {
                    OverlayResult::Close
                };
            }
            KeyCode::Esc => {
                if self.query.is_empty() {
                    return if self.startup {
                        OverlayResult::CloseWith(AppAction::StartFresh)
                    } else {
                        OverlayResult::Close
                    };
                }
                self.clear_query_preserving_selection();
            }
            KeyCode::Char('t') if ctrl => self.open_transcript(),
            KeyCode::Char('e') if ctrl => self.toggle_expansion(),
            KeyCode::Char('o') if ctrl => self.toggle_density(),
            KeyCode::Enter => {
                if let Some(r) = self.rows.get(self.selected) {
                    return OverlayResult::CloseWith(AppAction::LoadSession(r.id.clone()));
                }
            }
            KeyCode::Up => self.move_to(self.selected.saturating_sub(1)),
            KeyCode::Char('p' | 'k') if ctrl => self.move_to(self.selected.saturating_sub(1)),
            KeyCode::Down => self.move_to(self.selected + 1),
            KeyCode::Char('n' | 'j') if ctrl => self.move_to(self.selected + 1),
            KeyCode::PageUp => self.move_to(self.selected.saturating_sub(self.page_rows())),
            KeyCode::Char('b') if ctrl => {
                self.move_to(self.selected.saturating_sub(self.page_rows()))
            }
            KeyCode::PageDown => self.move_to(self.selected + self.page_rows()),
            KeyCode::Char('f') if ctrl => self.move_to(self.selected + self.page_rows()),
            KeyCode::Home => self.move_to(0),
            KeyCode::End => self.move_to(usize::MAX),
            KeyCode::Tab | KeyCode::BackTab => {
                self.focus = match self.focus {
                    Focus::Filter => Focus::Sort,
                    Focus::Sort => Focus::Filter,
                };
            }
            KeyCode::Left | KeyCode::Right => self.change_focused(),
            KeyCode::Char('h' | 'l') if ctrl => self.change_focused(),
            KeyCode::Backspace => {
                let mut q = self.query.clone();
                q.pop();
                self.set_query(q);
            }
            KeyCode::Char(c) if plain => {
                let mut q = self.query.clone();
                q.push(c);
                self.set_query(q);
            }
            _ => {}
        }
        OverlayResult::Pending
    }

    fn change_focused(&mut self) {
        match self.focus {
            Focus::Filter => self.toggle_filter(),
            Focus::Sort => self.toggle_sort(),
        }
    }

    fn move_to(&mut self, target: usize) {
        if self.rows.is_empty() {
            return;
        }
        self.selected = target.min(self.rows.len() - 1);
        self.ensure_selected_visible();
    }
}

impl Overlay for ResumePicker {
    fn render(&mut self, area: Rect, buf: &mut Buffer) {
        if area.is_empty() {
            return;
        }
        if let Some(pg) = &mut self.pager {
            pg.overlay.sync_cells(&pg.cells, &[], area.width);
            pg.overlay.render(area, buf);
            return;
        }
        let list_h = area.height.saturating_sub(CHROME_HEIGHT);
        let list_w = area.width.saturating_sub(LIST_INSET);
        // The list sizes itself from the last frame; the first one settles the scroll position.
        if self.view_rows != Some(list_h as usize) || self.view_width != Some(list_w) {
            self.view_rows = Some(list_h as usize).filter(|r| *r > 0);
            self.view_width = Some(list_w);
            self.ensure_selected_visible();
        }
        let chrome = |y: u16| {
            Rect::new(
                area.x + 1,
                area.y + y,
                area.width.saturating_sub(2),
                1.min(area.height.saturating_sub(y)),
            )
        };
        let title = if palette().light_bg() {
            "Resume a previous session"
                .bold()
                .fg(palette().best_color((0, 100, 0)))
        } else {
            "Resume a previous session".bold().cyan()
        };
        let r = chrome(0);
        put(buf, r, r.y, &Line::from(title));
        let r = chrome(2);
        put(buf, r, r.y, &self.search_line(r.width));
        let list = Rect::new(area.x + 2, area.y + 4, list_w, list_h);
        self.render_list(list, buf);
        let footer = Rect::new(
            area.x,
            area.y + 4 + list_h,
            area.width,
            4.min(area.height.saturating_sub(4 + list_h)),
        );
        self.render_footer(footer, buf, list_h);
    }

    fn handle_key(&mut self, key: KeyEvent) -> OverlayResult {
        if let Some(pg) = &mut self.pager {
            // Anything the pager asks for beyond closing (editing a message) has no meaning here.
            if !matches!(pg.overlay.handle_key(key), OverlayResult::Pending) {
                self.pager = None;
            }
            return OverlayResult::Pending;
        }
        self.key_list(key)
    }

    fn handle_paste(&mut self, text: &str) {
        if self.pager.is_some() {
            return;
        }
        let pasted = text.split_whitespace().collect::<Vec<_>>().join(" ");
        if pasted.is_empty() {
            return;
        }
        let mut q = self.query.clone();
        if !q.is_empty() && !q.ends_with(char::is_whitespace) {
            q.push(' ');
        }
        q.push_str(&pasted);
        self.set_query(q);
    }

    fn animation_interval(&self) -> Option<Duration> {
        None
    }
}

// ---- pieces --------------------------------------------------------------------------------

fn put(buf: &mut Buffer, clip: Rect, y: u16, line: &Line<'_>) {
    tuikit::paint::put_line(buf, clip.x, y, line, clip);
}

/// Codex's `truncate_text`: counts graphemes and ends with `...`.
pub fn truncate_text(text: &str, max: usize) -> String {
    let mut g = text.grapheme_indices(true);
    let Some((byte_index, _)) = g.nth(max) else {
        return text.to_string();
    };
    if max >= 3 {
        match text.grapheme_indices(true).nth(max - 3) {
            Some((b, _)) => format!("{}...", &text[..b]),
            None => text.to_string(),
        }
    } else {
        text[..byte_index].to_string()
    }
}

fn dense_column_text(text: &str, width: usize) -> String {
    let t = truncate_text(text, width.saturating_sub(1));
    let pad = width.saturating_sub(width_of(&t));
    format!("{t}{}", " ".repeat(pad))
}

fn selected_session_style() -> Style {
    Style::default().fg(palette().picker_highlight())
}

fn dense_row_background_style(selected: bool) -> Style {
    let p = palette();
    let c = if selected {
        p.overlay_bg(0.12, 0.12)
    } else {
        p.overlay_bg(0.055, 0.04)
    };
    c.map_or_else(Style::default, |c| Style::default().bg(c))
}

fn dense_selected_style() -> Style {
    selected_session_style().patch(dense_row_background_style(true))
}

fn selection_marker(selected: bool, expanded: bool) -> Span<'static> {
    match (selected, expanded) {
        (true, true) => Span::styled("⌄ ", selected_session_style().bold()),
        (true, false) => Span::styled("❯ ", selected_session_style().bold()),
        (false, _) => "  ".into(),
    }
}

/// Pad a row to `width` and put `style` under all of it.
fn fill_line(mut line: Line<'static>, style: Style, width: u16) -> Line<'static> {
    let pad = (width as usize).saturating_sub(line_width(&line));
    if pad > 0 {
        line.spans.push(Span::styled(" ".repeat(pad), style));
    }
    line.style = line.style.patch(style);
    line
}

fn toolbar_value(label: &'static str, active: bool, focused: bool) -> Span<'static> {
    if active {
        let v = format!("[{label}]");
        if focused { v.magenta() } else { v.into() }
    } else {
        format!(" {label} ").dim()
    }
}

#[derive(Clone)]
struct FooterHint {
    key: String,
    wide: String,
    compact: String,
    priority: u8,
}

#[derive(Clone, Copy)]
enum LabelMode {
    Wide,
    Compact,
    KeyOnly,
}

fn hint_key_style() -> Style {
    if palette().light_bg() {
        Style::default().fg(Color::Black)
    } else {
        Style::default()
    }
}

fn hint_label_style() -> Style {
    if palette().light_bg() {
        Style::default().fg(Color::DarkGray)
    } else {
        Style::default().dim()
    }
}

fn hints_width(hints: &[&FooterHint], mode: LabelMode) -> usize {
    1 + hints
        .iter()
        .enumerate()
        .map(|(i, h)| {
            let label = match mode {
                LabelMode::Wide => 1 + width_of(&h.wide),
                LabelMode::Compact => 1 + width_of(&h.compact),
                LabelMode::KeyOnly => 0,
            };
            let w = width_of(&h.key) + label;
            if i == 0 { w } else { w + FOOTER_HINT_GAP }
        })
        .sum::<usize>()
}

fn fit_hints(hints: &[&FooterHint], mode: LabelMode, width: u16) -> Option<Line<'static>> {
    if hints_width(hints, mode) > width as usize {
        return None;
    }
    let mut spans = vec![Span::styled(" ", hint_label_style())];
    for (i, h) in hints.iter().enumerate() {
        if i > 0 {
            spans.push(Span::styled(
                " ".repeat(FOOTER_HINT_GAP),
                hint_label_style(),
            ));
        }
        spans.push(Span::styled(h.key.clone(), hint_key_style()));
        let label = match mode {
            LabelMode::Wide => Some(h.wide.as_str()),
            LabelMode::Compact => Some(h.compact.as_str()),
            LabelMode::KeyOnly => None,
        };
        if let Some(l) = label {
            spans.push(Span::styled(" ", hint_label_style()));
            spans.push(Span::styled(l.to_string(), hint_label_style()));
        }
    }
    Some(Line::from(spans))
}

fn hint_line_for_row(hints: &[FooterHint], width: u16) -> Line<'static> {
    let refs: Vec<&FooterHint> = hints.iter().collect();
    if width >= FOOTER_COMPACT_BREAKPOINT {
        if let Some(l) = fit_hints(&refs, LabelMode::Wide, width) {
            return l;
        }
    }
    if let Some(l) = fit_hints(&refs, LabelMode::Compact, width) {
        return l;
    }
    if let Some(l) = fit_hints(&refs, LabelMode::KeyOnly, width) {
        return l;
    }
    let mut order: Vec<usize> = (0..hints.len()).collect();
    order.sort_by_key(|i| hints[*i].priority);
    for keep in (1..=order.len()).rev() {
        let mut idx = order[..keep].to_vec();
        idx.sort_unstable();
        let cand: Vec<&FooterHint> = idx.iter().map(|i| &hints[*i]).collect();
        if let Some(l) = fit_hints(&cand, LabelMode::KeyOnly, width) {
            return l;
        }
    }
    Line::default()
}

// ---- comfortable metadata row --------------------------------------------------------------

enum FooterPart {
    Date(String),
    Branch(Option<String>),
    Cwd(Option<String>),
}

impl FooterPart {
    fn text(&self) -> &str {
        match self {
            FooterPart::Date(t) => t,
            FooterPart::Branch(Some(t)) | FooterPart::Cwd(Some(t)) => t,
            FooterPart::Branch(None) => "no branch",
            FooterPart::Cwd(None) => "no cwd",
        }
    }

    fn prefix(&self) -> Option<&'static str> {
        match self {
            FooterPart::Date(_) => None,
            FooterPart::Branch(_) => Some(BRANCH_ICON),
            FooterPart::Cwd(_) => Some(CWD_ICON),
        }
    }
}

fn cwd_column_width(width: usize) -> usize {
    let avail = width.saturating_sub(META_INDENT + DATE_WIDTH + 2 * FIELD_GAP);
    (avail / 2).clamp(MIN_CWD_WIDTH, MAX_CWD_WIDTH)
}

fn part_width(p: &FooterPart, padded: bool, cwd_w: usize) -> usize {
    let pw = p.prefix().map_or(0, width_of);
    let gap = usize::from(p.prefix().is_some() && !p.text().is_empty());
    let actual = pw + gap + width_of(p.text());
    match p {
        FooterPart::Date(_) if padded => DATE_WIDTH.max(actual),
        FooterPart::Cwd(_) if padded => cwd_w,
        _ => actual,
    }
}

fn parts_width(parts: &[FooterPart], cwd_w: usize) -> usize {
    META_INDENT
        + parts
            .iter()
            .enumerate()
            .map(|(i, p)| part_width(p, i + 1 < parts.len(), cwd_w))
            .sum::<usize>()
}

fn pack_footer_parts(parts: Vec<FooterPart>, width: u16) -> Vec<Line<'static>> {
    let avail = width as usize;
    if avail <= META_INDENT {
        return Vec::new();
    }
    let cwd_w = cwd_column_width(avail);
    if parts_width(&parts, cwd_w) <= avail {
        return vec![footer_line(parts, avail, cwd_w)];
    }
    let mut lines = Vec::new();
    let mut current: Vec<FooterPart> = Vec::new();
    for part in parts {
        let mut cand = std::mem::take(&mut current);
        cand.push(part);
        if cand.len() > 1 && parts_width(&cand, cwd_w) > avail {
            let last = cand.pop().expect("candidate has two parts");
            lines.push(footer_line(cand, avail, cwd_w));
            cand = vec![last];
        }
        current = cand;
    }
    if !current.is_empty() {
        lines.push(footer_line(current, avail, cwd_w));
    }
    lines
}

fn footer_line(parts: Vec<FooterPart>, width: usize, cwd_w: usize) -> Line<'static> {
    let mut spans: Vec<Span<'static>> = vec!["  ".into()];
    let mut remaining = width.saturating_sub(META_INDENT);
    let count = parts.len();
    for (i, part) in parts.into_iter().enumerate() {
        if i > 0 {
            let g = FIELD_GAP.min(remaining);
            if g > 0 {
                spans.push(" ".repeat(g).dim());
                remaining -= g;
            }
        }
        let padded = i + 1 < count;
        let target = match part {
            FooterPart::Date(_) if padded => Some(DATE_WIDTH),
            FooterPart::Cwd(_) if padded => Some(cwd_w),
            _ => None,
        };
        let used = push_footer_part(&mut spans, part, target, remaining);
        remaining = remaining.saturating_sub(used);
        if let Some(t) = target {
            let pad = t.saturating_sub(used);
            if pad > 0 {
                spans.push(" ".repeat(pad).dim());
                remaining = remaining.saturating_sub(pad);
            }
        }
    }
    Line::from(spans)
}

fn push_footer_part(
    spans: &mut Vec<Span<'static>>,
    part: FooterPart,
    target: Option<usize>,
    available: usize,
) -> usize {
    let text = part.text().to_string();
    let Some(prefix) = part.prefix() else {
        let t = truncate_text(&text, available);
        let w = width_of(&t);
        spans.push(t.dim());
        return w;
    };
    let pw = width_of(prefix);
    if available <= pw {
        let p = truncate_text(prefix, available);
        let w = width_of(&p);
        spans.push(p.dim());
        return w;
    }
    spans.push(prefix.to_string().dim());
    let mut used = pw;
    if !text.is_empty() && used < available {
        spans.push(" ".dim());
        used += 1;
    }
    let text_w = target
        .unwrap_or(available)
        .saturating_sub(used)
        .min(available.saturating_sub(used));
    let t = truncate_text(&text, text_w);
    let rendered = width_of(&t);
    match part {
        FooterPart::Branch(None) | FooterPart::Cwd(None) => spans.push(t.dim().italic()),
        _ => spans.push(t.dim()),
    }
    used + rendered
}

// ---- expanded row --------------------------------------------------------------------------

fn expanded_detail_line(label: &str, value: &str, width: u16) -> Line<'static> {
    const LABEL_WIDTH: usize = 10;
    let value_w = (width as usize).saturating_sub(4 + LABEL_WIDTH + 2).max(1);
    Line::from(vec![
        "  │ ".dim(),
        format!("{label:<LABEL_WIDTH$}").dim(),
        "  ".dim(),
        truncate_text(value, value_w).into(),
    ])
}

fn expanded_time_detail_line(label: &str, now: i64, ts: Option<i64>, width: u16) -> Line<'static> {
    let Some(ts) = ts else {
        return expanded_detail_line(label, "-", width);
    };
    let v = format!("{} · {}", relative_time_long(now, ts), format_timestamp(ts));
    expanded_detail_line(label, &v, width)
}

fn conversation_style(speaker: Speaker) -> Style {
    let light = palette().light_bg();
    match (speaker, light) {
        (Speaker::User, false) => Style::default().fg(Color::Gray).italic(),
        (Speaker::User, true) => Style::default().fg(Color::DarkGray).italic(),
        (Speaker::Assistant, false) => Style::default().fg(Color::DarkGray),
        (Speaker::Assistant, true) => Style::default().fg(Color::Gray),
    }
}

fn conversation_preview_lines(lines: &[PreviewLine], width: u16) -> Vec<Line<'static>> {
    if lines.is_empty() {
        return vec![Line::from(vec![
            "  └ ".dim(),
            Span::from("No transcript preview available").italic().dim(),
        ])];
    }
    let content_w = width.saturating_sub(4).max(1) as usize;
    let mut rendered: Vec<(Style, Line<'static>)> = Vec::new();
    for l in lines {
        let st = conversation_style(l.speaker);
        let base = Line::from(Span::styled(l.text.clone(), st));
        for w in adaptive_wrap_line(&base, &WrapOpts::new(content_w)) {
            rendered.push((st, w));
        }
    }
    let n = rendered.len();
    rendered
        .into_iter()
        .enumerate()
        .map(|(i, (st, line))| {
            let prefix = if i + 1 == n { "  └ " } else { "  │ " };
            let mut spans = vec![Span::styled(
                prefix,
                Style {
                    fg: st.fg,
                    ..Style::default()
                },
            )];
            spans.extend(line.spans);
            Line::from(spans)
        })
        .collect()
}

/// The last six non-empty lines of the conversation, oldest first.
fn preview_from_items(items: &[HistoryItem]) -> Vec<PreviewLine> {
    const MAX: usize = 6;
    let mut out: Vec<PreviewLine> = Vec::new();
    for it in items.iter().rev() {
        let (speaker, text) = match it {
            HistoryItem::User(t) => (Speaker::User, t),
            HistoryItem::Assistant(t) => (Speaker::Assistant, t),
            _ => continue,
        };
        let lines: Vec<&str> = text
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .collect();
        for l in lines.into_iter().rev() {
            out.push(PreviewLine {
                speaker,
                text: l.to_string(),
            });
        }
        if out.len() >= MAX {
            break;
        }
    }
    out.truncate(MAX);
    out.reverse();
    out
}

// ---- session files -------------------------------------------------------------------------

fn same_path(a: &str, b: &str) -> bool {
    a.trim_end_matches('/') == b.trim_end_matches('/')
}

/// `timestamp` of the first line of a wizard session file.
fn read_created(path: &std::path::Path) -> Option<i64> {
    use std::io::BufReader;
    let f = std::fs::File::open(path).ok()?;
    let mut line = String::new();
    BufReader::new(f).take_line(&mut line)?;
    let v: serde_json::Value = serde_json::from_str(line.trim()).ok()?;
    parse_rfc3339(v.get("timestamp")?.as_str()?)
}

trait TakeLine {
    fn take_line(&mut self, out: &mut String) -> Option<()>;
}

impl<R: std::io::BufRead> TakeLine for R {
    fn take_line(&mut self, out: &mut String) -> Option<()> {
        // The header line is short; a bounded read keeps a damaged file from being slurped.
        let mut buf = Vec::new();
        let mut taken = 0usize;
        loop {
            let chunk = self.fill_buf().ok()?;
            if chunk.is_empty() {
                break;
            }
            let end = chunk.iter().position(|b| *b == b'\n');
            let n = end.map_or(chunk.len(), |e| e + 1);
            buf.extend_from_slice(&chunk[..n]);
            self.consume(n);
            taken += n;
            if end.is_some() || taken > 16 * 1024 {
                break;
            }
        }
        *out = String::from_utf8_lossy(&buf).to_string();
        Some(())
    }
}

/// The user and assistant text of a session file as history items. Tool traffic and hook
/// notes are left out: the preview and the transcript view show the conversation.
pub fn read_history(path: &std::path::Path) -> Option<Vec<HistoryItem>> {
    let meta = std::fs::metadata(path).ok()?;
    if meta.len() > 32 * 1024 * 1024 {
        return None;
    }
    let text = std::fs::read_to_string(path).ok()?;
    let mut out = Vec::new();
    for line in text.lines() {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let Some(msg) = v.get("message") else {
            continue;
        };
        if v.get("system_note").is_some() {
            continue;
        }
        let role = msg.get("role").and_then(|r| r.as_str()).unwrap_or("");
        let body: String = match msg.get("content") {
            Some(serde_json::Value::String(s)) => s.clone(),
            Some(serde_json::Value::Array(parts)) => parts
                .iter()
                .filter(|p| p.get("type").and_then(|t| t.as_str()) == Some("text"))
                .filter_map(|p| p.get("text").and_then(|t| t.as_str()))
                .collect::<Vec<_>>()
                .join("\n"),
            _ => String::new(),
        };
        if body.trim().is_empty() {
            continue;
        }
        match role {
            "user" => out.push(HistoryItem::User(body)),
            "assistant" => out.push(HistoryItem::Assistant(body)),
            _ => {}
        }
    }
    Some(out)
}

// ---- time ----------------------------------------------------------------------------------

pub fn relative_time(now: i64, ts: Option<i64>) -> String {
    let Some(ts) = ts else {
        return "-".into();
    };
    let secs = (now - ts).max(0);
    if secs == 0 {
        return "now".into();
    }
    if secs < 60 {
        return format!("{secs}s ago");
    }
    let m = secs / 60;
    if m < 60 {
        return format!("{m}m ago");
    }
    let h = m / 60;
    if h < 24 {
        return format!("{h}h ago");
    }
    format!("{}d ago", h / 24)
}

fn plural(v: i64, unit: &str) -> String {
    if v == 1 {
        format!("1 {unit} ago")
    } else {
        format!("{v} {unit}s ago")
    }
}

fn relative_time_long(now: i64, ts: i64) -> String {
    let secs = (now - ts).max(0);
    if secs == 0 {
        return "now".into();
    }
    if secs < 60 {
        return plural(secs, "second");
    }
    let m = secs / 60;
    if m < 60 {
        return plural(m, "minute");
    }
    let h = m / 60;
    if h < 24 {
        return plural(h, "hour");
    }
    plural(h / 24, "day")
}

/// Days since 1970-01-01 to (year, month, day), proleptic Gregorian.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let mp = if m > 2 { m - 3 } else { m + 9 } as i64;
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// `2026-10-05T23:17:23.530Z` (or a `+hh:mm` offset) as unix seconds.
pub fn parse_rfc3339(s: &str) -> Option<i64> {
    let (date, rest) = s.split_once('T')?;
    let mut dp = date.split('-');
    let y: i64 = dp.next()?.parse().ok()?;
    let m: u32 = dp.next()?.parse().ok()?;
    let d: u32 = dp.next()?.parse().ok()?;
    let time_end = rest.find(['Z', '+', '-']).unwrap_or(rest.len());
    let (time, zone) = rest.split_at(time_end);
    let mut tp = time.split(':');
    let hh: i64 = tp.next()?.parse().ok()?;
    let mm: i64 = tp.next()?.parse().ok()?;
    let ss: f64 = tp.next().unwrap_or("0").parse().ok()?;
    let mut secs = days_from_civil(y, m, d) * 86_400 + hh * 3600 + mm * 60 + ss.floor() as i64;
    if let Some(sign) = zone.chars().next().filter(|c| *c == '+' || *c == '-') {
        let (oh, om) = zone[1..].split_once(':')?;
        let off = oh.parse::<i64>().ok()? * 3600 + om.parse::<i64>().ok()? * 60;
        secs -= if sign == '+' { off } else { -off };
    }
    Some(secs)
}

pub fn format_timestamp(ts: i64) -> String {
    let days = ts.div_euclid(86_400);
    let rem = ts.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02}:{:02}",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::style::{ColorLevel, Palette, set_palette};

    fn dark() {
        set_palette(Palette::new(
            Some((230, 230, 230)),
            Some((0, 0, 0)),
            ColorLevel::TrueColor,
        ));
    }

    fn info(id: &str, title: &str, cwd: &str, updated: i64) -> SessionInfo {
        SessionInfo {
            id: id.into(),
            title: title.into(),
            cwd: cwd.into(),
            updated,
        }
    }

    fn opts(now: i64) -> ResumeOpts {
        ResumeOpts {
            startup: true,
            cwd: "/home/proj".into(),
            now,
            sessions_dir: None,
            density: Density::Dense,
            names: Default::default(),
            config_path: None,
        }
    }

    fn five() -> Vec<SessionInfo> {
        [940, 949, 963, 976, 989]
            .iter()
            .enumerate()
            .map(|(i, t)| {
                info(
                    &format!("id{}", i + 1),
                    &format!("session {} fake:text", i + 1),
                    "/home/proj",
                    *t,
                )
            })
            .collect()
    }

    fn render(p: &mut ResumePicker, w: u16, h: u16) -> (Buffer, Vec<String>) {
        let area = Rect::new(0, 0, w, h);
        let mut buf = Buffer::empty(area);
        p.render(area, &mut buf);
        let rows = (0..h)
            .map(|y| {
                (0..w)
                    .map(|x| buf[(x, y)].symbol().to_string())
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect();
        (buf, rows)
    }

    #[test]
    fn dense_rows_match_the_capture() {
        dark();
        let mut p = ResumePicker::new(&five(), opts(1000));
        let (_, rows) = render(&mut p, 120, 36);
        assert_eq!(rows[0], " Resume a previous session");
        assert!(rows[2].starts_with(" Type to search"));
        assert!(rows[2].ends_with("Filter: [Cwd] All    Sort: [Updated] Created"));
        assert_eq!(rows[4], "  ❯ 11s ago     session 5 fake:text");
        assert_eq!(rows[5], "    24s ago     session 4 fake:text");
        assert_eq!(rows[8], "    1m ago      session 1 fake:text");
        assert_eq!(rows[32].chars().filter(|c| *c == '─').count(), 106);
        assert!(rows[32].contains(" 1 / 5 · 100% "));
        assert_eq!(
            rows[33],
            " enter resume   esc start new   ctrl+c quit   tab focus sort/filter   ←/→ change option"
        );
        assert_eq!(
            rows[34],
            " ctrl+o comfortable view   ctrl+t transcript   ctrl+e expand   ↑/↓ browse"
        );
    }

    #[test]
    fn selected_and_zebra_rows_carry_the_overlay_backgrounds() {
        dark();
        let mut p = ResumePicker::new(&five(), opts(1000));
        let (buf, _) = render(&mut p, 120, 36);
        // First row selected: yellow on 12 percent white; row 2 (index 2) zebra at 5.5 percent.
        assert_eq!(buf[(2, 4)].bg, Color::Rgb(30, 30, 30));
        assert_eq!(buf[(117, 4)].bg, Color::Rgb(30, 30, 30));
        assert_eq!(buf[(118, 4)].bg, Color::Reset);
        assert_eq!(buf[(2, 5)].bg, Color::Reset);
        assert_eq!(buf[(2, 6)].bg, Color::Rgb(14, 14, 14));
        assert_eq!(buf[(2, 4)].fg, Color::Yellow);
    }

    #[test]
    fn comfortable_view_has_two_rows_and_a_blank() {
        dark();
        let mut p = ResumePicker::new(&five(), opts(1000));
        p.key_list(KeyEvent::new(KeyCode::Char('o'), KeyModifiers::CONTROL));
        let (_, rows) = render(&mut p, 120, 36);
        assert_eq!(rows[4], "  ❯ session 5 fake:text");
        assert_eq!(
            rows[5],
            format!("    11s ago       {BRANCH_ICON} no branch")
        );
        assert_eq!(rows[6], "");
        assert_eq!(rows[7], "    session 4 fake:text");
        assert!(rows[34].contains("ctrl+o dense view"));
    }

    #[test]
    fn keys_move_filter_and_search() {
        dark();
        let mut list = five();
        list.push(info("other", "elsewhere", "/home/else", 990));
        let mut p = ResumePicker::new(&list, opts(1000));
        assert_eq!(p.row_count(), 5);
        // Right on the Filter control switches to All.
        p.key_list(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE));
        assert_eq!(p.row_count(), 6);
        for c in "else".chars() {
            p.key_list(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
        }
        assert_eq!(p.row_count(), 1);
        let (_, rows) = render(&mut p, 120, 36);
        assert!(rows[2].starts_with(" Search: else"));
        assert!(rows[33].contains("esc clear search"));
        // Esc clears the query first, then asks to start fresh.
        assert_eq!(
            p.key_list(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
            OverlayResult::Pending
        );
        assert_eq!(p.row_count(), 6);
        assert_eq!(
            p.key_list(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
            OverlayResult::CloseWith(AppAction::StartFresh)
        );
    }

    #[test]
    fn enter_loads_the_selected_session_and_down_moves_the_marker() {
        dark();
        let mut p = ResumePicker::new(&five(), opts(1000));
        p.key_list(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        p.key_list(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        let (_, rows) = render(&mut p, 120, 36);
        assert!(rows[6].starts_with("  ❯ "), "{:?}", rows[6]);
        assert!(rows[32].contains(" 3 / 5 · 100% "));
        assert_eq!(
            p.key_list(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            OverlayResult::CloseWith(AppAction::LoadSession("id3".into()))
        );
    }

    #[test]
    fn empty_cwd_filter_says_no_sessions_yet() {
        dark();
        let mut p = ResumePicker::new(&[info("x", "t", "/elsewhere", 5)], opts(10));
        let (_, rows) = render(&mut p, 120, 36);
        assert_eq!(rows[4], "  No sessions yet");
        assert!(rows[32].contains(" 0 / 0 · 100% "));
    }

    #[test]
    fn in_session_footer_says_exit() {
        dark();
        let mut o = opts(10);
        o.startup = false;
        let mut p = ResumePicker::new(&[], o);
        let (_, rows) = render(&mut p, 120, 36);
        assert!(rows[33].contains("esc exit") && rows[33].contains("ctrl+c exit"));
        assert_eq!(
            p.key_list(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            OverlayResult::Close
        );
    }

    #[test]
    fn narrow_footers_follow_codex_fallbacks() {
        dark();
        let mut p = ResumePicker::new(&five(), opts(1000));
        let (_, rows) = render(&mut p, 96, 30);
        assert_eq!(
            rows[27],
            " enter resume   esc new   ctrl+c quit   tab focus   ←/→ option"
        );
        assert_eq!(
            rows[28],
            " ctrl+o comfy   ctrl+t preview   ctrl+e exp   ↑/↓ browse"
        );
    }

    #[test]
    fn ctrl_t_opens_the_transcript_pager_and_q_returns_to_the_list() {
        dark();
        let dir = std::env::temp_dir().join(format!("cxw-resume-pager-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("id5.jsonl"),
            concat!(
                "{\"message\":{\"role\":\"user\",\"content\":[{\"type\":\"text\",\"text\":\"hello there\"}]}}\n",
                "{\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"text\",\"text\":\"hi\"}]}}\n",
            ),
        )
        .unwrap();
        let mut o = opts(1000);
        o.sessions_dir = Some(dir.clone());
        let mut p = ResumePicker::new(&five(), o);
        p.handle_key(KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL));
        let (_, rows) = render(&mut p, 120, 36);
        assert!(
            rows[0].starts_with("/ T R A N S C R I P T"),
            "{:?}",
            rows[0]
        );
        assert!(rows.iter().any(|r| r.contains("› hello there")));
        assert_eq!(
            rows[33],
            " ↑/↓ to scroll   pgup/pgdn to page   home/end to jump"
        );
        assert_eq!(rows[34], " q to quit   esc to edit prev");
        p.handle_key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE));
        let (_, rows) = render(&mut p, 120, 36);
        assert_eq!(rows[0], " Resume a previous session");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_session_without_a_file_reports_no_preview() {
        dark();
        let mut p = ResumePicker::new(&five(), opts(1000));
        p.key_list(KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL));
        // No sessions dir: the picker says so on the search row and keeps the list.
        let (_, rows) = render(&mut p, 120, 36);
        assert_eq!(rows[2], " Could not load transcript preview");
        assert!(rows[4].starts_with("  ❯ "));
    }

    #[test]
    fn a_renamed_session_shows_its_name_and_is_searchable_by_it() {
        dark();
        let mut o = opts(1000);
        o.names.insert("id3".into(), "my-thread".into());
        let mut p = ResumePicker::new(&five(), o);
        let (_, rows) = render(&mut p, 120, 36);
        assert!(rows.iter().any(|r| r.ends_with("my-thread")));
        assert!(!rows.iter().any(|r| r.contains("session 3 fake:text")));
        for c in "my-th".chars() {
            p.key_list(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
        }
        assert_eq!(p.row_count(), 1);
        p.key_list(KeyEvent::new(KeyCode::Char('e'), KeyModifiers::CONTROL));
        let (_, rows) = render(&mut p, 120, 36);
        assert!(
            rows.iter()
                .any(|r| r.contains("Session:    my-thread (id3)")),
            "{rows:#?}"
        );
    }

    #[test]
    fn truncate_text_matches_codex_snapshots() {
        assert_eq!(
            dense_column_text(
                "Propose session picker redesign with enough title text to exercise truncation",
                34
            ),
            "Propose session picker redesig... "
        );
        assert_eq!(truncate_text("ab", 2), "ab");
        assert_eq!(truncate_text("abcdef", 2), "ab");
    }

    #[test]
    fn rfc3339_and_timestamps_round_trip() {
        let t = parse_rfc3339("2026-10-05T21:37:03.250Z").unwrap();
        assert_eq!(format_timestamp(t), "2026-10-05 21:37:03");
        assert_eq!(parse_rfc3339("2026-10-05T23:37:03+02:00"), Some(t));
        assert_eq!(relative_time(t + 44, Some(t)), "44s ago");
        assert_eq!(relative_time_long(t + 44, t), "44 seconds ago");
        assert_eq!(relative_time_long(t + 3600, t), "1 hour ago");
    }

    #[test]
    fn expanded_row_lists_details_and_a_preview_from_the_file() {
        dark();
        let dir = std::env::temp_dir().join(format!("cxw-resume-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("id1.jsonl"),
            concat!(
                "{\"timestamp\":\"1970-01-01T00:00:00Z\",\"cwd\":\"/home/proj\",\"version\":1}\n",
                "{\"message\":{\"role\":\"user\",\"content\":[{\"type\":\"text\",\"text\":\"Show me the recent transcript\"}]}}\n",
                "{\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"text\",\"text\":\"Here are the last few lines.\"}]}}\n",
            ),
        )
        .unwrap();
        let mut o = opts(1000);
        o.sessions_dir = Some(dir.clone());
        let mut p = ResumePicker::new(&five(), o);
        p.key_list(KeyEvent::new(KeyCode::Char('e'), KeyModifiers::CONTROL));
        // id5 is the first row and has no file: the failure line shows.
        let (_, rows) = render(&mut p, 120, 36);
        assert_eq!(rows[4], "  ⌄ 11s ago     session 5 fake:text");
        assert!(
            rows[5].starts_with("    │ Session:    id5"),
            "{:?}",
            rows[5]
        );
        assert!(
            rows.iter()
                .any(|r| r.contains("Could not load transcript preview"))
        );
        // Move to id1 (the last row), which has a file.
        p.key_list(KeyEvent::new(KeyCode::Char('e'), KeyModifiers::CONTROL));
        p.key_list(KeyEvent::new(KeyCode::End, KeyModifiers::NONE));
        p.key_list(KeyEvent::new(KeyCode::Char('e'), KeyModifiers::CONTROL));
        let (_, rows) = render(&mut p, 120, 36);
        assert!(
            rows.iter()
                .any(|r| r == "    │ Show me the recent transcript"),
            "{rows:#?}"
        );
        assert!(
            rows.iter()
                .any(|r| r == "    └ Here are the last few lines.")
        );
        assert!(
            rows.iter()
                .any(|r| r.starts_with("    │ Created:    ") && r.contains("1970-01-01 00:00:00"))
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn every_size_renders_without_panicking() {
        dark();
        for density in [Density::Dense, Density::Comfortable] {
            let mut o = opts(1000);
            o.density = density;
            let mut p = ResumePicker::new(&five(), o);
            p.key_list(KeyEvent::new(KeyCode::Char('e'), KeyModifiers::CONTROL));
            for w in 0..=130u16 {
                for h in [0u16, 1, 5, 8, 9, 12, 40] {
                    let area = Rect::new(0, 0, w, h);
                    let mut buf = Buffer::empty(area);
                    p.render(area, &mut buf);
                }
            }
        }
    }
}
