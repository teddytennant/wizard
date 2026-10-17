// OWNER: pager (Ctrl+T transcript, /diff, backtrack preview)
//! Alt-screen overlays (spec A.11, A.12): the transcript pager, the static pager used by `/diff`
//! and the full-screen approval details, and the backtrack preview that rides on the transcript.
//!
//! Ported from Codex's `pager_overlay.rs` and `app_backtrack.rs`. Codex keeps a list of
//! renderables; here the document is flattened into rows at the current width, which gives the
//! same scroll arithmetic with less machinery. Rows are rebuilt only when the width, the cells,
//! the live tail or the highlight change.

use std::io::Write;
use std::process::Command;
use std::time::Duration;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Widget};

use crate::app::App;
use crate::style::palette;
use crate::ui::history_cells::InfoCell;
use crate::ui::{AppAction, BoxedCell, Overlay, OverlayResult};
use crate::wrap::{WrapOpts, line_width, word_wrap_line};

const NO_PREVIOUS_MESSAGE: &str = "No previous message to edit.";

// ---- wrapping ---------------------------------------------------------------------------------

/// Lines wider than `width` are re-wrapped the way Codex's `Paragraph` with `trim: false` does,
/// the rest are kept byte for byte (trailing spaces included).
fn fit_lines(lines: &[Line<'static>], width: u16) -> Vec<Line<'static>> {
    let w = (width as usize).max(1);
    let mut out = Vec::with_capacity(lines.len());
    for l in lines {
        if line_width(l) <= w {
            out.push(l.clone());
        } else {
            let wrapped = word_wrap_line(l, &WrapOpts::new(w));
            if wrapped.is_empty() {
                out.push(Line::default());
            }
            for mut r in wrapped {
                r.style = l.style;
                out.push(r);
            }
        }
    }
    out
}

// ---- the pager view ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
struct Row {
    line: Line<'static>,
    style: Style,
}

/// One cell's span of rows, used to scroll a highlighted cell into view.
#[derive(Clone, Copy, Debug)]
struct Chunk {
    first: usize,
    last: usize,
}

#[derive(Debug)]
struct PagerView {
    rows: Vec<Row>,
    chunks: Vec<Chunk>,
    scroll_offset: usize,
    title: String,
    last_content_height: Option<usize>,
    last_total: Option<usize>,
    /// Percentages mean something only when the whole history is loaded.
    percent_visible: bool,
    pending_chunk: Option<usize>,
}

impl PagerView {
    fn new(title: &str, scroll_offset: usize) -> Self {
        Self {
            rows: Vec::new(),
            chunks: Vec::new(),
            scroll_offset,
            title: title.to_string(),
            last_content_height: None,
            last_total: None,
            percent_visible: true,
            pending_chunk: None,
        }
    }

    fn content_area(area: Rect) -> Rect {
        Rect::new(
            area.x,
            area.y.saturating_add(1),
            area.width,
            area.height.saturating_sub(2),
        )
    }

    fn is_scrolled_to_bottom(&self) -> bool {
        if self.scroll_offset == usize::MAX {
            return true;
        }
        let Some(h) = self.last_content_height else {
            return false;
        };
        if self.rows.is_empty() {
            return true;
        }
        let Some(total) = self.last_total else {
            return false;
        };
        total <= h || self.scroll_offset >= total - h
    }

    fn ensure_chunk_visible(&mut self, idx: usize, area: Rect) {
        let Some(c) = self.chunks.get(idx).copied() else {
            return;
        };
        if area.height == 0 {
            return;
        }
        let top = self.scroll_offset;
        let bottom = top.saturating_add(area.height.saturating_sub(1) as usize);
        if c.first < top {
            self.scroll_offset = c.first;
        } else if c.last > bottom {
            self.scroll_offset = c
                .last
                .saturating_sub(area.height.saturating_sub(1) as usize);
        }
    }

    fn render(&mut self, area: Rect, buf: &mut Buffer) {
        // `Clear`: every cell back to a default space, so a shorter page leaves nothing behind.
        for y in area.top()..area.bottom() {
            for x in area.left()..area.right() {
                buf[(x, y)].reset();
            }
        }
        self.render_header(area, buf);
        let content = Self::content_area(area);
        self.last_content_height = Some(content.height as usize);
        let total = self.rows.len();
        self.last_total = Some(total);
        if let Some(idx) = self.pending_chunk.take() {
            self.ensure_chunk_visible(idx, content);
        }
        self.scroll_offset = self
            .scroll_offset
            .min(total.saturating_sub(content.height as usize));
        self.render_content(content, buf);
        self.render_bottom_bar(area, content, buf, total);
    }

    fn render_header(&self, area: Rect, buf: &mut Buffer) {
        if area.height == 0 || area.width == 0 {
            return;
        }
        let dim = Style::default().add_modifier(Modifier::DIM);
        let fill = "/ ".repeat(area.width as usize / 2);
        buf.set_stringn(area.x, area.y, fill, area.width as usize, dim);
        buf.set_stringn(
            area.x,
            area.y,
            format!("/ {}", self.title),
            area.width as usize,
            dim,
        );
    }

    fn render_content(&self, area: Rect, buf: &mut Buffer) {
        let mut drawn = 0u16;
        for (i, row) in self
            .rows
            .iter()
            .skip(self.scroll_offset)
            .take(area.height as usize)
            .enumerate()
        {
            let r = Rect::new(area.x, area.y + i as u16, area.width, 1);
            Paragraph::new(row.line.clone())
                .style(row.style)
                .render(r, buf);
            drawn = i as u16 + 1;
        }
        // vi style filler: `~` in column 0 of every row below the document.
        for y in area.y + drawn..area.bottom() {
            if area.width == 0 {
                break;
            }
            buf[(area.x, y)].set_char('~');
        }
    }

    fn render_bottom_bar(&self, full: Rect, content: Rect, buf: &mut Buffer, total: usize) {
        let y = content.bottom();
        if y >= full.bottom() || full.width == 0 {
            return;
        }
        let dim = Style::default().add_modifier(Modifier::DIM);
        buf.set_stringn(
            full.x,
            y,
            "─".repeat(full.width as usize),
            full.width as usize,
            dim,
        );
        if !self.percent_visible {
            return;
        }
        let percent = if total == 0 {
            100
        } else {
            let max_scroll = total.saturating_sub(content.height as usize);
            if max_scroll == 0 {
                100
            } else {
                ((self.scroll_offset.min(max_scroll) as f32 / max_scroll as f32) * 100.0).round()
                    as u32
            }
        };
        let text = format!(" {percent}% ");
        let w = text.chars().count() as u16;
        if full.width > w {
            buf.set_stringn(full.x + full.width - w - 1, y, text, w as usize, dim);
        }
    }

    /// Returns true when the key moved the view.
    fn handle_key(&mut self, key: KeyEvent) -> bool {
        let page = self.last_content_height.unwrap_or(0);
        let half = page.saturating_add(1) / 2;
        let none = KeyModifiers::NONE;
        let ctrl = KeyModifiers::CONTROL;
        let is = |code: KeyCode, m: KeyModifiers| key.code == code && key.modifiers == m;
        if is(KeyCode::Up, none) || is(KeyCode::Char('k'), none) {
            self.scroll_offset = self.scroll_offset.saturating_sub(1);
        } else if is(KeyCode::Down, none) || is(KeyCode::Char('j'), none) {
            self.scroll_offset = self.scroll_offset.saturating_add(1);
        } else if is(KeyCode::PageUp, none)
            || is(KeyCode::Char(' '), KeyModifiers::SHIFT)
            || is(KeyCode::Char('b'), ctrl)
        {
            self.scroll_offset = self.scroll_offset.saturating_sub(page);
        } else if is(KeyCode::PageDown, none)
            || is(KeyCode::Char(' '), none)
            || is(KeyCode::Char('f'), ctrl)
        {
            self.scroll_offset = self.scroll_offset.saturating_add(page);
        } else if is(KeyCode::Char('d'), ctrl) {
            self.scroll_offset = self.scroll_offset.saturating_add(half);
        } else if is(KeyCode::Char('u'), ctrl) {
            self.scroll_offset = self.scroll_offset.saturating_sub(half);
        } else if is(KeyCode::Home, none) {
            self.scroll_offset = 0;
        } else if is(KeyCode::End, none) {
            self.scroll_offset = usize::MAX;
        } else {
            return false;
        }
        true
    }
}

fn is_close(key: &KeyEvent) -> bool {
    let m = key.modifiers;
    (key.code == KeyCode::Char('q') && m == KeyModifiers::NONE)
        || (key.code == KeyCode::Char('c') && m == KeyModifiers::CONTROL)
}

// ---- hints ------------------------------------------------------------------------------------

/// One dim line: a leading space, pairs separated by three spaces, keys joined by `/`.
fn hint_line(pairs: &[(&[&str], &str)]) -> Line<'static> {
    let mut s = String::from(" ");
    for (i, (keys, desc)) in pairs.iter().enumerate() {
        if i > 0 {
            s.push_str("   ");
        }
        s.push_str(&keys.join("/"));
        s.push(' ');
        s.push_str(desc);
    }
    Line::from(s).dim()
}

fn nav_hints() -> Line<'static> {
    hint_line(&[
        (&["↑", "↓"], "to scroll"),
        (&["pgup", "pgdn"], "to page"),
        (&["home", "end"], "to jump"),
    ])
}

