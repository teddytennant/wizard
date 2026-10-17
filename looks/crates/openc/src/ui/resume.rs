// OWNER: input
//! The resume dialog: two-line rows (title; age, messages, branch, size, directory), a fuzzy
//! filter, `space` for a preview, `ctrl+r` rename, `ctrl+d` delete, `ctrl+a` all projects and
//! `ctrl+b` this branch only. Rows come from `backend_claude::sessions`.

use std::collections::HashMap;
use std::path::PathBuf;

use backend_claude::sessions::SessionDetail;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use tuikit::editor::{Editor, EditorStyle};
use tuikit::paint::{fill, put_str};
use tuikit::width::{display_width, truncate, wrap};

use crate::ui::dialogs::{footer, panel, title, OvCtx, INSET};
use crate::ui::status::short_path;
use crate::ui::transcript::age_text;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Row {
    pub id: String,
    pub title: String,
    pub first_prompt: String,
    pub cwd: String,
    pub branch: String,
    pub updated: i64,
    pub bytes: u64,
    pub messages: usize,
    /// The count was extrapolated from a large file.
    pub approx: bool,
    pub path: Option<PathBuf>,
}

impl From<SessionDetail> for Row {
    fn from(d: SessionDetail) -> Row {
        Row {
            id: d.id,
            title: d.title,
            first_prompt: d.first_prompt,
            cwd: d.cwd,
            branch: d.branch,
            updated: d.updated,
            bytes: d.bytes,
            messages: d.messages,
            approx: d.approx,
            path: Some(d.path),
        }
    }
}

