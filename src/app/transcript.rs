//! The TUI's reading position on a conversation: the shared
//! [`TranscriptModel`], the fold state and scroll offset that belong to *this*
//! screen rather than to the conversation, and the subagent rail's panes.
//!
//! # Why there is no second transcript here
//!
//! There used to be. The TUI carried its own `Vec<TranscriptEntry>` and folded
//! `AgentEvent`s into it by hand, which is a re-implementation of
//! [`TranscriptModel::apply`] — the exact duplication that module was written
//! to remove, left standing on the surface that matters most. The two drifted
//! the way duplicated readings always do: the TUI dropped system notes the GUI
//! showed, and reported every replayed tool failure as a success.
//!
//! So [`TranscriptView`] owns a `TranscriptModel` and nothing that could
//! disagree with it. Every event goes in one door, and the rows on screen are
//! read straight back out of [`TranscriptModel::items`] — there is no copy to
//! keep synchronised, which is what makes an equivalence test against the
//! native GUI meaningful rather than a comparison of two hand-maintained
//! lists.
//!
//! # What is genuinely the view's
//!
//! One thing: whether each tool row is folded. That is a property of this
//! screen (the user clicked it), not of the conversation, so it cannot live in
//! the model — but it is keyed by item index, and item indices *move*: the
//! replay path splices a tool's images in behind the row that produced them.
//! [`TranscriptView::sync`] is therefore driven by
//! [`TranscriptModel::last_change`] rather than by re-deriving from scratch,
//! and a [`Change::Inserted`] shifts the fold flags exactly as it shifts the
//! items. A renderer that cached widgets per index would need the same
//! machinery, which is why the signal lives in the model rather than here.
//!
//! # Whose conversation this is
//!
//! Since the mesh's tier 2, a `TranscriptView` may hold a *peer's* session
//! rather than this machine's: a watched node's turn arrives as an
//! [`AgentEvent`], which is exactly what [`TranscriptModel::apply`] takes, so
//! there is no second model and no second reducer for a peer's transcript to
//! drift away in.
//!
//! What there has to be is an origin. A peer's turn rendering
//! indistinguishably from this machine's own output is not a missing nicety, it
//! is a machine somebody else controls writing lines into a surface a human
//! reads as their own agent — and every string on that stream was written by
//! the far end. So [`TranscriptOrigin`] is carried by the view, not passed to
//! the renderer, and [`TranscriptView::attributed`] is the only way to turn a
//! peer's item into lines: it stamps the marker onto **every physical line**,
//! including the ones inside a peer's own text.
//!
//! Per line rather than per item, because the sanitiser deliberately keeps
//! newlines (indentation and paragraphs are most of what a transcript means),
//! so an item-level prefix would leave every line after the first unmarked and
//! a peer could write `wizard: everything is fine` into the gap.
//!
//! The marker is built from [`NodeId::short`], which is derived from the peer's
//! public key rather than from the name it chose, so it is the one part of the
//! rendering a peer cannot influence at all.

use std::cell::Cell;
use std::time::{Duration, Instant};

use crate::agent::AgentEvent;
use crate::images::ImageRef;
use crate::mesh::NodeId;
use crate::transcript::{Change, ToolItem, ToolItemOutput, TranscriptItem, TranscriptModel};

/// Whose conversation a [`TranscriptView`] holds.
///
/// Two states rather than a `bool` and a label beside it: the peer case needs
/// the verified id and the announced name together, and a surface that could
/// have the flag without the identity is a surface that can render peer content
/// with no way to say whose it is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TranscriptOrigin {
    /// This machine's own session. The TUI's main chat, and every subagent
    /// pane under it.
    Local,
    /// A peer's session, arriving over the mesh.
    ///
    /// No session id, deliberately: one [`crate::mesh::Subscription`] is per
    /// *node* and carries every session that node is running, because a session
    /// id is peer-supplied text and asking a peer to route by a string it chose
    /// is not something this side can pin. The session each event belongs to
    /// arrives on the event.
    Peer(PeerOrigin),
}

/// Which machine a watched transcript belongs to, rendered.
///
/// Every field is private and [`PeerOrigin::new`] is the only constructor, so
/// the marker is derived from a [`NodeId`] in exactly one place. That is the
/// property worth protecting: a struct whose marker could be assigned would be
/// a struct through which a peer's announced name could become the attribution
/// on its own output.
///
/// The marker and the address are computed once rather than per line. A watched
/// session is a stream, and re-deriving a base64 address for every line of it is
/// work in proportion to how much the peer says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerOrigin {
    /// `wiz1AbCdEfG │`, from the peer's public key and from nothing it wrote.
    marker: String,
    /// The full address, for the banner. Full rather than short for the reason
    /// [`crate::mesh::Graph`] prints it when two labels collide: a short form
    /// is a prefix, and prefixes collide.
    address: String,
    /// What the peer calls itself, already sanitised
    /// ([`crate::mesh::PeerText`]) — or its short address when it has announced
    /// no name.
    label: String,
}

impl PeerOrigin {
    /// The origin for one peer, by verified id and announced name.
    pub fn new(node: NodeId, label: String) -> Self {
        Self {
            marker: format!("{} │", node.short()),
            address: node.address(),
            label,
        }
    }

    /// The marker stamped onto every line this peer wrote.
    pub fn marker(&self) -> &str {
        &self.marker
    }

    /// The peer's full address.
    pub fn address(&self) -> &str {
        &self.address
    }

    /// What the peer calls itself.
    pub fn label(&self) -> &str {
        &self.label
    }
}

