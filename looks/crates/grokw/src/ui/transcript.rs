// OWNER: transcript (scrollback: entries, gaps, scroll, selection, sticky headers; find, jump, the viewer and copy are in `transcript_nav` and `transcript_viewer`)
//! The scrollback pane. The agent-core transcript is turned into [`Entry`]s (a user prompt, an
//! agent message, a thinking block, a tool row, a verb group, a system line), each a list of
//! [`Row`]s already laid out for the terminal width. Rows hold *what* to paint, including which
//! cells animate; [`draw`] slices the document to the viewport and asks `anim` for the colours
//! of the current tick, so a running rail costs one repaint per tick and nothing when idle.

use std::collections::hash_map::DefaultHasher;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::time::Duration;

use agent_core::transcript::{Part, Role};
use agent_core::{NoticeLevel, StopReason};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Span;
use tuikit::width::{display_width, wrap};

use super::anim;
use super::markdown::{self, LineKind};
use super::{bold, put, st, tools, Layout};
use crate::app::{App, Focus};
use crate::theme::Theme;

/// `(message index, part index)`; markers use `usize::MAX` for the part.
pub type EntryKey = (usize, usize);

/// What a cell's colour is a function of.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Anim {
    None,
    /// Running-block wave blended from the screen background to this accent.
    Wave(Color),
}

#[derive(Clone, Debug)]
pub struct Seg {
    pub x: u16,
    pub text: String,
    pub style: Style,
    pub anim: Anim,
    /// Part of the block's text. Prefix glyphs and timestamps are not: find skips them and a
    /// copy leaves them out.
    pub copy: bool,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Rail {
    None,
    Solid(Color),
    Wave(Color),
}

#[derive(Clone, Debug)]
pub struct Row {
    pub segs: Vec<Seg>,
    pub rail: Rail,
    /// Background fills `(x0, x1_exclusive, colour)`, painted before the text.
    pub fills: Vec<(u16, u16, Color)>,
    /// Row index inside its entry (phase of the wave).
    pub brow: usize,
}

impl Row {
    pub fn new() -> Row {
        Row {
            segs: Vec::new(),
            rail: Rail::None,
            fills: Vec::new(),
            brow: 0,
        }
    }

    pub fn seg(mut self, x: u16, text: impl Into<String>, style: Style) -> Row {
        self.segs.push(Seg {
            x,
            text: text.into(),
            style,
            anim: Anim::None,
            copy: true,
        });
        self
    }

    /// A segment that is chrome, not text: a prefix glyph, a timestamp.
    pub fn chrome(mut self, x: u16, text: impl Into<String>, style: Style) -> Row {
        self.segs.push(Seg {
            x,
            text: text.into(),
            style,
            anim: Anim::None,
            copy: false,
        });
        self
    }

    pub fn wave(mut self, x: u16, text: impl Into<String>, style: Style, accent: Color) -> Row {
        self.segs.push(Seg {
            x,
            text: text.into(),
            style,
            anim: Anim::Wave(accent),
            copy: false,
        });
        self
    }

    /// Spans laid one after another from `x`.
    pub fn spans(mut self, mut x: u16, spans: &[Span<'static>]) -> Row {
        for s in spans {
            let w = display_width(&s.content) as u16;
            if !s.content.is_empty() {
                self.segs.push(Seg {
                    x,
                    text: s.content.to_string(),
                    style: s.style,
                    anim: Anim::None,
                    copy: true,
                });
            }
            x += w;
        }
        self
    }

    pub fn fill(mut self, x0: u16, x1: u16, c: Color) -> Row {
        self.fills.push((x0, x1, c));
        self
    }

    pub fn rail(mut self, r: Rail) -> Row {
        self.rail = r;
        self
    }

    pub fn blank() -> Row {
        Row::new()
    }

    pub fn animated(&self) -> bool {
        matches!(self.rail, Rail::Wave(_)) || self.segs.iter().any(|s| s.anim != Anim::None)
    }
}

impl Default for Row {
    fn default() -> Self {
        Row::new()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    User,
    Agent,
    Thinking,
    Tool,
    Group,
    System,
    Warning,
}

#[derive(Clone, Debug)]
pub struct Entry {
    pub key: EntryKey,
    pub kind: Kind,
    pub rows: Vec<Row>,
    /// Joins a dense run (gap 0) with a neighbour that is also groupable and collapsed.
    pub groupable: bool,
    pub collapsed: bool,
    pub selectable: bool,
    /// The fold keys can change this entry's rows.
    pub foldable: bool,
    pub running: bool,
    /// The selectable rows of an open verb group: each member of the run.
    pub members: Vec<Member>,
}

/// One member row of an open verb group.
#[derive(Clone, Debug)]
pub struct Member {
    /// The part this row stands for.
    pub key: EntryKey,
    /// Row index inside the entry.
    pub row: usize,
    /// Opened with `→`: its body is drawn in place of the one-liner.
    pub open: bool,
    /// The row as it looks while selected: colours un-muted, `›` for the bullet.
    pub sel: Row,
}

/// The laid-out transcript at one width.
#[derive(Clone, Debug, Default)]
pub struct Doc {
    pub entries: Vec<Entry>,
    /// First document row of each entry.
    pub starts: Vec<usize>,
    pub total: usize,
    pub width: u16,
    /// Minimal mode: the line that closes a turn follows its answer with no blank row.
    pub tight: bool,
}

impl Entry {
    /// A user prompt that shows all its lines (it is not clipped to three).
    fn collapsed_user(&self) -> bool {
        self.kind == Kind::User && self.collapsed
    }
}

impl Doc {
    pub fn gap_after(&self, i: usize) -> usize {
        let e = &self.entries[i];
        match self.entries.get(i + 1) {
            Some(n) if self.tight && n.kind == Kind::System => 0,
            Some(n) if e.groupable && e.collapsed && n.groupable && n.collapsed => 0,
            _ => 1,
        }
    }

