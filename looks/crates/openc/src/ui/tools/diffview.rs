//! The full-screen diff viewer: every file an agent changed, one at a time, unified or split
//! by width, with hunk and file navigation. Opened with `d` in nav mode, `/diff`, or `d` on a
//! pending edit in the permission panel; `esc` goes back to where you were.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseEvent, MouseEventKind};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Span;
use tuikit::paint::{fill, put_str};
use tuikit::width::{display_width, spans_width, truncate_left};

use super::diff::{highlight_rows, number_width, row_look, styled_code};
use super::{DKind, DRow, DiffData, DiffOpts, ToolEntry};
use crate::ui::row::{cut, sp, spaces, Cx, Row};

/// Whether every changed line of `f` fits in one half of a split body `bw` wide.
fn split_fits(f: &FileChange, bw: usize) -> bool {
    let half = bw.saturating_sub(1) / 2;
    f.hunks.iter().all(|d| {
        let gutter = if d.numbered { number_width(d) + 4 } else { 3 };
        let room = half.saturating_sub(gutter + 1);
        d.rows
            .iter()
            .filter(|r| !matches!(r.kind, DKind::Gap(_)))
            .all(|r| display_width(&r.text) <= room)
    })
}

/// Body width from which `auto` picks side by side.
const SPLIT_FROM: usize = 110;
/// Terminal width from which the file list gets a column of its own.
const LIST_FROM: usize = 100;
const CONTEXT: usize = 3;

/// All the edits of one file, in the order they happened.
#[derive(Clone, Debug)]
pub struct FileChange {
    pub path: String,
    pub new_file: bool,
    pub hunks: Vec<DiffData>,
    /// The tool call behind each hunk, parallel to `hunks`.
    pub ids: Vec<String>,
    pub added: usize,
    pub removed: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LayoutPref {
    Auto,
    Unified,
    Split,
}

#[derive(Debug, PartialEq, Eq)]
pub enum DvOut {
    None,
    Close,
}

/// A laid-out file: rows plus where each hunk header sits.
struct Rendered {
    key: (usize, usize, bool, bool),
    rows: Vec<Row>,
    hunks: Vec<usize>,
}

pub struct DiffView {
    pub title: String,
    pub files: Vec<FileChange>,
    pub sel: usize,
    pub scroll: usize,
    pub pref: LayoutPref,
    /// Opened from a permission panel for an edit that has not happened yet.
    pub pending: bool,
    /// The project directory; paths under it are shown relative.
    pub cwd: String,
    /// Hunk to scroll to on the next draw.
    want_hunk: Option<usize>,
    cache: Option<Rendered>,
    /// Body height of the last frame, for the page keys.
    vh: usize,
    /// File list rows of the last frame as `(y, file)`, for clicks.
    list_hits: Vec<(u16, usize)>,
}

/// Columns left empty at each side of the viewer.
const MARGIN: u16 = 2;

/// File changes out of tool entries (and their subagent calls) in transcript order. Only calls
/// that finished and were not refused count.
pub fn collect<'a>(entries: impl Iterator<Item = &'a ToolEntry>) -> Vec<FileChange> {
    let mut out: Vec<FileChange> = Vec::new();
    let mut add = |e: &ToolEntry| {
        let Some(d) = &e.call.diff else { return };
        if !e.done() {
            return;
        }
        let full = e.verb() == "Write";
        let data = DiffData::build_with(
            d.old.as_deref(),
            &d.new,
            &d.path,
            true,
            DiffOpts {
                context: CONTEXT,
                full,
            },
        );
        match out.iter_mut().find(|f| f.path == d.path) {
            Some(f) => {
                f.added += data.added;
                f.removed += data.removed;
                f.hunks.push(data);
                f.ids.push(e.call.id.clone());
            }
            None => out.push(FileChange {
                path: d.path.clone(),
                new_file: data.new_file,
                added: data.added,
                removed: data.removed,
                hunks: vec![data],
                ids: vec![e.call.id.clone()],
            }),
        }
    };
    for e in entries {
        add(e);
        for c in &e.children {
            add(c);
        }
    }
    out
}

/// Old and new line numbers of every row of a hunk. Unchanged lines advance both; a removed
/// line only the old side, an added one only the new.
fn numbers(d: &DiffData) -> Vec<(Option<usize>, Option<usize>)> {
    let first = d
        .rows
        .iter()
        .find(|r| !matches!(r.kind, DKind::Gap(_)))
        .map_or(1, |r| r.no);
    let (mut o, mut n) = (first, first);
    d.rows
        .iter()
        .map(|r| match r.kind {
            DKind::Ctx => {
                let v = (Some(o), Some(n));
                o += 1;
                n += 1;
                v
            }
            DKind::Del => {
                o += 1;
                (Some(o - 1), None)
            }
            DKind::Add => {
                n += 1;
                (None, Some(n - 1))
            }
            DKind::Gap(g) => {
                o += g;
                n += g;
                (None, None)
            }
        })
        .collect()
}

