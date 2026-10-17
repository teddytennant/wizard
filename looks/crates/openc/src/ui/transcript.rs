// OWNER: transcript
//! The transcript: a vector of blocks folded from the event stream, per-block row caches keyed
//! by width, and a viewport anchored at `(block, row)` so growth, folding and resize never move
//! what the reader is looking at.
//!
//! Only blocks that touch the viewport are laid out. Everything else costs its cached height,
//! or an estimate from its source length until it has been laid out once (design section e).

use std::collections::HashMap;
use std::time::{Duration, Instant};

use agent_core::{Event, HistoryItem, NoticeLevel, StopReason, Usage};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Style;
use tuikit::paint::{fill, put_str};

use super::blocks::{self, WelcomeInfo};
use super::md::MdState;
use super::row::{Cx, Row};
use super::search::{self, Hit, Search};
use super::select::{self, Gran, Pos, Sel};
use super::tools::{self, ToolEntry};

/// A streaming assistant answer waits this long for a half-typed word before showing it.
pub const PARTIAL_WORD_MS: u128 = 120;
/// Clicks on one cell closer together than this count as a double and then a triple click.
const CLICK_GAP: Duration = Duration::from_millis(400);
/// A turn must run this long to get a footer.
const FOOTER_MIN: Duration = Duration::from_secs(2);

/// What a button release means to the caller.
#[derive(Debug, PartialEq, Eq)]
pub enum MouseUp {
    Nothing,
    /// A plain click on `block`; `head` when it landed on a foldable summary row.
    Click {
        block: usize,
        head: bool,
    },
    /// A selection is ready to copy.
    Copy,
}

pub struct Assistant {
    pub text: String,
    pub streaming: bool,
    pub last_delta: Instant,
    md: MdState,
}

pub enum Kind {
    Welcome(WelcomeInfo),
    User {
        text: String,
        shown: String,
        expanded: bool,
    },
    Assistant(Assistant),
    Thought {
        text: String,
        started: Instant,
        took: Option<Duration>,
        replayed: bool,
    },
    Tools(Vec<ToolEntry>),
    Notice {
        level: NoticeLevel,
        text: String,
    },
    Fatal(String),
    /// A failed request that is being retried: one row, rewritten by each attempt, with a
    /// countdown. `settled` once anything else has happened, which freezes it.
    Retry {
        reason: String,
        attempt: u32,
        max: u32,
        until: Instant,
        /// Seconds the row last showed, so the tick knows when it is stale.
        secs: u64,
        settled: bool,
    },
    Interrupted(Duration),
    Footer {
        took: Duration,
        out: u64,
        cost: Option<f64>,
    },
}

struct Cache {
    ver: u64,
    width: usize,
    detail: bool,
    dark: bool,
    stamp: u64,
    rows: Vec<Row>,
}

pub struct Block {
    pub id: u64,
    pub kind: Kind,
    pub ver: u64,
    /// User override of the fold state; `None` means the kind's default.
    pub open: Option<bool>,
    /// Blank rows above: 1 between blocks, 0 between consecutive tool cards.
    pub lead: u16,
    est: u32,
    cache: Option<Cache>,
    /// Lowercased source for search, for the `ver` it was made at. A search keystroke asks every
    /// block whether it holds the text, and lowercasing all of them each time was most of the cost.
    lower: Option<(u64, String)>,
}

impl Block {
    fn new(id: u64, kind: Kind, prev: Option<&Kind>) -> Block {
        let lead = match (prev, &kind) {
            (None, _) => 0,
            (Some(Kind::Tools(_)), Kind::Tools(_)) => 0,
            // A folded thought is as quiet as a tool row and sits in the same run of rows
            // (finding 16: three rows spent on every thought of a tool turn).
            (Some(Kind::Tools(_)), Kind::Thought { .. })
            | (Some(Kind::Thought { .. }), Kind::Tools(_)) => 0,
            _ => 1,
        };
        let mut b = Block {
            id,
            kind,
            ver: 1,
            open: None,
            lead,
            est: 1,
            cache: None,
            lower: None,
        };
        b.est = b.estimate();
        b
    }

    fn estimate(&self) -> u32 {
        let lines = |s: &str| s.bytes().filter(|b| *b == b'\n').count() as u32 + 1;
        match &self.kind {
            Kind::Welcome(_) => blocks::WELCOME_ROWS as u32,
            Kind::User { shown, .. } => lines(shown),
            Kind::Assistant(a) => lines(&a.text) + lines(&a.text) / 6,
            Kind::Thought { .. } => 1,
            Kind::Tools(e) => e.len().min(3) as u32,
            Kind::Notice { text, .. } => lines(text),
            Kind::Fatal(_) => 3,
            Kind::Retry { .. } => 2,
            Kind::Interrupted(_) | Kind::Footer { .. } => 1,
        }
    }

    /// Rows for this width and mode are already cached.
    pub fn is_laid(&self, cx: &Cx) -> bool {
        self.cache
            .as_ref()
            .is_some_and(|c| c.ver == self.ver && c.width == cx.width && c.detail == cx.detail)
    }

    pub fn is_tool(&self) -> bool {
        matches!(self.kind, Kind::Tools(_))
    }

    pub fn is_user(&self) -> bool {
        matches!(self.kind, Kind::User { .. })
    }

    pub fn foldable(&self) -> bool {
        matches!(
            self.kind,
            Kind::Tools(_) | Kind::Thought { .. } | Kind::User { .. }
        )
    }

    /// Whether the block is open under `detail`.
    pub fn is_open(&self, detail: bool) -> bool {
        if detail {
            return true;
        }
        match &self.kind {
            Kind::Tools(e) => self.open.unwrap_or_else(|| tools::default_open(e)),
            Kind::Thought { .. } => self.open.unwrap_or(false),
            Kind::User { expanded, .. } => *expanded,
            _ => false,
        }
    }

    fn stamp(&self, cx: &Cx) -> u64 {
        match &self.kind {
            Kind::Tools(e)
                if e.iter()
                    .any(|x| x.running() || x.children.iter().any(ToolEntry::running)) =>
            {
                cx.spin as u64 + 1
            }
            Kind::Thought { took: None, .. } => cx.spin as u64 + 1,
            Kind::Assistant(a) if a.streaming => {
                1 + u64::from(
                    cx.now.saturating_duration_since(a.last_delta).as_millis() >= PARTIAL_WORD_MS,
                )
            }
            _ => 0,
        }
    }

    /// Does the layout change with the clock?
    pub fn animated(&self) -> bool {
        match &self.kind {
            Kind::Tools(e) => e
                .iter()
                .any(|x| x.running() || x.children.iter().any(ToolEntry::running)),
            Kind::Thought { took, .. } => took.is_none(),
            Kind::Assistant(a) => a.streaming,
            _ => false,
        }
    }

    pub fn rows(&mut self, cx: &Cx) -> &[Row] {
        let stamp = self.stamp(cx);
        let fresh = self.cache.as_ref().is_some_and(|c| {
            c.ver == self.ver
                && c.width == cx.width
                && c.detail == cx.detail
                && c.dark == cx.p.is_dark()
                && c.stamp == stamp
        });
        if !fresh {
            let rows = self.layout(cx);
            self.cache = Some(Cache {
                ver: self.ver,
                width: cx.width,
                detail: cx.detail,
                dark: cx.p.is_dark(),
                stamp,
                rows,
            });
        }
        self.cached_rows().expect("just set")
    }

    /// The laid-out rows, if there are any. An answer keeps them in its streaming state, so
    /// the cache holds none for it and a frame never copies the rows of a long answer.
    fn cached_rows(&self) -> Option<&[Row]> {
        let c = self.cache.as_ref()?;
        Some(match &self.kind {
            Kind::Assistant(a) => &a.md.rows,
            _ => &c.rows,
        })
    }

    fn layout(&mut self, cx: &Cx) -> Vec<Row> {
        let open = self.is_open(cx.detail);
        let user_open = self.open;
        match &mut self.kind {
            Kind::Welcome(w) => blocks::welcome(w, cx),
            Kind::User {
                text,
                shown,
                expanded,
            } => blocks::user(if *expanded { text } else { shown }, cx),
            Kind::Assistant(a) => {
                let partial_ok =
                    cx.now.saturating_duration_since(a.last_delta).as_millis() >= PARTIAL_WORD_MS;
                a.md.update(&a.text, cx, !a.streaming, partial_ok);
                Vec::new()
            }
            Kind::Thought {
                text,
                started,
                took,
                replayed,
            } => blocks::thought(
                text,
                *took,
                *replayed,
                open,
                cx,
                cx.now.saturating_duration_since(*started),
            ),
            Kind::Tools(e) => tools::layout(e, user_open, cx),
            Kind::Notice { level, text } => blocks::notice(*level, text, cx),
            Kind::Fatal(t) => blocks::fatal(t, cx),
            Kind::Retry {
                reason,
                attempt,
                max,
                until,
                settled,
                ..
            } => blocks::retry(reason, *attempt, *max, *until, *settled, cx),
            Kind::Interrupted(d) => blocks::interrupted(*d, cx),
            Kind::Footer { took, out, cost } => blocks::footer(*took, *out, *cost, cx),
        }
    }

    /// Height in rows including the lead.
    pub fn height(&mut self, cx: &Cx) -> usize {
        self.lead as usize + self.rows(cx).len()
    }