/// What every line wizard itself writes into a watched transcript carries.
///
/// The counterpart to the peer marker, and the reason both exist: in a surface
/// showing a peer's session, "the stream ended" and "the peer said the stream
/// ended" are different claims, and a reader has to be able to tell them apart
/// without knowing which events exist. A peer cannot produce a line with this
/// prefix, because every line it produces gets the peer's own marker instead.
pub const LOCAL_MARKER: &str = "wizard │";

impl TranscriptOrigin {
    /// The marker stamped onto every line of peer content, or `None` for a
    /// local conversation, which needs no marker because nothing else is on the
    /// screen to confuse it with.
    pub fn marker(&self) -> Option<&str> {
        match self {
            TranscriptOrigin::Local => None,
            TranscriptOrigin::Peer(peer) => Some(peer.marker()),
        }
    }

    /// The header a surface prints above a peer's transcript: the name, the
    /// **full** address, and both markers.
    ///
    /// The full address rather than [`NodeId::short`], for the reason
    /// [`crate::mesh::Graph`] prints it when two labels collide: a short form
    /// is a prefix and prefixes collide, and this is the one line whose job is
    /// to say exactly which machine is about to write on this screen.
    pub fn banner(&self) -> Option<String> {
        match self {
            TranscriptOrigin::Local => None,
            TranscriptOrigin::Peer(peer) => Some(format!(
                "watching {} at {} — every line below marked `{}` was written by that machine; \
                 lines marked `{LOCAL_MARKER}` are wizard's own",
                peer.label(),
                peer.address(),
                peer.marker()
            )),
        }
    }

    /// One line this machine wrote, marked as such, for a surface that is
    /// otherwise rendering a peer.
    pub fn local_line(text: &str) -> String {
        format!("{LOCAL_MARKER} {text}")
    }
}

/// One conversation as this screen is currently reading it.
#[derive(Debug)]
pub struct TranscriptView {
    /// Whose conversation this is. Immutable after construction: a view that
    /// could change hands mid-stream is a view whose already-rendered lines
    /// carry the wrong attribution.
    origin: TranscriptOrigin,
    /// The conversation. Private on purpose: every mutation has to go through
    /// a method here so [`TranscriptView::sync`] runs against exactly one
    /// [`Change`], which is the contract the signal is defined under.
    model: TranscriptModel,
    /// Whether the item at each index is folded, one flag per item. Only tool
    /// rows can be folded; the flag on any other row is inert and costs a
    /// byte, which is cheaper than a map keyed by an index that moves.
    folded: Vec<bool>,
    /// The model revision `folded` was last brought into step with, so a call
    /// that changed nothing does not re-derive (and so discard) the user's
    /// folds against a stale [`Change`].
    synced: u64,
    /// First visible line, measured from the top of the rendered content. Only
    /// consulted while [`Self::follow`] is false; when following, the live tail
    /// is always in view.
    pub scroll: u16,
    /// When true the view sticks to the bottom as new output arrives. Scrolling
    /// up clears it; scrolling back to the bottom (or Ctrl-End) restores it.
    pub follow: bool,
    /// Last-drawn max scroll (content lines past the viewport). Written by the
    /// renderer, which takes `&App`, so a key handler can convert a follow-tail
    /// view into a stable top-anchored offset without re-wrapping everything.
    pub max_scroll: Cell<u16>,
}

impl Default for TranscriptView {
    /// An empty view, **following the tail** — which is why this is written
    /// out rather than derived: a view that started with `follow: false` would
    /// silently stop showing the live turn.
    fn default() -> Self {
        Self {
            origin: TranscriptOrigin::Local,
            model: TranscriptModel::new(),
            folded: Vec::new(),
            synced: 0,
            scroll: 0,
            follow: true,
            max_scroll: Cell::new(0),
        }
    }
}

impl TranscriptView {
    /// An empty view of this machine's own conversation, following the tail.
    pub fn new() -> Self {
        Self::default()
    }

    /// An empty view of a **peer's** session.
    ///
    /// The same model and the same reducer as the local one — that is the whole
    /// point of a peer's turn being an [`AgentEvent`] — with an origin that
    /// every rendered line then carries. See the module docs.
    pub fn for_peer(node: NodeId, label: String) -> Self {
        Self {
            origin: TranscriptOrigin::Peer(PeerOrigin::new(node, label)),
            ..Self::default()
        }
    }

    // -- Reading ----------------------------------------------------------

    /// Whose conversation this is.
    pub fn origin(&self) -> &TranscriptOrigin {
        &self.origin
    }

    /// Every line of `item`, each one stamped with this view's origin.
    ///
    /// The only rendering path for a watched peer, and the reason it takes an
    /// item rather than an index: a surface streaming a peer prints the rows
    /// the model's [`Change`] just named, and an index would go stale the
    /// moment a tool's images spliced in behind the row that produced them.
    ///
    /// An item that renders to nothing at all still produces one line, so a row
    /// the model recorded cannot vanish from the screen without trace.
    pub fn attributed(&self, item: &TranscriptItem) -> Vec<String> {
        let rendered = plain_lines(item);
        match self.origin.marker() {
            None => rendered,
            Some(marker) => rendered
                .iter()
                .map(|line| format!("{marker} {line}"))
                .collect(),
        }
    }

    /// The conversation so far.
    pub fn items(&self) -> &[TranscriptItem] {
        self.model.items()
    }

    /// The model itself, for a caller that wants to snapshot or compare it —
    /// the GUI's equivalence check, chiefly. Read-only: mutating through here
    /// would bypass [`TranscriptView::sync`].
    pub fn model(&self) -> &TranscriptModel {
        &self.model
    }

