// OWNER: transcript (find, jump, drag selection and copy over the laid-out scrollback)
//! Things that work on the rows of the transcript rather than make them: the `/find` bar, the
//! `/jump` list, the text a drag selects, and what `y` copies. Everything here reads
//! [`Doc`] rows, so a match, a selection or a copy is exactly what is drawn.

use std::collections::HashSet;
use std::time::Duration;

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use unicode_width::UnicodeWidthChar;

use super::transcript::{Doc, EntryKey, Kind, Row, View};
use super::{bold, put, st, Layout};
use crate::app::{App, Focus};
use crate::theme::Theme;

/// Per-view state for the pieces below.
#[derive(Debug, Default)]
pub struct Nav {
    pub find: Option<Find>,
    pub jump: Option<Jump>,
    /// Messages shown as their markdown source (vim `r`).
    pub raw: HashSet<EntryKey>,
    pub drag: Option<Drag>,
    /// The left button is down over the transcript.
    pub pressed: bool,
    /// Last click, for the double-click window: where and when.
    pub click: Option<(usize, Duration)>,
    /// The fullscreen block viewer.
    pub viewer: Option<super::transcript_viewer::Viewer>,
}

/// One drawn cell of a row: its column, its character, and whether copying keeps it.
pub type Cell = (u16, char, bool);

/// The cells of a row in column order. Prefix glyphs and timestamps are not copied.
pub fn cells(row: &Row) -> Vec<Cell> {
    let mut out: Vec<Cell> = Vec::new();
    for s in &row.segs {
        let mut x = s.x;
        for ch in s.text.chars() {
            let w = ch.width().unwrap_or(0) as u16;
            if w > 0 {
                out.push((x, ch, s.copy));
            }
            x += w;
        }
    }
    out.sort_by_key(|c| c.0);
    out
}

/// A row as a string with the gaps between its segments filled with spaces.
fn plain(cells: &[Cell], copyable_only: bool) -> String {
    let mut s = String::new();
    let mut at = 0u16;
    let mut first = true;
    for &(x, ch, copy) in cells {
        if copyable_only && !copy {
            continue;
        }
        if first {
            at = x;
            first = false;
        }
        while at < x {
            s.push(' ');
            at += 1;
        }
        s.push(ch);
        at = x + ch.width().unwrap_or(1) as u16;
    }
    s.trim_end().to_string()
}

// ---- find -----------------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Hit {
    /// Document row.
    pub row: usize,
    pub c0: u16,
    pub c1: u16,
}

#[derive(Debug, Default)]
pub struct Find {
    pub query: String,
    /// Typing the query; `Enter` accepts and the keys `n`/`N` take over.
    pub composing: bool,
    pub cur: usize,
    pub hits: Vec<Hit>,
}

impl Find {
    /// `1/2`, `no matches`, or nothing for an empty query.
    pub fn counter(&self) -> Option<String> {
        if self.query.is_empty() {
            None
        } else if self.hits.is_empty() {
            Some("no matches".into())
        } else {
            Some(format!("{}/{}", self.cur + 1, self.hits.len()))
        }
    }
}

/// Every match of `query` in the drawn text, in document order. Case-insensitive unless the query
/// has an upper-case letter (smart case).
pub fn scan(doc: &Doc, query: &str) -> Vec<Hit> {
    if query.is_empty() {
        return Vec::new();
    }
    let fold = !query.chars().any(char::is_uppercase);
    let norm = |c: char| {
        if fold {
            c.to_lowercase().next().unwrap_or(c)
        } else {
            c
        }
    };
    let needle: Vec<char> = query.chars().map(norm).collect();
    let mut hits = Vec::new();
    for (i, e) in doc.entries.iter().enumerate() {
        for (r, row) in e.rows.iter().enumerate() {
            let cs: Vec<Cell> = cells(row).into_iter().filter(|c| c.2).collect();
            if cs.len() < needle.len() {
                continue;
            }
            let mut k = 0;
            while k + needle.len() <= cs.len() {
                if (0..needle.len()).all(|j| norm(cs[k + j].1) == needle[j]) {
                    let last = cs[k + needle.len() - 1];
                    hits.push(Hit {
                        row: doc.starts[i] + r,
                        c0: cs[k].0,
                        c1: last.0 + last.1.width().unwrap_or(1) as u16,
                    });
                    k += needle.len();
                } else {
                    k += 1;
                }
            }
        }
    }
    hits
}

impl View {
    pub fn find_open(&mut self, query: Option<&str>) {
        let mut f = Find {
            composing: true,
            ..Default::default()
        };
        if let Some(q) = query {
            f.query = q.to_string();
        }
        self.nav.find = Some(f);
        self.find_rescan();
        self.find_reveal();
    }