impl From<&agent_core::SessionInfo> for Row {
    fn from(s: &agent_core::SessionInfo) -> Row {
        Row {
            id: s.id.clone(),
            title: s.title.replace('\n', " "),
            cwd: s.cwd.clone(),
            updated: s.updated,
            ..Row::default()
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum ResumeOut {
    None,
    Close,
    /// Resume this session. `other_dir` when it belongs to another working directory, which
    /// the running backend cannot load.
    Pick {
        id: String,
        cwd: String,
        other_dir: bool,
    },
    Rename {
        id: String,
        path: PathBuf,
        title: String,
    },
    Delete {
        id: String,
        path: PathBuf,
    },
    /// `ctrl+a` pressed before the all-projects list has arrived.
    NeedAll,
}

enum Mode {
    Normal,
    Rename(Box<Editor>),
    ConfirmDelete,
}

const HINTS: &[(&str, &str)] = &[
    ("enter", "resume"),
    ("space", "preview"),
    ("ctrl+r", "rename"),
    ("ctrl+d", "delete"),
    ("ctrl+a", "all projects"),
    ("ctrl+b", "branch"),
];

pub struct Resume {
    this: Vec<Row>,
    all: Vec<Row>,
    all_loaded: bool,
    all_requested: bool,
    loaded: bool,
    pub show_all: bool,
    pub branch_only: bool,
    branch: String,
    cwd: String,
    current: String,
    now: i64,
    q: Editor,
    sel: usize,
    top: usize,
    /// Indices into the active list that pass the filter, with matched title chars.
    visible: Vec<(usize, Vec<usize>)>,
    mode: Mode,
    preview_open: bool,
    previews: HashMap<String, Vec<(bool, String)>>,
    note: Option<String>,
    // Filled while drawing, read by the mouse handler.
    list_rect: Rect,
    page: usize,
}

const ROW_H: u16 = 2;

impl Resume {
    pub fn new(cwd: &str, current: &str, branch: &str, now: i64) -> Resume {
        Resume {
            this: Vec::new(),
            all: Vec::new(),
            all_loaded: false,
            all_requested: false,
            loaded: false,
            show_all: false,
            branch_only: false,
            branch: branch.to_string(),
            cwd: cwd.to_string(),
            current: current.to_string(),
            now,
            q: Editor::new(),
            sel: 0,
            top: 0,
            visible: Vec::new(),
            mode: Mode::Normal,
            preview_open: false,
            previews: HashMap::new(),
            note: None,
            list_rect: Rect::default(),
            page: 5,
        }
    }

    /// Rows for this directory (`all == false`) or every project, replacing what was there.
    pub fn set_rows(&mut self, rows: Vec<Row>, all: bool) {
        if all {
            self.all.clear();
        } else {
            self.this.clear();
        }
        self.add_rows(rows, all, true);
    }

    /// A chunk of rows from the loader; `last` when no more are coming.
    pub fn add_rows(&mut self, mut rows: Vec<Row>, all: bool, last: bool) {
        let list = if all { &mut self.all } else { &mut self.this };
        if last && list.is_empty() {
            *list = rows;
        } else {
            list.append(&mut rows);
        }
        if last {
            if all {
                self.all_loaded = true;
            } else {
                self.loaded = true;
            }
        }
        // Keep newest first as chunks from several project directories interleave.
        let list = if all { &mut self.all } else { &mut self.this };
        list.sort_by_key(|r| std::cmp::Reverse(r.updated));
        self.refilter();
    }

    pub fn all_loaded(&self) -> bool {
        self.all_loaded
    }

    fn rows(&self) -> &[Row] {
        if self.show_all {
            &self.all
        } else {
            &self.this
        }
    }

    fn selected_row(&self) -> Option<&Row> {
        self.visible
            .get(self.sel)
            .and_then(|(i, _)| self.rows().get(*i))
    }

    pub fn selected_id(&self) -> Option<&str> {
        self.selected_row().map(|r| r.id.as_str())
    }

    pub fn paste(&mut self, s: &str) {
        match &mut self.mode {
            Mode::Rename(ed) => ed.paste(s),
            _ => {
                self.q.paste(&s.replace('\n', " "));
                self.refilter();
            }
        }
    }

    pub fn flash(&mut self, msg: impl Into<String>) {
        self.note = Some(msg.into());
    }

    /// Apply a rename or delete that the app carried out.
    pub fn renamed(&mut self, id: &str, title: &str) {
        for list in [&mut self.this, &mut self.all] {
            for r in list.iter_mut().filter(|r| r.id == id) {
                r.title = title.to_string();
            }
        }
        self.note = Some("renamed".into());
        self.refilter();
    }

    pub fn deleted(&mut self, id: &str) {
        self.this.retain(|r| r.id != id);
        self.all.retain(|r| r.id != id);
        self.previews.remove(id);
        self.note = Some("deleted".into());
        self.refilter();
    }

    fn refilter(&mut self) {
        let keep = self.selected_id().map(str::to_string);
        let terms: Vec<String> = self
            .q
            .text()
            .split_whitespace()
            .map(str::to_string)
            .collect();
        let mut v: Vec<(usize, i32, Vec<usize>)> = Vec::new();
        let branch = self.branch.clone();
        for (i, r) in self.rows().iter().enumerate() {
            if self.branch_only && r.branch != branch {
                continue;
            }
            let mut total = 0;
            let mut idx: Vec<usize> = Vec::new();
            let mut ok = true;
            for t in &terms {
                if let Some(m) = tuikit::fuzzy::score(t, &r.title) {
                    total += m.score + 20;
                    idx.extend(m.indices);
                } else if let Some(m) = tuikit::fuzzy::score(t, &r.first_prompt)
                    .or_else(|| tuikit::fuzzy::score(t, &r.cwd))
                    .or_else(|| tuikit::fuzzy::score(t, &r.branch))
                {
                    total += m.score;
                } else {
                    ok = false;
                    break;
                }
            }
            if ok {
                idx.sort_unstable();
                idx.dedup();
                v.push((i, total, idx));
            }
        }
        if !terms.is_empty() {
            // Best match first; the list is already newest first, so ties keep that.
            v.sort_by_key(|a| std::cmp::Reverse(a.1));
        }
        self.visible = v.into_iter().map(|(i, _, idx)| (i, idx)).collect();
        // Stay on the same session when it survived the filter.
        self.sel = keep
            .and_then(|id| {
                self.visible
                    .iter()
                    .position(|(i, _)| self.rows()[*i].id == id)
            })
            .unwrap_or(0);
        self.top = 0;
    }

    fn move_by(&mut self, d: isize) {
        let n = self.visible.len();
        if n == 0 {
            return;
        }
        self.sel = (self.sel as isize + d).clamp(0, n as isize - 1) as usize;
    }

    pub fn on_key(&mut self, key: KeyEvent) -> ResumeOut {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        self.note = None;
        match &mut self.mode {
            Mode::Rename(ed) => {
                match key.code {
                    KeyCode::Esc => self.mode = Mode::Normal,
                    KeyCode::Enter => {
                        let title = ed.text().trim().to_string();
                        self.mode = Mode::Normal;
                        if !title.is_empty() {
                            if let Some(r) = self.selected_row() {
                                if let Some(path) = r.path.clone() {
                                    return ResumeOut::Rename {
                                        id: r.id.clone(),
                                        path,
                                        title,
                                    };
                                }
                            }
                        }
                    }
                    _ => {
                        ed.apply_key(key);
                    }
                }
                return ResumeOut::None;
            }
            Mode::ConfirmDelete => {
                let yes = matches!(key.code, KeyCode::Char('y' | 'Y') | KeyCode::Enter);
                self.mode = Mode::Normal;
                if yes {
                    if let Some(r) = self.selected_row() {
                        if let Some(path) = r.path.clone() {
                            return ResumeOut::Delete {
                                id: r.id.clone(),
                                path,
                            };
                        }
                    }
                }
                return ResumeOut::None;
            }
            Mode::Normal => {}
        }
        match key.code {
            KeyCode::Esc => return ResumeOut::Close,
            KeyCode::Enter => {
                if let Some(r) = self.selected_row() {
                    return ResumeOut::Pick {
                        id: r.id.clone(),
                        cwd: r.cwd.clone(),
                        other_dir: r.path.is_some() && !r.cwd.is_empty() && r.cwd != self.cwd,
                    };
                }
            }
            KeyCode::Up => self.move_by(-1),
            KeyCode::Down => self.move_by(1),
            KeyCode::Char('p') if ctrl => self.move_by(-1),
            KeyCode::Char('n') if ctrl => self.move_by(1),
            KeyCode::PageUp => self.move_by(-(self.page as isize)),
            KeyCode::PageDown => self.move_by(self.page as isize),
            KeyCode::Char('r') if ctrl => {
                if let Some(r) = self.selected_row().filter(|r| r.path.is_some()) {
                    self.mode = Mode::Rename(Box::new(Editor::with_text(&r.title.clone())));
                } else {
                    self.note = Some("this backend cannot rename sessions".into());
                }
            }
            KeyCode::Char('d') if ctrl => {
                if self.selected_row().is_some_and(|r| r.path.is_some()) {
                    self.mode = Mode::ConfirmDelete;
                }
            }
            KeyCode::Char('a') if ctrl => {
                self.show_all = !self.show_all;
                self.refilter();
                if self.show_all && !self.all_loaded && !self.all_requested {
                    self.all_requested = true;
                    return ResumeOut::NeedAll;
                }
            }
            KeyCode::Char('b') if ctrl => {
                self.branch_only = !self.branch_only;
                self.refilter();
            }
            KeyCode::Char(' ') if !ctrl && self.q.is_empty() => {
                self.preview_open = !self.preview_open;
                self.load_preview();
            }
            _ => {
                self.q.apply_key(key);
                self.refilter();
            }
        }
        if self.preview_open {
            self.load_preview();
        }
        ResumeOut::None
    }

    pub fn on_mouse(&mut self, ev: MouseEvent) -> ResumeOut {
        match ev.kind {
            MouseEventKind::ScrollUp => self.move_by(-3),
            MouseEventKind::ScrollDown => self.move_by(3),
            MouseEventKind::Down(MouseButton::Left) => {
                let r = self.list_rect;
                if ev.column >= r.x && ev.column < r.right() && ev.row >= r.y && ev.row < r.bottom()
                {
                    let n = ((ev.row - r.y) / ROW_H) as usize + self.top;
                    if n < self.visible.len() {
                        if self.sel == n {
                            return self.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
                        }
                        self.sel = n;
                    }
                }
            }
            _ => {}
        }
        if self.preview_open {
            self.load_preview();
        }
        ResumeOut::None
    }

    fn load_preview(&mut self) {
        let Some(r) = self.selected_row() else { return };
        if self.previews.contains_key(&r.id) {
            return;
        }
        let id = r.id.clone();
        let items = match &r.path {
            // A transcript this large would stall the keypress; the title will have to do.
            Some(p) if r.bytes <= 24 * 1024 * 1024 => backend_claude::sessions::preview(p, 5, 400),
            _ => Vec::new(),
        };
        self.previews.insert(id, items);
    }

    fn size_text(b: u64) -> String {
        match b {
            0..=1023 => format!("{b} B"),
            1024..=1_048_575 => format!("{} KB", b / 1024),
            _ => format!("{:.1} MB", b as f64 / 1_048_576.0),
        }
    }

    /// Second line of a row: age, messages, branch, size, directory.
    fn meta(&self, r: &Row) -> String {
        let mut parts: Vec<String> = Vec::new();
        if r.updated > 0 {
            parts.push(age_text(self.now - r.updated));
        }
        if r.messages > 0 {
            let tilde = if r.approx { "~" } else { "" };
            parts.push(format!("{tilde}{} msg", r.messages));
        }
        if !r.branch.is_empty() {
            parts.push(r.branch.clone());
        }
        if r.bytes > 0 {
            parts.push(Self::size_text(r.bytes));
        }
        if self.show_all || (!r.cwd.is_empty() && r.cwd != self.cwd) {
            parts.push(short_path(&r.cwd, 36));
        }
        parts.join(" · ")
    }

    pub fn draw(&mut self, buf: &mut Buffer, screen: Rect, cx: &OvCtx) -> Option<(u16, u16)> {
        let p = cx.p;
        // Tall enough for the sessions there are, up to two thirds of the screen: a dialog of
        // three sessions in twenty rows was eight empty rows (round 2 finding 16). It counts
        // the sessions in scope, not the ones the search leaves, so typing does not resize it.
        let max_h = (screen.height * 2 / 3).clamp(8, 28);
        let foot = crate::ui::dialogs::footer_lines(crate::ui::dialogs::dlg_width(screen), HINTS)
            .len() as u16;
        let wanted = 5 + ROW_H * self.rows().len().max(1) as u16 + foot + 1;
        let h = wanted.clamp(8, max_h);
        let r = panel(buf, screen, h, cx);
        let bg = p.raised;
        let faint = Style::new().fg(p.faint).bg(bg);
        title(buf, r, "Resume a session", cx);
        let scope = if self.show_all {
            "all projects"
        } else {
            "this directory"
        };
        let scope = if self.branch_only {
            format!("{scope}, branch {}", self.branch)
        } else {
            scope.to_string()
        };
        // Search row.
        let sy = r.y + 3;
        let renaming = matches!(self.mode, Mode::Rename(_));
        let label = if renaming { "rename " } else { "" };
        put_str(
            buf,
            r.x + 2,
            sy,
            cx.g.bar,
            Style::new().fg(p.accent).bg(bg),
            r,
        );
        let lx = put_str(
            buf,
            r.x + INSET,
            sy,
            label,
            Style::new().fg(p.accent).bg(bg),
            r,
        );
        let done = if self.show_all {
            self.all_loaded
        } else {
            self.loaded
        };
        let right = if done {
            format!("{} of {}  {scope}", self.visible.len(), self.rows().len())
        } else {
            format!("{} so far  {scope}", self.rows().len())
        };
        let rw = display_width(&right) as u16;
        let st = EditorStyle {
            text: Style::new().fg(p.text).bg(bg),
            placeholder: Some(("Search sessions".into(), faint)),
        };
        let in_w = r.width.saturating_sub(lx - r.x + INSET + rw + 2);
        let area = Rect::new(lx, sy, in_w, 1);
        let ed: &mut Editor = match &mut self.mode {
            Mode::Rename(e) => e,
            _ => &mut self.q,
        };
        let cursor = ed.render(area, buf, &st).cursor;
        put_str(
            buf,
            r.right().saturating_sub(rw + INSET),
            sy,
            &right,
            faint,
            r,
        );

        // List and preview areas.
        let footer_y = r.bottom().saturating_sub(2);
        let top = sy + 2;
        // The hints take as many rows as they need at this width; the list gives them up.
        let foot_rows = crate::ui::dialogs::footer_lines(r.width, HINTS).len() as u16;
        let list_h = footer_y.saturating_sub(top + foot_rows);
        let split = self.preview_open && r.width >= 90;
        let list_w = if split {
            46
        } else {
            r.width.saturating_sub(2 * INSET)
        };
        let list = Rect::new(r.x + INSET, top, list_w, list_h);
        self.list_rect = list;
        let fit = (list_h / ROW_H).max(1) as usize;
        self.page = fit;
        if self.sel < self.top {
            self.top = self.sel;
        } else if self.sel >= self.top + fit {
            self.top = self.sel + 1 - fit;
        }
        if self.visible.is_empty() {
            let msg = if !done {
                "reading sessions..."
            } else if self.rows().is_empty() {
                "no sessions here yet"
            } else {
                "nothing matches"
            };
            put_str(buf, list.x, list.y, msg, faint, r);
        }
        let reverse = matches!(
            p.depth,
            crate::palette::Depth::Ansi16 | crate::palette::Depth::Mono
        );
        let rows = self.rows().to_vec();
        for (n, (i, idx)) in self.visible.iter().enumerate().skip(self.top).take(fit) {
            let row = &rows[*i];
            let y = list.y + ((n - self.top) as u16) * ROW_H;
            let sel = n == self.sel;
            let rbg = if sel { p.line } else { bg };
            let mut base = Style::new().bg(rbg);
            if sel && reverse {
                base = base.add_modifier(Modifier::REVERSED);
            }
            fill(
                buf,
                Rect::new(r.x + 1, y, r.width.saturating_sub(2), ROW_H),
                base,
            );
            put_str(
                buf,
                r.x + 2,
                y,
                if sel { cx.g.select } else { " " },
                base.fg(p.accent),
                r,
            );
            let tw = list.width.saturating_sub(2) as usize;
            let shown = truncate(&row.title, tw);
            let mut x = list.x;
            // By grapheme, with the match indices still counted in chars: drawing one char at a
            // time cut a family emoji into three people.
            let mut ci = 0;
            for g in unicode_segmentation::UnicodeSegmentation::graphemes(shown.as_str(), true) {
                let n = g.chars().count();
                let st = if (ci..ci + n).any(|i| idx.contains(&i)) {
                    base.fg(p.accent).add_modifier(Modifier::BOLD)
                } else {
                    base.fg(p.text)
                };
                ci += n;
                x = put_str(buf, x, y, g, st, r);
            }
            if row.id == self.current {
                let t = "current";
                let tx = list.right().saturating_sub(display_width(t) as u16 + 1);
                put_str(buf, tx, y, t, base.fg(if sel { p.dim } else { p.faint }), r);
            }
            let meta = if sel && matches!(self.mode, Mode::ConfirmDelete) {
                "delete this session for good?  y delete   any other key keeps it".to_string()
            } else {
                self.meta(row)
            };
            let mstyle = if sel && matches!(self.mode, Mode::ConfirmDelete) {
                base.fg(p.warn)
            } else {
                // `faint` on the selection step is 4.06:1; `dim` clears 4.5.
                base.fg(if sel { p.dim } else { p.faint })
            };
            put_str(buf, list.x, y + 1, &truncate(&meta, tw), mstyle, r);
        }
        // Preview.
        if self.preview_open {
            let area = if split {
                Rect::new(
                    list.right() + 2,
                    top,
                    r.right().saturating_sub(list.right() + 4),
                    list_h,
                )
            } else {
                // Narrow: the preview takes the list's place.
                fill(buf, list, Style::new().bg(bg));
                list
            };
            self.draw_preview(buf, area, cx);
        }
        // Footer.
        let hints = HINTS;
        if let Some(n) = &self.note {
            put_str(
                buf,
                r.x + INSET,
                footer_y,
                n,
                Style::new().fg(p.warn).bg(bg),
                r,
            );
        } else {
            footer(buf, r, footer_y, hints, cx);
        }
        if matches!(self.mode, Mode::Rename(_)) {
            Some(cursor.unwrap_or((area.x, sy)))
        } else {
            cursor
        }
    }

    fn draw_preview(&self, buf: &mut Buffer, area: Rect, cx: &OvCtx) {
        let p = cx.p;
        let bg = p.raised;
        let Some(row) = self.selected_row() else {
            return;
        };
        if area.width < 10 || area.height == 0 {
            return;
        }
        let w = area.width as usize;
        let mut y = area.y;
        let items = self.previews.get(&row.id);
        let put = |buf: &mut Buffer, y: &mut u16, s: &str, st: Style| {
            if *y < area.bottom() {
                put_str(buf, area.x, *y, s, st, area);
                *y += 1;
            }
        };
        match items {
            Some(items) if !items.is_empty() => {
                let mut prev_user_first = true;
                for (n, (user, text)) in items.iter().enumerate() {
                    // A gap after the first prompt marks that the middle is left out.
                    if n == 1 && prev_user_first && items.len() > 1 && row.messages > items.len() {
                        put(buf, &mut y, "...", Style::new().fg(p.faint).bg(bg));
                    }
                    prev_user_first = false;
                    let who = if *user { "you" } else { "claude" };
                    put(
                        buf,
                        &mut y,
                        who,
                        Style::new()
                            .fg(if *user { p.user } else { p.accent })
                            .bg(bg)
                            .add_modifier(Modifier::BOLD),
                    );
                    for l in wrap(text, w).into_iter().take(3) {
                        put(buf, &mut y, &l, Style::new().fg(p.dim).bg(bg));
                    }
                    y += 1;
                }
            }
            _ => {
                let t = if row.first_prompt.is_empty() {
                    "no preview for this session"
                } else {
                    &row.first_prompt
                };
                for l in wrap(t, w).into_iter().take(6) {
                    put(buf, &mut y, &l, Style::new().fg(p.dim).bg(bg));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(id: &str, title: &str, ago: i64, branch: &str, cwd: &str) -> Row {
        Row {
            id: id.into(),
            title: title.into(),
            first_prompt: format!("first prompt of {title}"),
            cwd: cwd.into(),
            branch: branch.into(),
            updated: 1_000_000 - ago,
            bytes: 2048,
            messages: 7,
            approx: false,
            path: Some(PathBuf::from(format!("/nope/{id}.jsonl"))),
        }
    }

    fn dlg() -> Resume {
        let mut d = Resume::new("/w/a", "s1", "main", 1_000_000);
        d.set_rows(
            vec![
                row("s1", "Fix the parser", 60, "main", "/w/a"),
                row("s2", "Add a json flag", 7200, "feat", "/w/a"),
                row("s3", "Explain the build graph", 90_000, "main", "/w/a"),
            ],
            false,
        );
        d
    }

    fn key(c: KeyCode) -> KeyEvent {
        KeyEvent::new(c, KeyModifiers::NONE)
    }

    fn ctrl(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
    }

    fn type_str(d: &mut Resume, s: &str) {
        for ch in s.chars() {
            d.on_key(key(KeyCode::Char(ch)));
        }
    }

    #[test]
    fn filter_is_fuzzy_over_the_title_and_enter_picks() {
        let mut d = dlg();
        type_str(&mut d, "bld");
        assert_eq!(d.selected_id(), Some("s3"));
        assert_eq!(
            d.on_key(key(KeyCode::Enter)),
            ResumeOut::Pick {
                id: "s3".into(),
                cwd: "/w/a".into(),
                other_dir: false
            }
        );
    }

    #[test]
    fn branch_filter_and_all_projects_toggle() {
        let mut d = dlg();
        d.on_key(ctrl('b'));
        assert_eq!(d.visible.len(), 2, "only main");
        d.on_key(ctrl('b'));
        assert_eq!(d.visible.len(), 3);
        assert_eq!(d.on_key(ctrl('a')), ResumeOut::NeedAll);
        d.set_rows(vec![row("x", "Other project", 10, "main", "/w/b")], true);
        assert_eq!(d.selected_id(), Some("x"));
        assert_eq!(
            d.on_key(key(KeyCode::Enter)),
            ResumeOut::Pick {
                id: "x".into(),
                cwd: "/w/b".into(),
                other_dir: true
            }
        );
    }

    #[test]
    fn rename_edits_the_title_and_reports_it() {
        let mut d = dlg();
        d.on_key(ctrl('r'));
        for _ in 0..20 {
            d.on_key(key(KeyCode::Backspace));
        }
        type_str(&mut d, "New name");
        match d.on_key(key(KeyCode::Enter)) {
            ResumeOut::Rename { id, title, .. } => {
                assert_eq!((id.as_str(), title.as_str()), ("s1", "New name"))
            }
            o => panic!("{o:?}"),
        }
        d.renamed("s1", "New name");
        assert_eq!(d.rows()[0].title, "New name");
    }

    #[test]
    fn delete_needs_a_confirming_y() {
        let mut d = dlg();
        d.on_key(ctrl('d'));
        assert_eq!(d.on_key(key(KeyCode::Char('n'))), ResumeOut::None);
        assert!(matches!(d.mode, Mode::Normal));
        d.on_key(ctrl('d'));
        assert!(matches!(
            d.on_key(key(KeyCode::Char('y'))),
            ResumeOut::Delete { .. }
        ));
        d.deleted("s1");
        assert_eq!(d.visible.len(), 2);
    }

    #[test]
    fn space_previews_only_while_the_search_is_empty() {
        let mut d = dlg();
        d.on_key(key(KeyCode::Char(' ')));
        assert!(d.preview_open);
        d.on_key(key(KeyCode::Char(' ')));
        assert!(!d.preview_open);
        type_str(&mut d, "build graph");
        assert!(!d.preview_open, "a space inside a query is a space");
        assert_eq!(d.visible.len(), 1);
    }

    #[test]
    fn escape_closes() {
        assert_eq!(dlg().on_key(key(KeyCode::Esc)), ResumeOut::Close);
    }
}