    fn known_height(&self, width: usize) -> usize {
        match &self.cache {
            Some(c) if c.width == width && c.ver == self.ver => {
                self.lead as usize + self.cached_rows().map_or(0, <[Row]>::len)
            }
            _ => self.lead as usize + self.est as usize,
        }
    }

    /// Cached row at `r` (a row index counted from the lead); blank for lead rows.
    pub fn row_at(&self, r: usize) -> Option<&Row> {
        let rows = self.cached_rows()?;
        r.checked_sub(self.lead as usize).and_then(|i| rows.get(i))
    }

    /// Does the source hold `lowered` (already lowercase)? The lowercase copy is kept for the
    /// block's current version unless the block is huge.
    fn source_has(&mut self, lowered: &str) -> bool {
        const KEEP: usize = 1 << 20;
        if let Some((v, l)) = &self.lower {
            if *v == self.ver {
                return l.contains(lowered);
            }
        }
        let l = self.text().to_lowercase();
        let has = l.contains(lowered);
        self.lower = (l.len() <= KEEP).then_some((self.ver, l));
        has
    }

    pub fn text(&self) -> String {
        match &self.kind {
            Kind::Welcome(_) => String::new(),
            Kind::User { text, .. } => text.clone(),
            Kind::Assistant(a) => a.text.clone(),
            Kind::Thought { text, .. } => text.clone(),
            Kind::Tools(e) => tools::copy_text(e),
            Kind::Notice { text, .. } => text.clone(),
            Kind::Fatal(t) => t.clone(),
            Kind::Retry { reason, .. } => format!("Request failed · {reason}"),
            Kind::Interrupted(_) => "interrupted".into(),
            Kind::Footer { .. } => String::new(),
        }
    }
}

/// Search layouts a block is always allowed, and the time after which the rest of a long
/// transcript's matching blocks are left as placeholders until a jump reaches them.
const SEARCH_MIN_LAYOUTS: usize = 40;
const SEARCH_LAYOUT_BUDGET: Duration = Duration::from_millis(8);

/// The hits in one block, from its laid-out rows: every match with a row and column, or one
/// hidden hit when the text matches only inside a fold.
fn block_hits(
    b: &mut Block,
    bi: usize,
    cx: &Cx,
    needle: &[char],
    sensitive: bool,
    lowered: &str,
) -> Vec<Hit> {
    let lead = b.lead as usize;
    let mut out = Vec::new();
    for (i, r) in b.rows(cx).iter().enumerate() {
        for (col, len) in search::occurrences(&r.text(), needle, sensitive) {
            out.push(Hit {
                block: bi,
                row: lead + i,
                col,
                len,
                pending: false,
            });
        }
    }
    if out.is_empty() && b.foldable() && b.source_has(lowered) {
        out.push(Hit {
            block: bi,
            row: lead,
            col: 0,
            len: 0,
            pending: false,
        });
    }
    out
}

struct Turn {
    started: Instant,
    cost0: Option<f64>,
    out_sum: u64,
    prev_out: u64,
    first_block: usize,
}

/// Where the viewport is.
#[derive(Clone, Debug)]
pub struct View {
    /// Follow the end of the transcript.
    pub sticky: bool,
    /// Top of the viewport as `(block, row in block)` when not sticky.
    pub top: (usize, usize),
    /// Nav-mode cursor.
    pub cursor: Option<usize>,
    pub scrolled_at: Option<Instant>,
    pub rev_at_unstick: u64,
    /// `(block, row, width, chars)`: how many letters and digits sit above the top row inside
    /// its block, counted at `width`. A resize re-wraps the block, and the row that holds the
    /// same character is the one that stays on top.
    top_mark: Option<(usize, usize, usize, usize)>,
}

impl Default for View {
    fn default() -> Self {
        View {
            sticky: true,
            top: (0, 0),
            cursor: None,
            scrolled_at: None,
            rev_at_unstick: 0,
            top_mark: None,
        }
    }
}

/// What is on screen: `(block, row)` pairs top to bottom. A transcript shorter than the
/// viewport starts at the first row and leaves the rest blank, so a row once drawn keeps its
/// place while the answer grows (the composer is docked at the bottom edge either way).
#[derive(Default, Debug)]
pub struct Frame {
    pub rows: Vec<(usize, usize)>,
}

pub struct Transcript {
    pub blocks: Vec<Block>,
    next_id: u64,
    tools: HashMap<String, (usize, usize)>,
    subs: HashMap<String, (usize, usize, usize)>,
    turn: Option<Turn>,
    pub usage: Usage,
    pub busy: bool,
    pub rev: u64,
    pub search: Option<Search>,
    /// Block of the last frame's row, for mouse hit testing: `(screen_y, block, head)`.
    pub hits: Vec<(u16, usize, bool)>,
    /// A spinner is on screen in the transcript, so the status line does not need its own.
    pub spinner_visible: bool,
    /// Mouse selection, and what the mouse needs to resolve positions: the column origin and
    /// top of the painted area, and `(screen y, block, row)` for every content row painted.
    pub sel: Option<Sel>,
    origin: (u16, u16),
    screen_rows: Vec<(u16, usize, usize)>,
    last_click: Option<(Instant, u16, u16, u8)>,
    sel_width: usize,
    pub turn_started: Option<Instant>,
    /// User messages so far, for the rail.
    pub turns: usize,
    /// Time of the event being applied, so replays in tests are deterministic.
    now: Instant,
    changes_cache: Vec<(String, usize, usize, bool)>,
    changes_dirty: bool,
}

impl Default for Transcript {
    fn default() -> Self {
        Self::new()
    }
}

impl Transcript {
    pub fn new() -> Transcript {
        let mut t = Transcript {
            blocks: Vec::new(),
            next_id: 1,
            tools: HashMap::new(),
            subs: HashMap::new(),
            turn: None,
            usage: Usage::default(),
            busy: false,
            rev: 0,
            search: None,
            hits: Vec::new(),
            spinner_visible: false,
            sel: None,
            origin: (0, 0),
            screen_rows: Vec::new(),
            last_click: None,
            sel_width: 0,
            turn_started: None,
            turns: 0,
            now: Instant::now(),
            changes_cache: Vec::new(),
            changes_dirty: false,
        };
        t.push(Kind::Welcome(WelcomeInfo::default()));
        t
    }

    pub fn set_welcome(&mut self, info: WelcomeInfo) {
        if let Some(Block {
            kind: Kind::Welcome(w),
            ver,
            ..
        }) = self.blocks.first_mut()
        {
            *w = info;
            *ver += 1;
            self.rev += 1;
        }
    }

    /// Drop everything but the welcome block.
    pub fn reset(&mut self) {
        let info = match self.blocks.first() {
            Some(Block {
                kind: Kind::Welcome(w),
                ..
            }) => w.clone(),
            _ => WelcomeInfo::default(),
        };
        self.blocks.clear();
        self.tools.clear();
        self.subs.clear();
        self.turn = None;
        self.busy = false;
        self.turn_started = None;
        self.search = None;
        self.sel = None;
        // A new session starts with an empty context; the cost keeps running.
        self.usage.context_tokens = 0;
        self.usage.input_tokens = 0;
        self.usage.output_tokens = 0;
        self.turns = 0;
        self.changes_dirty = true;
        self.rev += 1;
        self.push(Kind::Welcome(info));
    }

    fn push(&mut self, kind: Kind) -> usize {
        self.close_streaming();
        let id = self.next_id;
        self.next_id += 1;
        let prev = self.blocks.last().map(|b| &b.kind);
        let b = Block::new(id, kind, prev);
        self.blocks.push(b);
        self.rev += 1;
        self.blocks.len() - 1
    }

    /// The tail text block and thought stop growing when anything else arrives.
    fn close_streaming(&mut self) {
        let now = self.now;
        if let Some(b) = self.blocks.last_mut() {
            match &mut b.kind {
                Kind::Assistant(a) if a.streaming => {
                    a.streaming = false;
                    b.ver += 1;
                }
                Kind::Thought { took, started, .. } if took.is_none() => {
                    *took = Some(now.saturating_duration_since(*started));
                    b.ver += 1;
                }
                _ => {}
            }
        }
    }

    pub fn push_user(&mut self, text: String, shown: String) {
        self.turns += 1;
        self.push(Kind::User {
            text,
            shown,
            expanded: false,
        });
    }

    fn append_text(&mut self, t: &str, now: Instant) {
        if let Some(Block {
            kind: Kind::Assistant(a),
            ver,
            est,
            ..
        }) = self.blocks.last_mut()
        {
            if a.streaming {
                a.text.push_str(t);
                a.last_delta = now;
                *ver += 1;
                // Only the new bytes are counted: recounting the whole answer on every delta made
                // applying n bytes cost O(n^2).
                let new_lines = t.bytes().filter(|b| *b == b'\n').count() as u32;
                *est = est.saturating_add(new_lines);
                return;
            }
        }
        self.push(Kind::Assistant(Assistant {
            text: t.to_string(),
            streaming: true,
            last_delta: now,
            md: MdState::default(),
        }));
    }

    fn append_thought(&mut self, t: &str, now: Instant) {
        if let Some(Block {
            kind: Kind::Thought {
                text, took: None, ..
            },
            ver,
            ..
        }) = self.blocks.last_mut()
        {
            text.push_str(t);
            *ver += 1;
            return;
        }
        self.push(Kind::Thought {
            text: t.to_string(),
            started: now,
            took: None,
            replayed: false,
        });
    }