    pub fn find_close(&mut self) {
        self.nav.find = None;
    }

    /// Recompute the hits against the current document, keeping the cursor in range.
    pub fn find_rescan(&mut self) {
        let Some(f) = self.nav.find.as_mut() else {
            return;
        };
        f.hits = scan(&self.doc, &f.query);
        if f.cur >= f.hits.len() {
            f.cur = 0;
        }
    }

    pub fn find_edit(&mut self, f: impl FnOnce(&mut String)) {
        if let Some(fd) = self.nav.find.as_mut() {
            f(&mut fd.query);
            fd.cur = 0;
        }
        self.find_rescan();
        self.find_reveal();
    }

    pub fn find_step(&mut self, forward: bool) {
        if let Some(f) = self.nav.find.as_mut() {
            let n = f.hits.len();
            if n > 0 {
                f.cur = if forward {
                    (f.cur + 1) % n
                } else {
                    (f.cur + n - 1) % n
                };
            }
        }
        self.find_reveal();
    }

    /// Scroll so the current match is on screen, centred when it was not.
    pub fn find_reveal(&mut self) {
        let Some(h) = self
            .nav
            .find
            .as_ref()
            .and_then(|f| f.hits.get(f.cur).copied())
        else {
            return;
        };
        let vh = self.view_rows.max(1);
        let top = self.top();
        if h.row < top || h.row >= top + vh {
            let to = h.row.saturating_sub(vh / 2);
            self.offset = Some(to.min(self.max_offset()));
            self.flip = None;
        }
    }
}

/// Reverse the cells of every match that is on screen.
pub fn paint_hits(buf: &mut Buffer, view: &View, lay: &Layout) {
    let Some(f) = view.nav.find.as_ref() else {
        return;
    };
    let top = view.top();
    let vh = lay.view.height as usize;
    for h in &f.hits {
        if h.row >= top && h.row < top + vh {
            let y = lay.view.y + (h.row - top) as u16;
            buf.set_style(
                Rect::new(h.c0, y, h.c1 - h.c0, 1),
                Style::new().add_modifier(Modifier::REVERSED),
            );
        }
    }
}

/// The divider and the `search:` row under the viewport.
pub fn draw_find(buf: &mut Buffer, app: &App, lay: &Layout) {
    let (Some(f), Some(y)) = (app.view.nav.find.as_ref(), lay.find_y) else {
        return;
    };
    let th = &app.theme;
    let x0 = lay.hpad;
    let w = lay.w.saturating_sub(2 * lay.hpad);
    if lay.h > 0 && y + 1 < lay.h {
        put(buf, x0, y, &"─".repeat(w as usize), st(th.gray_dim));
        let bar = y + 1;
        let x = put(buf, x0, bar, " search: ", st(th.gray));
        let counter = f.counter();
        let cw = counter.as_ref().map_or(0, |c| c.chars().count() as u16);
        let room = w.saturating_sub(9 + cw + 2) as usize;
        let shown: String = f.query.chars().take(room).collect();
        let x = put(buf, x, bar, &shown, st(th.text_primary));
        if f.composing {
            // the caret is one inverted cell
            put(
                buf,
                x,
                bar,
                " ",
                Style::new().fg(th.bg_base).bg(th.text_primary),
            );
        }
        if let Some(c) = counter {
            put(buf, x0 + w - cw, bar, &c, st(th.gray));
        }
    }
}

// ---- jump -----------------------------------------------------------------------------------

#[derive(Debug)]
pub struct Jump {
    pub sel: usize,
    /// One row per turn, oldest first: the prompt's entry and a one-line preview.
    pub turns: Vec<(EntryKey, String)>,
    /// What to put back on `Esc`.
    pub restore: (Option<usize>, Option<usize>, Option<EntryKey>),
}

impl View {
    /// Open `/jump` over the turns the transcript has. `None` when there is nothing to jump to.
    pub fn jump_open(&mut self, prompts: Vec<(EntryKey, String)>) -> bool {
        if prompts.len() < 2 {
            return false;
        }
        let sel = prompts.len() - 1;
        self.nav.jump = Some(Jump {
            sel,
            turns: prompts,
            restore: (self.offset, self.flip, self.selected),
        });
        self.jump_preview();
        true
    }