fn draw_hints(area: Rect, buf: &mut Buffer, second: Line<'static>) {
    if area.height < 2 {
        return;
    }
    Paragraph::new(nav_hints()).render(Rect::new(area.x, area.y, area.width, 1), buf);
    Paragraph::new(second).render(Rect::new(area.x, area.y + 1, area.width, 1), buf);
}

// ---- transcript overlay -----------------------------------------------------------------------

/// What the pager says about history that is not loaded yet. codexw always holds the whole
/// transcript, so nothing sets these outside tests; the labels are kept for parity.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum HistoryState {
    #[default]
    Idle,
    LoadingOlder,
    Partial,
    Failed,
    Complete,
}

impl HistoryState {
    fn has_unloaded(self) -> bool {
        matches!(self, Self::LoadingOlder | Self::Partial | Self::Failed)
    }
    fn label(self) -> Option<&'static str> {
        match self {
            Self::Idle => None,
            Self::LoadingOlder => Some(" loading older history... "),
            Self::Partial => Some(" partial history | PgUp for earlier "),
            Self::Failed => Some(" history unavailable | PgUp to retry "),
            Self::Complete => Some(" start of history "),
        }
    }
}

#[derive(Debug)]
struct Entry {
    /// Address of the cell this entry was built from, to notice a replaced cell.
    id: usize,
    user: bool,
    cont: bool,
    text: Option<String>,
    lines: Vec<Line<'static>>,
}

fn cell_id(c: &BoxedCell) -> usize {
    (&**c as *const dyn crate::ui::HistoryCell).cast::<()>() as usize
}

/// The original text of a user cell. The cell trait has no accessor, so the cell is rendered at
/// an absurd width and the `› ` gutter and the tint rows are taken off again.
fn user_text_of(c: &BoxedCell) -> String {
    let lines = c.display_lines(10_000);
    let mut rows: Vec<String> = lines
        .iter()
        .map(|l| {
            let s: String = l.spans.iter().map(|s| s.content.as_ref()).collect();
            s.strip_prefix("› ")
                .or_else(|| s.strip_prefix("  "))
                .unwrap_or(&s)
                .to_string()
        })
        .collect();
    while rows.first().is_some_and(|r| r.trim().is_empty()) {
        rows.remove(0);
    }
    while rows.last().is_some_and(|r| r.trim().is_empty()) {
        rows.pop();
    }
    rows.join("\n")
}