    /// Reasoning and assistant text streamed since the last commit, in that
    /// order — the live tail, drawn below the items.
    pub fn streaming(&self) -> (&str, &str) {
        self.model.streaming()
    }

    pub fn len(&self) -> usize {
        self.items().len()
    }

    pub fn is_empty(&self) -> bool {
        self.items().is_empty()
    }

    pub fn get(&self, index: usize) -> Option<&TranscriptItem> {
        self.items().get(index)
    }

    pub fn last(&self) -> Option<&TranscriptItem> {
        self.items().last()
    }

    pub fn iter(&self) -> std::slice::Iter<'_, TranscriptItem> {
        self.items().iter()
    }

    /// Whether the row at `index` is drawn folded. Meaningless for anything
    /// but a tool row, and `false` past the end.
    pub fn folded(&self, index: usize) -> bool {
        self.folded.get(index).copied().unwrap_or(false)
    }

    // -- Folding ----------------------------------------------------------

    /// Fold or unfold the row at `index`.
    pub fn toggle(&mut self, index: usize) {
        if let Some(flag) = self.folded.get_mut(index) {
            *flag = !*flag;
        }
    }

    /// Fold or unfold the most recent tool row (Ctrl-T).
    pub fn toggle_last_tool(&mut self) {
        if let Some(index) = self.last_tool_row() {
            self.toggle(index);
        }
    }

    /// Force the most recent tool row open or shut, for the two surface-made
    /// cards whose fold is a decision rather than a measurement.
    ///
    /// The omakase plan opens: it is the only record of a choice the user
    /// never got to review, and a short one would otherwise read as an
    /// unremarkable tool call. The ultra guidance shuts whatever its size: the
    /// point of that turn is the answer below it, not the drafts behind it.
    pub fn set_last_tool_folded(&mut self, folded: bool) {
        if let Some(index) = self.last_tool_row() {
            self.folded[index] = folded;
        }
    }

    fn last_tool_row(&self) -> Option<usize> {
        self.items()
            .iter()
            .rposition(|item| matches!(item, TranscriptItem::Tool(_)))
    }

    // -- Writing ----------------------------------------------------------

    /// Fold one live agent event into the conversation.
    pub fn apply(&mut self, event: &AgentEvent) {
        self.model.apply(event);
        self.sync();
    }

    /// Record something the user said.
    pub fn user(&mut self, text: String, images: Vec<ImageRef>) {
        self.model.user(text, images);
        self.sync();
    }

    /// Record a complete assistant message (a subagent run reports whole
    /// messages rather than deltas).
    pub fn assistant(&mut self, text: String) {
        self.model.assistant(text);
        self.sync();
    }

    /// Record an informational line this surface raised itself.
    pub fn notice(&mut self, text: String) {
        self.model.notice(text);
        self.sync();
    }

    /// Echo a line the user typed at a running command's console into that
    /// command's card; see [`TranscriptModel::console_echo`].
    pub fn console_echo(&mut self, line: &str) {
        self.model.console_echo(line);
        self.sync();
    }

    /// Move the streamed tail into items.
    pub fn commit(&mut self) {
        self.model.commit();
        self.sync();
    }

    /// Forget the conversation (`/clear`).
    pub fn clear(&mut self) {
        self.model.clear();
        self.sync();
    }

    /// Write a late result into the most recent tool row `wanted` accepts;
    /// see [`TranscriptModel::amend_tool`].
    pub fn amend_tool(&mut self, wanted: impl Fn(&ToolItem) -> bool, output: ToolItemOutput) {
        self.model.amend_tool(wanted, output);
        self.sync();
    }

    /// Replay a stored session — `/resume`, or the truncated conversation a
    /// `/rewind` leaves behind — one record at a time.
    ///
    /// One record at a time rather than [`TranscriptModel::seed`]'s
    /// all-at-once, because that is what puts the fold flags through the same
    /// per-change bookkeeping a live turn uses. A session in which a tool
    /// returned an image splices that image in *behind* the row that produced
    /// it, and the rows below it shift; seeding would hide that behind one
    /// `Reset` here and leave the arm that handles it untested until the GUI
    /// hit it.
    pub fn replay(&mut self, entries: &[crate::agent::session::SessionEntry]) {
        self.clear();
        for entry in entries {
            self.model.fold_entry(entry);
            self.sync();
        }
        self.scroll_to_bottom();
    }

    // -- Scrolling --------------------------------------------------------

    /// Scroll by `delta` lines. Positive moves toward older content (up);
    /// negative toward the live tail.
    pub fn scroll_by(&mut self, delta: i16) {
        let max = self.max_scroll.get();
        (self.scroll, self.follow) = scroll_step(self.follow, self.scroll, max, delta);
    }

    /// Jump to the live tail and re-enable stick-to-bottom.
    pub fn scroll_to_bottom(&mut self) {
        self.scroll = 0;
        self.follow = true;
    }

    // -- Keeping the fold flags in step -----------------------------------

    /// Bring `folded` back into step with the items, using the one change the
    /// model just made rather than re-deriving the lot. See the module docs
    /// for why the insert case is not optional.
    fn sync(&mut self) {
        let revision = self.model.revision();
        if revision == self.synced {
            // Nothing happened (an empty commit, a prompt that said nothing),
            // so `last_change` is a leftover from an earlier call and acting
            // on it would throw away folds the user has since set.
            return;
        }
        self.synced = revision;
        match self.model.last_change() {
            // The tail is not an item.
            Change::Streaming => {}
            Change::Reset => {
                self.folded = self.model.items().iter().map(folds_by_default).collect()
            }
            Change::Appended(at) => {
                // Everything from `at` down is new, so nothing the user folded
                // is being discarded here.
                self.folded.truncate(at);
                for item in &self.model.items()[self.folded.len()..] {
                    self.folded.push(folds_by_default(item));
                }
            }
            Change::Inserted(at) => {
                let flag = self.model.items().get(at).is_some_and(folds_by_default);
                let at = at.min(self.folded.len());
                self.folded.insert(at, flag);
            }
            Change::Mutated(at) => {
                // A row that just gained its result: the fold policy is about
                // the output, which did not exist until now.
                if let (Some(item), Some(flag)) =
                    (self.model.items().get(at), self.folded.get_mut(at))
                {
                    *flag = folds_by_default(item);
                }
            }
        }
    }
}