    /// Scroll so the turn under the cursor is at the top, as `Enter` would leave it.
    pub fn jump_preview(&mut self) {
        let Some(key) = self
            .nav
            .jump
            .as_ref()
            .and_then(|j| j.turns.get(j.sel).map(|t| t.0))
        else {
            return;
        };
        if let Some(i) = self.doc.index_of(key) {
            let to = self.doc.starts[i];
            self.offset = Some(to.min(self.max_offset()));
            self.flip = None;
        }
    }

    pub fn jump_move(&mut self, delta: i32) {
        if let Some(j) = self.nav.jump.as_mut() {
            let max = j.turns.len() as i32 - 1;
            j.sel = (j.sel as i32 + delta).clamp(0, max) as usize;
        }
        self.jump_preview();
    }

    /// `Enter`: stay where the preview put the view and select the prompt.
    pub fn jump_commit(&mut self) {
        if let Some(j) = self.nav.jump.take() {
            if let Some((k, _)) = j.turns.get(j.sel) {
                self.selected = Some(*k);
            }
        }
    }

    pub fn jump_cancel(&mut self) {
        if let Some(j) = self.nav.jump.take() {
            self.offset = j.restore.0;
            self.flip = j.restore.1;
            self.selected = j.restore.2;
        }
    }
}

/// Rows the list takes: a title row, up to 15 turns, a blank row at each end, capped at 60% of
/// the screen (spec `overlay_list.rs`).
pub fn overlay_height(turns: usize, screen_h: u16) -> u16 {
    let rows = turns.min(15) as u16;
    let h = 2 + rows;
    let cap = ((screen_h as u32 * 60 / 100).max(6)) as u16;
    h.min(cap) + 1
}

/// Draw a docked list in the prompt's slot: panel background, accent bar down the left, bold
/// title, one row per item with the cursor row on the selection band.
pub fn draw_list(
    buf: &mut Buffer,
    area: Rect,
    th: &Theme,
    title: &str,
    rows: &[(String, String)],
    sel: usize,
) {
    if area.height == 0 || area.width < 10 {
        return;
    }
    buf.set_style(area, Style::new().bg(th.bg_light));
    for y in area.y..area.y + area.height {
        put(buf, area.x, y, "┃", st(th.accent_user).bg(th.bg_light));
    }
    let cx = area.x + 3;
    let cw = area.width.saturating_sub(5);
    put(
        buf,
        cx,
        area.y + 1,
        title,
        bold(th.accent_user).bg(th.bg_light),
    );
    let visible = area.height.saturating_sub(3) as usize;
    let first = if visible > 0 && sel >= visible {
        sel - visible + 1
    } else {
        0
    };
    for (n, (lead, text)) in rows.iter().enumerate().skip(first).take(visible) {
        let y = area.y + 2 + (n - first) as u16;
        let cursor = n == sel;
        let bg = if cursor { th.bg_visual } else { th.bg_light };
        if cursor {
            buf.set_style(Rect::new(cx - 1, y, cw + 2, 1), th_overlay(th));
        }
        let x = put(buf, cx, y, lead, st(th.gray).bg(bg));
        let mut text_style = st(th.text_primary).bg(bg);
        if cursor {
            text_style = text_style.add_modifier(Modifier::BOLD);
        }
        let room = (cw as usize).saturating_sub(lead.chars().count());
        let t = if text.chars().count() > room {
            let cut: String = text.chars().take(room.saturating_sub(3)).collect();
            format!("{cut}...")
        } else {
            text.clone()
        };
        put(buf, x, y, &t, text_style);
    }
}

/// The selection band: a background on RGB themes, reverse video where there is none.
fn th_overlay(th: &Theme) -> Style {
    if th.bandless {
        Style::new().add_modifier(Modifier::REVERSED)
    } else {
        Style::new().bg(th.bg_visual)
    }
}

pub fn draw_jump(buf: &mut Buffer, app: &App, lay: &Layout) {
    let Some(j) = app.view.nav.jump.as_ref() else {
        return;
    };
    let w = j.turns.len().to_string().len();
    let rows: Vec<(String, String)> = j
        .turns
        .iter()
        .enumerate()
        .map(|(i, (_, p))| {
            (
                format!("{:>w$} ", i + 1),
                if p.is_empty() {
                    "(no preview)".to_string()
                } else {
                    p.clone()
                },
            )
        })
        .collect();
    draw_list(
        buf,
        lay.composer,
        &app.theme,
        "Jump to which turn?",
        &rows,
        j.sel,
    );
}

// ---- drag selection and copy ----------------------------------------------------------------

/// A selection made with the mouse, in document rows and columns.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Drag {
    pub from: (usize, u16),
    pub to: (usize, u16),
}

impl Drag {
    fn ordered(&self) -> ((usize, u16), (usize, u16)) {
        if self.from <= self.to {
            (self.from, self.to)
        } else {
            (self.to, self.from)
        }
    }