    fn finish(&mut self) {
        let mut at = 0;
        self.starts.clear();
        for i in 0..self.entries.len() {
            self.starts.push(at);
            at += self.entries[i].rows.len() + self.gap_after(i);
        }
        self.total = at;
    }

    /// Entry and row index for a document row; `None` on a gap row.
    pub fn locate(&self, row: usize) -> Option<(usize, usize)> {
        let i = match self.starts.binary_search(&row) {
            Ok(i) => i,
            Err(0) => return None,
            Err(i) => i - 1,
        };
        let r = row - self.starts[i];
        (r < self.entries[i].rows.len()).then_some((i, r))
    }

    pub fn index_of(&self, key: EntryKey) -> Option<usize> {
        self.entries.iter().position(|e| e.key == key)
    }
}

/// Geometry every entry builder works from.
#[derive(Clone, Copy, Debug)]
pub struct Geo {
    pub w: u16,
    pub hpad: u16,
    /// Content column (the bullet).
    pub cx: u16,
    pub cw: usize,
    /// Prose wrap width: content minus the timestamp reserve.
    pub text_w: usize,
    pub stamps: bool,
    /// Minimal mode (see [`Doc::tight`]).
    pub tight: bool,
}

impl Geo {
    pub fn new(w: u16, compact: bool, stamps: bool) -> Geo {
        let hpad: u16 = if compact { 1 } else { 2 };
        let cw = (w as usize).saturating_sub(2 * hpad as usize + 5).max(10);
        Geo {
            w,
            hpad,
            cx: hpad + 3,
            cw,
            text_w: if stamps {
                cw.saturating_sub(10).max(10)
            } else {
                cw
            },
            stamps,
            tight: false,
        }
    }

    pub fn band(&self) -> (u16, u16) {
        (self.hpad, self.w.saturating_sub(self.hpad))
    }
}

#[derive(Default)]
pub struct View {
    /// Top row when the user has scrolled; `None` follows the end.
    pub offset: Option<usize>,
    /// Message index of the prompt just sent: it is pinned to the top until output outgrows it.
    pub flip: Option<usize>,
    /// Explicit fold choices; `true` is expanded.
    pub folds: HashMap<EntryKey, bool>,
    /// Members of open groups opened with `→`; apart from `folds` because a group is keyed by
    /// its first tool, which is also that tool's own key.
    pub open_members: std::collections::HashSet<EntryKey>,
    /// Ctrl+E: every thinking block expanded.
    pub think_open: bool,
    pub selected: Option<EntryKey>,
    /// The member row selected inside the selected open group, with the group it belongs to.
    pub sel_member: Option<(EntryKey, usize)>,
    pub doc: Doc,
    /// Find, jump, drag selection (`transcript_nav`).
    pub nav: super::transcript_nav::Nav,
    doc_sig: u64,
    cache: HashMap<EntryKey, (u64, Entry)>,
    pub view_rows: usize,
}

impl View {
    pub fn reset(&mut self) {
        self.offset = None;
        self.flip = None;
        self.folds.clear();
        self.open_members.clear();
        self.selected = None;
        self.sel_member = None;
        self.nav = Default::default();
        self.cache.clear();
        self.doc = Doc::default();
        self.doc_sig = 0;
    }

    pub fn on_send(&mut self, msg_count: usize) {
        self.offset = None;
        self.flip = Some(msg_count.saturating_sub(1));
    }

    pub fn follow_if_flipped(&mut self) {}

    pub fn following(&self) -> bool {
        self.offset.is_none()
    }

    fn flip_top(&self) -> Option<usize> {
        let m = self.flip?;
        let i = self.doc.index_of((m, 0))?;
        Some(self.doc.starts[i])
    }

    /// Rows the scroll can span: the document, or enough to pin the sent prompt at the top.
    pub fn extent(&self) -> usize {
        match self.flip_top() {
            Some(top) if self.offset.is_none() => self.doc.total.max(top + self.view_rows),
            _ => self.doc.total,
        }
    }

    pub fn max_offset(&self) -> usize {
        self.extent().saturating_sub(self.view_rows)
    }

    pub fn top(&self) -> usize {
        match self.offset {
            Some(o) => o.min(self.max_offset()),
            None => self.max_offset(),
        }
    }

    pub fn scroll_up(&mut self, n: usize) {
        let top = self.top();
        self.offset = Some(top.saturating_sub(n));
        self.flip = None;
    }

    pub fn scroll_down(&mut self, n: usize) {
        let top = self.top();
        let max = self.max_offset();
        let to = (top + n).min(max);
        self.offset = if to >= max { None } else { Some(to) };
    }

    pub fn to_top(&mut self) {
        self.offset = Some(0);
        self.flip = None;
    }

    pub fn to_bottom(&mut self) {
        self.offset = None;
        self.flip = None;
    }

    pub fn is_expanded(&self, key: EntryKey, default: bool) -> bool {
        self.folds.get(&key).copied().unwrap_or(default)
    }

    fn selectable(&self) -> Vec<usize> {
        self.doc
            .entries
            .iter()
            .enumerate()
            .filter(|(_, e)| e.selectable)
            .map(|(i, _)| i)
            .collect()
    }

    /// Select the last selectable entry and bring it into view.
    pub fn select_last(&mut self) {
        if let Some(&i) = self.selectable().last() {
            self.selected = Some(self.doc.entries[i].key);
            self.reveal();
        }
    }

    /// After a page key: select the first (`top`) or last selectable entry that is on screen.
    pub fn select_in_view(&mut self, top: bool) {
        let (a, b) = (self.top(), self.top() + self.view_rows);
        let on_screen = |i: usize| {
            let e = &self.doc.entries[i];
            e.selectable && self.doc.starts[i] < b && self.doc.starts[i] + e.rows.len() > a
        };
        let n = self.doc.entries.len();
        let pick = if top {
            (0..n).find(|&i| on_screen(i))
        } else {
            (0..n).rev().find(|&i| on_screen(i))
        };
        if let Some(i) = pick {
            self.selected = Some(self.doc.entries[i].key);
            self.sel_member = None;
        }
    }