/// Whether a row starts folded.
///
/// A call still running is always open — its card is where you watch it work.
/// Once it answers, a failure folds (the ✗ carries the signal without dumping
/// the payload; Ctrl-T or a click opens it) and so does anything long enough
/// to bury the reply underneath it.
///
/// One rule, applied to a live row and a replayed one alike. The TUI used to
/// have two: replay folded *every* answered call whatever its size, so a
/// three-line `git status` you had just watched come back open closed itself
/// when you resumed the session. Two rules over one conversation is the same
/// class of bug the shared model exists to remove, one level up.
fn folds_by_default(item: &TranscriptItem) -> bool {
    match item {
        TranscriptItem::Tool(tool) => match &tool.output {
            Some(output) => output.is_error || collapse_long(&output.content),
            None => false,
        },
        _ => false,
    }
}

// ---------------------------------------------------------------------------
// A peer's session as a line stream
// ---------------------------------------------------------------------------

/// A watched peer's session, rendered as an append-only stream of attributed
/// lines.
///
/// The shape `wizard peers watch` needs and the TUI does not: a terminal that
/// scrolls cannot redraw a row it already printed, so this turns the model's
/// [`Change`] signal into "the lines that appeared since the last event". The
/// model underneath is [`TranscriptModel`], unchanged and shared with the TUI —
/// there is no second reducer here, only a second way of getting rows onto a
/// screen.
///
/// Nothing renders without the peer's marker: [`PeerStream::apply`] returns
/// lines from [`TranscriptView::attributed`], and the only other producer is
/// [`PeerStream::local`], which stamps [`LOCAL_MARKER`] instead. A peer's text
/// cannot land in the second set, because a peer's text only ever arrives
/// through `apply`.
#[derive(Debug)]
pub struct PeerStream {
    view: TranscriptView,
    /// Lines already emitted for the item at each index, parallel to
    /// [`TranscriptModel::items`] and kept in step by the same [`Change`]
    /// bookkeeping the fold flags use.
    printed: Vec<usize>,
    /// Complete lines of the *uncommitted* reasoning tail already emitted.
    ///
    /// The tail has to reach the screen as it arrives, or a peer streaming a
    /// long reply shows nothing at all until something commits it — which for a
    /// turn that is only text is the very end. A scrolling surface cannot redraw
    /// a partial line, so only whole ones go out, and the counter is what stops
    /// the committed row repeating them.
    thinking_lines: usize,
    /// The same, for the assistant tail.
    text_lines: usize,
}

impl PeerStream {
    /// A stream for one peer, by verified id and announced name.
    pub fn new(node: NodeId, label: String) -> Self {
        Self {
            view: TranscriptView::for_peer(node, label),
            printed: Vec::new(),
            thinking_lines: 0,
            text_lines: 0,
        }
    }

    /// The header to print above everything: who is about to write here.
    pub fn banner(&self) -> String {
        self.view
            .origin()
            .banner()
            .expect("a peer stream always has a peer origin")
    }

    /// The conversation so far, for a caller that wants to inspect rather than
    /// print — the equality a test asserts against the shared model.
    pub fn view(&self) -> &TranscriptView {
        &self.view
    }

    /// One line **this** machine wrote, marked as wizard's own.
    ///
    /// An associated function rather than a method, so it is usable before a
    /// stream exists and so it cannot accidentally take peer-derived text from
    /// `self`.
    pub fn local(text: &str) -> String {
        TranscriptOrigin::local_line(text)
    }

    /// Fold one of the peer's agent events in, and return the lines that
    /// appeared because of it.
    ///
    /// Empty is the ordinary answer. A status-only event (a step counter, a
    /// token count) adds nothing, and a delta that does not finish a line adds
    /// nothing either: a fragment is going to change, and a surface that had
    /// already printed it would have to unprint it.
    pub fn apply(&mut self, event: &AgentEvent) -> Vec<String> {
        let before = self.view.model().revision();
        self.view.apply(event);
        if self.view.model().revision() == before {
            return Vec::new();
        }
        match self.view.model().last_change() {
            Change::Streaming => self.drain_tails(),
            Change::Reset => {
                self.printed.clear();
                self.thinking_lines = 0;
                self.text_lines = 0;
                self.emit(0..self.view.len())
            }
            Change::Appended(at) => {
                self.printed.truncate(at);
                self.printed.resize(self.view.len(), 0);
                for index in at..self.view.len() {
                    // A committed reasoning or assistant row *is* the tail that
                    // was already streamed line by line. Seeding its counter
                    // with what went out is what keeps the commit from
                    // repeating everything the peer just said; what is left is
                    // the final partial line, which is exactly the part that
                    // was held back.
                    let tail = match self.view.get(index) {
                        Some(TranscriptItem::Thinking(_)) => 1,
                        Some(TranscriptItem::Text(_)) => 2,
                        _ => 0,
                    };
                    self.printed[index] = match tail {
                        1 => std::mem::take(&mut self.thinking_lines),
                        2 => std::mem::take(&mut self.text_lines),
                        _ => 0,
                    };
                }
                self.emit(at..self.view.len())
            }
            Change::Inserted(at) => {
                let at = at.min(self.printed.len());
                self.printed.insert(at, 0);
                self.emit(at..at + 1)
            }
            Change::Mutated(at) => self.emit(at..at + 1),
        }
    }