    fn tool_event(&mut self, call: &agent_core::ToolCall, now: Instant, replay: bool) {
        if tools::hidden(&call.name) {
            return;
        }
        if call.kind == agent_core::ToolKind::Edit {
            self.changes_dirty = true;
        }
        if let Some(pid) = &call.parent_id {
            if let Some(&(bi, ei)) = self.tools.get(pid) {
                if let Some(Block {
                    kind: Kind::Tools(entries),
                    ver,
                    ..
                }) = self.blocks.get_mut(bi)
                {
                    let parent = &mut entries[ei];
                    match parent.children.iter_mut().find(|c| c.call.id == call.id) {
                        Some(c) => c.update(call.clone(), now),
                        None => parent
                            .children
                            .push(ToolEntry::new(call.clone(), now, replay)),
                    }
                    *ver += 1;
                    self.rev += 1;
                }
            }
            return;
        }
        if let Some(&(bi, ei)) = self.tools.get(&call.id) {
            if let Some(Block {
                kind: Kind::Tools(entries),
                ver,
                ..
            }) = self.blocks.get_mut(bi)
            {
                entries[ei].update(call.clone(), now);
                *ver += 1;
                self.rev += 1;
                return;
            }
        }
        self.close_streaming();
        let entry = ToolEntry::new(call.clone(), now, replay);
        // Merge into the previous card when both are reads or both are searches.
        if tools::mergeable(call.kind) {
            if let Some(bi) = self.blocks.len().checked_sub(1) {
                if let Block {
                    kind: Kind::Tools(entries),
                    ver,
                    open: None,
                    ..
                } = &mut self.blocks[bi]
                {
                    if entries
                        .iter()
                        .all(|e| e.call.kind == call.kind && e.call.parent_id.is_none())
                    {
                        entries.push(entry);
                        let ei = entries.len() - 1;
                        *ver += 1;
                        self.tools.insert(call.id.clone(), (bi, ei));
                        self.rev += 1;
                        return;
                    }
                }
            }
        }
        let bi = self.push(Kind::Tools(vec![entry]));
        self.tools.insert(call.id.clone(), (bi, 0));
    }

    /// A turn ended by an interrupt: calls that failed in the last moments were cut short by it.
    fn mark_interrupted(&mut self, from: usize, now: Instant) {
        let recent = Duration::from_secs(3);
        for b in self.blocks.iter_mut().skip(from) {
            if let Kind::Tools(entries) = &mut b.kind {
                let mut changed = false;
                for e in entries.iter_mut() {
                    let mut cut = |c: &mut ToolEntry| {
                        if c.failed()
                            && !c.interrupted
                            && c.ended
                                .is_none_or(|t| now.saturating_duration_since(t) < recent)
                        {
                            c.interrupted = true;
                            changed = true;
                        }
                    };
                    e.children.iter_mut().for_each(&mut cut);
                    cut(e);
                }
                if changed {
                    b.ver += 1;
                }
            }
        }
    }

    fn fail_running(&mut self, from: usize, now: Instant) {
        self.changes_dirty = true;
        for b in self.blocks.iter_mut().skip(from) {
            if let Kind::Tools(entries) = &mut b.kind {
                let mut changed = false;
                for e in entries.iter_mut() {
                    for c in e.children.iter_mut() {
                        if c.running() {
                            c.call.status = agent_core::ToolStatus::Failed;
                            c.took = c.started.map(|s| now.saturating_duration_since(s));
                            changed = true;
                        }
                    }
                    if e.running() {
                        e.call.status = agent_core::ToolStatus::Failed;
                        e.took = e.started.map(|s| now.saturating_duration_since(s));
                        changed = true;
                    }
                }
                if changed {
                    b.ver += 1;
                }
            }
        }
    }

    /// Whole seconds a countdown to `until` shows.
    fn retry_secs(until: Instant, now: Instant) -> u64 {
        until.saturating_duration_since(now).as_secs_f64().ceil() as u64
    }

    /// Any event after a retry row means the retries are over; it stops counting down.
    fn settle_retry(&mut self) {
        if let Some(b) = self.blocks.last_mut() {
            if let Kind::Retry { settled, .. } = &mut b.kind {
                if !*settled {
                    *settled = true;
                    b.ver += 1;
                }
            }
        }
    }

    /// When the open retry row next changes what it shows (the next whole second).
    pub fn retry_wake(&self, now: Instant) -> Option<Instant> {
        match self.blocks.last().map(|b| &b.kind) {
            Some(Kind::Retry {
                until,
                settled: false,
                ..
            }) if *until > now => {
                let secs = Self::retry_secs(*until, now);
                Some(*until - Duration::from_secs(secs.saturating_sub(1)))
            }
            _ => None,
        }
    }

    /// Redraw the open retry row when its countdown has moved on.
    pub fn tick_retry(&mut self, now: Instant) {
        if let Some(b) = self.blocks.last_mut() {
            if let Kind::Retry {
                until,
                secs,
                settled: false,
                ..
            } = &mut b.kind
            {
                let s = Self::retry_secs(*until, now);
                if s != *secs {
                    *secs = s;
                    b.ver += 1;
                    self.rev += 1;
                }
            }
        }
    }

    pub fn apply(&mut self, ev: &Event, now: Instant) {
        self.now = now;
        self.rev += 1;
        if matches!(
            ev,
            Event::TextDelta(_)
                | Event::ThoughtDelta(_)
                | Event::Tool(_)
                | Event::TurnEnd(_)
                | Event::Fatal(_)
        ) {
            self.settle_retry();
        }
        match ev {
            Event::TurnStart => {
                if self.turn.is_none() {
                    self.turn = Some(Turn {
                        started: now,
                        cost0: self.usage.cost_usd,
                        out_sum: 0,
                        prev_out: 0,
                        first_block: self.blocks.len(),
                    });
                    self.turn_started = Some(now);
                }
                self.busy = true;
            }
            Event::TextDelta(t) => {
                self.busy = true;
                self.append_text(t, now);
            }
            Event::ThoughtDelta(t) => {
                self.busy = true;
                self.append_thought(t, now);
            }
            Event::Tool(c) => {
                self.busy = true;
                self.tool_event(c, now, false);
            }
            Event::Usage(u) => {
                if let Some(t) = self.turn.as_mut() {
                    if u.output_tokens < t.prev_out {
                        t.out_sum += t.prev_out;
                    }
                    t.prev_out = u.output_tokens;
                }
                self.usage = u.clone();
            }
            Event::TurnEnd(reason) => {
                self.close_streaming();
                let turn = self.turn.take();
                self.turn_started = None;
                self.busy = false;
                let first = turn
                    .as_ref()
                    .map_or(self.blocks.len().saturating_sub(1), |t| t.first_block);
                self.fail_running(first, now);
                let took = turn
                    .as_ref()
                    .map_or(Duration::ZERO, |t| now.saturating_duration_since(t.started));
                match reason {
                    StopReason::Cancelled => {
                        self.mark_interrupted(first, now);
                        // A card that carries `■ interrupted` already says so; the standalone
                        // line is for a turn stopped in prose or thought (finding 8).
                        let card_says_so = matches!(
                            self.blocks.last().map(|b| &b.kind),
                            Some(Kind::Tools(es)) if es.iter().any(|e| e.interrupted)
                        );
                        if !card_says_so {
                            self.push(Kind::Interrupted(took));
                        }
                    }
                    _ if took >= FOOTER_MIN && *reason == StopReason::EndTurn => {
                        let out = turn.as_ref().map_or(0, |t| t.out_sum + t.prev_out);
                        let cost = match (self.usage.cost_usd, turn.as_ref().and_then(|t| t.cost0))
                        {
                            (Some(c), Some(c0)) => Some((c - c0).max(0.0)),
                            (Some(c), None) => Some(c),
                            _ => None,
                        };
                        self.push(Kind::Footer { took, out, cost });
                    }
                    _ => {}
                }
            }
            Event::Notice { text, .. }
                if backend_claude::mapper::RetryNotice::parse(text).is_some() =>
            {
                let r = backend_claude::mapper::RetryNotice::parse(text).expect("checked");
                let until = now + Duration::from_millis(r.delay_ms);
                let kind = Kind::Retry {
                    reason: r.reason,
                    attempt: r.attempt,
                    max: r.max,
                    until,
                    secs: Self::retry_secs(until, now),
                    settled: false,
                };
                // The same failure again: rewrite the row instead of stacking another.
                match self.blocks.last_mut() {
                    Some(b) if matches!(b.kind, Kind::Retry { settled: false, .. }) => {
                        b.kind = kind;
                        b.ver += 1;
                    }
                    _ => {
                        self.push(kind);
                    }
                }
            }
            Event::Notice { level, text } => {
                self.push(Kind::Notice {
                    level: *level,
                    text: text.clone(),
                });
            }
            Event::Fatal(text) => {
                self.close_streaming();
                self.busy = false;
                self.turn = None;
                self.turn_started = None;
                let first = self.blocks.len().saturating_sub(8);
                self.fail_running(first, now);
                self.push(Kind::Fatal(text.clone()));
            }
            Event::History { items, .. } => self.replay(items),
            _ => {}
        }
    }