#[derive(Debug)]
pub struct TranscriptOverlay {
    view: PagerView,
    entries: Vec<Entry>,
    width: u16,
    live: Vec<Line<'static>>,
    /// Cell index to highlight (the selected user message).
    highlight: Option<usize>,
    /// Backtrack preview: Esc and arrows pick the user message to edit.
    preview: bool,
    /// Index among user messages, `usize::MAX` for none.
    nth: usize,
    /// Start the preview as soon as the cells are known (Esc Esc from the main view).
    start_preview: bool,
    history: HistoryState,
    dirty: bool,
}

impl TranscriptOverlay {
    pub fn new() -> Self {
        Self {
            view: PagerView::new("T R A N S C R I P T", usize::MAX),
            entries: Vec::new(),
            width: 0,
            live: Vec::new(),
            highlight: None,
            preview: false,
            nth: usize::MAX,
            start_preview: false,
            history: HistoryState::Idle,
            dirty: true,
        }
    }

    /// Opened by the second Esc: the last user message is already selected.
    pub fn backtrack() -> Self {
        let mut o = Self::new();
        o.start_preview = true;
        o
    }

    pub fn set_history_state(&mut self, state: HistoryState) {
        if state == self.history {
            return;
        }
        self.history = state;
        self.view.percent_visible = !state.has_unloaded();
    }

    fn user_indices(&self) -> Vec<usize> {
        self.entries
            .iter()
            .enumerate()
            .filter(|(_, e)| e.user)
            .map(|(i, _)| i)
            .collect()
    }

    fn select(&mut self, nth: usize) {
        let users = self.user_indices();
        match users.get(nth) {
            Some(&cell) => {
                self.nth = nth;
                self.highlight = Some(cell);
                self.view.pending_chunk = Some(cell);
            }
            None => {
                self.nth = usize::MAX;
                self.highlight = None;
            }
        }
        self.dirty = true;
    }

    /// First Esc inside the overlay, or the second Esc from the main view.
    fn begin_preview(&mut self) -> bool {
        let n = self.user_indices().len();
        if n == 0 {
            return false;
        }
        self.preview = true;
        self.select(n - 1);
        true
    }

    fn step_back(&mut self) {
        let n = self.user_indices().len();
        if n == 0 {
            return;
        }
        let next = match self.nth {
            usize::MAX => n - 1,
            0 => 0,
            k => (k - 1).min(n - 1),
        };
        self.select(next);
    }

    fn step_forward(&mut self) {
        let n = self.user_indices().len();
        if n == 0 {
            return;
        }
        let next = match self.nth {
            usize::MAX => n - 1,
            k => (k + 1).min(n - 1),
        };
        self.select(next);
    }

    fn rebuild_rows(&mut self) {
        let w = self.width.max(1);
        let tint = palette().user_message_style();
        let mut rows: Vec<Row> = Vec::new();
        let mut chunks = Vec::new();
        for (i, e) in self.entries.iter().enumerate() {
            let first = rows.len();
            if i > 0 && !e.cont {
                rows.push(Row {
                    line: Line::default(),
                    style: Style::default(),
                });
            }
            let style = if e.user {
                if self.highlight == Some(i) {
                    tint.add_modifier(Modifier::REVERSED)
                } else {
                    tint
                }
            } else {
                Style::default()
            };
            for line in fit_lines(&e.lines, w) {
                rows.push(Row { line, style });
            }
            chunks.push(Chunk {
                first,
                last: rows.len().saturating_sub(1).max(first),
            });
        }
        if !self.live.is_empty() {
            if !self.entries.is_empty() {
                rows.push(Row {
                    line: Line::default(),
                    style: Style::default(),
                });
            }
            for line in fit_lines(&self.live, w) {
                rows.push(Row {
                    line,
                    style: Style::default(),
                });
            }
        }
        self.view.rows = rows;
        self.view.chunks = chunks;
        self.dirty = false;
    }

    fn render_history_state(&self, area: Rect, buf: &mut Buffer) {
        let Some(label) = self.history.label() else {
            return;
        };
        if area.height == 0 {
            return;
        }
        let w = (label.chars().count() as u16).min(area.width);
        buf.set_stringn(
            area.right() - w,
            area.y,
            label,
            w as usize,
            Style::default().add_modifier(Modifier::DIM),
        );
    }

    fn second_hint(&self) -> Line<'static> {
        if self.highlight.is_some() {
            hint_line(&[
                (&["q"], "to quit"),
                (&["esc", "←"], "to edit prev"),
                (&["→"], "to edit next"),
                (&["enter"], "to edit message"),
            ])
        } else {
            hint_line(&[(&["q"], "to quit"), (&["esc"], "to edit prev")])
        }
    }

    pub fn render_into(&mut self, area: Rect, buf: &mut Buffer) {
        if self.dirty {
            self.rebuild_rows();
        }
        let top_h = area.height.saturating_sub(3);
        let top = Rect::new(area.x, area.y, area.width, top_h);
        let bottom = Rect::new(area.x, area.y + top_h, area.width, area.height - top_h);
        self.view.render(top, buf);
        self.render_history_state(top, buf);
        let second = self.second_hint();
        draw_hints(bottom, buf, second);
    }
}

impl Default for TranscriptOverlay {
    fn default() -> Self {
        Self::new()
    }
}

impl Overlay for TranscriptOverlay {
    fn render(&mut self, area: Rect, buf: &mut Buffer) {
        self.render_into(area, buf);
    }