    /// The whole lines of the uncommitted tails that have not gone out yet.
    ///
    /// A tail that *shrank* is [`AgentEvent::StreamRetrying`]: the model cleared
    /// it because the completion died and is about to be re-generated from
    /// scratch. A scrolling surface cannot take back what it printed, so it says
    /// so instead — a silent restart would leave the peer's half-sentence
    /// standing above its replacement with nothing to explain the repetition.
    fn drain_tails(&mut self) -> Vec<String> {
        let (thinking, text) = self.view.streaming();
        let whole = |tail: &str| tail.matches('\n').count();
        let (now_thinking, now_text) = (whole(thinking), whole(text));
        if now_thinking < self.thinking_lines || now_text < self.text_lines {
            self.thinking_lines = 0;
            self.text_lines = 0;
            return vec![Self::local(
                "the peer's completion died mid-reply and is being re-generated; the \
                 unfinished text above this line is about to be repeated",
            )];
        }
        let marker = self.view.origin().marker().unwrap_or_default().to_string();
        let mut lines: Vec<String> = Vec::new();
        for (tail, emitted, upto, prefix) in [
            (thinking, self.thinking_lines, now_thinking, "· "),
            (text, self.text_lines, now_text, ""),
        ] {
            lines.extend(
                tail.split('\n')
                    .take(upto)
                    .skip(emitted)
                    .map(|line| format!("{marker} {prefix}{line}")),
            );
        }
        self.thinking_lines = now_thinking;
        self.text_lines = now_text;
        lines
    }

    /// The lines of `rows` that have not been emitted yet, growing the
    /// per-row counters as it goes.
    fn emit(&mut self, rows: std::ops::Range<usize>) -> Vec<String> {
        let mut lines = Vec::new();
        for index in rows {
            // Rendered before anything is mutated, so the borrow of `view` ends
            // here rather than running across the write to `printed`.
            let Some(rendered) = self.view.get(index).map(|item| self.view.attributed(item)) else {
                continue;
            };
            if self.printed.len() <= index {
                self.printed.resize(index + 1, 0);
            }
            let already = self.printed[index];
            // A row whose rendering *shrank* was rewritten rather than
            // extended: a running command's partial output being replaced by
            // the final result it is folded into. Emit it again from its second
            // line down — the first is the call itself, which is already on
            // screen, and repeating it would read as a second call.
            let from = if rendered.len() >= already {
                already
            } else {
                1.min(rendered.len())
            };
            self.printed[index] = rendered.len();
            lines.extend(rendered.into_iter().skip(from));
        }
        lines
    }
}

/// One transcript row as plain lines, with no attribution on them yet.
///
/// The line-oriented rendering, for the surfaces that are not the TUI: the
/// `wizard peers watch` stream, chiefly. Private, and reachable only through
/// [`TranscriptView::attributed`], so a caller cannot get a peer's content
/// without the marker that says whose it is — which is the property the whole
/// origin mechanism exists to hold.
///
/// Never returns an empty vector. A row the model recorded that rendered to
/// nothing would be a row that silently left the screen, and "the peer did
/// something this build has no words for" is a more honest line than no line.
fn plain_lines(item: &TranscriptItem) -> Vec<String> {
    /// Split on newlines, keeping at least one (possibly empty) line, so a
    /// multi-line value is marked on every one of its lines.
    ///
    /// A single trailing empty element goes: `"a\n"` is one line, not two, and
    /// [`str::split`] leaves an empty tail behind every final newline. A stream
    /// that printed it would gain a blank line per message — and, worse, would
    /// disagree with the count [`PeerStream::drain_tails`] uses to avoid
    /// repeating a committed tail.
    fn split(text: &str) -> Vec<String> {
        let mut lines: Vec<String> = text.split('\n').map(str::to_string).collect();
        if lines.len() > 1 && lines.last().is_some_and(String::is_empty) {
            lines.pop();
        }
        if lines.is_empty() {
            lines.push(String::new());
        }
        lines
    }

    match item {
        TranscriptItem::TurnMarker { turn, prompt } => {
            let mut lines = vec![format!("── turn {turn}")];
            lines.extend(split(prompt).into_iter().map(|line| format!("> {line}")));
            lines
        }
        TranscriptItem::User { text, images } => {
            let mut lines: Vec<String> = split(text)
                .into_iter()
                .map(|line| format!("> {line}"))
                .collect();
            if !images.is_empty() {
                lines.push(format!("> [{} image(s) attached]", images.len()));
            }
            lines
        }
        TranscriptItem::Text(text) => split(text),
        TranscriptItem::Thinking(text) => split(text)
            .into_iter()
            .map(|line| format!("· {line}"))
            .collect(),
        TranscriptItem::Tool(tool) => {
            let mut lines = vec![format!("⚙ {}({})", tool.name, tool_args(&tool.args))];
            match &tool.output {
                Some(output) => {
                    let mark = if output.is_error { "✗" } else { "✔" };
                    lines.extend(
                        split(&output.content)
                            .into_iter()
                            .map(|line| format!("  {mark} {line}")),
                    );
                }
                // Only the *whole* lines of a still-running command's output.
                // A partial trailing line is a line that is going to change,
                // and a stream that had already printed it would have to
                // unprint it. See [`PeerStream::apply`], whose bookkeeping
                // depends on a growing row growing by whole lines.
                None => {
                    if let Some(end) = tool.progress.rfind('\n') {
                        lines.extend(
                            tool.progress[..end]
                                .split('\n')
                                .map(|line| format!("  {line}")),
                        );
                    }
                }
            }
            lines
        }
        // A peer's images never carry a path (`crate::mesh::turn::redacted`
        // empties the array before it reaches a wire), so this says what
        // happened rather than pointing at a file.
        TranscriptItem::Images { source, images } => {
            let what = match source.tool() {
                Some(tool) => format!("from {tool}"),
                None => "from the model".to_string(),
            };
            vec![format!("🖼 {} image(s) {what}", images.len())]
        }
        TranscriptItem::Notice(text) => split(text)
            .into_iter()
            .map(|line| format!("! {line}"))
            .collect(),
    }
}