    /// Columns `[a, b)` of document row `r` the selection covers, if any.
    pub fn span(&self, r: usize, width: u16) -> Option<(u16, u16)> {
        let ((r0, c0), (r1, c1)) = self.ordered();
        if r < r0 || r > r1 {
            return None;
        }
        let a = if r == r0 { c0 } else { 0 };
        let b = if r == r1 { c1 + 1 } else { width };
        (b > a).then_some((a, b))
    }
}

/// Invert the selected cells (`bg_base` on `text_primary`; reverse video on bandless themes).
pub fn paint_drag(buf: &mut Buffer, view: &View, lay: &Layout, th: &Theme) {
    let Some(d) = view.nav.drag else { return };
    let top = view.top();
    let vh = lay.view.height as usize;
    for vy in 0..vh {
        let r = top + vy;
        if let Some((a, b)) = d.span(r, lay.w.saturating_sub(1)) {
            let y = lay.view.y + vy as u16;
            let rect = Rect::new(a, y, b - a, 1);
            if th.bandless {
                buf.set_style(rect, Style::new().add_modifier(Modifier::REVERSED));
            } else {
                buf.set_style(rect, Style::new().fg(th.bg_base).bg(th.text_primary));
            }
        }
    }
}

impl View {
    /// The text under a selection, rows joined by newlines. Gaps between blocks are blank lines.
    pub fn selected_text(&self, d: Drag) -> String {
        let ((r0, _), (r1, _)) = d.ordered();
        let mut out: Vec<String> = Vec::new();
        for r in r0..=r1.min(self.doc.total.saturating_sub(1)) {
            let Some((a, b)) = d.span(r, u16::MAX) else {
                continue;
            };
            let line = match self.doc.locate(r) {
                Some((i, ri)) => {
                    let cs: Vec<Cell> = cells(&self.doc.entries[i].rows[ri])
                        .into_iter()
                        .filter(|c| c.0 >= a && c.0 < b)
                        .collect();
                    plain(&cs, true)
                }
                None => String::new(),
            };
            out.push(line);
        }
        out.join("\n").trim_end().to_string()
    }

    /// The text of a whole entry, as drawn, for entries a `y` cannot read from the transcript.
    pub fn entry_text(&self, key: EntryKey) -> String {
        let Some(i) = self.doc.index_of(key) else {
            return String::new();
        };
        self.doc.entries[i]
            .rows
            .iter()
            .map(|r| plain(&cells(r), true))
            .collect::<Vec<_>>()
            .join("\n")
            .trim_end()
            .to_string()
    }

    /// Document row under a screen row, if the screen row is in the viewport.
    pub fn row_at(&self, lay: &Layout, y: u16) -> Option<usize> {
        if y < lay.view.y || y >= lay.view.y + lay.view.height {
            return None;
        }
        let r = self.top() + (y - lay.view.y) as usize;
        (r < self.doc.total).then_some(r)
    }

    /// Previous or next user prompt relative to the entry `from` (or the viewport top).
    pub fn prompt_near(&self, forward: bool) -> Option<EntryKey> {
        let prompts: Vec<usize> = self
            .doc
            .entries
            .iter()
            .enumerate()
            .filter(|(_, e)| e.kind == Kind::User)
            .map(|(i, _)| i)
            .collect();
        let here = self.selected.and_then(|k| self.doc.index_of(k));
        let at = match here {
            Some(i) => i,
            None => {
                let top = self.top();
                *prompts.iter().rev().find(|&&i| self.doc.starts[i] <= top)?
            }
        };
        let next = if forward {
            prompts.iter().copied().find(|&i| i > at)
        } else {
            prompts.iter().copied().rev().find(|&i| i < at)
        }?;
        Some(self.doc.entries[next].key)
    }
}

// ---- copy ---------------------------------------------------------------------------------