    fn handle_key(&mut self, key: KeyEvent) -> OverlayResult {
        let plain = key.modifiers == KeyModifiers::NONE;
        let ctrl_t = key.code == KeyCode::Char('t') && key.modifiers == KeyModifiers::CONTROL;
        if is_close(&key) || ctrl_t {
            return OverlayResult::Close;
        }
        if self.preview {
            match key.code {
                KeyCode::Esc | KeyCode::Left if plain => {
                    self.step_back();
                    return OverlayResult::Pending;
                }
                KeyCode::Right if plain => {
                    self.step_forward();
                    return OverlayResult::Pending;
                }
                KeyCode::Enter if plain => {
                    let users = self.user_indices();
                    let text = users
                        .get(self.nth)
                        .and_then(|&i| self.entries[i].text.clone());
                    return match text {
                        Some(text) => OverlayResult::CloseWith(AppAction::EditPrompt {
                            nth: self.nth,
                            text,
                        }),
                        None => OverlayResult::Close,
                    };
                }
                _ => {}
            }
        } else if key.code == KeyCode::Esc && plain {
            if self.begin_preview() {
                return OverlayResult::Pending;
            }
            return OverlayResult::CloseWith(AppAction::Info(NO_PREVIOUS_MESSAGE.into()));
        }
        self.view.handle_key(key);
        OverlayResult::Pending
    }

    fn sync_cells(&mut self, cells: &[BoxedCell], live_tail: &[Line<'static>], width: u16) {
        // A view pinned to the bottom stays pinned when the document grows (spec A.11.4).
        let follow = self.view.is_scrolled_to_bottom();
        let stale = self.width != width
            || self.entries.len() != cells.len()
            || self
                .entries
                .iter()
                .zip(cells)
                .any(|(e, c)| e.id != cell_id(c));
        if stale {
            let same_cells = self.entries.len() == cells.len()
                && self
                    .entries
                    .iter()
                    .zip(cells)
                    .all(|(e, c)| e.id == cell_id(c));
            let old = std::mem::take(&mut self.entries);
            self.entries = cells
                .iter()
                .enumerate()
                .map(|(i, c)| {
                    let user = c.is_user_message();
                    let text = if user {
                        match old.get(i) {
                            Some(e) if same_cells && e.text.is_some() => e.text.clone(),
                            _ => Some(user_text_of(c)),
                        }
                    } else {
                        None
                    };
                    Entry {
                        id: cell_id(c),
                        user,
                        cont: c.is_stream_continuation(),
                        text,
                        lines: c.transcript_lines(width),
                    }
                })
                .collect();
            self.width = width;
            self.dirty = true;
        }
        if self.live != live_tail {
            self.live = live_tail.to_vec();
            self.dirty = true;
        }
        if self.dirty && follow {
            self.view.scroll_offset = usize::MAX;
        }
        if self.start_preview {
            self.start_preview = false;
            self.begin_preview();
        }
    }

    fn animation_interval(&self) -> Option<Duration> {
        // Only a visible live tail animates, and only while the view follows it.
        (!self.live.is_empty() && self.view.is_scrolled_to_bottom())
            .then_some(Duration::from_millis(50))
    }
}

// ---- static overlay ---------------------------------------------------------------------------

/// A fixed document under a title: `/diff`, the full-screen approval details.
#[derive(Debug)]
pub struct StaticOverlay {
    view: PagerView,
    lines: Vec<Line<'static>>,
    width: u16,
}

impl StaticOverlay {
    pub fn new(title: &str, lines: Vec<Line<'static>>) -> Self {
        Self {
            view: PagerView::new(title, 0),
            lines,
            width: 0,
        }
    }

    pub fn render_into(&mut self, area: Rect, buf: &mut Buffer) {
        if self.width != area.width {
            self.width = area.width;
            self.view.rows = fit_lines(&self.lines, area.width)
                .into_iter()
                .map(|line| Row {
                    line,
                    style: Style::default(),
                })
                .collect();
        }
        let top_h = area.height.saturating_sub(3);
        let top = Rect::new(area.x, area.y, area.width, top_h);
        let bottom = Rect::new(area.x, area.y + top_h, area.width, area.height - top_h);
        self.view.render(top, buf);
        draw_hints(bottom, buf, hint_line(&[(&["q"], "to quit")]));
    }
}

impl Overlay for StaticOverlay {
    fn render(&mut self, area: Rect, buf: &mut Buffer) {
        self.render_into(area, buf);
    }

    fn handle_key(&mut self, key: KeyEvent) -> OverlayResult {
        if is_close(&key) {
            return OverlayResult::Close;
        }
        self.view.handle_key(key);
        OverlayResult::Pending
    }
}

// ---- ANSI text to lines -----------------------------------------------------------------------

/// Parse the SGR sequences of `git diff --color` into styled lines. Every other escape and every
/// control character is dropped, so file content cannot move the cursor or retitle the window.
pub fn ansi_lines(text: &str) -> Vec<Line<'static>> {
    text.lines().map(ansi_line).collect()
}

fn ansi_line(src: &str) -> Line<'static> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut style = Style::default();
    let mut buf = String::new();
    let mut it = src.chars().peekable();
    let flush = |buf: &mut String, style: Style, spans: &mut Vec<Span<'static>>| {
        if !buf.is_empty() {
            spans.push(Span::styled(std::mem::take(buf), style));
        }
    };
    while let Some(c) = it.next() {
        match c {
            '\u{1b}' => {
                if it.peek() == Some(&'[') {
                    it.next();
                    let mut params = String::new();
                    let mut fin = None;
                    for p in it.by_ref() {
                        if p.is_ascii_digit() || p == ';' || p == ':' {
                            params.push(p);
                        } else {
                            fin = Some(p);
                            break;
                        }
                    }
                    if fin == Some('m') {
                        flush(&mut buf, style, &mut spans);
                        style = apply_sgr(style, &params);
                    }
                } else {
                    // OSC and friends: skip to the terminator.
                    if it.peek() == Some(&']') {
                        for p in it.by_ref() {
                            if p == '\u{7}' || p == '\u{1b}' {
                                break;
                            }
                        }
                    } else {
                        it.next();
                    }
                }
            }
            '\t' => buf.push_str("    "),
            c if c.is_control() => {}
            c => buf.push(c),
        }
    }
    flush(&mut buf, style, &mut spans);
    Line::from(spans)
}