    fn replay(&mut self, items: &[HistoryItem]) {
        self.reset();
        let now = Instant::now();
        for it in items {
            match it {
                HistoryItem::User(t) => {
                    let shown = t.clone();
                    self.push_user(t.clone(), shown);
                }
                HistoryItem::Assistant(t) => {
                    self.push(Kind::Assistant(Assistant {
                        text: t.clone(),
                        streaming: false,
                        last_delta: now,
                        md: MdState::default(),
                    }));
                }
                HistoryItem::Thought(t) => {
                    self.push(Kind::Thought {
                        text: t.clone(),
                        started: now,
                        took: Some(Duration::ZERO),
                        replayed: true,
                    });
                }
                HistoryItem::Tool(c) => self.tool_event(c, now, true),
            }
        }
    }

    // ---- folding and copy ----------------------------------------------------------------

    pub fn toggle(&mut self, i: usize, detail: bool) {
        let Some(b) = self.blocks.get_mut(i) else {
            return;
        };
        if !b.foldable() {
            return;
        }
        match &mut b.kind {
            Kind::User { expanded, .. } => *expanded = !*expanded,
            _ => {
                let cur = b.is_open(detail);
                b.open = Some(!cur);
            }
        }
        b.ver += 1;
        self.rev += 1;
    }

    /// Fold or unfold every foldable block of the turn containing `i`.
    pub fn toggle_turn(&mut self, i: usize, detail: bool) {
        let start = (0..=i.min(self.blocks.len().saturating_sub(1)))
            .rev()
            .find(|k| self.blocks[*k].is_user())
            .unwrap_or(0);
        let end = (i + 1..self.blocks.len())
            .find(|k| self.blocks[*k].is_user())
            .unwrap_or(self.blocks.len());
        let any_closed = (start..end).any(|k| {
            self.blocks[k].foldable()
                && !self.blocks[k].is_user()
                && !self.blocks[k].is_open(detail)
        });
        for k in start..end {
            let b = &mut self.blocks[k];
            if b.foldable() && !b.is_user() {
                b.open = Some(any_closed);
                b.ver += 1;
            }
        }
        self.rev += 1;
    }

    /// Run a fold change and keep block `i` on the screen row it was on, so the line you
    /// clicked does not slide away when its body opens or closes.
    pub fn keep_row(
        &mut self,
        view: &mut View,
        i: usize,
        vh: usize,
        cx: &Cx,
        change: impl FnOnce(&mut Transcript),
    ) {
        let at = self.screen_rows.iter().find(|r| r.1 == i).copied();
        change(self);
        let Some((y, _, k)) = at else {
            return;
        };
        let dy = y.saturating_sub(self.origin.1) as isize;
        let k = k.min(self.height(i, cx).saturating_sub(1));
        if view.sticky {
            view.rev_at_unstick = self.rev;
        }
        let shown = view.scrolled_at;
        view.sticky = false;
        view.top = (i, k);
        view.top_mark = None;
        self.scroll_by(view, -dy, vh, cx);
        view.scrolled_at = shown;
    }

    pub fn turn_text(&self, i: usize) -> String {
        let start = (0..=i.min(self.blocks.len().saturating_sub(1)))
            .rev()
            .find(|k| self.blocks[*k].is_user())
            .unwrap_or(0);
        let end = (i + 1..self.blocks.len())
            .find(|k| self.blocks[*k].is_user())
            .unwrap_or(self.blocks.len());
        let mut s = String::new();
        for b in &self.blocks[start..end] {
            let t = b.text();
            if t.is_empty() {
                continue;
            }
            match &b.kind {
                Kind::User { .. } => s.push_str(&format!("> {}\n\n", t.replace('\n', "\n> "))),
                _ => s.push_str(&format!("{t}\n\n")),
            }
        }
        s.trim_end().to_string()
    }

    pub fn last_assistant_text(&self, nth_from_end: usize) -> Option<String> {
        self.blocks
            .iter()
            .rev()
            .filter_map(|b| match &b.kind {
                Kind::Assistant(a) => Some(a.text.clone()),
                _ => None,
            })
            .nth(nth_from_end)
    }

    /// Every row of every block as text, blank lead rows included. Tests use it to check that
    /// a row once laid out never changes while the answer streams.
    pub fn document(&mut self, cx: &Cx) -> Vec<String> {
        let mut out = Vec::new();
        for b in &mut self.blocks {
            let lead = b.lead as usize;
            out.extend(std::iter::repeat_n(String::new(), lead));
            out.extend(b.rows(cx).iter().map(Row::text));
        }
        out
    }

    pub fn has_conversation(&self) -> bool {
        self.blocks
            .iter()
            .any(|b| !matches!(b.kind, Kind::Welcome(_)))
    }

    /// Files touched by edits: `(path, added, removed, new)`. Recomputed only after an edit
    /// call changed, so a long transcript does not pay for it on every frame.
    pub fn changes(&mut self) -> &[(String, usize, usize, bool)] {
        if self.changes_dirty {
            self.changes_dirty = false;
            let mut out: Vec<(String, usize, usize, bool)> = Vec::new();
            for b in &self.blocks {
                let Kind::Tools(entries) = &b.kind else {
                    continue;
                };
                for e in entries {
                    let Some(d) = &e.diff else { continue };
                    if e.failed() || e.running() {
                        continue;
                    }
                    match out.iter_mut().find(|o| o.0 == d.path) {
                        Some(o) => {
                            o.1 += d.added;
                            o.2 += d.removed;
                        }
                        None => out.push((d.path.clone(), d.added, d.removed, d.new_file)),
                    }
                }
            }
            self.changes_cache = out;
        }
        &self.changes_cache
    }

    /// Subagents still running. A running call is always near the tail, so only the last
    /// blocks are looked at.
    pub fn running_agents(&self) -> usize {
        self.blocks
            .iter()
            .rev()
            .take(100)
            .filter(|b| matches!(&b.kind, Kind::Tools(e) if e.iter().any(|x| x.running() && x.verb() == "Task")))
            .count()
    }

    pub fn animating(&self) -> bool {
        self.blocks.iter().rev().take(40).any(Block::animated)
    }

    // ---- search --------------------------------------------------------------------------

    pub fn set_search(&mut self, q: &str) {
        if q.is_empty() {
            self.search = None;
            return;
        }
        match self.search.as_mut() {
            Some(s) if s.query == q => {}
            _ => self.search = Some(Search::new(q)),
        }
    }

    /// `(position, total)` of the active search.
    pub fn search_counts(&self) -> Option<(usize, usize)> {
        self.search.as_ref().map(Search::counts)
    }

    /// The total of the active search still counts blocks not laid out yet as one hit each.
    pub fn search_approximate(&self) -> bool {
        self.search.as_ref().is_some_and(Search::approximate)
    }

    /// Bring the hit list up to date. Cheap when nothing changed; while an answer streams a
    /// growing transcript does not trigger a rescan on every delta.
    pub fn resolve_search(&mut self, cx: &Cx) {
        let busy = self.busy;
        let rev = self.rev;
        let Some(s) = self.search.as_mut() else {
            return;
        };
        let fresh =
            !s.stale && s.width == cx.width && s.detail == cx.detail && (s.rev == rev || busy);
        if fresh {
            return;
        }
        let (needle, sensitive) = s.needle();
        let lowered = s.query.to_lowercase();
        let mut hits = Vec::new();
        let started = std::time::Instant::now();
        let mut laid_now = 0usize;
        for (bi, b) in self.blocks.iter_mut().enumerate() {
            if matches!(b.kind, Kind::Welcome(_)) {
                continue;
            }
            let laid = b.is_laid(cx);
            // A block that was never laid out is only worth a layout when its source has the
            // text; markup in the way (`**a**b`) can hide a match from this prefilter.
            if !laid {
                if !b.source_has(&lowered) {
                    continue;
                }
                // Laying out every block of a long transcript freezes the screen. After a
                // minimum, and a time budget, the rest are placeholders that a jump resolves.
                if laid_now >= SEARCH_MIN_LAYOUTS && started.elapsed() >= SEARCH_LAYOUT_BUDGET {
                    hits.push(Hit {
                        block: bi,
                        row: b.lead as usize,
                        col: 0,
                        len: 0,
                        pending: true,
                    });
                    continue;
                }
                laid_now += 1;
            }
            hits.extend(block_hits(b, bi, cx, &needle, sensitive, &lowered));
        }
        let prev = s.current();
        s.hits = hits;
        s.cur = prev
            .and_then(|p| s.hits.iter().position(|h| *h >= p))
            .unwrap_or(0);
        s.stale = false;
        s.width = cx.width;
        s.detail = cx.detail;
        s.rev = rev;
    }

    /// Select the first hit at or after `from_block` (wrapping), unfolding the block when the
    /// match is hidden in it. Returns the hit to scroll to.
    pub fn search_jump(&mut self, from_block: usize, cx: &Cx) -> Option<Hit> {
        self.resolve_search(cx);
        let s = self.search.as_mut()?;
        if s.hits.is_empty() {
            return None;
        }
        s.cur = s
            .hits
            .iter()
            .position(|h| h.block >= from_block)
            .unwrap_or(0);
        self.open_hidden(cx)
    }

    /// Step to the next or previous hit.
    pub fn search_step(&mut self, forward: bool, cx: &Cx) -> Option<Hit> {
        self.resolve_search(cx);
        let s = self.search.as_mut()?;
        let n = s.hits.len();
        if n == 0 {
            return None;
        }
        s.cur = if forward {
            (s.cur + 1) % n
        } else {
            (s.cur + n - 1) % n
        };
        self.open_hidden(cx)
    }