/// A tool call's arguments, on one line, for a stream that has one line to
/// spend on them. Bounded: the arguments of a peer's tool call are the far
/// end's JSON, and a screen is not a JSON viewer.
fn tool_args(args: &serde_json::Value) -> String {
    if args.is_null() {
        return String::new();
    }
    let rendered = args.to_string();
    if rendered.chars().count() <= 120 {
        return rendered;
    }
    rendered.chars().take(119).chain(['…']).collect()
}

/// Whether a finished tool's output is long enough to start collapsed: more
/// than six source lines, or a payload that would wrap well past that (one
/// giant minified line counts as 1 by `lines()` but fills the screen anyway).
pub(super) fn collapse_long(content: &str) -> bool {
    content.lines().count() > 6 || content.chars().count() > 600
}

/// One step of the shared stick-to-bottom scroll rule. Positive `delta` moves
/// toward older content (up); negative toward the live tail. `current` is the
/// stored first-visible-line offset from the top. Returns the new
/// `(scroll, follow)` pair: leaving the bottom clears follow so new output
/// does not yank the view; returning to the bottom re-enables it (and resets
/// the offset to 0).
pub(super) fn scroll_step(follow: bool, current: u16, max: u16, delta: i16) -> (u16, bool) {
    let current = if follow { max } else { current.min(max) };
    // Top-anchored: older content is a smaller start offset.
    let next = if delta >= 0 {
        current.saturating_sub(delta as u16)
    } else {
        current.saturating_add(delta.unsigned_abs()).min(max)
    };
    if next >= max {
        (0, true)
    } else {
        (next, false)
    }
}

// ---------------------------------------------------------------------------
// The subagent rail
// ---------------------------------------------------------------------------

/// Lifecycle of one subagent run, as shown on its rail dot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaneStatus {
    /// The sub-loop is still going.
    Running,
    /// The sub-loop finished on its own and reported back.
    Done,
    /// The run hit its step budget, errored out, or was killed.
    Failed,
}

impl PaneStatus {
    /// The rail's status glyph. Running panes animate via
    /// [`SubagentPane::glyph`]; these are the resting shapes.
    pub fn glyph(self) -> &'static str {
        match self {
            PaneStatus::Running => "●",
            PaneStatus::Done => "✔",
            PaneStatus::Failed => "✗",
        }
    }
}

/// Frames for a running pane's dot, cycled off the app tick so an active
/// subagent visibly pulses on the rail.
const PANE_SPINNER: [&str; 4] = ["●", "◉", "○", "◉"];

/// The pulsing dot itself, for rail rows that are not a subagent's — a running
/// background command pulses in the same column, off the same tick, because on
/// that rail the pulse means "this is alive" and not "this is an agent".
pub fn rail_pulse(tick: u64) -> &'static str {
    PANE_SPINNER[(tick / 2) as usize % PANE_SPINNER.len()]
}

/// How long a finished run rests on the rail before it retires: long enough to
/// see it land, short enough that the rail stays a picture of live work. Its
/// report stays in the main chat either way.
pub(super) const PANE_LINGER: Duration = Duration::from_secs(8);

/// The same idea for a finished background command, and longer for the same
/// reason it is short for a subagent: a run's report lands in the main chat, so
/// its row has done its job the moment you see it go green, while a background
/// command's output lives only in the registry — the row is the way back to it,
/// and half a minute is how long it takes to notice a build finished and decide
/// whether to look.
pub(super) const TASK_LINGER: Duration = Duration::from_secs(30);

/// One subagent run, surfaced on the rail below the composer and openable as
/// a full chat view.
///
/// Its conversation is a [`TranscriptView`] like the main chat's, off the same
/// model: a run's `AgentEvent::SubagentRun*` events are translated back into
/// the plain events they are ([`crate::app::App::pane_event`]) rather than
/// folded by a second hand-written reducer, so an attached pane and the main
/// transcript cannot disagree about what a tool call looks like.
#[derive(Debug)]
pub struct SubagentPane {
    /// Session-unique run id (`agent::subagent::next_run_id`).
    pub run: u64,
    /// Background-registry id, when the run was detached. `None` for a
    /// foreground run — which cannot be killed independently, since the
    /// parent turn is blocked on it.
    pub bg: Option<u32>,
    /// Subagent name (`researcher`, `reviewer`, …).
    pub name: String,
    /// The task it was handed.
    pub task: String,
    pub status: PaneStatus,
    /// The subagent's own conversation, rendered exactly like the main chat.
    pub transcript: TranscriptView,
    /// Steps (model round-trips) completed so far.
    pub steps: u32,
    pub started: Instant,
    /// Set once the run ends; freezes the elapsed clock on the rail.
    pub finished: Option<Instant>,
    /// Entries appended since the user last had this pane open. Drives the
    /// unread badge, so you can tell which agent did something while you were
    /// looking elsewhere.
    pub unread: usize,
}