/// What `y` (the content) or `Y` (the command, path, URL, pattern or query) copies for the
/// selected entry, read from the transcript rather than from what is drawn.
pub fn entry_copy_text(app: &App, key: EntryKey, meta: bool) -> Option<String> {
    use agent_core::transcript::{Part, Role};
    let m = app.tr.messages.get(key.0)?;
    if m.role == Role::User {
        return match m.parts.first() {
            Some(Part::Text(t)) => Some(t.clone()),
            _ => None,
        };
    }
    match m.parts.get(key.1)? {
        Part::Text(t) => Some(t.clone()),
        Part::Thought { text, .. } => Some(text.clone()),
        Part::Tool(c) => {
            use super::tools::{classify, Class};
            let input = |keys: &[&str]| {
                keys.iter()
                    .find_map(|k| c.input.get(*k).and_then(|v| v.as_str()))
                    .map(str::to_string)
            };
            let class = classify(c)?;
            if meta {
                return match class {
                    Class::Execute => Some(super::tools::command_of(c)),
                    Class::Read | Class::List | Class::Edit => {
                        input(&["path", "file_path", "file"])
                            .or_else(|| c.diff.as_ref().map(|d| d.path.clone()))
                    }
                    Class::Fetch => input(&["url"]),
                    Class::WebSearch => input(&["query"]),
                    Class::Search => input(&["pattern", "query"]),
                    _ => None,
                };
            }
            match (class, &c.diff) {
                (Class::Edit, Some(d)) => {
                    let old = d.old.as_deref().unwrap_or("");
                    Some(
                        similar::TextDiff::from_lines(old, &d.new)
                            .unified_diff()
                            .header(&format!("a/{}", d.path), &format!("b/{}", d.path))
                            .to_string(),
                    )
                }
                _ => c.output.clone().filter(|o| !o.is_empty()),
            }
        }
    }
}

/// Copy the selected entry and say so. A group of rows has no source of its own, so it copies
/// what is drawn.
pub fn copy_selected(app: &mut App, meta: bool) {
    let Some(key) = app.view.target() else { return };
    let text = entry_copy_text(app, key, meta).unwrap_or_else(|| app.view.entry_text(key));
    if text.is_empty() {
        app.toast("Nothing to copy");
        return;
    }
    crate::app::copy_to_clipboard(&text);
    app.toast_for("Copied!", 30);
}

// ---- App-level actions the keys call --------------------------------------------------------

impl App {
    /// Select the next or previous user prompt.
    pub fn view_prompt(&mut self, forward: bool) {
        if let Some(k) = self.view.prompt_near(forward) {
            self.view.selected = Some(k);
            self.view.reveal();
        }
    }

    /// `J`/`K`: bring the next or previous turn to the top of the viewport.
    pub fn view_turn(&mut self, forward: bool) {
        if let Some(k) = self.view.prompt_near(forward) {
            self.view.selected = Some(k);
            if let Some(i) = self.view.doc.index_of(k) {
                let to = self.view.doc.starts[i];
                self.view.offset = Some(to.min(self.view.max_offset()));
                self.view.flip = None;
            }
        }
    }

    /// `E`: expand everything foldable, or collapse it all when everything is open.
    pub fn view_fold_all(&mut self) {
        let any_closed = self
            .view
            .doc
            .entries
            .iter()
            .any(|e| e.foldable && e.collapsed);
        let keys: Vec<EntryKey> = self
            .view
            .doc
            .entries
            .iter()
            .filter(|e| e.foldable)
            .map(|e| e.key)
            .collect();
        for k in keys {
            self.view.folds.insert(k, any_closed);
        }
    }

    /// `r`: the selected message as its markdown source, and back.
    pub fn view_raw(&mut self) {
        let Some(k) = self.view.selected else { return };
        let is_msg = self
            .view
            .doc
            .index_of(k)
            .is_some_and(|i| self.view.doc.entries[i].kind == Kind::Agent);
        if is_msg && !self.view.nav.raw.remove(&k) {
            self.view.nav.raw.insert(k);
        }
    }

    /// `Enter`: open the block viewer; a group header or a prompt folds instead.
    pub fn view_open_selected(&mut self) {
        // a member of an open group opens its own block
        if self.view.member().is_some() {
            if let Some(k) = self.view.target() {
                self.view.nav.viewer = Some(super::transcript_viewer::Viewer::new(k));
            }
            return;
        }
        let Some(k) = self.view.selected else { return };
        let Some(i) = self.view.doc.index_of(k) else {
            return;
        };
        match self.view.doc.entries[i].kind {
            Kind::Group | Kind::User => self.view.fold(None),
            _ => self.view.nav.viewer = Some(super::transcript_viewer::Viewer::new(k)),
        }
    }

    pub fn viewer_close(&mut self) {
        self.view.nav.viewer = None;
    }

    /// `Enter` in the viewer: put the block in the composer as a quote and go there.
    pub fn viewer_quote(&mut self) {
        let Some(v) = self.view.nav.viewer.take() else {
            return;
        };
        let text = super::transcript_viewer::text_of(&v.rows);
        let quoted: String = text.lines().map(|l| format!("> {l}\n")).collect::<String>() + "\n";
        self.ed.paste(&quoted);
        self.focus = Focus::Prompt;
        self.view.selected = None;
    }
}