    /// Lay out `block` and put its real hits where its placeholder was; the current hit becomes
    /// the first of them, or the hit that follows when there are none.
    fn expand_pending(&mut self, block: usize, cx: &Cx) {
        let Some(s) = self.search.as_ref() else {
            return;
        };
        let (needle, sensitive) = s.needle();
        let lowered = s.query.to_lowercase();
        let Some(at) = s.hits.iter().position(|h| h.block == block && h.pending) else {
            return;
        };
        let real = block_hits(
            &mut self.blocks[block],
            block,
            cx,
            &needle,
            sensitive,
            &lowered,
        );
        let Some(s) = self.search.as_mut() else {
            return;
        };
        s.hits.splice(at..=at, real);
        s.cur = if s.hits.is_empty() {
            0
        } else {
            at.min(s.hits.len() - 1)
        };
    }

    /// A hit with no length sits in text a fold hides: open the block and aim at the first
    /// real match inside it.
    fn open_hidden(&mut self, cx: &Cx) -> Option<Hit> {
        // A placeholder is laid out now and replaced by its real hits (maybe none).
        while let Some(h) = self.search.as_ref().and_then(Search::current) {
            if !h.pending {
                break;
            }
            self.expand_pending(h.block, cx);
        }
        let hit = self.search.as_ref()?.current()?;
        if hit.len > 0 {
            return Some(hit);
        }
        if !self.blocks[hit.block].is_open(cx.detail) {
            self.blocks[hit.block].open = Some(true);
            self.blocks[hit.block].ver += 1;
            self.rev += 1;
        }
        if let Some(s) = self.search.as_mut() {
            s.stale = true;
        }
        self.resolve_search(cx);
        let s = self.search.as_mut()?;
        if let Some(i) = s
            .hits
            .iter()
            .position(|h| h.block == hit.block && h.len > 0)
        {
            s.cur = i;
        }
        s.current()
    }

    /// Scroll so the row of `hit` is on screen, a third of the way down when it has to move.
    pub fn reveal_hit(&mut self, view: &mut View, hit: Hit, vh: usize, cx: &Cx) {
        if hit.block >= self.blocks.len() || vh == 0 {
            return;
        }
        view.scrolled_at = Some(Instant::now());
        let f = self.frame(vh, view, cx);
        if f.rows.contains(&(hit.block, hit.row)) {
            return;
        }
        if view.sticky {
            view.rev_at_unstick = self.rev;
        }
        view.sticky = false;
        view.top = (hit.block, hit.row);
        self.scroll_by(view, -((vh / 3) as isize), vh, cx);
    }

    // ---- viewport ------------------------------------------------------------------------

    pub fn height(&mut self, i: usize, cx: &Cx) -> usize {
        self.blocks[i].height(cx)
    }

    /// Letters and digits in the rows of block `b` above row `r`.
    fn chars_before(&mut self, b: usize, r: usize, cx: &Cx) -> usize {
        let lead = self.blocks[b].lead as usize;
        let rows = self.blocks[b].rows(cx);
        rows.iter()
            .take(r.saturating_sub(lead))
            .map(row_chars)
            .sum()
    }

    /// The row of block `b` that holds the `n`th letter or digit; `fallback` when there is
    /// nothing above (the top row was blank or a rule).
    fn row_with_chars(&mut self, b: usize, n: usize, fallback: usize, cx: &Cx) -> usize {
        if n == 0 {
            return fallback;
        }
        let lead = self.blocks[b].lead as usize;
        let rows = self.blocks[b].rows(cx);
        let mut seen = 0;
        for (i, row) in rows.iter().enumerate() {
            seen += row_chars(row);
            if seen > n {
                return lead + i;
            }
        }
        (lead + rows.len()).saturating_sub(1)
    }

    /// Top anchor that shows the last `vh` rows.
    fn bottom_anchor(&mut self, vh: usize, cx: &Cx) -> (usize, usize) {
        let mut need = vh;
        let mut i = self.blocks.len();
        while i > 0 {
            i -= 1;
            let h = self.height(i, cx);
            if h >= need {
                return (i, h - need);
            }
            need -= h;
        }
        (0, 0)
    }

    /// Rows from `top` to the end, stopping early once more than `limit`.
    fn rows_from(&mut self, top: (usize, usize), limit: usize, cx: &Cx) -> usize {
        let mut n = 0;
        for i in top.0..self.blocks.len() {
            let h = self.height(i, cx);
            n += if i == top.0 {
                h.saturating_sub(top.1)
            } else {
                h
            };
            if n > limit {
                break;
            }
        }
        n
    }

    pub fn frame(&mut self, vh: usize, view: &mut View, cx: &Cx) -> Frame {
        let mut f = Frame::default();
        if vh == 0 || self.blocks.is_empty() {
            return f;
        }
        if !view.sticky {
            let (mut b, mut r) = view.top;
            b = b.min(self.blocks.len() - 1);
            let h = self.height(b, cx);
            r = r.min(h.saturating_sub(1));
            match view.top_mark {
                Some((mb, _, mw, mc)) if mb == b && mw != cx.width => {
                    // The block was re-wrapped: keep the same text on top, not the same row.
                    r = self.row_with_chars(b, mc, r, cx).min(h.saturating_sub(1));
                    view.top_mark = Some((b, r, cx.width, mc));
                }
                Some((mb, mr, mw, _)) if (mb, mr, mw) == (b, r, cx.width) => {}
                _ => {
                    let n = self.chars_before(b, r, cx);
                    view.top_mark = Some((b, r, cx.width, n));
                }
            }
            view.top = (b, r);
            if self.rows_from(view.top, vh, cx) <= vh {
                view.sticky = true;
            }
        }
        let (mut b, mut r) = if view.sticky {
            self.bottom_anchor(vh, cx)
        } else {
            view.top
        };
        while f.rows.len() < vh && b < self.blocks.len() {
            let h = self.height(b, cx);
            for k in r..h {
                if f.rows.len() >= vh {
                    break;
                }
                f.rows.push((b, k));
            }
            b += 1;
            r = 0;
        }
        f
    }

    /// Row estimate: `(rows above the viewport top, total rows)`.
    pub fn extent(&self, view: &View, vh: usize, width: usize) -> (usize, usize) {
        let mut above = 0;
        let mut total = 0;
        let top_block = if view.sticky { None } else { Some(view.top) };
        for (i, b) in self.blocks.iter().enumerate() {
            let h = b.known_height(width);
            if let Some((tb, tr)) = top_block {
                if i < tb {
                    above += h;
                } else if i == tb {
                    above += tr;
                }
            }
            total += h;
        }
        if view.sticky {
            above = total.saturating_sub(vh);
        }
        (above, total)
    }

    pub fn scroll_by(&mut self, view: &mut View, delta: isize, vh: usize, cx: &Cx) {
        if self.blocks.is_empty() || delta == 0 {
            return;
        }
        view.scrolled_at = Some(Instant::now());
        if view.sticky {
            if delta > 0 {
                return;
            }
            view.top = self.bottom_anchor(vh, cx);
            view.sticky = false;
            view.rev_at_unstick = self.rev;
        }
        let (mut b, mut r) = view.top;
        if delta > 0 {
            let mut left = delta as usize;
            loop {
                let h = self.height(b, cx);
                if r + left < h {
                    r += left;
                    break;
                }
                left -= h - r;
                r = 0;
                if b + 1 >= self.blocks.len() {
                    view.sticky = true;
                    return;
                }
                b += 1;
            }
        } else {
            let mut left = (-delta) as usize;
            loop {
                if r >= left {
                    r -= left;
                    break;
                }
                left -= r;
                if b == 0 {
                    r = 0;
                    break;
                }
                b -= 1;
                r = self.height(b, cx);
            }
            if r >= self.height(b, cx) && r > 0 {
                r = self.height(b, cx) - 1;
            }
        }
        view.top = (b, r);
        if self.rows_from(view.top, vh, cx) <= vh {
            view.sticky = true;
        }
    }

    pub fn to_bottom(&self, view: &mut View) {
        view.sticky = true;
        view.scrolled_at = Some(Instant::now());
    }

    pub fn to_top(&mut self, view: &mut View) {
        view.sticky = false;
        view.top = (0, 0);
        view.rev_at_unstick = self.rev;
        view.scrolled_at = Some(Instant::now());
    }

    /// Scroll so block `i` is on screen (top-aligned when it is above or taller than the view).
    pub fn reveal(&mut self, view: &mut View, i: usize, vh: usize, cx: &Cx) {
        if i >= self.blocks.len() || vh == 0 {
            return;
        }
        view.scrolled_at = Some(Instant::now());
        let f = self.frame(vh, view, cx);
        let first = f.rows.iter().position(|(b, _)| *b == i);
        let last = f.rows.iter().rposition(|(b, _)| *b == i);
        let h = self.height(i, cx);
        let lead = self.blocks[i].lead as usize;
        let fully = match (first, last) {
            (Some(a), Some(z)) => {
                z - a + 1 >= h - lead.min(h) || (f.rows[a].1 <= lead && f.rows[z].1 + 1 == h)
            }
            _ => false,
        };
        if fully {
            return;
        }
        let below = first.is_some() && f.rows.first().is_some_and(|(b, _)| *b <= i) && h <= vh;
        if below {
            // Partly cut off at the bottom: bring its end into view.
            let mut top = (i, 0usize);
            let mut need = vh.saturating_sub(h);
            let mut b = i;
            while need > 0 && b > 0 {
                b -= 1;
                let hb = self.height(b, cx);
                if hb >= need {
                    top = (b, hb - need);
                    need = 0;
                } else {
                    need -= hb;
                    top = (b, 0);
                }
            }
            view.top = top;
        } else {
            view.top = (i, lead.min(h.saturating_sub(1)));
        }
        if view.sticky {
            view.rev_at_unstick = self.rev;
        }
        view.sticky = false;
        if self.rows_from(view.top, vh, cx) <= vh {
            view.sticky = true;
        }
    }