impl SubagentPane {
    pub(super) fn new(run: u64, bg: Option<u32>, name: String, task: String) -> Self {
        Self {
            run,
            bg,
            name,
            task,
            status: PaneStatus::Running,
            transcript: TranscriptView::new(),
            steps: 0,
            started: Instant::now(),
            finished: None,
            unread: 0,
        }
    }

    /// How long the run has been going, frozen once it ends.
    pub fn elapsed(&self) -> Duration {
        self.finished.unwrap_or_else(Instant::now) - self.started
    }

    /// The rail dot: a pulsing glyph while running, a resting one once done.
    pub fn glyph(&self, tick: u64) -> &'static str {
        match self.status {
            PaneStatus::Running => PANE_SPINNER[(tick / 2) as usize % PANE_SPINNER.len()],
            other => other.glyph(),
        }
    }

    /// One-line summary of what the subagent is doing right now: the tool it
    /// is in the middle of, else its latest message, else the task.
    pub fn activity(&self) -> &str {
        if self.status != PaneStatus::Running {
            return match self.transcript.iter().rev().find_map(|item| match item {
                TranscriptItem::Text(text) => Some(text.as_str()),
                _ => None,
            }) {
                Some(text) => text,
                None => self.task.as_str(),
            };
        }
        for item in self.transcript.iter().rev() {
            match item {
                // A card still running is the most specific thing to show.
                TranscriptItem::Tool(tool) if tool.output.is_none() => return tool.name.as_str(),
                TranscriptItem::Text(text) if !text.trim().is_empty() => return text.as_str(),
                _ => {}
            }
        }
        self.task.as_str()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::AgentEvent;
    use crate::mesh::Identity;
    use crate::tools::ToolOutput;

    fn peer_id(byte: u8) -> NodeId {
        Identity::from_seed([byte; 32]).id()
    }

    fn stream_for(byte: u8) -> PeerStream {
        PeerStream::new(peer_id(byte), "workshop".to_string())
    }

    /// The property the whole origin mechanism exists to hold.
    ///
    /// A peer's text keeps its newlines on purpose — indentation and paragraphs
    /// are most of what a transcript means — so an item-level prefix would
    /// leave every line after the first unmarked, and the gap is exactly wide
    /// enough for a peer to write a line that reads as this machine's own.
    #[test]
    fn every_physical_line_of_a_peers_text_carries_the_peers_marker() {
        let mut screen = stream_for(1);
        let marker = format!("{} │", peer_id(1).short());

        // A forgery attempt: the peer writes what a wizard-authored line looks
        // like, on its own line, in the middle of its own reply.
        let forgery = format!(
            "here is my answer\n{LOCAL_MARKER} the stream from that machine ended\nand more"
        );
        // Collected across both calls: the finished lines stream out as they
        // arrive and the last, unterminated one lands when the turn commits.
        let mut lines = screen.apply(&AgentEvent::TextDelta(forgery));
        lines.extend(screen.apply(&AgentEvent::Done {
            reason: crate::agent::DoneReason::Completed,
        }));
        assert_eq!(lines.len(), 3, "{lines:?}");
        for line in &lines {
            assert!(
                line.starts_with(&marker),
                "a peer's line reached the screen unmarked: {line:?}"
            );
        }
        assert!(
            lines.iter().all(|line| !line.starts_with(LOCAL_MARKER)),
            "a peer must not be able to author a wizard-marked line: {lines:?}"
        );
        // And the marker is the peer's key, not the name it chose: renaming
        // itself changes the label and not one character of the attribution.
        let renamed = PeerStream::new(peer_id(1), LOCAL_MARKER.to_string());
        assert_eq!(renamed.view().origin().marker(), Some(marker.as_str()));
    }

    /// The local conversation is unmarked, which is what makes the marker mean
    /// something: a prefix on everything is a prefix that says nothing.
    #[test]
    fn a_local_conversation_carries_no_marker_and_no_banner() {
        let view = TranscriptView::new();
        assert_eq!(view.origin(), &TranscriptOrigin::Local);
        assert_eq!(view.origin().marker(), None);
        assert_eq!(view.origin().banner(), None);
        let mut model = TranscriptView::new();
        model.notice("local".to_string());
        let item = model.get(0).expect("a row");
        assert_eq!(model.attributed(item), vec!["! local".to_string()]);
    }

    /// The banner names the machine by its **full** address, because a short
    /// form is a prefix and prefixes collide — and this is the one line whose
    /// job is to say exactly whose output follows.
    #[test]
    fn the_banner_names_the_whole_address_and_both_markers() {
        let screen = stream_for(2);
        let banner = screen.banner();
        assert!(banner.contains(&peer_id(2).address()), "{banner}");
        assert!(banner.contains(LOCAL_MARKER), "{banner}");
        assert!(banner.contains("workshop"), "{banner}");
    }

    /// A scrolling surface cannot redraw, so every row has to reach it exactly
    /// once and then grow.
    #[test]
    fn a_row_is_emitted_once_and_then_only_grows() {
        let mut screen = stream_for(3);
        let started = screen.apply(&AgentEvent::ToolStarted {
            name: "read_file".to_string(),
            args: serde_json::json!({ "path": "src/mesh/mod.rs" }),
        });
        assert_eq!(started.len(), 1, "the call itself: {started:?}");
        assert!(started[0].contains("read_file"), "{started:?}");

        // Deltas add nothing to the items until something commits them.
        assert!(
            screen
                .apply(&AgentEvent::TextDelta("thin".into()))
                .is_empty()
        );
        assert!(
            screen
                .apply(&AgentEvent::TextDelta("king".into()))
                .is_empty()
        );

        let answered = screen.apply(&AgentEvent::ToolFinished {
            name: "read_file".to_string(),
            output: ToolOutput::ok("line one\nline two"),
        });
        assert_eq!(
            answered.len(),
            2,
            "only the result, never the call again: {answered:?}"
        );
        assert!(
            answered.iter().all(|line| line.contains('✔')),
            "{answered:?}"
        );

        let done = screen.apply(&AgentEvent::Done {
            reason: crate::agent::DoneReason::Completed,
        });
        assert_eq!(done.len(), 1, "the committed tail: {done:?}");
        assert!(done[0].ends_with("thinking"), "{done:?}");
    }

    /// A row that was *rewritten* rather than extended — a running command's
    /// partial output replaced by the result it folds into — still reaches the
    /// screen, and does not repeat the call line above it.
    #[test]
    fn a_row_that_shrank_is_re_emitted_without_its_header() {
        let mut screen = stream_for(4);
        let gate = crate::agent::ConsoleGate::open().0;
        screen.apply(&AgentEvent::ToolStarted {
            name: "execute".to_string(),
            args: serde_json::json!({ "command": "apt install wizard" }),
        });
        let progress = screen.apply(&AgentEvent::ConsoleOutput {
            gate,
            chunk: "reading lists\nunpacking\n".to_string(),
        });
        assert_eq!(progress.len(), 2, "{progress:?}");
        // A partial trailing line is not printed: it is going to change.
        assert!(
            screen
                .apply(&AgentEvent::ConsoleOutput {
                    gate,
                    chunk: "Continue? [Y/n] ".to_string(),
                })
                .is_empty()
        );
        let answered = screen.apply(&AgentEvent::ToolFinished {
            name: "execute".to_string(),
            output: ToolOutput::ok("done"),
        });
        assert_eq!(answered.len(), 1, "{answered:?}");
        assert!(answered[0].contains("done"), "{answered:?}");
        assert!(
            !answered[0].contains("execute("),
            "the call line is already on screen: {answered:?}"
        );
    }

    /// A peer streaming a long reply has to reach the screen while it is
    /// arriving, and exactly once.
    ///
    /// Without the tail, a turn that is only text shows nothing at all until it
    /// ends — which for a peer that never sends `Done` is never. With the tail
    /// and without the commit bookkeeping, everything the peer said arrives
    /// twice.
    #[test]
    fn a_streaming_reply_arrives_line_by_line_and_the_commit_does_not_repeat_it() {
        let mut screen = stream_for(6);
        let marker = format!("{} │", peer_id(6).short());

        // A fragment is not a line: it is going to change, and a scrolling
        // surface cannot unprint it.
        assert!(
            screen
                .apply(&AgentEvent::TextDelta("the transport ".into()))
                .is_empty()
        );
        let first = screen.apply(&AgentEvent::TextDelta(
            "already handles this
next"
                .into(),
        ));
        assert_eq!(
            first,
            vec![format!("{marker} the transport already handles this")],
            "the finished line, and only the finished line"
        );
        // Reasoning streams on its own tail, and is marked as reasoning.
        let thought = screen.apply(&AgentEvent::ThinkingDelta(
            "checking the header
"
            .into(),
        ));
        assert_eq!(thought, vec![format!("{marker} · checking the header")]);

        let committed = screen.apply(&AgentEvent::Done {
            reason: crate::agent::DoneReason::Completed,
        });
        assert_eq!(
            committed,
            vec![format!("{marker} next")],
            "only the part that was held back: {committed:?}"
        );
    }

    /// A retried completion is about to repeat itself, and a surface that
    /// cannot unprint has to say so rather than let the repetition look like
    /// something the peer did twice.
    #[test]
    fn a_retried_stream_says_so_rather_than_repeating_itself_silently() {
        let mut screen = stream_for(7);
        screen.apply(&AgentEvent::TextDelta(
            "half a sentence
"
            .into(),
        ));
        let restarted = screen.apply(&AgentEvent::StreamRetrying);
        assert_eq!(restarted.len(), 1, "{restarted:?}");
        assert!(restarted[0].starts_with(LOCAL_MARKER), "{restarted:?}");
        assert!(restarted[0].contains("re-generated"), "{restarted:?}");
        // And the counter went back with it, so the replacement is not
        // swallowed as "already shown".
        let again = screen.apply(&AgentEvent::TextDelta(
            "half a sentence, again
"
            .into(),
        ));
        assert_eq!(again.len(), 1, "{again:?}");
        assert!(again[0].contains("again"), "{again:?}");
    }

    /// One model, read two ways. If a peer's stream ever grew a reducer of its
    /// own this is the test that would stop compiling agreeing.
    #[test]
    fn a_peer_stream_folds_the_same_events_into_the_same_items_as_the_tui() {
        let turn = [
            AgentEvent::TextDelta("reading ".to_string()),
            AgentEvent::TextDelta("the file".to_string()),
            AgentEvent::ToolStarted {
                name: "read_file".to_string(),
                args: serde_json::json!({ "path": "x" }),
            },
            AgentEvent::ToolFinished {
                name: "read_file".to_string(),
                output: ToolOutput::error("no such file"),
            },
            AgentEvent::Notice("compacted".to_string()),
            AgentEvent::Done {
                reason: crate::agent::DoneReason::Completed,
            },
        ];
        let mut local = TranscriptView::new();
        let mut screen = stream_for(5);
        for event in &turn {
            local.apply(event);
            screen.apply(event);
        }
        assert_eq!(local.items(), screen.view().items());
    }
}