    /// The selected member of the selected open group.
    pub fn member(&self) -> Option<usize> {
        let (k, i) = self.sel_member?;
        (Some(k) == self.selected).then_some(i)
    }

    /// The part the keys act on: the selected member of an open group, else the selected entry.
    pub fn target(&self) -> Option<EntryKey> {
        let sel = self.selected?;
        match (self.member(), self.doc.index_of(sel)) {
            (Some(m), Some(i)) => self.doc.entries[i]
                .members
                .get(m)
                .map(|m| m.key)
                .or(Some(sel)),
            _ => Some(sel),
        }
    }

    /// Move the selection by `dir` entries (-1 up, 1 down). Inside an open group the rows of the
    /// run are visited one by one first.
    pub fn select(&mut self, dir: i32) {
        if let Some(i) = self.selected.and_then(|k| self.doc.index_of(k)) {
            let n = self.doc.entries[i].members.len();
            let key = self.doc.entries[i].key;
            if n > 0 {
                let to = match (dir > 0, self.member()) {
                    (true, None) => Some(Some(0)),
                    (true, Some(k)) if k + 1 < n => Some(Some(k + 1)),
                    (false, Some(0)) => Some(None),
                    (false, Some(k)) => Some(Some(k - 1)),
                    _ => None,
                };
                if let Some(to) = to {
                    self.sel_member = to.map(|m| (key, m));
                    self.reveal();
                    return;
                }
            }
        }
        let list = self.selectable();
        if list.is_empty() {
            return;
        }
        let cur = self
            .selected
            .and_then(|k| self.doc.index_of(k))
            .and_then(|i| list.iter().position(|&x| x == i));
        let next = match (cur, dir) {
            (None, _) => list.len() - 1,
            (Some(c), d) if d < 0 => c.saturating_sub(1),
            (Some(c), _) => (c + 1).min(list.len() - 1),
        };
        let e = &self.doc.entries[list[next]];
        self.selected = Some(e.key);
        // going up into an open group lands on its last member
        self.sel_member = (dir < 0 && !e.members.is_empty()).then(|| (e.key, e.members.len() - 1));
        self.reveal();
    }

    /// Fold the selected entry: `Some(true)` expands, `Some(false)` collapses, `None` flips. On a
    /// member of an open group the member folds first; a closed member's `←` closes the group.
    pub fn fold(&mut self, expand: Option<bool>) {
        let Some(key) = self.selected else { return };
        let Some(i) = self.doc.index_of(key) else {
            return;
        };
        if let Some(m) = self
            .member()
            .and_then(|m| self.doc.entries[i].members.get(m))
        {
            let (mk, open) = (m.key, m.open);
            match expand.unwrap_or(!open) {
                true => {
                    self.open_members.insert(mk);
                    return;
                }
                false if open => {
                    self.open_members.remove(&mk);
                    return;
                }
                false => {}
            }
        }
        let e = &self.doc.entries[i];
        if !e.foldable {
            return;
        }
        let open = !e.collapsed;
        let want = expand.unwrap_or(!open);
        self.folds.insert(key, want);
        if !want {
            self.sel_member = None;
        }
    }