    /// Next block index after `i` (or before) that passes `pred`.
    pub fn find_from(
        &self,
        i: usize,
        forward: bool,
        pred: impl Fn(&Block) -> bool,
    ) -> Option<usize> {
        if forward {
            (i + 1..self.blocks.len()).find(|k| pred(&self.blocks[*k]))
        } else {
            (0..i).rev().find(|k| pred(&self.blocks[*k]))
        }
    }

    // ---- mouse selection -----------------------------------------------------------------

    /// `(block, row, display column)` under screen cell `(x, y)`, or `None` outside content.
    fn cell_at(&self, x: u16, y: u16) -> Option<(usize, usize, usize)> {
        let (_, b, k) = *self.screen_rows.iter().find(|r| r.0 == y)?;
        Some((b, k, x.saturating_sub(self.origin.0) as usize))
    }

    /// Like [`Self::cell_at`] but a pointer above or below the content snaps to its first or
    /// last painted row, so a drag past the edge keeps extending.
    fn cell_near(&self, x: u16, y: u16) -> Option<(usize, usize, usize)> {
        if let Some(c) = self.cell_at(x, y) {
            return Some(c);
        }
        let (first, last) = (self.screen_rows.first()?, self.screen_rows.last()?);
        if y < first.0 {
            Some((first.1, first.2, 0))
        } else if y > last.0 {
            Some((last.1, last.2, usize::MAX / 2))
        } else {
            None
        }
    }

    fn block_range(&self, b: usize) -> select::Range {
        let b_rows = self.blocks.get(b).map_or(1, |bl| match bl.cached_rows() {
            Some(rows) => bl.lead as usize + rows.len(),
            None => bl.known_height(self.sel_width),
        });
        (
            Pos {
                block: b,
                row: 0,
                col: 0,
            },
            Pos {
                block: b,
                row: b_rows.saturating_sub(1),
                col: usize::MAX / 2,
            },
        )
    }

    fn gesture_range(&self, gran: Gran, (b, k, col): (usize, usize, usize)) -> select::Range {
        match gran {
            Gran::Char => (
                Pos {
                    block: b,
                    row: k,
                    col,
                },
                Pos {
                    block: b,
                    row: k,
                    col: col + 1,
                },
            ),
            Gran::Word => {
                let text = self.blocks[b].row_at(k).map(Row::text).unwrap_or_default();
                let (c0, c1) = select::word_at(&text, col);
                (
                    Pos {
                        block: b,
                        row: k,
                        col: c0,
                    },
                    Pos {
                        block: b,
                        row: k,
                        col: c1,
                    },
                )
            }
            Gran::Block => self.block_range(b),
        }
    }

    /// Button down at a screen cell. Returns whether it landed on transcript content.
    pub fn mouse_down(&mut self, x: u16, y: u16, now: Instant) -> bool {
        let Some(at) = self.cell_at(x, y) else {
            self.sel = None;
            return false;
        };
        let n = match self.last_click {
            Some((t, lx, ly, n))
                if (lx, ly) == (x, y) && now.saturating_duration_since(t) < CLICK_GAP =>
            {
                n % 3 + 1
            }
            _ => 1,
        };
        self.last_click = Some((now, x, y, n));
        let gran = [Gran::Char, Gran::Word, Gran::Block][usize::from(n - 1)];
        let r = self.gesture_range(gran, at);
        self.sel = Some(Sel {
            gran,
            anchor: r,
            head: r,
            dragged: false,
        });
        true
    }

    /// Pointer moved with the button down. Returns whether the selection changed.
    pub fn mouse_drag(&mut self, x: u16, y: u16) -> bool {
        let Some(gran) = self.sel.as_ref().map(|s| s.gran) else {
            return false;
        };
        let Some(at) = self.cell_near(x, y) else {
            return false;
        };
        let r = self.gesture_range(gran, at);
        let Some(sel) = self.sel.as_mut() else {
            return false;
        };
        let moved = r != sel.head || !sel.dragged;
        sel.dragged |= gran != Gran::Char || r.0 != sel.anchor.0;
        sel.head = r;
        moved
    }

    /// Button released. A drag or a double or triple click ends in a copy; a plain click is
    /// handed back so the caller can fold the block under it.
    pub fn mouse_up(&mut self, x: u16, y: u16) -> MouseUp {
        match self.sel.as_ref() {
            Some(s) if s.active() => MouseUp::Copy,
            Some(_) => {
                self.sel = None;
                match self.cell_at(x, y) {
                    Some((b, k, _)) => MouseUp::Click {
                        block: b,
                        head: self.blocks[b].row_at(k).is_some_and(|r| r.head),
                    },
                    None => MouseUp::Nothing,
                }
            }
            None => MouseUp::Nothing,
        }
    }

    /// The selected text: each row cut to the selected columns, soft-wrapped code lines joined
    /// back, the user bar and the common indent stripped.
    pub fn selection_text(&mut self, cx: &Cx) -> Option<String> {
        let sel = self.sel.clone().filter(Sel::active)?;
        let (s, e) = sel.bounds();
        let wrap_glyph = cx.g.wrap;
        let bar = format!("{} ", cx.g.bar);
        // `(text, covers the row from column 0)`; `None` marks a blank row.
        let mut lines: Vec<(String, bool)> = Vec::new();
        for b in s.block..=e.block.min(self.blocks.len().saturating_sub(1)) {
            let block = &mut self.blocks[b];
            let lead = block.lead as usize;
            let rows = block.rows(cx);
            let total = lead + rows.len();
            let k0 = if b == s.block { s.row } else { 0 };
            let k1 = if b == e.block {
                e.row
            } else {
                total.saturating_sub(1)
            };
            for k in k0..=k1.min(total.saturating_sub(1)) {
                let text = k
                    .checked_sub(lead)
                    .map(|i| rows[i].text())
                    .unwrap_or_default();
                let w = tuikit::width::display_width(&text);
                match sel.cols(b, k, w.max(1)) {
                    Some((c0, c1)) => {
                        let piece = select::slice_cols(&text, c0, c1);
                        lines.push((piece.trim_end().to_string(), c0 == 0));
                    }
                    None => lines.push((String::new(), true)),
                }
            }
        }
        let mut out: Vec<(String, bool)> = Vec::new();
        for (mut t, from0) in lines {
            if from0 {
                if let Some(r) = t.strip_prefix(bar.as_str()) {
                    t = r.to_string();
                }
                let trimmed = t.trim_start();
                if let (Some(rest), Some(prev)) = (trimmed.strip_prefix(wrap_glyph), out.last_mut())
                {
                    prev.0.push_str(rest);
                    continue;
                }
            }
            out.push((t, from0));
        }
        let indent = out
            .iter()
            .filter(|(t, f)| *f && !t.trim().is_empty())
            .map(|(t, _)| t.len() - t.trim_start().len())
            .min()
            .unwrap_or(0);
        let mut text: Vec<String> = out
            .into_iter()
            .map(|(t, f)| {
                if f && t.len() >= indent && t[..indent].trim().is_empty() {
                    t[indent..].to_string()
                } else {
                    t
                }
            })
            .collect();
        while text.last().is_some_and(|l| l.trim().is_empty()) {
            text.pop();
        }
        while text.first().is_some_and(|l| l.trim().is_empty()) {
            text.remove(0);
        }
        (!text.is_empty()).then(|| text.join("\n"))
    }

    // ---- painting ------------------------------------------------------------------------

    /// `(frame row, span)` of the lowest spinner glyph on screen.
    fn last_spinner(&self, frame: &Frame, cx: &Cx) -> Option<(usize, usize)> {
        for (i, (bi, ri)) in frame.rows.iter().enumerate().rev() {
            let Some(row) = self.blocks.get(*bi).and_then(|b| b.row_at(*ri)) else {
                continue;
            };
            if let Some(si) = row.spans.iter().position(|s| is_spinner(s, cx)) {
                return Some((i, si));
            }
        }
        None
    }