fn hunk_label(
    i: usize,
    n: usize,
    d: &DiffData,
    nums: &[(Option<usize>, Option<usize>)],
    sep: &str,
) -> String {
    let mut s = String::new();
    if d.numbered && !d.new_file {
        if let Some(l) = nums.iter().find_map(|(o, n)| n.or(*o)) {
            s.push_str(&format!("line {l}"));
        }
    } else if d.new_file {
        s.push_str("new file");
    }
    if n > 1 {
        if !s.is_empty() {
            s.push_str(&format!(" {sep} "));
        }
        s.push_str(&format!("edit {} of {n}", i + 1));
    }
    if !s.is_empty() {
        s.push_str(&format!(" {sep} "));
    }
    s.push_str(&format!("+{} -{}", d.added, d.removed));
    s
}

/// One side of a split row, or a whole unified row: gutter, then code, padded to `width`.
fn pad_to(mut spans: Vec<Span<'static>>, width: usize, bg: Option<Color>) -> Vec<Span<'static>> {
    let w = spans_width(&spans);
    if w < width {
        spans.push(Span::raw(" ".repeat(width - w)));
    }
    if let Some(bg) = bg {
        for s in &mut spans {
            if s.style.bg.is_none() || s.style.bg == Some(Color::Reset) {
                s.style = s.style.bg(bg);
            }
        }
    }
    spans
}

struct Side {
    no: Option<usize>,
    row: Option<usize>,
}

impl DiffView {
    pub fn new(title: &str, files: Vec<FileChange>) -> DiffView {
        DiffView {
            title: title.to_string(),
            files,
            sel: 0,
            scroll: 0,
            pref: LayoutPref::Auto,
            pending: false,
            cwd: String::new(),
            want_hunk: None,
            cache: None,
            vh: 20,
            list_hits: Vec::new(),
        }
    }

    /// A viewer for one edit that is waiting for a decision.
    pub fn pending(data: &DiffData, tool: &str) -> DiffView {
        let f = FileChange {
            path: data.path.clone(),
            new_file: data.new_file,
            added: data.added,
            removed: data.removed,
            hunks: vec![data.clone()],
            ids: Vec::new(),
        };
        let mut v = DiffView::new(&format!("{tool} waiting for your answer"), vec![f]);
        v.pending = true;
        v
    }

    /// Show `file`, scrolled to its `hunk`th change.
    pub fn select(&mut self, file: usize, hunk: usize) {
        self.sel = file.min(self.files.len().saturating_sub(1));
        self.scroll = 0;
        self.cache = None;
        self.want_hunk = Some(hunk);
    }

    /// File and hunk index of the edit made by call `id`.
    pub fn locate(&self, id: &str) -> Option<(usize, usize)> {
        self.files
            .iter()
            .enumerate()
            .find_map(|(fi, f)| f.ids.iter().position(|x| x == id).map(|h| (fi, h)))
    }

    /// `path` without the project directory in front.
    pub fn rel<'a>(&self, path: &'a str) -> &'a str {
        if self.cwd.is_empty() {
            return path;
        }
        path.strip_prefix(&self.cwd)
            .map(|r| r.trim_start_matches('/'))
            .filter(|r| !r.is_empty())
            .unwrap_or(path)
    }

    pub fn totals(&self) -> (usize, usize) {
        self.files
            .iter()
            .fold((0, 0), |(a, r), f| (a + f.added, r + f.removed))
    }

    /// `auto` is side by side only when it helps: the body is wide enough and no changed line
    /// would have to wrap in half of it. A half that wraps every other line reads worse than
    /// the unified view (round 2 finding 5: `opts.stri` / `ct);` at 120 columns).
    fn split_for(&self, body_w: usize, f: &FileChange) -> bool {
        match self.pref {
            LayoutPref::Split => true,
            LayoutPref::Unified => false,
            LayoutPref::Auto => body_w >= SPLIT_FROM && split_fits(f, body_w),
        }
    }

    // ---- layout of one file ----------------------------------------------------------------

    fn render(&self, f: &FileChange, bw: usize, split: bool, cx: &Cx) -> (Vec<Row>, Vec<usize>) {
        let p = cx.p;
        let mut rows: Vec<Row> = Vec::new();
        let mut starts: Vec<usize> = Vec::new();
        // File title.
        let tag = if f.new_file { "new" } else { "modified" };
        let counts = format!("+{} -{}", f.added, f.removed);
        let left = vec![
            spaces(1),
            sp(
                truncate_left(
                    self.rel(&f.path),
                    bw.saturating_sub(counts.len() + tag.len() + 8),
                ),
                p.s_text().add_modifier(Modifier::BOLD),
            ),
            spaces(2),
            sp(tag, p.s_faint()),
        ];
        let right = vec![sp(counts, p.s_faint()), spaces(1)];
        let mut spans = left;
        let gap = bw.saturating_sub(spans_width(&spans) + spans_width(&right));
        spans.push(spaces(gap));
        spans.extend(right);
        rows.push(Row::new(spans).bg(p.raised));
        let n = f.hunks.len();
        for (i, d) in f.hunks.iter().enumerate() {
            let nums = numbers(d);
            // Hunk header.
            let label = hunk_label(i, n, d, &nums, cx.g.sep);
            let lead = format!(" {} {label} ", cx.g.dashed.repeat(2));
            let fill_n = bw.saturating_sub(display_width(&lead));
            starts.push(rows.len());
            rows.push(Row::new(vec![
                sp(lead, p.s_faint()),
                sp(cx.g.dashed.repeat(fill_n), p.s_line()),
            ]));
            let hl = highlight_rows(d, cx);
            if split {
                self.split_rows(d, &nums, hl, bw, cx, &mut rows);
            } else {
                self.unified_rows(d, &nums, hl, bw, cx, &mut rows);
            }
            if i + 1 < n {
                rows.push(Row::blank());
            }
        }
        (rows, starts)
    }

    fn gap_row(&self, g: usize, bw: usize, cx: &Cx) -> Row {
        let p = cx.p;
        let word = if g == 1 { "line" } else { "lines" };
        let text = format!(" {} {g} unchanged {word}", cx.g.dashed);
        Row::new(vec![sp(cut(&text, bw), p.s_faint())])
    }

    fn unified_rows(
        &self,
        d: &DiffData,
        nums: &[(Option<usize>, Option<usize>)],
        hl: Vec<Vec<Span<'static>>>,
        bw: usize,
        cx: &Cx,
        out: &mut Vec<Row>,
    ) {
        let p = cx.p;
        // A hunk that could not be placed in its file has no numbers to show, and a column of
        // `~` or blanks is width the code could use (round 2 finding 5): no gutter at all.
        let nw = if d.numbered { number_width(d) } else { 0 };
        let gutter = if d.numbered {
            1 + nw + 1 + nw + 1 + 1 + 1
        } else {
            3
        };
        let code_w = bw.saturating_sub(gutter + 1).max(8);
        for ((r, (o, n)), spans) in d.rows.iter().zip(nums).zip(hl) {
            if let DKind::Gap(g) = r.kind {
                out.push(self.gap_row(g, bw, cx));
                continue;
            }
            let (bg, fg, sign) = row_look(r.kind, cx);
            let st = Style::new().fg(fg);
            let num = |v: &Option<usize>| match v {
                Some(x) => format!("{x:>nw$}"),
                None => " ".repeat(nw),
            };
            let code = styled_code(r, spans, cx);
            let wrapped = if code.is_empty() {
                vec![Vec::new()]
            } else {
                crate::ui::wrapcode::wrap_code(&code, code_w)
            };
            for (i, seg) in wrapped.into_iter().enumerate() {
                let mut row = vec![spaces(1)];
                if i == 0 {
                    if d.numbered {
                        row.push(sp(num(o), st));
                        row.push(spaces(1));
                        row.push(sp(num(n), st));
                        row.push(spaces(1));
                    }
                    row.push(sp(sign, st));
                    row.push(spaces(1));
                } else {
                    row.push(spaces(if d.numbered { 2 * nw + 2 } else { 0 }));
                    row.push(sp(cx.g.wrap, p.s_faint()));
                    row.push(spaces(if d.numbered { 2 } else { 1 }));
                }
                row.extend(seg);
                out.push(Row::new(pad_to(row, bw, None)).bg(bg));
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    /// One side of a split row, wrapped: each entry is one drawn row of exactly `half` cells.
    fn side_rows(
        &self,
        r: Option<&DRow>,
        spans: Vec<Span<'static>>,
        no: Option<usize>,
        numbered: bool,
        nw: usize,
        half: usize,
        cx: &Cx,
    ) -> Vec<Vec<Span<'static>>> {
        let p = cx.p;
        let Some(r) = r else {
            return vec![pad_to(Vec::new(), half, Some(p.bg))];
        };
        let (bg, fg, sign) = row_look(r.kind, cx);
        let st = Style::new().fg(fg);
        let code_w = half
            .saturating_sub(if numbered { nw + 4 } else { 3 })
            .max(6);
        let code = styled_code(r, spans, cx);
        let wrapped = if code.is_empty() {
            vec![Vec::new()]
        } else {
            crate::ui::wrapcode::wrap_code(&code, code_w)
        };
        wrapped
            .into_iter()
            .enumerate()
            .map(|(i, seg)| {
                let mut row = vec![spaces(1)];
                if i == 0 {
                    if numbered {
                        let num = match no {
                            Some(x) => format!("{x:>nw$}"),
                            None => " ".repeat(nw),
                        };
                        row.push(sp(num, st));
                        row.push(spaces(1));
                    }
                    row.push(sp(sign, st));
                    row.push(spaces(1));
                } else {
                    row.push(spaces(nw));
                    row.push(sp(cx.g.wrap, p.s_faint()));
                    row.push(spaces(if numbered { 2 } else { 1 }));
                }
                row.extend(seg);
                pad_to(row, half, Some(bg))
            })
            .collect()
    }

    fn split_rows(
        &self,
        d: &DiffData,
        nums: &[(Option<usize>, Option<usize>)],
        hl: Vec<Vec<Span<'static>>>,
        bw: usize,
        cx: &Cx,
        out: &mut Vec<Row>,
    ) {
        let p = cx.p;
        let nw = if d.numbered { number_width(d) } else { 0 };
        let half = bw.saturating_sub(1) / 2;
        // Pair removals with the additions that replace them.
        let mut pairs: Vec<(Side, Side)> = Vec::new();
        let mut gaps: Vec<(usize, usize)> = Vec::new();
        let mut i = 0;
        let rows = &d.rows;
        while i < rows.len() {
            match rows[i].kind {
                DKind::Gap(g) => {
                    gaps.push((pairs.len(), g));
                    i += 1;
                }
                DKind::Ctx => {
                    pairs.push((
                        Side {
                            no: nums[i].0,
                            row: Some(i),
                        },
                        Side {
                            no: nums[i].1,
                            row: Some(i),
                        },
                    ));
                    i += 1;
                }
                _ => {
                    let dels: Vec<usize> = (i..rows.len())
                        .take_while(|&k| rows[k].kind == DKind::Del)
                        .collect();
                    let a0 = i + dels.len();
                    let adds: Vec<usize> = (a0..rows.len())
                        .take_while(|&k| rows[k].kind == DKind::Add)
                        .collect();
                    for k in 0..dels.len().max(adds.len()) {
                        pairs.push((
                            Side {
                                no: dels.get(k).and_then(|&x| nums[x].0),
                                row: dels.get(k).copied(),
                            },
                            Side {
                                no: adds.get(k).and_then(|&x| nums[x].1),
                                row: adds.get(k).copied(),
                            },
                        ));
                    }
                    i = a0 + adds.len();
                    if dels.is_empty() && adds.is_empty() {
                        i += 1;
                    }
                }
            }
        }
        let sep = sp(cx.g.vline, p.s_line());
        let mut gi = 0;
        for (pi, (l, r)) in pairs.iter().enumerate() {
            while gi < gaps.len() && gaps[gi].0 == pi {
                out.push(self.gap_row(gaps[gi].1, bw, cx));
                gi += 1;
            }
            let lr = self.side_rows(
                l.row.map(|k| &d.rows[k]),
                l.row.map(|k| hl[k].clone()).unwrap_or_default(),
                l.no,
                d.numbered,
                nw,
                half,
                cx,
            );
            let rr = self.side_rows(
                r.row.map(|k| &d.rows[k]),
                r.row.map(|k| hl[k].clone()).unwrap_or_default(),
                r.no,
                d.numbered,
                nw,
                bw - half - 1,
                cx,
            );
            let n = lr.len().max(rr.len());
            for k in 0..n {
                let mut spans = lr
                    .get(k)
                    .cloned()
                    .unwrap_or_else(|| pad_to(Vec::new(), half, Some(p.bg)));
                spans.push(sep.clone());
                spans.extend(
                    rr.get(k)
                        .cloned()
                        .unwrap_or_else(|| pad_to(Vec::new(), bw - half - 1, Some(p.bg))),
                );
                out.push(Row::new(spans));
            }
        }
        while gi < gaps.len() {
            out.push(self.gap_row(gaps[gi].1, bw, cx));
            gi += 1;
        }
    }

    // ---- keys and mouse --------------------------------------------------------------------

    fn total_rows(&self) -> usize {
        self.cache.as_ref().map_or(0, |c| c.rows.len())
    }

    fn max_scroll(&self) -> usize {
        self.total_rows().saturating_sub(self.vh)
    }

    fn scroll_by(&mut self, d: isize) {
        let max = self.max_scroll();
        self.scroll = (self.scroll as isize + d).clamp(0, max as isize) as usize;
    }

    fn go_file(&mut self, i: usize) {
        if i < self.files.len() && i != self.sel {
            self.sel = i;
            self.scroll = 0;
            self.cache = None;
        }
    }

    fn step_hunk(&mut self, fwd: bool) {
        let Some(c) = &self.cache else { return };
        let target = if fwd {
            c.hunks.iter().copied().find(|&h| h > self.scroll)
        } else {
            c.hunks.iter().copied().rev().find(|&h| h < self.scroll)
        };
        // A hunk near the end cannot sit at the top; the last rows are as far as it goes.
        self.scroll = match target {
            Some(h) => h,
            None if fwd => self.max_scroll(),
            None => 0,
        }
        .min(self.max_scroll());
    }

    pub fn on_key(&mut self, key: KeyEvent) -> DvOut {
        let half = (self.vh / 2).max(1) as isize;
        let page = self.vh.saturating_sub(2).max(1) as isize;
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => return DvOut::Close,
            KeyCode::Char('c') if ctrl => return DvOut::Close,
            KeyCode::Char('j') | KeyCode::Down => self.scroll_by(1),
            KeyCode::Char('k') | KeyCode::Up => self.scroll_by(-1),
            KeyCode::Char('d') if ctrl => self.scroll_by(half),
            KeyCode::Char('u') if ctrl => self.scroll_by(-half),
            KeyCode::Char('d') => self.scroll_by(half),
            KeyCode::Char('u') => self.scroll_by(-half),
            KeyCode::PageDown | KeyCode::Char(' ') | KeyCode::Char('f') => self.scroll_by(page),
            KeyCode::PageUp | KeyCode::Char('b') => self.scroll_by(-page),
            KeyCode::Home | KeyCode::Char('g') => self.scroll = 0,
            KeyCode::End | KeyCode::Char('G') => self.scroll = self.max_scroll(),
            KeyCode::Char('n') | KeyCode::Char('}') => self.step_hunk(true),
            KeyCode::Char('N') | KeyCode::Char('p') | KeyCode::Char('{') => self.step_hunk(false),
            KeyCode::Char(']') | KeyCode::Tab | KeyCode::Char('l') | KeyCode::Right => {
                let n = self.files.len().max(1);
                self.go_file((self.sel + 1) % n);
            }
            KeyCode::Char('[') | KeyCode::BackTab | KeyCode::Char('h') | KeyCode::Left => {
                let n = self.files.len().max(1);
                self.go_file((self.sel + n - 1) % n);
            }
            KeyCode::Char('s') => {
                let split_now = self.cache.as_ref().is_some_and(|c| c.key.2);
                self.pref = if split_now {
                    LayoutPref::Unified
                } else {
                    LayoutPref::Split
                };
                self.cache = None;
            }
            KeyCode::Char(c @ '1'..='9') => self.go_file(c as usize - '1' as usize),
            _ => {}
        }
        DvOut::None
    }

    pub fn on_mouse(&mut self, m: MouseEvent) {
        match m.kind {
            MouseEventKind::ScrollDown => self.scroll_by(3),
            MouseEventKind::ScrollUp => self.scroll_by(-3),
            MouseEventKind::Down(_) => {
                if let Some(&(_, f)) = self.list_hits.iter().find(|(y, _)| *y == m.row) {
                    if m.column < 40 {
                        self.go_file(f);
                    }
                }
            }
            _ => {}
        }
    }

    // ---- drawing ---------------------------------------------------------------------------

    fn put_row(buf: &mut Buffer, x: u16, y: u16, w: u16, row: &Row, bg: Color) {
        let rect = Rect::new(x, y, w, 1);
        let bg = row.bg.unwrap_or(bg);
        fill(buf, rect, Style::new().bg(bg));
        let mut cx_ = x;
        for s in &row.spans {
            let st = if s.style.bg.is_some() {
                s.style
            } else {
                Style::new().bg(bg).patch(s.style)
            };
            cx_ = put_str(buf, cx_, y, &s.content, st, rect);
        }
    }

    pub fn draw(&mut self, buf: &mut Buffer, area: Rect, cx: &Cx) {
        let p = cx.p;
        fill(buf, area, Style::new().bg(p.bg).fg(p.text));
        // Two columns of margin each side from 60 columns up, like the rest of the app: the
        // tints and the header used to run to the last cell (finding 18).
        let margin = if area.width >= 60 { MARGIN } else { 0 };
        let area = Rect::new(
            area.x + margin,
            area.y,
            area.width.saturating_sub(2 * margin),
            area.height,
        );
        let (w, h) = (area.width as usize, area.height as usize);
        if h < 4 || w < 20 {
            return;
        }
        self.list_hits.clear();
        // Header.
        let (a, r) = self.totals();
        let mut head = vec![
            spaces(2),
            sp("diff", p.s_text().add_modifier(Modifier::BOLD)),
            sp(
                format!(
                    "  {} {} {} +{a} -{r}",
                    self.files.len(),
                    if self.files.len() == 1 {
                        "file"
                    } else {
                        "files"
                    },
                    cx.g.sep
                ),
                p.s_dim(),
            ),
        ];
        if self.pending {
            head.push(sp(format!("  {}", self.title), p.s_warn()));
        }
        Self::put_row(buf, area.x, area.y, area.width, &Row::new(head), p.bg);
        let wide = w >= LIST_FROM && self.files.len() > 1;
        // As wide as the longest name needs, not a third of the screen for four short names.
        let list_w = if wide {
            let longest = self
                .files
                .iter()
                .map(|f| {
                    display_width(self.rel(&f.path)) + format!("+{} -{}", f.added, f.removed).len()
                })
                .max()
                .unwrap_or(0);
            (longest + 7).clamp(24, (w / 3).clamp(24, 38))
        } else {
            0
        };
        let strip = !wide && self.files.len() > 1;
        let body_x = area.x + if wide { list_w as u16 + 1 } else { 0 };
        let body_w = w - if wide { list_w + 1 } else { 0 };
        let y0 = area.y + 1 + u16::from(strip);
        let foot_y = area.y + area.height - 1;
        let body_h = (foot_y - y0) as usize;
        self.vh = body_h;
        // The file list or the one-row strip.
        if wide {
            for (i, f) in self.files.iter().enumerate() {
                let y = area.y + 1 + i as u16;
                if y >= foot_y {
                    break;
                }
                let on = i == self.sel;
                let counts = format!("+{} -{}", f.added, f.removed);
                let name_w = list_w.saturating_sub(counts.len() + 5);
                let shown = self.rel(&f.path).to_string();
                let row = Row::new(vec![
                    spaces(1),
                    sp(
                        if on { cx.g.select } else { " " },
                        p.s_accent().add_modifier(Modifier::BOLD),
                    ),
                    spaces(1),
                    sp(
                        truncate_left(&shown, name_w),
                        if on { p.s_text() } else { p.s_dim() },
                    ),
                    spaces(list_w.saturating_sub(
                        4 + display_width(&truncate_left(&shown, name_w)) + counts.len(),
                    )),
                    sp(counts, p.s_faint()),
                    spaces(1),
                ]);
                let bg = if on { p.raised } else { p.bg };
                Self::put_row(buf, area.x, y, list_w as u16, &row, bg);
                self.list_hits.push((y, i));
            }
            for y in (area.y + 1)..foot_y {
                put_str(buf, area.x + list_w as u16, y, cx.g.vline, p.s_line(), area);
            }
        } else if strip {
            let f = &self.files[self.sel];
            let row = Row::new(vec![
                spaces(2),
                sp(
                    format!(
                        "{} {}/{} {}",
                        cx.g.select,
                        self.sel + 1,
                        self.files.len(),
                        ""
                    ),
                    p.s_accent(),
                ),
                sp(
                    truncate_left(self.rel(&f.path), w.saturating_sub(24)),
                    p.s_text(),
                ),
                sp(format!("  +{} -{}", f.added, f.removed), p.s_faint()),
            ]);
            Self::put_row(buf, area.x, area.y + 1, area.width, &row, p.bg);
        }
        // Body.
        if self.files.is_empty() {
            let msg = "No files changed in this session.";
            put_str(buf, area.x + 2, y0 + 1, msg, p.s_dim(), area);
        } else {
            let split = self.split_for(body_w, &self.files[self.sel]);
            let key = (body_w, self.sel, split, p.is_dark());
            if self.cache.as_ref().is_none_or(|c| c.key != key) {
                let (rows, hunks) = self.render(&self.files[self.sel], body_w, split, cx);
                self.cache = Some(Rendered { key, rows, hunks });
            }
            if let Some(hk) = self.want_hunk.take() {
                let at = self
                    .cache
                    .as_ref()
                    .and_then(|c| c.hunks.get(hk).copied())
                    .unwrap_or(0);
                self.scroll = at;
            }
            self.scroll = self.scroll.min(self.max_scroll());
            if let Some(c) = &self.cache {
                for (i, row) in c.rows.iter().skip(self.scroll).take(body_h).enumerate() {
                    Self::put_row(buf, body_x, y0 + i as u16, body_w as u16, row, p.bg);
                }
            }
        }
        // Footer.
        let key = |k: &str, l: &str| -> Vec<Span<'static>> {
            vec![
                sp(k.to_string(), p.s_text().add_modifier(Modifier::BOLD)),
                sp(format!(" {l}"), p.s_dim()),
                spaces(2),
            ]
        };
        let mut foot = vec![spaces(2)];
        foot.extend(key("j/k", "scroll"));
        foot.extend(key("n/N", "hunk"));
        if self.files.len() > 1 {
            foot.extend(key("[/]", "file"));
        }
        let split_now = self.cache.as_ref().is_some_and(|c| c.key.2);
        foot.extend(key("s", if split_now { "unified" } else { "split" }));
        foot.extend(key(
            "esc",
            if self.pending {
                "back to the question"
            } else {
                "back"
            },
        ));
        let total = self.total_rows();
        let pct = if total <= body_h {
            100
        } else {
            (self.scroll * 100 / (total - body_h).max(1)).min(100)
        };
        let right = vec![sp(format!("{pct}%"), p.s_faint()), spaces(2)];
        let gap = w.saturating_sub(spans_width(&foot) + spans_width(&right));
        foot.push(spaces(gap));
        foot.extend(right);
        // Drop hints from the right while the row is too wide.
        let mut f2 = foot;
        while spans_width(&f2) > w && f2.len() > 4 {
            f2.remove(f2.len() - 4);
        }
        Self::put_row(buf, area.x, foot_y, area.width, &Row::new(f2), p.bg);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::palette::{Depth, Kind, Palette, UNICODE};
    use agent_core::{FileDiff, ToolCall, ToolKind, ToolStatus};
    use std::time::Instant;
    use tuikit::testing::TestTerminal;

    fn edit(id: &str, path: &str, old: &str, new: &str) -> ToolEntry {
        let call = ToolCall {
            id: id.into(),
            name: "Edit".into(),
            kind: ToolKind::Edit,
            status: ToolStatus::Completed,
            diff: Some(FileDiff {
                path: path.into(),
                old: Some(old.into()),
                new: new.into(),
            }),
            ..Default::default()
        };
        ToolEntry::new(call, Instant::now(), true)
    }

    fn entries() -> Vec<ToolEntry> {
        vec![
            edit("1", "/nonexistent/a.rs", "let x = 1;\n", "let x = 2;\n"),
            edit(
                "2",
                "/nonexistent/b.rs",
                "fn a() {}\n",
                "fn a() {}\nfn b() {}\n",
            ),
            edit(
                "3",
                "/nonexistent/a.rs",
                "let y = 1;\n",
                "let y = 5;\nlet z = 6;\n",
            ),
        ]
    }

    fn draw_view(v: &mut DiffView, w: u16, h: u16) -> String {
        let pal = Palette::new(Kind::Hearth, Depth::True);
        let th = pal.theme();
        let cx = Cx {
            p: &pal,
            theme: &th,
            g: &UNICODE,
            width: w as usize,
            detail: false,
            now: Instant::now(),
            spin: 0,
        };
        let mut t = TestTerminal::new(w, h);
        t.draw(|b, a| v.draw(b, a, &cx));
        t.plain()
    }

    #[test]
    fn edits_to_one_file_are_merged_in_order_of_first_appearance() {
        let es = entries();
        let files = collect(es.iter());
        assert_eq!(files.len(), 2);
        assert_eq!(files[0].path, "/nonexistent/a.rs");
        assert_eq!(files[0].hunks.len(), 2);
        assert_eq!((files[0].added, files[0].removed), (3, 2));
        assert_eq!(files[1].path, "/nonexistent/b.rs");
    }

    #[test]
    fn the_viewer_keeps_a_margin_and_shows_a_tilde_where_a_line_number_is_unknown() {
        // Finding 18: tints to the last column, and blank gutters on hunks 2 and 3.
        let pal = Palette::new(Kind::Hearth, Depth::True);
        let th = pal.theme();
        let es = entries();
        for (w, h) in [(80u16, 24u16), (120, 36), (160, 45)] {
            let cx = Cx {
                p: &pal,
                theme: &th,
                g: &UNICODE,
                width: w as usize,
                detail: false,
                now: Instant::now(),
                spin: 0,
            };
            let mut v = DiffView::new("changes", collect(es.iter()));
            let mut t = TestTerminal::new(w, h);
            t.draw(|b, a| v.draw(b, a, &cx));
            let buf = t.buffer();
            for y in 0..h {
                for x in (0..2).chain(w - 2..w) {
                    assert_eq!(
                        buf[(x, y)].bg,
                        pal.bg,
                        "{w}x{h}: ({x},{y}) is outside the margin but not background"
                    );
                }
            }
            let s = t.plain();
            assert!(
                !s.contains('~'),
                "{w}x{h}: no number column when none is known\n{s}"
            );
        }
    }

    #[test]
    fn running_and_refused_edits_are_not_changes() {
        let mut e = edit("1", "/nonexistent/a.rs", "a\n", "b\n");
        e.call.status = ToolStatus::Failed;
        assert!(collect([e.clone()].iter()).is_empty());
        e.call.status = ToolStatus::Running;
        assert!(collect([e].iter()).is_empty());
    }

    #[test]
    fn old_and_new_numbers_advance_independently() {
        let d = DiffData::build(Some("a\nb\nc\n"), "a\nB\nB2\nc\n", "/nonexistent/f", true);
        let n = numbers(&d);
        let kinds: Vec<_> = d.rows.iter().map(|r| r.kind).collect();
        assert_eq!(kinds[0], DKind::Ctx);
        assert_eq!(n[0], (Some(1), Some(1)));
        // The removed line has only an old number, the two added lines only new ones.
        let del = kinds.iter().position(|k| *k == DKind::Del).unwrap();
        assert_eq!(n[del], (Some(2), None));
        let adds: Vec<_> = n
            .iter()
            .zip(&kinds)
            .filter(|(_, k)| **k == DKind::Add)
            .map(|(x, _)| x.1.unwrap())
            .collect();
        assert_eq!(adds, [2, 3]);
        let last = n.last().unwrap();
        assert_eq!(*last, (Some(3), Some(4)));
    }

    #[test]
    fn wide_screens_get_the_file_list_and_split_and_every_row_fits() {
        let es = entries();
        let mut v = DiffView::new("changes", collect(es.iter()));
        let s = draw_view(&mut v, 150, 30);
        assert!(s.contains("a.rs") && s.contains("b.rs"), "{s}");
        assert!(s.lines().next().unwrap().contains("diff"), "{s}");
        assert!(
            s.contains("unified"),
            "split is the default here, so the key offers unified\n{s}"
        );
        for l in s.lines() {
            assert!(display_width(l) <= 150, "{l:?}");
        }
        // Side by side: a removed line and its replacement share a row.
        assert!(
            s.lines()
                .any(|l| l.contains("let x = 1;") && l.contains("let x = 2;")),
            "{s}"
        );
    }

    #[test]
    fn narrow_screens_are_unified_with_a_file_strip() {
        let es = entries();
        let mut v = DiffView::new("changes", collect(es.iter()));
        let s = draw_view(&mut v, 70, 24);
        assert!(s.contains("1/2"), "{s}");
        assert!(s.contains("split"), "{s}");
        assert!(
            !s.lines()
                .any(|l| l.contains("let x = 1;") && l.contains("let x = 2;")),
            "{s}"
        );
        assert!(s.lines().any(|l| l.contains("let x = 1;")), "{s}");
    }

    #[test]
    fn keys_move_between_hunks_and_files_and_esc_closes() {
        let es = entries();
        let mut v = DiffView::new("changes", collect(es.iter()));
        draw_view(&mut v, 70, 10);
        let k = |c: char| KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE);
        assert_eq!(v.on_key(k('j')), DvOut::None);
        draw_view(&mut v, 70, 10);
        v.on_key(k('n'));
        draw_view(&mut v, 70, 10);
        assert!(v.scroll > 0);
        v.on_key(k(']'));
        assert_eq!((v.sel, v.scroll), (1, 0));
        v.on_key(k('s'));
        assert_eq!(v.pref, LayoutPref::Split);
        assert_eq!(
            v.on_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
            DvOut::Close
        );
    }

    #[test]
    fn no_changes_says_so() {
        let mut v = DiffView::new("changes", Vec::new());
        let s = draw_view(&mut v, 80, 12);
        assert!(s.contains("No files changed"), "{s}");
    }
}