    /// Scroll so the selected entry and the corner rows around it are on screen.
    pub fn reveal(&mut self) {
        let Some(key) = self.selected else { return };
        let Some(i) = self.doc.index_of(key) else {
            return;
        };
        let start = self.doc.starts[i];
        let end = start + self.doc.entries[i].rows.len();
        let vh = self.view_rows.max(1);
        let top = self.top();
        if start < top + 1 && start > 0 {
            self.offset = Some(start - 1);
        } else if end >= top + vh {
            let to = (end + 1).saturating_sub(vh);
            self.offset = if to >= self.max_offset() {
                None
            } else {
                Some(to)
            };
        }
        if self.offset.is_some() {
            self.flip = None;
        }
    }
}

// ---- formatting -----------------------------------------------------------------------------

/// `format_duration`: under 10 s `{:.1}s`, under 60 s `{n}s`, under 1 h `{m}m{s}s`, else `{h}h{m}m`.
pub fn fmt_duration(d: Duration) -> String {
    let secs = d.as_secs_f64();
    if secs < 10.0 {
        format!("{secs:.1}s")
    } else if secs < 60.0 {
        format!("{}s", secs as u64)
    } else if secs < 3600.0 {
        let s = secs as u64;
        format!("{}m{}s", s / 60, s % 60)
    } else {
        let s = secs as u64;
        format!("{}h{}m", s / 3600, (s % 3600) / 60)
    }
}

/// `Thought for` duration: `{:.1}s` under a minute, else `{m}m{:.0}s`.
pub fn fmt_thought(d: Duration) -> String {
    let secs = d.as_secs_f64();
    if secs < 60.0 {
        format!("{secs:.1}s")
    } else {
        format!("{}m{:.0}s", (secs / 60.0) as u64, secs % 60.0)
    }
}

/// `  5:42 PM`, hour without padding, nine columns (ten from 10 o'clock).
pub fn fmt_stamp(h: u8, m: u8) -> String {
    let (h12, ap) = match h {
        0 => (12, "AM"),
        1..=11 => (h, "AM"),
        12 => (12, "PM"),
        _ => (h - 12, "PM"),
    };
    format!("  {h12}:{m:02} {ap}")
}

/// Local wall clock `(hour, minute)` now.
pub fn local_hm() -> (u8, u8) {
    unsafe {
        let t = libc::time(std::ptr::null_mut());
        let mut tm: libc::tm = std::mem::zeroed();
        libc::localtime_r(&t, &mut tm);
        (tm.tm_hour as u8, tm.tm_min as u8)
    }
}

fn hash_of<T: Hash>(t: &T) -> u64 {
    let mut h = DefaultHasher::new();
    t.hash(&mut h);
    h.finish()
}

// ---- building -------------------------------------------------------------------------------

fn stamp_row(row: Row, geo: &Geo, th: &Theme, stamp: Option<(u8, u8)>, bg: Option<Color>) -> Row {
    let Some((h, m)) = stamp else { return row };
    if !geo.stamps {
        return row;
    }
    let s = fmt_stamp(h, m);
    let w = display_width(&s) as u16;
    let x = (geo.cx + geo.cw as u16).saturating_sub(w);
    let mut style = st(th.gray_solid());
    if let Some(b) = bg {
        style = style.bg(b);
    }
    row.chrome(x, s, style)
}

fn user_entry(
    key: EntryKey,
    text: &str,
    geo: &Geo,
    th: &Theme,
    stamp: Option<(u8, u8)>,
    expanded: Option<bool>,
) -> Entry {
    // the `❯ ` prefix takes two columns, except in compact mode
    let prefix = if geo.hpad == 1 { 0 } else { 2 };
    let wrap_w = if geo.stamps {
        geo.cw.saturating_sub(10 + prefix)
    } else {
        geo.cw.saturating_sub(prefix)
    }
    .max(8);
    let mut lines = wrap(text.trim_end_matches('\n'), wrap_w);
    let foldable = lines.len() > 3;
    let collapsed = foldable && !expanded.unwrap_or(false);
    if collapsed {
        // keep three rows; the third is re-wrapped two cells narrower and gets ` …`
        let third = wrap(&lines[2], wrap_w.saturating_sub(2))
            .into_iter()
            .next()
            .unwrap_or_default();
        lines.truncate(2);
        lines.push(format!("{third} …"));
    }
    let (b0, b1) = geo.band();
    let bg = th.bg_light;
    let band = |r: Row| r.fill(b0, b1, bg);
    // compact mode drops the padding rows and the `❯ ` prefix
    let compact = geo.hpad == 1;
    // minimal mode keeps the glyph and drops the padding rows
    let padded = !compact && !geo.tight;
    let mut rows = if padded {
        vec![band(Row::new())]
    } else {
        Vec::new()
    };
    for (i, l) in lines.iter().enumerate() {
        let mut r = band(Row::new());
        // bandless themes have no band to set the prompt apart, so its text is bold
        let weight = |s: Style| {
            if th.bandless {
                s.add_modifier(Modifier::BOLD)
            } else {
                s
            }
        };
        if i == 0 && !compact {
            r = r.chrome(geo.cx, "❯", weight(st(th.accent_user).bg(bg)));
        }
        r = r.seg(
            if compact { geo.cx } else { geo.cx + 2 },
            l.clone(),
            weight(st(th.text_primary).bg(bg)),
        );
        if i == 0 {
            r = stamp_row(r, geo, th, stamp, Some(bg));
        }
        rows.push(r);
    }
    if padded {
        rows.push(band(Row::new()));
    }
    Entry {
        key,
        kind: Kind::User,
        rows,
        groupable: false,
        collapsed,
        selectable: true,
        foldable,
        running: false,
        members: Vec::new(),
    }
}

fn md_rows(rows: &mut Vec<Row>, lines: Vec<markdown::MdLine>, geo: &Geo, th: &Theme, rail: Rail) {
    for l in lines {
        let mut r = Row::new().rail(rail);
        if l.kind == LineKind::Code {
            r = r.fill(geo.cx, geo.cx + geo.cw as u16, th.md_code_bg);
            let spans: Vec<Span<'static>> = l
                .spans
                .iter()
                .map(|s| Span::styled(s.content.clone(), s.style.bg(th.md_code_bg)))
                .collect();
            r = r.spans(geo.cx, &spans);
        } else {
            r = r.spans(geo.cx, &l.spans);
        }
        r.brow = rows.len();
        rows.push(r);
    }
}

fn agent_entry(
    key: EntryKey,
    text: &str,
    geo: &Geo,
    th: &Theme,
    stamp: Option<(u8, u8)>,
    raw: bool,
) -> Entry {
    let mut rows = Vec::new();
    if raw {
        // the markdown source, wrapped and unstyled
        for line in text.trim_end().split('\n') {
            for l in wrap(line, geo.text_w.max(10)) {
                rows.push(Row::new().seg(geo.cx, l, st(th.md_text)));
            }
        }
    } else {
        let lines = markdown::render(text, geo.text_w, geo.cw, th, false);
        md_rows(&mut rows, lines, geo, th, Rail::None);
    }
    if rows.is_empty() {
        rows.push(Row::new());
    }
    let first = rows.remove(0);
    rows.insert(0, stamp_row(first, geo, th, stamp, None));
    Entry {
        key,
        kind: Kind::Agent,
        rows,
        groupable: false,
        collapsed: false,
        selectable: true,
        foldable: false,
        running: false,
        members: Vec::new(),
    }
}

fn thinking_entry(
    key: EntryKey,
    text: &str,
    took: Option<Duration>,
    expanded: bool,
    geo: &Geo,
    th: &Theme,
) -> Entry {
    let running = took.is_none();
    let label = if running {
        "Thinking…".to_string()
    } else {
        match took {
            Some(d) if d > Duration::ZERO => format!(" for {}", fmt_thought(d)),
            _ => String::new(),
        }
    };
    let head = |bullet: Color, space: Color, rail: Rail, anim: Option<Color>| {
        let mut r = Row::new().rail(rail);
        r = match anim {
            Some(a) => r.wave(geo.cx, "◆", st(bullet), a),
            None => r.seg(geo.cx, "◆", st(bullet)),
        };
        r = r.seg(geo.cx + 1, " ", st(space));
        if running {
            r.seg(geo.cx + 2, "Thinking…", bold(th.gray))
        } else {
            let r = r.seg(geo.cx + 2, "Thought", bold(th.gray));
            if label.is_empty() {
                r
            } else {
                r.seg(geo.cx + 9, label.clone(), st(th.gray))
            }
        }
    };
    if !running && !expanded {
        return Entry {
            key,
            kind: Kind::Thinking,
            rows: vec![head(th.gray_solid(), th.gray, Rail::None, None)],
            groupable: true,
            collapsed: true,
            selectable: true,
            foldable: !text.trim().is_empty(),
            running: false,
            members: Vec::new(),
        };
    }
    let body = markdown::render(text.trim(), geo.text_w, geo.cw, th, true);
    let mut rows = Vec::new();
    let (rail, anim, bullet) = if running {
        (Rail::Wave(th.gray_dim), Some(th.gray_dim), th.gray_dim)
    } else {
        (Rail::Solid(th.gray_dim), None, th.gray_bright)
    };
    rows.push(head(bullet, th.gray_dim, rail, anim));
    rows.push(Row::new().rail(rail));
    let mut body_rows = Vec::new();
    md_rows(&mut body_rows, body, geo, th, rail);
    if running {
        // truncated: `…` then the last three rows (blank rows count)
        let n = body_rows.len();
        if n > 3 {
            rows.push(Row::new().rail(rail).seg(geo.cx, "…", st(th.gray)));
            body_rows.drain(..n - 3);
        }
    }
    rows.extend(body_rows);
    for (i, r) in rows.iter_mut().enumerate() {
        r.brow = i;
    }
    Entry {
        key,
        kind: Kind::Thinking,
        rows,
        groupable: true,
        collapsed: false,
        selectable: true,
        foldable: !running,
        running,
        members: Vec::new(),
    }
}

fn system_entry(
    key: EntryKey,
    text: &str,
    geo: &Geo,
    th: &Theme,
    level: Option<NoticeLevel>,
) -> Entry {
    let (color, rail, kind) = match level {
        None | Some(NoticeLevel::Info) => (th.gray, Rail::None, Kind::System),
        Some(NoticeLevel::Warn) => (th.warning, Rail::Solid(th.warning), Kind::Warning),
        Some(NoticeLevel::Error) => (th.accent_error, Rail::Solid(th.accent_error), Kind::Warning),
    };
    let mut rows = Vec::new();
    for line in text.trim_end().lines() {
        for l in wrap(line, geo.cw) {
            rows.push(Row::new().rail(rail).seg(geo.cx, l, st(color)));
        }
    }
    if rows.is_empty() {
        rows.push(Row::new().rail(rail));
    }
    Entry {
        key,
        kind,
        rows,
        groupable: false,
        collapsed: false,
        selectable: false,
        foldable: false,
        running: false,
        members: Vec::new(),
    }
}

fn marker_text(stop: StopReason, took: Duration) -> Option<String> {
    let d = fmt_duration(took);
    Some(match stop {
        StopReason::EndTurn => format!("Worked for {d}"),
        StopReason::Cancelled => format!("Turn cancelled by user in {d}."),
        StopReason::MaxTurns => format!("Agent was unable to make progress. Turn ended in {d}."),
        StopReason::Error => format!("Turn failed in {d}."),
    })
}

/// Lay out the whole transcript at `geo`, reusing entries whose source did not change.
pub fn build(app: &mut App, geo: &Geo) -> Doc {
    let th = app.theme.clone();
    let cwd = app.opts.cwd.clone();
    let hm_default = app.fixed_hm;
    let mut entries: Vec<Entry> = Vec::new();
    let mut fresh: HashMap<EntryKey, (u64, Entry)> = HashMap::new();
    let geo_sig = hash_of(&(geo.w, geo.hpad, geo.stamps, th.kind as u8));
    let think_open = app.view.think_open;
    let msgs_len = app.tr.messages.len();

    for mi in 0..msgs_len {
        let role = app.tr.messages[mi].role;
        match role {
            Role::User => {
                let key = (mi, 0);
                let stamp = *app
                    .stamps
                    .entry(key)
                    .or_insert_with(|| Some(hm_default.unwrap_or_else(local_hm)));
                let text = match app.tr.messages[mi].parts.first() {
                    Some(Part::Text(t)) => t.clone(),
                    _ => String::new(),
                };
                let exp = app.view.folds.get(&key).copied();
                let sig = hash_of(&(geo_sig, &text, exp, stamp));
                let e = cached(&mut app.view.cache, &mut fresh, key, sig, || {
                    user_entry(key, &text, geo, &th, stamp, exp)
                });
                entries.push(e);
            }
            Role::Notice(level) => {
                let key = (mi, 0);
                let text = match app.tr.messages[mi].parts.first() {
                    Some(Part::Text(t)) => t.clone(),
                    _ => String::new(),
                };
                let sig = hash_of(&(geo_sig, &text, level as u8));
                let e = cached(&mut app.view.cache, &mut fresh, key, sig, || {
                    system_entry(key, &text, geo, &th, Some(level))
                });
                entries.push(e);
            }
            Role::Assistant => {
                let parts = app.tr.messages[mi].parts.clone();
                let took = app.tr.messages[mi].took;
                let stop = app.tr.messages[mi].stop;
                let mut stamps: Vec<Option<(u8, u8)>> = Vec::new();
                for (pi, part) in parts.iter().enumerate() {
                    if matches!(part, Part::Text(_)) {
                        let s = *app
                            .stamps
                            .entry((mi, pi))
                            .or_insert_with(|| Some(hm_default.unwrap_or_else(local_hm)));
                        stamps.push(s);
                    } else {
                        stamps.push(None);
                    }
                }
                let collapsed_thought = |pi: usize| match &parts[pi] {
                    Part::Thought { took: Some(_), .. } => {
                        !app.view.is_expanded((mi, pi), think_open)
                    }
                    _ => false,
                };
                for item in plan_parts(&parts, &collapsed_thought) {
                    match item {
                        Planned::Text(pi) => {
                            let Part::Text(t) = &parts[pi] else { continue };
                            if t.trim().is_empty() {
                                continue;
                            }
                            let key = (mi, pi);
                            let raw = app.view.nav.raw.contains(&key);
                            let sig = hash_of(&(geo_sig, t, stamps[pi], raw));
                            let e = cached(&mut app.view.cache, &mut fresh, key, sig, || {
                                agent_entry(key, t, geo, &th, stamps[pi], raw)
                            });
                            entries.push(e);
                        }
                        Planned::Thought(pi) => {
                            let Part::Thought { text, took, .. } = &parts[pi] else {
                                continue;
                            };
                            let key = (mi, pi);
                            let expanded = app.view.is_expanded(key, think_open);
                            let sig = hash_of(&(
                                geo_sig,
                                text,
                                took.map(|d| d.as_millis() / 100),
                                expanded,
                            ));
                            let e = cached(&mut app.view.cache, &mut fresh, key, sig, || {
                                thinking_entry(key, text, *took, expanded, geo, &th)
                            });
                            entries.push(e);
                        }
                        Planned::Tool(pi) => {
                            let Part::Tool(call) = &parts[pi] else {
                                continue;
                            };
                            let key = (mi, pi);
                            let Some(class) = tools::classify(call) else {
                                continue;
                            };
                            let expanded = app.view.folds.get(&key).copied();
                            let sig = hash_of(&(
                                geo_sig,
                                &call.id,
                                &call.title,
                                call.status as u8,
                                call.output.as_ref().map(|o| o.len()),
                                call.diff
                                    .as_ref()
                                    .map(|d| (d.new.len(), d.old.as_ref().map(|o| o.len()))),
                                expanded,
                            ));
                            let cwd2 = cwd.clone();
                            let e = cached(&mut app.view.cache, &mut fresh, key, sig, || {
                                tools::tool_entry(key, call, class, expanded, geo, &th, &cwd2)
                            });
                            entries.push(e);
                        }
                        Planned::Group(members) => {
                            let first_tool = members
                                .iter()
                                .copied()
                                .find(|&i| matches!(parts[i], Part::Tool(_)))
                                .unwrap_or(members[0]);
                            let key = (mi, first_tool);
                            let expanded = app.view.is_expanded(key, false);
                            let items: Vec<(EntryKey, &Part)> =
                                members.iter().map(|&i| ((mi, i), &parts[i])).collect();
                            let plain: Vec<&Part> = items.iter().map(|m| m.1).collect();
                            let open_set = &app.view.open_members;
                            let opened = |k: EntryKey| open_set.contains(&k);
                            let bits: Vec<bool> = items.iter().map(|m| opened(m.0)).collect();
                            let sig = hash_of(&(geo_sig, expanded, group_sig(&plain), bits));
                            let cwd2 = cwd.clone();
                            let e = cached(&mut app.view.cache, &mut fresh, key, sig, || {
                                tools::group_entry(key, &items, expanded, geo, &th, &cwd2, &opened)
                            });
                            entries.push(e);
                        }
                    }
                }
                if let (Some(took), Some(stop)) = (took, stop) {
                    let has_body = !parts.is_empty();
                    if has_body {
                        if let Some(t) = marker_text(stop, took) {
                            let key = (mi, usize::MAX);
                            let sig = hash_of(&(geo_sig, &t));
                            let e = cached(&mut app.view.cache, &mut fresh, key, sig, || {
                                system_entry(key, &t, geo, &th, None)
                            });
                            entries.push(e);
                        }
                    }
                }
            }
        }
    }
    app.view.cache = fresh;
    let mut doc = Doc {
        entries,
        width: geo.w,
        tight: geo.tight,
        ..Default::default()
    };
    doc.finish();
    doc
}

/// One entry laid out open at `geo`, for the block viewer. `None` for rows with nothing to open.
pub fn build_open(app: &App, key: EntryKey, geo: &Geo) -> Option<Entry> {
    let th = &app.theme;
    let m = app.tr.messages.get(key.0)?;
    match m.role {
        Role::User => {
            let Some(Part::Text(t)) = m.parts.first() else {
                return None;
            };
            Some(user_entry(key, t, geo, th, None, Some(true)))
        }
        Role::Notice(_) => None,
        Role::Assistant => match m.parts.get(key.1)? {
            Part::Text(t) => Some(agent_entry(
                key,
                t,
                geo,
                th,
                None,
                app.view.nav.raw.contains(&key),
            )),
            Part::Thought { text, took, .. } => Some(thinking_entry(
                key,
                text,
                Some(took.unwrap_or_default()),
                true,
                geo,
                th,
            )),
            Part::Tool(c) => {
                let class = tools::classify(c)?;
                Some(tools::tool_entry(
                    key,
                    c,
                    class,
                    Some(true),
                    geo,
                    th,
                    &app.opts.cwd,
                ))
            }
        },
    }
}

fn group_sig(items: &[&Part]) -> u64 {
    let mut h = DefaultHasher::new();
    for p in items {
        match p {
            Part::Tool(c) => (
                &c.id,
                c.status as u8,
                &c.title,
                c.output.as_ref().map(|o| o.len()),
            )
                .hash(&mut h),
            Part::Thought { text, took, .. } => {
                (text.len(), took.map(|d| d.as_millis() / 100)).hash(&mut h)
            }
            Part::Text(t) => t.len().hash(&mut h),
        }
    }
    h.finish()
}

fn cached(
    old: &mut HashMap<EntryKey, (u64, Entry)>,
    fresh: &mut HashMap<EntryKey, (u64, Entry)>,
    key: EntryKey,
    sig: u64,
    make: impl FnOnce() -> Entry,
) -> Entry {
    let e = match old.remove(&key) {
        Some((s, e)) if s == sig => e,
        _ => make(),
    };
    fresh.insert(key, (sig, e.clone()));
    e
}

enum Planned {
    Text(usize),
    Thought(usize),
    Tool(usize),
    Group(Vec<usize>),
}

/// Walk a message's parts: consecutive read/search/list/fetch tools, with the collapsed finished
/// thoughts touching them, fold into one verb group; everything else is its own entry.
fn plan_parts(parts: &[Part], collapsed_thought: &dyn Fn(usize) -> bool) -> Vec<Planned> {
    let groupable = |p: &Part| match p {
        Part::Tool(c) => tools::classify(c).is_some_and(|k| k.group_bucket().is_some()),
        _ => false,
    };
    let finished_thought = |p: &Part| matches!(p, Part::Thought { took: Some(_), .. });
    let mut out = Vec::new();
    let mut i = 0;
    while i < parts.len() {
        if groupable(&parts[i]) || collapsed_thought(i) {
            // collapsed thoughts in front, then tools with any finished thoughts between them
            // (those fold in whether or not they are open), then collapsed thoughts behind
            let mut s = i;
            while s < parts.len() && collapsed_thought(s) {
                s += 1;
            }
            if s < parts.len() && groupable(&parts[s]) {
                let mut last = s;
                let mut j = s;
                while j < parts.len() {
                    if groupable(&parts[j]) {
                        last = j;
                        j += 1;
                    } else if finished_thought(&parts[j]) {
                        j += 1;
                    } else {
                        break;
                    }
                }
                let mut e = last + 1;
                while e < parts.len() && collapsed_thought(e) {
                    e += 1;
                }
                out.push(Planned::Group((i..e).collect()));
                i = e;
                continue;
            }
        }
        match &parts[i] {
            Part::Text(_) => out.push(Planned::Text(i)),
            Part::Thought { .. } => out.push(Planned::Thought(i)),
            Part::Tool(_) => out.push(Planned::Tool(i)),
        }
        i += 1;
    }
    out
}

// ---- painting -------------------------------------------------------------------------------

pub(super) fn paint_row(buf: &mut Buffer, row: &Row, y: u16, tick: u64, th: &Theme, hpad: u16) {
    for &(x0, x1, c) in &row.fills {
        super::fill(buf, Rect::new(x0, y, x1.saturating_sub(x0), 1), c);
    }
    match row.rail {
        Rail::None => {}
        Rail::Solid(c) => {
            put(buf, hpad, y, "┃", st(c));
        }
        Rail::Wave(a) => {
            let c = anim::rail(th.bg_base, a, tick, row.brow);
            put(buf, hpad, y, "┃", st(c));
        }
    }
    for s in &row.segs {
        let style = match s.anim {
            Anim::None => s.style,
            Anim::Wave(a) => s.style.fg(anim::rail(th.bg_base, a, tick, row.brow)),
        };
        put(buf, s.x, y, &s.text, style);
    }
}

pub fn draw(buf: &mut Buffer, app: &mut App, lay: &Layout) {
    let th = app.theme.clone();
    let geo = Geo::new(lay.w, lay.compact, app.timestamps);
    let doc = build(app, &geo);
    app.view.doc = doc;
    app.view.view_rows = lay.view.height as usize;
    app.view.find_rescan();
    let tick = app.tick();
    let top = app.view.top();
    let vh = lay.view.height as usize;
    let mut animating = false;
    let sel = app.view.selected;
    let focus_sb = app.focus == Focus::Scrollback;
    for vy in 0..vh {
        let row_idx = top + vy;
        let y = lay.view.y + vy as u16;
        if let Some((ei, ri)) = app.view.doc.locate(row_idx) {
            let e = &app.view.doc.entries[ei];
            let row = &e.rows[ri];
            animating |= row.animated();
            paint_row(buf, row, y, tick, &th, geo.hpad);
            if focus_sb && sel == Some(e.key) {
                paint_selection_row(buf, e, ri, y, &geo, &th, app.view.member());
            }
        }
    }
    app.anim_in_view = animating;
    super::transcript_nav::paint_hits(buf, &app.view, lay);
    super::transcript_nav::paint_drag(buf, &app.view, lay, &th);
    if !lay.compact {
        draw_sticky(buf, app, lay, &geo, top);
    }
    if focus_sb {
        if let Some(key) = sel {
            paint_selection_edges(buf, app, key, lay, &geo, &th, top, vh);
        }
    }
    draw_scrollbar(buf, app, lay);
    draw_indicator(buf, app, lay);
}

/// The last user prompt that has scrolled off the top stays pinned over the first viewport rows
/// (its collapsed band, at most five rows) with a blank gap row below that hosts `▲`.
fn draw_sticky(buf: &mut Buffer, app: &App, lay: &Layout, geo: &Geo, top: usize) {
    let doc = &app.view.doc;
    let th = &app.theme;
    let Some(i) = (0..doc.entries.len()).rev().find(|&i| {
        doc.entries[i].kind == Kind::User && doc.starts[i] + doc.entries[i].rows.len() <= top
    }) else {
        return;
    };
    let e = &doc.entries[i];
    // an expanded prompt longer than three lines does not pin
    if e.foldable && !e.collapsed_user() {
        return;
    }
    // the pinned copy keeps its timestamp and wrap when `/timestamps` is off (87-timestamps-off)
    let rebuilt;
    let rows: &[Row] = match app.tr.messages.get(e.key.0).and_then(|m| m.parts.first()) {
        Some(Part::Text(t)) if !geo.stamps && geo.hpad != 1 => {
            let g = Geo {
                stamps: true,
                text_w: geo.cw.saturating_sub(10).max(10),
                ..*geo
            };
            let stamp = app.stamps.get(&e.key).copied().flatten();
            rebuilt = user_entry(e.key, t, &g, th, stamp, app.view.folds.get(&e.key).copied());
            &rebuilt.rows
        }
        _ => &e.rows,
    };
    let h = rows.len().min(5) as u16;
    if h + 1 >= lay.view.height {
        return;
    }
    // a newer prompt already at the top (the one just sent) must not be painted over
    if (i + 1..doc.entries.len())
        .find(|&j| doc.entries[j].kind == Kind::User)
        .is_some_and(|j| doc.starts[j] <= top + h as usize)
    {
        return;
    }
    let tick = app.tick();
    for (r, row) in rows.iter().take(h as usize).enumerate() {
        let y = lay.view.y + r as u16;
        super::fill(buf, Rect::new(0, y, lay.w.saturating_sub(1), 1), th.bg_base);
        paint_row(buf, row, y, tick, th, geo.hpad);
    }
    let gap = lay.view.y + h;
    super::fill(
        buf,
        Rect::new(0, gap, lay.w.saturating_sub(1), 1),
        th.bg_base,
    );
    // `▲` when the reply being read starts well above what is shown
    let first_visible = top + h as usize + 1;
    if let Some((ei, ri)) = doc.locate(first_visible) {
        let hidden = ri;
        if doc.entries[ei].kind == Kind::Agent && hidden >= 3 {
            put(buf, lay.w / 2, gap, "▲", st(th.gray_solid()));
        }
    }
}

fn paint_selection_row(
    buf: &mut Buffer,
    e: &Entry,
    ri: usize,
    y: u16,
    geo: &Geo,
    th: &Theme,
    member: Option<usize>,
) {
    let c = th.selection_border;
    let (l, r) = (geo.hpad - 1, geo.w.saturating_sub(geo.hpad));
    put(buf, l, y, "│", st(c));
    put(buf, r, y, "│", st(c));
    let band = Rect::new(geo.hpad, y, geo.w.saturating_sub(2 * geo.hpad), 1);
    // paint `row` over the panel background (reverse video where there is none)
    let panel = |buf: &mut Buffer, row: &Row, glyph: bool| {
        super::fill(buf, band, th.bg_dark);
        for s in &row.segs {
            put(buf, s.x, y, &s.text, s.style.bg(th.bg_dark));
        }
        if glyph {
            if let Some(first) = row.segs.first() {
                if first.text.starts_with('◆') || first.text.starts_with('◈') {
                    put(buf, first.x, y, "›", first.style.bg(th.bg_dark));
                }
            }
        }
        if th.bandless {
            buf.set_style(band, Style::new().add_modifier(Modifier::REVERSED));
        }
    };
    if !e.members.is_empty() {
        // an open group: the header takes the panel colour, and so does the member under the cursor
        if ri == 0 {
            panel(buf, &e.rows[0], false);
        } else if let Some(m) = member.and_then(|k| e.members.get(k)) {
            if m.row == ri {
                panel(buf, &m.sel, false);
            }
        }
    } else if e.collapsed && ri == 0 && e.kind != Kind::User {
        // a selected collapsed header takes the panel background across the row
        panel(buf, &e.rows[ri], true);
    }
}

#[allow(clippy::too_many_arguments)]
fn paint_selection_edges(
    buf: &mut Buffer,
    app: &App,
    key: EntryKey,
    lay: &Layout,
    geo: &Geo,
    th: &Theme,
    top: usize,
    vh: usize,
) {
    let doc = &app.view.doc;
    let Some(i) = doc.index_of(key) else { return };
    let start = doc.starts[i];
    let end = start + doc.entries[i].rows.len();
    let c = th.selection_border;
    let (l, r) = (geo.hpad - 1, geo.w.saturating_sub(geo.hpad));
    let row_y = |row: usize| -> Option<u16> {
        (row >= top && row < top + vh).then(|| lay.view.y + (row - top) as u16)
    };
    // corners sit on the gap row above and below the entry when that row is on screen
    if start > 0 {
        if let Some(y) = row_y(start - 1) {
            put(buf, l, y, "┌", st(c));
            put(buf, r, y, "┐", st(c));
        }
    } else if top == 0 {
        let y = lay.view.y.saturating_sub(1);
        if y > lay.header_y {
            put(buf, l, y, "┌", st(c));
            put(buf, r, y, "┐", st(c));
        }
    }
    if let Some(y) = row_y(end) {
        put(buf, l, y, "└", st(c));
        put(buf, r, y, "┘", st(c));
    }
    // a clipped edge shows dotted sides
    if start < top {
        if let Some(y) = row_y(top) {
            put(buf, l, y, "┆", st(c));
            put(buf, r, y, "┆", st(c));
        }
    }
    if end > top + vh {
        if let Some(y) = row_y(top + vh - 1) {
            put(buf, l, y, "┆", st(c));
            put(buf, r, y, "┆", st(c));
        }
    }
}

fn draw_scrollbar(buf: &mut Buffer, app: &App, lay: &Layout) {
    let v = &app.view;
    let total = v.extent();
    let vh = lay.view.height as usize;
    if total <= vh || vh == 0 {
        return;
    }
    let th = &app.theme;
    let x = lay.w.saturating_sub(1);
    let track = th.scrollbar_bg;
    let thumb = if v.following() {
        crate::theme::blend(th.scrollbar_bg, th.scrollbar_fg, 0.4)
    } else {
        th.scrollbar_fg
    };
    let (pos, size) = super::thumb_rows(total, vh, v.top(), vh).unwrap_or((0, vh));
    for i in 0..vh {
        let y = lay.view.y + i as u16;
        if i >= pos && i < pos + size {
            put(buf, x, y, "█", st(thumb).bg(thumb));
        } else {
            put(buf, x, y, " ", Style::new().bg(track));
        }
    }
}

fn draw_indicator(buf: &mut Buffer, app: &App, lay: &Layout) {
    let v = &app.view;
    if v.following() {
        return;
    }
    let below = (v.top() + lay.view.height as usize) < v.doc.total;
    if below {
        put(buf, lay.w / 2, lay.host_y, "▼", st(app.theme.gray_solid()));
    }
}