    /// Draw a frame into `area`. `x` and `c` are the column origin and width inside `area`.
    #[allow(clippy::too_many_arguments)]
    pub fn paint(
        &mut self,
        buf: &mut Buffer,
        area: Rect,
        x: u16,
        c: u16,
        frame: &Frame,
        view: &View,
        cx: &Cx,
    ) {
        let p = cx.p;
        self.hits.clear();
        self.screen_rows.clear();
        self.origin = (x, area.y);
        if self.sel_width != cx.width {
            // Columns mean something else after a re-wrap.
            self.sel = None;
            self.sel_width = cx.width;
        }
        let sel = self.sel.clone().filter(Sel::active);
        let col = Rect::new(x, area.y, c, area.height).intersection(area);
        let search = self.search.as_ref().map(|s| {
            let (n, cs) = s.needle();
            (n, cs, s.current())
        });
        // One spinner per screen: when several tool rows run at once the lowest one keeps
        // animating and the rest hold the static active glyph.
        let spin_at = self.last_spinner(frame, cx);
        self.spinner_visible = spin_at.is_some();
        for (i, (bi, ri)) in frame.rows.iter().enumerate() {
            let y = area.y + i as u16;
            let Some(block) = self.blocks.get(*bi) else {
                continue;
            };
            let row = block.row_at(*ri);
            self.hits.push((y, *bi, row.is_some_and(|r| r.head)));
            self.screen_rows.push((y, *bi, *ri));
            let Some(row) = row else { continue };
            let bg = row.bg.unwrap_or(p.bg);
            if row.bg.is_some() {
                fill(buf, Rect::new(x, y, c, 1), Style::new().bg(bg));
            }
            let mut cx_ = x;
            for (si, s) in row.spans.iter().enumerate() {
                let text = if is_spinner(s, cx) && spin_at != Some((i, si)) {
                    cx.g.busy
                } else {
                    &s.content
                };
                cx_ = put_str(buf, cx_, y, text, Style::new().bg(bg).patch(s.style), col);
            }
            if let Some((c0, c1)) = sel.as_ref().and_then(|s| s.cols(*bi, *ri, row.width())) {
                tint(buf, y, x, c0, c1.min(c as usize), p.sel_bg, p.text);
            }
            if view.cursor == Some(*bi) && x < buf.area.right() {
                if let Some(cell) = buf.cell_mut((x, y)) {
                    cell.set_symbol(cx.g.bar)
                        .set_style(Style::new().fg(p.accent).bg(bg));
                }
            }
            if let Some((needle, cs, cur)) = &search {
                for (col, len) in search::occurrences(&row.text(), needle, *cs) {
                    let is_cur =
                        cur.is_some_and(|h| h.block == *bi && h.row == *ri && h.col == col);
                    let bg = if is_cur { p.cur_bg } else { p.accent_bg };
                    tint(buf, y, x, col, (col + len).min(c as usize), bg, p.text);
                    if is_cur {
                        for k in col..(col + len).min(c as usize) {
                            if let Some(cell) = buf.cell_mut((x + k as u16, y)) {
                                cell.modifier |= ratatui::style::Modifier::BOLD;
                            }
                        }
                    }
                }
            }
        }
    }
}

fn row_chars(row: &Row) -> usize {
    row.spans
        .iter()
        .map(|s| s.content.chars().filter(|c| c.is_alphanumeric()).count())
        .sum()
}

/// Paint `bg` behind display columns `[c0, c1)` of the row at `(x, y)`. A foreground that would
/// fall under 4.5:1 on it (faint text, a comment) is lifted to `fallback`.
fn tint(
    buf: &mut Buffer,
    y: u16,
    x: u16,
    c0: usize,
    c1: usize,
    bg: ratatui::style::Color,
    fallback: ratatui::style::Color,
) {
    for cc in c0..c1 {
        let Some(px) = u16::try_from(x as usize + cc).ok() else {
            break;
        };
        if let Some(cell) = buf.cell_mut((px, y)) {
            cell.bg = bg;
            if crate::palette::contrast(cell.fg, bg) < 4.5 {
                cell.fg = fallback;
            }
        }
    }
}

fn is_spinner(s: &ratatui::text::Span<'_>, cx: &Cx) -> bool {
    s.style.fg == Some(cx.p.accent) && cx.g.spinner.contains(&s.content.as_ref())
}