fn apply_sgr(mut style: Style, params: &str) -> Style {
    let nums: Vec<u32> = if params.is_empty() {
        vec![0]
    } else {
        params
            .split([';', ':'])
            .map(|p| p.parse().unwrap_or(0))
            .collect()
    };
    let mut i = 0;
    while i < nums.len() {
        match nums[i] {
            0 => style = Style::default(),
            1 => style = style.add_modifier(Modifier::BOLD),
            2 => style = style.add_modifier(Modifier::DIM),
            3 => style = style.add_modifier(Modifier::ITALIC),
            4 => style = style.add_modifier(Modifier::UNDERLINED),
            7 => style = style.add_modifier(Modifier::REVERSED),
            9 => style = style.add_modifier(Modifier::CROSSED_OUT),
            22 => style = style.remove_modifier(Modifier::BOLD | Modifier::DIM),
            23 => style = style.remove_modifier(Modifier::ITALIC),
            24 => style = style.remove_modifier(Modifier::UNDERLINED),
            27 => style = style.remove_modifier(Modifier::REVERSED),
            n @ 30..=37 => style = style.fg(ansi_color(n - 30)),
            39 => style.fg = None,
            n @ 40..=47 => style = style.bg(ansi_color(n - 40)),
            49 => style.bg = None,
            n @ 90..=97 => style = style.fg(ansi_color(n - 90 + 8)),
            n @ 100..=107 => style = style.bg(ansi_color(n - 100 + 8)),
            n @ (38 | 48) => {
                let color = match nums.get(i + 1) {
                    Some(5) => nums
                        .get(i + 2)
                        .map(|&v| Color::Indexed(v as u8))
                        .inspect(|_| i += 2),
                    Some(2) => {
                        let c = (nums.get(i + 2), nums.get(i + 3), nums.get(i + 4));
                        if let (Some(&r), Some(&g), Some(&b)) = c {
                            i += 4;
                            Some(Color::Rgb(r as u8, g as u8, b as u8))
                        } else {
                            None
                        }
                    }
                    _ => None,
                };
                if let Some(c) = color {
                    style = if n == 38 { style.fg(c) } else { style.bg(c) };
                }
            }
            _ => {}
        }
        i += 1;
    }
    style
}

fn ansi_color(n: u32) -> Color {
    Color::Indexed(n as u8)
}

// ---- git diff ---------------------------------------------------------------------------------

/// Tracked changes, then each untracked file as a diff against `/dev/null`, as Codex's
/// `get_git_diff` does. `Ok(None)` when `cwd` is not inside a repository.
pub fn git_diff(cwd: &std::path::Path) -> Result<Option<String>, String> {
    fn git(cwd: &std::path::Path, args: &[&str]) -> std::io::Result<std::process::Output> {
        // Repository config must not pick executable helpers, and the diff must not write
        // anything (no index refresh, no hooks).
        Command::new("git")
            .current_dir(cwd)
            .env("GIT_OPTIONAL_LOCKS", "0")
            .args([
                "-c",
                "core.hooksPath=/dev/null",
                "-c",
                "core.fsmonitor=false",
            ])
            .args(args)
            .stdin(std::process::Stdio::null())
            .output()
    }
    let inside = git(cwd, &["rev-parse", "--is-inside-work-tree"]).map_err(|e| e.to_string())?;
    if !inside.status.success() || !String::from_utf8_lossy(&inside.stdout).starts_with("true") {
        return Ok(None);
    }
    const DIFF: [&str; 6] = [
        "diff",
        "--no-textconv",
        "--no-ext-diff",
        "--submodule=short",
        "--ignore-submodules=dirty",
        "--color",
    ];
    let tracked = git(cwd, &DIFF).map_err(|e| e.to_string())?;
    if !tracked.status.success() {
        return Err(format!("git diff failed with status {}", tracked.status));
    }
    let mut text = String::from_utf8_lossy(&tracked.stdout).into_owned();
    let others =
        git(cwd, &["ls-files", "--others", "--exclude-standard"]).map_err(|e| e.to_string())?;
    for file in String::from_utf8_lossy(&others.stdout)
        .split('\n')
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        let mut args = DIFF.to_vec();
        args.extend(["--no-index", "--", "/dev/null", file]);
        // `--no-index` exits 1 when the files differ.
        let out = git(cwd, &args).map_err(|e| e.to_string())?;
        text.push_str(&String::from_utf8_lossy(&out.stdout));
    }
    Ok(Some(text))
}

/// The lines `/diff` shows for a `git_diff` result.
pub fn diff_pager_lines(result: Result<Option<String>, String>) -> Vec<Line<'static>> {
    let text = match result {
        Ok(Some(t)) => t,
        Ok(None) => "`/diff` — _not inside a git repository_".to_string(),
        Err(e) => format!("Failed to compute diff: {e}"),
    };
    if text.trim().is_empty() {
        vec![Line::from("No changes detected.".italic())]
    } else {
        ansi_lines(&text)
    }
}

// ---- App entry points -------------------------------------------------------------------------

impl<W: Write> App<W> {
    /// Ctrl+T: the transcript pager on the alternate screen, pinned to the bottom.
    pub fn open_pager(&mut self) {
        self.open_overlay(Box::new(TranscriptOverlay::new()));
    }

    /// Second Esc on an empty composer: the pager with the last user message selected.
    pub fn backtrack(&mut self) {
        if self.cells.iter().any(|c| c.is_user_message()) {
            self.open_overlay(Box::new(TranscriptOverlay::backtrack()));
        } else {
            self.push_cell(Box::new(InfoCell {
                text: NO_PREVIOUS_MESSAGE.into(),
                hint: None,
            }));
        }
    }

    /// Enter in the backtrack preview. Codex forks the thread before the prompt; wizard cannot
    /// fork over ACP, so this goes through its own `/rewind` (see `rewind.rs`).
    pub fn edit_previous_prompt(&mut self, nth: usize, text: String) {
        self.esc_primed = false;
        self.pane.footer.esc_hint = false;
        self.rewind_prompt(nth, text);
    }

    /// `/diff`: static pager titled `D I F F`.
    pub fn open_diff(&mut self) {
        let lines = diff_pager_lines(git_diff(&self.opts.cwd));
        self.open_overlay(Box::new(StaticOverlay::new("D I F F", lines)));
    }