pub fn age_text(secs: i64) -> String {
    match secs {
        i64::MIN..=59 => "just now".into(),
        60..=3599 => format!("{}m ago", secs / 60),
        3600..=86_399 => format!("{}h ago", secs / 3600),
        _ => format!("{}d ago", secs / 86_400),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::palette::{Depth, Kind as PKind, Palette, UNICODE};
    use agent_core::{Event, ToolCall, ToolKind, ToolStatus};

    fn cx_for<'a>(p: &'a Palette, t: &'a tuikit::theme::Theme, width: usize) -> Cx<'a> {
        Cx {
            p,
            theme: t,
            g: &UNICODE,
            width,
            detail: false,
            now: Instant::now(),
            spin: 0,
        }
    }

    fn tool(id: &str, name: &str, kind: ToolKind) -> ToolCall {
        ToolCall {
            id: id.into(),
            name: name.into(),
            kind,
            title: id.into(),
            status: ToolStatus::Completed,
            ..Default::default()
        }
    }

    fn turn(t: &mut Transcript, user: &str, text: &str) {
        let now = Instant::now();
        t.push_user(user.into(), user.into());
        t.apply(&Event::TurnStart, now);
        t.apply(&Event::TextDelta(text.into()), now);
        t.apply(&Event::TurnEnd(StopReason::EndTurn), now);
    }

    #[test]
    fn deltas_extend_one_block_and_tools_split_text() {
        let mut t = Transcript::new();
        let now = Instant::now();
        t.apply(&Event::TurnStart, now);
        t.apply(&Event::TextDelta("he".into()), now);
        t.apply(&Event::TextDelta("llo".into()), now);
        t.apply(&Event::Tool(tool("a", "Bash", ToolKind::Execute)), now);
        t.apply(&Event::TextDelta("after".into()), now);
        assert_eq!(t.blocks.len(), 4);
        assert!(
            matches!(&t.blocks[1].kind, Kind::Assistant(a) if a.text == "hello" && !a.streaming)
        );
        assert!(matches!(&t.blocks[3].kind, Kind::Assistant(a) if a.streaming));
    }

    #[test]
    fn consecutive_tools_have_no_gap_and_reads_merge() {
        let mut t = Transcript::new();
        let now = Instant::now();
        t.apply(&Event::Tool(tool("a", "Bash", ToolKind::Execute)), now);
        t.apply(&Event::Tool(tool("b", "Read", ToolKind::Read)), now);
        t.apply(&Event::Tool(tool("c", "Read", ToolKind::Read)), now);
        assert_eq!(t.blocks.len(), 3);
        assert_eq!(t.blocks[2].lead, 0);
        assert!(matches!(&t.blocks[2].kind, Kind::Tools(e) if e.len() == 2));
    }

    #[test]
    fn tool_update_replaces_in_place_and_subagent_children_nest() {
        let mut t = Transcript::new();
        let now = Instant::now();
        let mut parent = tool("p", "Task", ToolKind::Other);
        parent.status = ToolStatus::Running;
        t.apply(&Event::Tool(parent.clone()), now);
        let mut child = tool("c1", "Read", ToolKind::Read);
        child.parent_id = Some("p".into());
        t.apply(&Event::Tool(child), now);
        parent.status = ToolStatus::Completed;
        t.apply(&Event::Tool(parent), now);
        assert_eq!(t.blocks.len(), 2);
        let Kind::Tools(e) = &t.blocks[1].kind else {
            panic!()
        };
        assert_eq!(e[0].children.len(), 1);
        assert!(!e[0].running());
    }

    #[test]
    fn interrupt_keeps_the_partial_answer_and_fails_running_tools() {
        let mut t = Transcript::new();
        let now = Instant::now();
        t.apply(&Event::TurnStart, now);
        t.apply(&Event::TextDelta("partial".into()), now);
        let mut c = tool("a", "Bash", ToolKind::Execute);
        c.status = ToolStatus::Running;
        t.apply(&Event::Tool(c), now);
        t.apply(&Event::TurnEnd(StopReason::Cancelled), now);
        // The card says `■ interrupted`, so there is no second line saying it (finding 8).
        assert!(matches!(t.blocks.last().unwrap().kind, Kind::Tools(_)));
        assert!(!t
            .blocks
            .iter()
            .any(|b| matches!(b.kind, Kind::Interrupted(_))));
        let Kind::Tools(e) = &t.blocks[2].kind else {
            panic!()
        };
        assert!(e[0].failed() && e[0].interrupted);
        assert!(!t.busy);
    }

    #[test]
    fn an_interrupt_in_prose_still_gets_its_one_line() {
        let mut t = Transcript::new();
        let now = Instant::now();
        t.apply(&Event::TurnStart, now);
        t.apply(&Event::TextDelta("partial".into()), now);
        t.apply(&Event::TurnEnd(StopReason::Cancelled), now);
        assert!(matches!(
            t.blocks.last().unwrap().kind,
            Kind::Interrupted(_)
        ));
    }

    #[test]
    fn a_thought_shares_the_run_of_tool_rows() {
        let mut t = Transcript::new();
        let now = Instant::now();
        t.apply(&Event::TurnStart, now);
        t.apply(&Event::ThoughtDelta("look first".into()), now);
        t.apply(&Event::Tool(tool("a", "Read", ToolKind::Read)), now);
        t.apply(&Event::ThoughtDelta("then more".into()), now);
        t.apply(&Event::Tool(tool("b", "Bash", ToolKind::Execute)), now);
        t.apply(&Event::TextDelta("done".into()), now);
        let kinds: Vec<(&str, u16)> = t
            .blocks
            .iter()
            .map(|b| {
                (
                    match &b.kind {
                        Kind::Thought { .. } => "thought",
                        Kind::Tools(_) => "tools",
                        Kind::Assistant(_) => "text",
                        _ => "other",
                    },
                    b.lead,
                )
            })
            .filter(|(k, _)| *k != "other")
            .collect();
        // Thoughts and tool rows run together; the answer is still a block of its own.
        assert_eq!(
            kinds,
            [
                ("thought", 1),
                ("tools", 0),
                ("thought", 0),
                ("tools", 0),
                ("text", 1)
            ],
            "{kinds:?}"
        );
    }

    #[test]
    fn viewport_anchors_survive_growth_and_resize() {
        let p = Palette::new(PKind::Hearth, Depth::True);
        let th = p.theme();
        let mut t = Transcript::new();
        for i in 0..50 {
            turn(
                &mut t,
                &format!("question {i}"),
                &format!("answer {i}\n\nsecond paragraph {i}"),
            );
        }
        let mut v = View::default();
        let cx = cx_for(&p, &th, 60);
        let f = t.frame(10, &mut v, &cx);
        assert_eq!(f.rows.len(), 10);
        assert!(v.sticky);
        t.scroll_by(&mut v, -25, 10, &cx);
        assert!(!v.sticky);
        let before = t.frame(10, &mut v, &cx).rows[0];
        // New output below does not move the view.
        turn(&mut t, "later", "more text");
        assert_eq!(t.frame(10, &mut v, &cx).rows[0], before);
        // A resize keeps the same block at the top.
        let narrow = cx_for(&p, &th, 30);
        assert_eq!(t.frame(10, &mut v, &narrow).rows[0].0, before.0);
        // Scrolling past the end re-sticks.
        t.scroll_by(&mut v, 10_000, 10, &cx);
        assert!(v.sticky);
    }

    #[test]
    fn short_content_starts_at_the_top_row() {
        let p = Palette::new(PKind::Hearth, Depth::True);
        let th = p.theme();
        let mut t = Transcript::new();
        let mut v = View::default();
        let cx = cx_for(&p, &th, 60);
        let f = t.frame(10, &mut v, &cx);
        // Only the welcome block: its rows start at row 0 and the rest of the view is blank.
        assert_eq!(f.rows.len(), blocks::WELCOME_ROWS);
        assert_eq!(f.rows[0], (0, 0));
    }

    #[test]
    fn toggle_and_turn_text() {
        let mut t = Transcript::new();
        let now = Instant::now();
        t.push_user("hi".into(), "hi".into());
        t.apply(&Event::Tool(tool("a", "Bash", ToolKind::Execute)), now);
        t.toggle(2, false);
        assert!(t.blocks[2].is_open(false));
        t.toggle(2, false);
        assert!(!t.blocks[2].is_open(false));
        assert!(t.turn_text(2).starts_with("> hi"));
    }

    #[test]
    fn search_finds_blocks_by_text() {
        let mut t = Transcript::new();
        turn(&mut t, "find the Needle", "nothing here");
        turn(&mut t, "other", "a needle appears");
        t.set_search("needle");
        let p = Palette::new(PKind::Hearth, Depth::True);
        let th = p.theme();
        t.resolve_search(&cx_for(&p, &th, 60));
        assert_eq!(t.search_counts(), Some((1, 2)));
    }

    fn varied(n: usize) -> Transcript {
        let mut t = Transcript::new();
        for i in 0..n {
            let body: String = (0..(i * 7 % 13) + 1)
                .map(|k| format!("line {k} of block {i}\n\n"))
                .collect();
            turn(&mut t, &format!("question {i}"), &body);
        }
        t
    }

    #[test]
    fn reveal_always_brings_the_cursor_block_on_screen() {
        let p = Palette::new(PKind::Hearth, Depth::True);
        let th = p.theme();
        let cx = cx_for(&p, &th, 50);
        let mut t = varied(25);
        let mut v = View::default();
        let vh = 11;
        let n = t.blocks.len();
        let order: Vec<usize> = (1..n)
            .chain((1..n).rev())
            .chain([5, 40, 2, n - 1, 1])
            .collect();
        for i in order {
            t.reveal(&mut v, i, vh, &cx);
            let f = t.frame(vh, &mut v, &cx);
            let rows: Vec<usize> = f
                .rows
                .iter()
                .filter(|(b, _)| *b == i)
                .map(|(_, r)| *r)
                .collect();
            let h = t.blocks[i].height(&cx);
            let lead = t.blocks[i].lead as usize;
            assert!(
                !rows.is_empty(),
                "block {i} (height {h}) not on screen; view {v:?}"
            );
            if h - lead <= vh {
                assert_eq!(*rows.last().unwrap(), h - 1, "bottom of block {i} cut off");
            } else {
                assert!(rows.contains(&lead), "top of tall block {i} not shown");
            }
        }
    }

    #[test]
    fn random_scrolling_keeps_the_view_valid() {
        let p = Palette::new(PKind::Hearth, Depth::True);
        let th = p.theme();
        let mut t = varied(30);
        let mut v = View::default();
        let mut seed = 12345u64;
        let mut rnd = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        for step in 0..600 {
            let width = [24usize, 50, 90][(rnd() % 3) as usize];
            let vh = (rnd() % 20) as usize + 1;
            let cx = cx_for(&p, &th, width);
            let delta = (rnd() % 61) as isize - 30;
            t.scroll_by(&mut v, delta, vh, &cx);
            let f = t.frame(vh, &mut v, &cx);
            let total: usize = (0..t.blocks.len()).map(|i| t.blocks[i].height(&cx)).sum();
            assert_eq!(f.rows.len(), vh.min(total), "step {step}");
            assert!(f.rows.len() <= vh);
            // Rows are consecutive and inside their blocks.
            for w in f.rows.windows(2) {
                let (a, b) = (w[0], w[1]);
                assert!(
                    b.0 == a.0 && b.1 == a.1 + 1 || b.0 == a.0 + 1 && b.1 == 0,
                    "step {step}: {a:?} then {b:?}"
                );
            }
            if v.sticky {
                let last = f.rows.last().expect("rows");
                assert_eq!(last.0, t.blocks.len() - 1);
                assert_eq!(
                    last.1,
                    t.blocks[last.0].height(&cx) - 1,
                    "sticky view must end at the last row"
                );
            }
        }
    }

    #[test]
    fn history_replay_rebuilds_without_timers() {
        let mut t = Transcript::new();
        t.apply(
            &Event::History {
                session_id: "s".into(),
                items: vec![
                    HistoryItem::User("q".into()),
                    HistoryItem::Thought("hm".into()),
                    HistoryItem::Tool(tool("a", "Bash", ToolKind::Execute)),
                    HistoryItem::Assistant("a".into()),
                ],
            },
            Instant::now(),
        );
        assert_eq!(t.blocks.len(), 5);
        assert!(!t.busy && !t.animating());
    }

    #[test]
    fn a_search_in_a_long_transcript_does_not_lay_out_every_matching_block() {
        // Finding 14: every block holding the text was laid out on every keystroke, 13 s for
        // a million rows and 767 MB kept afterwards.
        let p = Palette::new(PKind::Hearth, Depth::True);
        let th = p.theme();
        let cx = cx_for(&p, &th, 60);
        let mut t = Transcript::new();
        for i in 0..1500 {
            turn(
                &mut t,
                &format!("question {i} about the parser"),
                &format!("The parser reads token {i}.\n\nMore about the parser here."),
            );
        }
        t.set_search("parser");
        let t0 = std::time::Instant::now();
        t.resolve_search(&cx);
        let first = t0.elapsed();
        let laid = t.blocks.iter().filter(|b| b.is_laid(&cx)).count();
        let (_, total) = t.search_counts().unwrap();
        // every block with the text is counted (a placeholder each, real hits where laid out), but
        // only a bounded number was laid out
        assert!(total >= 3000, "{total}");
        assert!(t.search_approximate());
        assert!(laid < 1500, "{laid} blocks laid out for one search");
        assert!(first < std::time::Duration::from_secs(5), "{first:?}");
        // a jump lays out the block it lands on and replaces its placeholder with real hits
        let hit = t.search_jump(1000, &cx).unwrap();
        assert!(!hit.pending && hit.len > 0, "{hit:?}");
        assert!(hit.block >= 1000);
        assert!(t.blocks[hit.block].is_laid(&cx));
        let (_, after) = t.search_counts().unwrap();
        assert_ne!(after, 0);
    }

    #[test]
    fn a_placeholder_with_no_real_match_is_dropped_when_a_jump_reaches_it() {
        // The source holds the text (a link target) but no row does.
        let p = Palette::new(PKind::Hearth, Depth::True);
        let th = p.theme();
        let cx = cx_for(&p, &th, 60);
        let mut t = Transcript::new();
        turn(
            &mut t,
            "hello",
            "see [docs](https://example.com/needle) for details",
        );
        turn(&mut t, "again", "a real needle in the text");
        t.set_search("needle");
        t.resolve_search(&cx);
        assert_eq!(
            t.search_counts(),
            Some((1, 1)),
            "only the visible text matches"
        );
        // put placeholders in front of both answers, as a long transcript's search would
        let at = |needle: &str| {
            t.blocks
                .iter()
                .position(|b| b.text().contains(needle))
                .unwrap()
        };
        let (first, second) = (at("[docs]"), at("a real needle"));
        let s = t.search.as_mut().unwrap();
        let ph = |block| Hit {
            block,
            row: 1,
            col: 0,
            len: 0,
            pending: true,
        };
        s.hits = vec![ph(first), ph(second)];
        s.cur = 0;
        let hit = t.open_hidden(&cx).expect("the second block really matches");
        assert_eq!(hit.block, second);
        assert!(!hit.pending && hit.len > 0, "{hit:?}");
        let s = t.search.as_ref().unwrap();
        assert!(
            s.hits.iter().all(|h| !h.pending && h.block == second),
            "{:?}",
            s.hits
        );
    }
}