    /// A static pager opened by a bottom view (`E X E C`, `P A T C H`).
    pub fn open_static(&mut self, title: &str, lines: Vec<Line<'static>>) {
        self.open_overlay(Box::new(StaticOverlay::new(title, lines)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::style::{ColorLevel, Palette, set_palette};
    use crate::ui::HistoryCell;

    #[derive(Debug)]
    struct TestCell {
        lines: Vec<Line<'static>>,
        user: bool,
    }

    impl HistoryCell for TestCell {
        fn display_lines(&self, _w: u16) -> Vec<Line<'static>> {
            self.lines.clone()
        }
        fn is_user_message(&self) -> bool {
            self.user
        }
    }

    fn cell(text: &str) -> BoxedCell {
        Box::new(TestCell {
            lines: text.lines().map(|l| Line::from(l.to_string())).collect(),
            user: false,
        })
    }

    fn user(text: &str) -> BoxedCell {
        let style = Style::default().bg(Color::Rgb(30, 30, 30));
        Box::new(TestCell {
            lines: vec![
                Line::default().style(style),
                Line::from(format!("› {text}")).style(style),
                Line::default().style(style),
            ],
            user: true,
        })
    }

    fn dark() {
        set_palette(Palette::new(
            Some((230, 230, 230)),
            Some((0, 0, 0)),
            ColorLevel::TrueColor,
        ));
    }

    fn rows(buf: &Buffer) -> Vec<String> {
        (0..buf.area.height)
            .map(|y| {
                (0..buf.area.width)
                    .map(|x| buf[(x, y)].symbol().to_string())
                    .collect::<String>()
            })
            .collect()
    }

    fn render(o: &mut dyn Overlay, w: u16, h: u16) -> Buffer {
        let area = Rect::new(0, 0, w, h);
        let mut buf = Buffer::empty(area);
        o.render(area, &mut buf);
        buf
    }

    fn overlay_with(cells: &[BoxedCell], w: u16) -> TranscriptOverlay {
        dark();
        let mut o = TranscriptOverlay::new();
        o.sync_cells(cells, &[], w);
        o
    }

    // codex_tui__pager_overlay__tests__transcript_overlay_snapshot_basic
    #[test]
    fn transcript_snapshot_basic() {
        let cells = vec![cell("alpha"), cell("beta"), cell("gamma")];
        let mut o = overlay_with(&cells, 40);
        let got = rows(&render(&mut o, 40, 10));
        let want = [
            "/ T R A N S C R I P T / / / / / / / / / ",
            "alpha                                   ",
            "                                        ",
            "beta                                    ",
            "                                        ",
            "gamma                                   ",
            "───────────────────────────────── 100% ─",
            " ↑/↓ to scroll   pgup/pgdn to page   hom",
            " q to quit   esc to edit prev           ",
            "                                        ",
        ];
        assert_eq!(got, want);
    }

    // codex_tui__pager_overlay__tests__transcript_overlay_renders_live_tail
    #[test]
    fn transcript_renders_live_tail() {
        dark();
        let cells = vec![cell("alpha")];
        let mut o = TranscriptOverlay::new();
        o.sync_cells(&cells, &[Line::from("tail")], 40);
        let got = rows(&render(&mut o, 40, 10));
        let want = [
            "/ T R A N S C R I P T / / / / / / / / / ",
            "alpha                                   ",
            "                                        ",
            "tail                                    ",
            "~                                       ",
            "~                                       ",
            "───────────────────────────────── 100% ─",
            " ↑/↓ to scroll   pgup/pgdn to page   hom",
            " q to quit   esc to edit prev           ",
            "                                        ",
        ];
        assert_eq!(got, want);
        assert_eq!(o.animation_interval(), Some(Duration::from_millis(50)));
    }

    // codex_tui__pager_overlay__tests__static_overlay_snapshot_basic
    #[test]
    fn static_snapshot_basic() {
        let mut o = StaticOverlay::new(
            "S T A T I C",
            vec![Line::from("one"), Line::from("two"), Line::from("three")],
        );
        let got = rows(&render(&mut o, 40, 10));
        let want = [
            "/ S T A T I C / / / / / / / / / / / / / ",
            "one                                     ",
            "two                                     ",
            "three                                   ",
            "~                                       ",
            "~                                       ",
            "───────────────────────────────── 100% ─",
            " ↑/↓ to scroll   pgup/pgdn to page   hom",
            " q to quit                              ",
            "                                        ",
        ];
        assert_eq!(got, want);
    }

    #[test]
    fn static_overlay_wraps_long_lines() {
        let mut o = StaticOverlay::new(
            "S T A T I C",
            vec![Line::from(
                "a very long line that should wrap when rendered within a narrow pager overlay width",
            )],
        );
        let got = rows(&render(&mut o, 24, 12));
        assert_eq!(got[1].trim_end(), "a very long line that");
        assert_eq!(got[2].trim_end(), "should wrap when");
        assert_eq!(got[3].trim_end(), "rendered within a narrow");
        assert_eq!(got[4].trim_end(), "pager overlay width");
    }

    // transcript_overlay_snapshots_paginated_history_states
    #[test]
    fn paginated_history_states() {
        let cells = vec![cell("recent transcript")];
        let mut o = overlay_with(&cells, 72);
        o.set_history_state(HistoryState::LoadingOlder);
        let b = rows(&render(&mut o, 72, 10));
        assert_eq!(
            b[0].trim_end(),
            "/ T R A N S C R I P T / / / / / / / / / / / /  loading older history..."
        );
        assert_eq!(b[1].trim_end(), "recent transcript");
        assert_eq!(b[6], "─".repeat(72), "no percent while history is partial");
        o.set_history_state(HistoryState::Complete);
        let b = rows(&render(&mut o, 72, 10));
        assert_eq!(
            b[0].trim_end(),
            "/ T R A N S C R I P T / / / / / / / / / / / / / / / /  start of history"
        );
        assert_eq!(
            b[6],
            format!("{} 100% ─", "─".repeat(65)),
            "complete history shows the percent again"
        );
    }

    #[test]
    fn header_is_dim_and_fills_the_width() {
        let cells = vec![cell("x")];
        let mut o = overlay_with(&cells, 120);
        let b = render(&mut o, 120, 36);
        let h = rows(&b)[0].clone();
        assert!(h.starts_with("/ T R A N S C R I P T / / /"));
        assert_eq!(h.trim_end().chars().count(), 119);
        assert!(b[(0, 0)].modifier.contains(Modifier::DIM));
        assert!(b[(60, 0)].modifier.contains(Modifier::DIM));
        // separator at H-4, hints at H-3 and H-2
        let r = rows(&b);
        assert!(r[32].starts_with('─'));
        assert_eq!(
            r[33].trim_end(),
            " ↑/↓ to scroll   pgup/pgdn to page   home/end to jump"
        );
        assert_eq!(r[34].trim_end(), " q to quit   esc to edit prev");
        assert_eq!(r[35].trim(), "");
    }

    fn numbered(n: usize) -> Vec<BoxedCell> {
        (0..n).map(|i| cell(&format!("line-{i:02}"))).collect()
    }

    fn scroll_to(o: &mut TranscriptOverlay, v: usize) {
        o.view.scroll_offset = v;
    }

    fn visible_numbers(o: &mut TranscriptOverlay, w: u16, h: u16) -> Vec<usize> {
        let b = render(o, w, h);
        rows(&b)
            .iter()
            .filter_map(|r| r.trim().strip_prefix("line-").and_then(|s| s.parse().ok()))
            .collect()
    }

    // transcript_overlay_paging_is_continuous_and_round_trips
    #[test]
    fn paging_is_continuous_and_round_trips() {
        let cells = numbered(50);
        let mut o = overlay_with(&cells, 40);
        scroll_to(&mut o, 0);
        render(&mut o, 40, 15);
        let page = o.view.last_content_height.unwrap();
        let p1 = visible_numbers(&mut o, 40, 15);
        assert_eq!(p1[0], 0);
        scroll_to(&mut o, page);
        let p2 = visible_numbers(&mut o, 40, 15);
        assert_eq!(p2.len(), p1.len());
        assert_eq!(p2[0], *p1.last().unwrap() + 1);
        scroll_to(&mut o, 3);
        let before = visible_numbers(&mut o, 40, 15);
        scroll_to(&mut o, 3 + page);
        visible_numbers(&mut o, 40, 15);
        scroll_to(&mut o, 3);
        assert_eq!(visible_numbers(&mut o, 40, 15), before);
    }

    // transcript_overlay_keeps_scroll_pinned_at_bottom / preserves_manual_scroll_position
    #[test]
    fn follows_the_tail_only_when_pinned() {
        dark();
        let mut cells = numbered(20);
        let mut o = TranscriptOverlay::new();
        o.sync_cells(&cells, &[], 40);
        render(&mut o, 40, 12);
        assert!(o.view.is_scrolled_to_bottom());
        cells.push(cell("tail"));
        o.sync_cells(&cells, &[], 40);
        assert_eq!(o.view.scroll_offset, usize::MAX);
        render(&mut o, 40, 12);
        let b = rows(&render(&mut o, 40, 12));
        assert!(b.iter().any(|r| r.starts_with("tail")));

        scroll_to(&mut o, 0);
        cells.push(cell("more"));
        o.sync_cells(&cells, &[], 40);
        assert_eq!(o.view.scroll_offset, 0);
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn press(o: &mut TranscriptOverlay, code: KeyCode, m: KeyModifiers) -> OverlayResult {
        o.handle_key(KeyEvent::new(code, m))
    }

    /// Reference xi-14-pager-key-*: 120x36 over a 131 row document, page 31, half page 16.
    #[test]
    fn vi_keys_move_by_the_spec_amounts() {
        let cells = vec![cell(
            &(0..132)
                .map(|i| format!("l{i}"))
                .collect::<Vec<_>>()
                .join("\n"),
        )];
        let mut o = overlay_with(&cells, 120);
        render(&mut o, 120, 36);
        let page = o.view.last_content_height.unwrap();
        assert_eq!(page, 31);
        let total = o.view.last_total.unwrap();
        let max = total - page;
        o.handle_key(key(KeyCode::Home));
        assert_eq!(o.view.scroll_offset, 0);
        let none = KeyModifiers::NONE;
        let ctrl = KeyModifiers::CONTROL;
        press(&mut o, KeyCode::Char('j'), none);
        press(&mut o, KeyCode::Down, none);
        assert_eq!(o.view.scroll_offset, 2);
        press(&mut o, KeyCode::Char('k'), none);
        assert_eq!(o.view.scroll_offset, 1);
        press(&mut o, KeyCode::Char(' '), none);
        assert_eq!(o.view.scroll_offset, 1 + page);
        press(&mut o, KeyCode::Char(' '), KeyModifiers::SHIFT);
        assert_eq!(o.view.scroll_offset, 1);
        press(&mut o, KeyCode::Char('f'), ctrl);
        press(&mut o, KeyCode::Char('b'), ctrl);
        assert_eq!(o.view.scroll_offset, 1);
        press(&mut o, KeyCode::Char('d'), ctrl);
        assert_eq!(o.view.scroll_offset, 17);
        press(&mut o, KeyCode::Char('u'), ctrl);
        assert_eq!(o.view.scroll_offset, 1);
        press(&mut o, KeyCode::PageDown, none);
        press(&mut o, KeyCode::PageUp, none);
        assert_eq!(o.view.scroll_offset, 1);
        o.handle_key(key(KeyCode::End));
        render(&mut o, 120, 36);
        assert_eq!(o.view.scroll_offset, max);
    }

    #[test]
    fn q_ctrl_c_and_ctrl_t_close() {
        let cells = vec![cell("x")];
        let mut o = overlay_with(&cells, 40);
        let none = KeyModifiers::NONE;
        assert_eq!(
            press(&mut o, KeyCode::Char('q'), none),
            OverlayResult::Close
        );
        assert_eq!(
            press(&mut o, KeyCode::Char('c'), KeyModifiers::CONTROL),
            OverlayResult::Close
        );
        assert_eq!(
            press(&mut o, KeyCode::Char('t'), KeyModifiers::CONTROL),
            OverlayResult::Close
        );
        let mut s = StaticOverlay::new("D I F F", vec![]);
        assert_eq!(s.handle_key(key(KeyCode::Char('q'))), OverlayResult::Close);
        assert_eq!(
            s.handle_key(KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL)),
            OverlayResult::Pending,
            "Ctrl+T does not close a static overlay"
        );
        assert_eq!(s.handle_key(key(KeyCode::Esc)), OverlayResult::Pending);
    }

    #[test]
    fn backtrack_selects_the_last_user_message_and_steps() {
        dark();
        let cells = vec![
            cell("header"),
            user("first"),
            cell("answer one"),
            user("second"),
            cell("answer two"),
        ];
        let mut o = TranscriptOverlay::backtrack();
        o.sync_cells(&cells, &[], 80);
        assert_eq!(o.highlight, Some(3));
        let b = rows(&render(&mut o, 80, 24));
        assert_eq!(
            b[22].trim_end(),
            " q to quit   esc/← to edit prev   → to edit next   enter to edit message"
        );
        // reversed on the tinted rows
        let row = (1..21).rev().find(|&y| b[y].starts_with('›')).unwrap() as u16;
        assert!(
            render(&mut o, 80, 24)[(0, row)]
                .modifier
                .contains(Modifier::REVERSED)
        );

        let none = KeyModifiers::NONE;
        press(&mut o, KeyCode::Esc, none);
        assert_eq!(o.highlight, Some(1));
        press(&mut o, KeyCode::Left, none);
        assert_eq!(o.highlight, Some(1), "clamped at the oldest message");
        press(&mut o, KeyCode::Right, none);
        assert_eq!(o.highlight, Some(3));
        press(&mut o, KeyCode::Right, none);
        assert_eq!(o.highlight, Some(3), "clamped at the newest");
        assert_eq!(
            press(&mut o, KeyCode::Enter, none),
            OverlayResult::CloseWith(AppAction::EditPrompt {
                nth: 1,
                text: "second".into()
            })
        );
    }

    /// Reference pager-06-backtrack-1: the selected cell, inset row first, starts content row 1.
    #[test]
    fn backtrack_scrolls_the_selected_cell_to_the_top() {
        dark();
        let mut cells = vec![cell("header"), user("go fake:tour")];
        cells.extend((0..40).map(|i| cell(&format!("answer {i}"))));
        let mut o = TranscriptOverlay::backtrack();
        o.sync_cells(&cells, &[], 120);
        let b = rows(&render(&mut o, 120, 36));
        assert_eq!(b[1].trim(), "");
        assert_eq!(b[2].trim(), "");
        assert!(b[3].starts_with("› go fake:tour"), "{b:?}");
        assert!(b[32].contains(" 3%") || b[32].contains(" 2%"), "{}", b[32]);
    }

    #[test]
    fn esc_in_the_pager_starts_the_preview_once() {
        dark();
        let cells = vec![cell("header"), user("only")];
        let mut o = TranscriptOverlay::new();
        o.sync_cells(&cells, &[], 80);
        let none = KeyModifiers::NONE;
        let b = rows(&render(&mut o, 80, 24));
        assert!(b[22].contains("esc to edit prev") && !b[22].contains("enter"));
        assert_eq!(press(&mut o, KeyCode::Esc, none), OverlayResult::Pending);
        assert_eq!(o.highlight, Some(1));
        // xg-01-esc-3: a further Esc with a single user message changes nothing
        press(&mut o, KeyCode::Esc, none);
        assert_eq!(o.highlight, Some(1));

        let mut empty = TranscriptOverlay::new();
        empty.sync_cells(&[cell("header")], &[], 80);
        assert_eq!(
            press(&mut empty, KeyCode::Esc, none),
            OverlayResult::CloseWith(AppAction::Info("No previous message to edit.".into()))
        );
    }

    #[test]
    fn resize_relays_out() {
        dark();
        let cells = vec![cell(&"word ".repeat(40))];
        let mut o = TranscriptOverlay::new();
        o.sync_cells(&cells, &[], 80);
        let wide = rows(&render(&mut o, 80, 24));
        o.sync_cells(&cells, &[], 40);
        let narrow = rows(&render(&mut o, 40, 24));
        let count = |r: &[String]| r.iter().filter(|l| l.contains("word")).count();
        assert!(count(&narrow) > count(&wide));
        assert_eq!(narrow[0].chars().count(), 40);
    }

    #[test]
    fn ansi_parser_keeps_sgr_and_drops_everything_else() {
        let l = ansi_line(
            "\u{1b}[1mdiff\u{1b}[0m \u{1b}[38;5;2m+x\u{1b}[39m\u{1b}]0;evil\u{7}y\u{1b}[2Jz\tq",
        );
        let text: String = l.spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(text, "diff +xyz    q");
        assert!(l.spans[0].style.add_modifier.contains(Modifier::BOLD));
        assert_eq!(l.spans[2].style.fg, Some(Color::Indexed(2)));
    }

    #[test]
    fn empty_diff_says_so_in_italic() {
        let l = diff_pager_lines(Ok(Some("\n".into())));
        assert_eq!(l.len(), 1);
        assert_eq!(l[0].spans[0].content, "No changes detected.");
        assert!(l[0].spans[0].style.add_modifier.contains(Modifier::ITALIC));
        let l = diff_pager_lines(Ok(None));
        assert_eq!(
            l[0].spans[0].content,
            "`/diff` — _not inside a git repository_"
        );
        let l = diff_pager_lines(Err("boom".into()));
        assert_eq!(l[0].spans[0].content, "Failed to compute diff: boom");
    }
}
