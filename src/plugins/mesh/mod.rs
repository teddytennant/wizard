//! The mesh data model: other machines running Wizard, what they can do, and
//! what this machine will let them do.
//!
//! This is P2 of the v2 plan: the model, the store, the seam, and the two
//! implementations behind it. [`transport::LoopbackTransport`] runs in this
//! process and opens no socket; [`quic::QuicTransport`] crosses machines, and
//! **its listener is off unless `[mesh] listen` says otherwise**
//! ([`crate::config::MeshConfig`]), because a mesh that opened a socket on
//! install would be a security surface nobody asked for.
//!
//! The scope is narrow on purpose. Two nodes on different machines, each
//! directly reachable or on the same LAN — no NAT traversal, no relay, no
//! overlay. Three message kinds — liveness, announcement, session-event
//! subscription — and no delegated work, so there is no task on the wire that
//! nothing would run.
//!
//! ```text
//! Node        = { id, addr, name, caps, last_seen }
//! Capability  = { models[], tools[], skills[], subagents[], accepts_work }
//! Peer        = { node, trust: Trusted | Known | Blocked, added_at }
//! ```
//!
//! # Identity is the address
//!
//! A node's id is an ed25519 public key and its address is a reversible
//! encoding of that key ([`node`]). Nothing assigns it, so nothing has to be
//! asked where a node lives: there is no registry to look a name up in,
//! because the name *is* the key. That is the whole of what makes this
//! serverless, and it is why discovery is a paste rather than a lookup.
//!
//! # The security model is the workstream
//!
//! Wizard has shipped fail-open defaults before: the Telegram allowlist that
//! defaulted to allow-all, project hooks that executed themselves on session
//! start. A mesh is where that class of mistake stops being a local problem,
//! so every default here leans the other way, and each one is pinned by a test
//! rather than by a comment:
//!
//! - **Deny by default.** [`Capability::accepts_work`] is `false` unless a
//!   node says otherwise, and a pasted address lands at [`Trust::Known`],
//!   which may not send or receive work.
//! - **Trust is a human's decision, recorded on disk.** Three states, changed
//!   only by an explicit call, never inferred from a peer's behaviour or from
//!   what a peer says about itself. Re-announcing does not un-block anybody.
//! - **Revocation is real.** [`Mesh::set_trust`] away from `Trusted` drops the
//!   peer's live subscriptions through the transport in the same call, because
//!   a revocation that leaves a stream running has revoked nothing. Over a
//!   socket that means closing the QUIC connection, so every stream on it fails
//!   at once on both machines rather than at some timeout.
//! - **The publisher consents too.** A peer asking to watch *this* node is
//!   checked against this machine's own store before an event is written to it.
//!   [`consent`] is the seam that carries the answer to the transport, and
//!   [`Mesh::with_consent`] is what keeps it in step with the store — refreshed
//!   inside the same call that records a decision, so there is no second step
//!   for a caller to forget.
//! - **Limits apply to trusted peers too.** A trusted peer with a retry loop
//!   can spend an API budget as effectively as a hostile one, so [`Mesh::admit`]
//!   meters every admission per peer ([`Limits`]) and refuses anything that is
//!   not explicitly [`Trust::Trusted`].
//!
//!   Read that one as a gate built ahead of the thing it guards, not as
//!   enforcement running today. Delegated work is mesh tier 3 and is cut from
//!   this release, so there is no inbound path by which a peer can ask this
//!   machine to do anything: [`PeerEventKind`] carries only reports
//!   (`SessionStarted`, `Turn`, `SessionEnded`, `CapabilityChanged`), and
//!   `PeerTurn::clean` drops any event that is a request. `admit` therefore
//!   has no caller, and nothing metered is going unmetered — there is nothing
//!   to meter. It stays because the tier that needs it will need exactly this,
//!   and because deleting a correct gate to re-derive it later is how gates end
//!   up missing.
//! - **Nothing a peer sends is trusted input.** Every string that crosses the
//!   boundary is a [`PeerText`], sanitised at construction, and it must never
//!   reach a system prompt. [`crate::trust`] draws exactly this line for
//!   project files; this is the same line, one machine further out.
//!
//! # Watching a peer's session
//!
//! Tier 2 of P2, and the first thing here that carries anything: a trusted
//! peer's session events arrive as [`PeerEvent`]s and render in this machine's
//! own transcript. [`Mesh::subscribe`] opens the stream, [`Mesh::publish_turn`]
//! feeds a local session's events into it, and what crosses is
//! [`crate::agent::AgentEvent`] itself, wrapped in [`PeerTurn`]. There is no
//! second event enum to keep in step: see [`turn`] for why the one that used to
//! be here is gone.
//!
//! A subscription is **read-only and display-only**, and every clause of that
//! is enforced rather than asserted:
//!
//! - [`Mesh::subscribe`] refuses anything that is not [`Trust::Trusted`]. A
//!   stream from a merely-known node is an inbound channel nobody approved.
//! - [`Mesh::set_trust`] away from `Trusted` severs live streams in *both*
//!   directions, so un-trusting a peer stops this machine's own session events
//!   reaching it as well.
//! - [`PeerTurn::sanitize`] is the only way to build a payload, and the agent
//!   events that *ask* this machine for something rather than reporting what
//!   happened do not cross at all.
//! - Delivery is lossy under backpressure and bounded on both axes: at most
//!   [`transport::SUBSCRIPTION_BUFFER`] events queued per subscription and at
//!   most [`PeerTurn::MAX_TEXT`] characters of text per event.
//!
//! # For the graph explorer
//!
//! The explorer renders from cached state, offline, and must not lie: see
//! [`peer::Presence`], which distinguishes "seen just now" from "seen a while
//! ago" from "pasted in and never heard from". [`Graph`] turns a store into
//! the vertices and edges to draw, and [`peer::synthetic_peers`] generates a
//! deterministic mesh for tests and for the 50-nodes-at-60fps performance bar.

pub mod capability;
pub mod cli;
pub mod consent;
pub mod discovery;
pub mod node;
pub mod peer;
pub mod plugin;
pub mod quic;
pub mod tee;
pub mod tls;
pub mod transport;
pub mod turn;
pub mod wire;
pub mod x509;

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use anyhow::{Context, Result, anyhow};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Deserializer, Serialize};

use crate::agent::AgentEvent;
use crate::text::is_invisible;

pub use capability::{Capability, CapabilityKind, LimitExceeded, Limits};
pub use consent::{Consent, TrustLedger};
pub use discovery::Discovery;
pub use node::{Identity, Node, NodeId};
pub use peer::{Peer, PeerStore, Presence, Trust};
pub use plugin::MeshPlugin;
pub use quic::QuicTransport;
pub use tee::MeshTee;
pub use transport::{LoopbackTransport, PeerEvent, PeerEventKind, Subscription, Transport};
pub use turn::PeerTurn;

// ---------------------------------------------------------------------------
// The untrusted-input boundary
// ---------------------------------------------------------------------------

/// A string that came from a peer.
///
/// Not a `String`, and the difference is the point. A node name, a model name
/// and an event summary are all written by a machine this one does not
/// control, and they end up in a terminal, in a log, and in a graph label. So
/// the only way to build one is [`PeerText::sanitize`], which happens at the
/// boundary (including inside `Deserialize`, so nothing can be smuggled in by
/// decoding a record instead of constructing one).
///
/// What sanitising removes, and why:
///
/// - **Control characters**, including `ESC`, replaced by a space. A
///   capability list that repaints the terminal, moves the cursor or sets a
///   window title is not a capability list. Wizard renders peer text into a
///   TUI whose whole surface is escape sequences.
/// - **Everything that draws nothing**, deleted outright: format characters
///   (`Cf`), private-use code points (`Co`), noncharacters, and every
///   `Default_Ignorable_Code_Point`. That set is where the zero-width space,
///   the zero-width joiner, the word joiner, the soft hyphen, the byte-order
///   mark, the variation selectors, the Hangul fillers and the whole
///   `U+E0000` Tag block live. See [`crate::text::is_invisible`] for the
///   tables and for what is deliberately *not* in them.
/// - **Bidirectional formatting overrides** (`U+202A`..`U+202E`,
///   `U+2066`..`U+2069`, and the LRM/RLM/ALM marks). The trojan-source class:
///   text that renders in an order other than the one it is stored in, so what
///   a human approves is not what is recorded.
/// - **Runs of whitespace**, collapsed to one space and trimmed, so alignment
///   cannot be forged and a name cannot be padded off the side of a table.
/// - **Length past [`PeerText::MAX_CHARS`]**, truncated with an ellipsis, so
///   one peer cannot occupy a whole screen or a whole log line.
///
/// # Why an invisible character is deleted and a control character is not
///
/// A control character has visible consequences: `"read\nfile"` is two words
/// on screen, so dropping the newline would silently join them and a space is
/// the honest replacement. An invisible character has none: `"read\u{200b}file"`
/// is *already* `readfile` to every human who looks at it, so replacing it
/// with a space would invent a word break the reader never saw and hand a peer
/// a way to forge one. Deleting is what makes the sanitised text equal to what
/// a person sees, which is the property the rest of this module leans on.
///
/// # Strip, not reject
///
/// A name carrying invisible characters is cleaned and kept, not refused
/// wholesale. Three reasons, decided rather than defaulted:
///
/// 1. What is left is exactly what a human would have seen on screen, so
///    keeping it hides nothing.
/// 2. Refusing gains nothing a peer cannot already have: a peer that wants no
///    name simply sends none, and [`Node::label`] already falls back to the
///    address for empty text.
/// 3. One of these classes shows up in honest names. Any emoji written with a
///    presentation selector carries `U+FE0F`, and refusing the name would
///    punish an ordinary peer for a class that is only dangerous by
///    comparison.
///
/// What rejecting would have bought is the ability to keep two peers from
/// rendering identically, and that is worth having, so it is done one layer
/// out instead and unconditionally: [`Graph::build`] disambiguates any two
/// node vertices that would carry the same label, by address.
///
/// # What this does not do
///
/// There is no NFC/NFKC pass, because normalisation needs a Unicode table this
/// crate does not carry and adding one to reach this module is not a trade
/// worth making. So mixed-script homoglyphs survive: Cyrillic `а` renders like
/// Latin `a` and always will. Nothing string-shaped fixes that, which is why
/// the module's first claim is that identity is the *address* and never the
/// name, and why the graph prints the address the moment two labels collide.
///
/// There is deliberately no `Display` impl. Rendering goes through
/// [`PeerText::as_str`], which is greppable, and the absence of `Display`
/// means peer text cannot land in a `format!` that builds a prompt without
/// somebody writing the call that does it.
///
/// Sanitising is **not** enough to make this safe to put in a system prompt.
/// Nothing makes it safe to put in a system prompt: "ignore your previous
/// instructions" survives every filter above intact. Peer text is data to
/// render, never instructions to follow.
#[derive(Clone, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
pub struct PeerText(String);

impl PeerText {
    /// Longest peer string kept, in characters (not bytes, so truncation
    /// cannot split a multi-byte character).
    pub const MAX_CHARS: usize = 64;

    /// Clean a string that came from a peer. The only way to build one.
    ///
    /// The label policy of [`sanitize_label`]: one line, whitespace collapsed,
    /// capped at [`PeerText::MAX_CHARS`]. A peer's *turn text* is cleaned by
    /// the other policy, [`sanitize_body`], because a transcript is not a
    /// label; see [`turn`].
    pub fn sanitize(raw: &str) -> Self {
        Self(sanitize_label(raw, Self::MAX_CHARS))
    }

    /// The sanitised text, for rendering. Never for building a prompt.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Whether anything survived sanitising.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

/// Clean a peer's string as a **label**: one line, whitespace collapsed and
/// trimmed, invisible characters deleted, capped at `max_chars` with a visible
/// ellipsis.
///
/// What [`PeerText`] is built out of, and what a member name inside a peer's
/// event is cleaned with. The policy fits a thing that gets drawn on one row of
/// a table or one vertex of a graph: alignment cannot be forged, a name cannot
/// be padded off the side, and a run of newlines cannot turn one row into
/// twelve.
///
/// One pass, and it stops at `max_chars` rather than cleaning the whole input
/// and trimming afterwards: `raw` is attacker-sized, and a sanitiser that
/// allocates in proportion to what it is *given* rather than to what it *keeps*
/// is a bound in the docs and not in the code.
fn sanitize_label(raw: &str, max_chars: usize) -> String {
    if max_chars == 0 {
        return String::new();
    }
    let mut cleaned = String::with_capacity(raw.len().min(max_chars.saturating_mul(4)));
    // Characters kept so far. Not `cleaned.len()`: the cap is in characters so
    // truncation cannot split a multi-byte one.
    let mut kept = 0usize;
    // Whitespace was seen since the last kept character. Emitted only if
    // something follows it, which collapses runs and trims both ends without a
    // second pass.
    let mut pending_space = false;
    let mut overflowed = false;

    for ch in raw.chars() {
        if is_invisible(ch) {
            continue;
        }
        if ch.is_control() || ch.is_whitespace() {
            pending_space = !cleaned.is_empty();
            continue;
        }
        if pending_space {
            if kept == max_chars {
                overflowed = true;
                break;
            }
            cleaned.push(' ');
            kept += 1;
            pending_space = false;
        }
        if kept == max_chars {
            overflowed = true;
            break;
        }
        cleaned.push(ch);
        kept += 1;
    }

    if overflowed {
        // The cap is full and there was more: give the last slot back to an
        // ellipsis so the elision is visible rather than silent.
        cleaned.pop();
        cleaned.push('…');
    }
    cleaned
}

/// Clean a peer's string as a **body**: the text of a turn, on its way into
/// somebody's transcript.
///
/// The same danger and a different shape, so a different policy. What is
/// removed is identical in spirit to [`sanitize_label`]: invisible characters
/// deleted (the tables in [`crate::text::is_invisible`], which is what closes
/// the trojan-source and zero-width classes), and every control character replaced
/// by a space, because `ESC` in a transcript is somebody else repainting this
/// terminal.
///
/// What is *kept*, and why the label policy would be a bug here:
///
/// - **Leading and trailing whitespace.** A [`crate::agent::AgentEvent::TextDelta`]
///   is a fragment of a sentence, not a whole one. Trimming `" world"` joins
///   two words the far end's model put apart, which is a sanitiser that changes
///   what was said.
/// - **Runs of spaces.** Indentation is most of what a code block means, and a
///   watcher reading a peer's diff through a sanitiser that left-aligned every
///   line would be reading something else.
/// - **Newlines**, up to two in a row. Paragraphs and code blocks survive; a
///   peer cannot scroll a watcher's screen with nothing but line breaks.
///
/// Capped at `max_chars` with the same visible ellipsis. The cap here is a
/// budget spent across a whole event rather than a per-string constant: see
/// [`turn::PeerTurn::MAX_TEXT`].
fn sanitize_body(raw: &str, max_chars: usize) -> String {
    if max_chars == 0 {
        return String::new();
    }
    let mut cleaned = String::with_capacity(raw.len().min(max_chars.saturating_mul(4)));
    let mut kept = 0usize;
    // Line breaks emitted since the last other character, so a run collapses
    // without the buffering that would swallow a fragment's own trailing
    // newline.
    let mut lines = 0usize;
    let mut overflowed = false;

    for ch in raw.chars() {
        if is_invisible(ch) {
            continue;
        }
        let ch = if ch == '\n' {
            lines += 1;
            if lines > 2 {
                continue;
            }
            '\n'
        } else {
            // Tabs, carriage returns, `ESC`, and the C1 controls all become a
            // space: each of them either moves a cursor or draws nothing, and
            // a space is the honest width of what a human saw.
            //
            // The run counter is only cleared by something that draws. It used
            // to reset on *any* non-newline, so a single space between the line
            // breaks defeated the cap entirely — `" \n"` repeated yielded as
            // many lines as the character budget allowed, which is exactly the
            // screen-scrolling the paragraph above says a peer cannot do. A run
            // of blank lines is still a run when the blank lines contain
            // spaces.
            if ch.is_control() || ch.is_whitespace() {
                ' '
            } else {
                lines = 0;
                ch
            }
        };
        if kept == max_chars {
            overflowed = true;
            break;
        }
        cleaned.push(ch);
        kept += 1;
    }

    if overflowed {
        cleaned.pop();
        cleaned.push('…');
    }
    cleaned
}

impl std::fmt::Debug for PeerText {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Safe to print: it was sanitised on the way in. Quoted so an empty
        // one is visible in a log line rather than looking like a missing
        // field.
        write!(f, "{:?}", self.0)
    }
}

/// Sanitises on the way in, so a record read from disk or off a wire gets the
/// same treatment as one built in this process.
///
/// A visitor rather than `String::deserialize(..).map(sanitize)`, so a format
/// that can hand back a borrowed slice (serde_json does, for any string with
/// no escapes in it) never allocates the peer's copy at all: what gets
/// allocated is the [`PeerText::MAX_CHARS`] that survive, not the megabytes
/// that were sent.
impl<'de> Deserialize<'de> for PeerText {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct SanitizingVisitor;

        impl serde::de::Visitor<'_> for SanitizingVisitor {
            type Value = PeerText;

            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("a string")
            }

            fn visit_str<E: serde::de::Error>(self, raw: &str) -> Result<PeerText, E> {
                Ok(PeerText::sanitize(raw))
            }

            fn visit_string<E: serde::de::Error>(self, raw: String) -> Result<PeerText, E> {
                Ok(PeerText::sanitize(&raw))
            }
        }

        deserializer.deserialize_str(SanitizingVisitor)
    }
}

// ---------------------------------------------------------------------------
// Admission
// ---------------------------------------------------------------------------

/// May this peer have what it is asking for?
///
/// Shaped after [`crate::trust::Gate`]: an answer, plus a line meant for the
/// operator when the answer is no. The refusal is for a human to read, not for
/// a model to act on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Admission {
    Allowed,
    Refused(String),
}

impl Admission {
    pub fn is_allowed(&self) -> bool {
        matches!(self, Admission::Allowed)
    }

    /// The refusal reason, if it was one.
    pub fn reason(&self) -> Option<&str> {
        match self {
            Admission::Allowed => None,
            Admission::Refused(why) => Some(why),
        }
    }
}

// ---------------------------------------------------------------------------
// The mesh
// ---------------------------------------------------------------------------

/// This node, its peers, and the way it reaches them.
///
/// The one object the GUI holds. Every mutation that has a security
/// consequence goes through here rather than through [`PeerStore`] directly,
/// because some of them are two steps that must not come apart: revoking trust
/// also drops subscriptions, and admitting work also meters it.
pub struct Mesh {
    identity: Identity,
    name: PeerText,
    caps: Capability,
    store: PeerStore,
    transport: Arc<dyn Transport>,
    /// What the transport is allowed to serve, kept in step with the store.
    ///
    /// The publisher's half of the trust decision, which a network transport
    /// needs and cannot get any other way: it cannot borrow the store (the
    /// `Mesh` owns it and holds the transport behind an `Arc`, so that is a
    /// cycle) and must not own a copy of it (that is a second store to keep in
    /// step). See [`consent`].
    consent: TrustLedger,
}

impl Mesh {
    /// Assemble a mesh from an identity, a peer store and a transport.
    ///
    /// The local node advertises nothing until [`Mesh::set_local`] says
    /// otherwise: `accepts_work` starts false here too, so a node that is
    /// merely running does not offer itself as somebody else's compute.
    ///
    /// A stored record whose id is *this* node is dropped on the way in.
    /// [`Mesh::add_peer`] refuses to create one, but the store is a file, and a
    /// file can be hand-edited, restored from another machine's backup, or
    /// written by a future transport that folds an observed set into it. Such
    /// a record would let [`Mesh::admit`] consult a planted trust decision
    /// about the local node instead of refusing it by name, and would draw a
    /// self-loop on the graph. It is not a peer record, so it is not kept.
    ///
    /// The ledger this builds is its own and is not shared with anything, which
    /// is correct for [`LoopbackTransport`] (whose subscribers are in this
    /// process and were approved by their own `Mesh`) and wrong for a network
    /// transport. Use [`Mesh::with_consent`] for one of those.
    pub fn new(identity: Identity, store: PeerStore, transport: Arc<dyn Transport>) -> Self {
        Self::with_consent(identity, store, transport, TrustLedger::new())
    }

    /// Assemble a mesh whose transport shares `consent`.
    ///
    /// The ledger is filled from the store here and refreshed by every method
    /// that changes what is decided about a peer. It is not a second step a
    /// caller has to remember, for the same reason [`Mesh::set_trust`] fuses
    /// revoking and persisting into the call that records the decision: a
    /// caller that has to remember one will forget it, and the thing forgotten
    /// would be a peer that keeps being served after it stopped being trusted.
    pub fn with_consent(
        identity: Identity,
        mut store: PeerStore,
        transport: Arc<dyn Transport>,
        consent: TrustLedger,
    ) -> Self {
        store.forget(&identity.id());
        let mesh = Self {
            identity,
            name: PeerText::default(),
            caps: Capability::none(),
            store,
            transport,
            consent,
        };
        mesh.refresh_consent();
        mesh
    }

    /// Re-derive what the transport may serve from what the store now says.
    ///
    /// Wholesale, because an incremental update has a removal path to forget,
    /// and a forgotten removal here is a peer that was dropped from the store
    /// and can still be served.
    fn refresh_consent(&self) {
        self.consent.replace(self.store.iter());
    }

    /// Set what this node calls itself and what it advertises.
    pub fn set_local(&mut self, name: &str, caps: Capability) {
        self.name = PeerText::sanitize(name);
        self.caps = caps.normalised();
    }

    /// This node's id, which is also its address.
    pub fn local_id(&self) -> NodeId {
        self.identity.id()
    }

    /// The record this node announces.
    pub fn local_node(&self) -> Node {
        Node {
            id: self.local_id(),
            name: self.name.clone(),
            caps: self.caps.clone(),
            last_seen: None,
        }
    }

    /// The peer store, read-only. Mutations with a security consequence are
    /// methods on [`Mesh`].
    pub fn store(&self) -> &PeerStore {
        &self.store
    }

    /// Persist the peer store.
    pub fn save(&self) -> Result<()> {
        self.store.save()
    }

    /// Write the store back after a decision that has to survive a restart.
    ///
    /// Every method on [`Mesh`] that changes what is *decided* about a peer
    /// calls this before it returns, for the same reason [`Mesh::set_trust`]
    /// fuses revocation into the recording: a caller that has to remember the
    /// second step is a caller that will forget it, and the step being
    /// forgotten here is the one that makes a Blocked decision outlive the
    /// process that made it.
    ///
    /// An ephemeral store (the synthetic mesh, tests) has deliberately chosen
    /// not to have a file, so there is nothing here to fail. [`Mesh::save`]
    /// still refuses loudly for a caller that asks for a write outright, which
    /// is the case where silence would be a lie.
    ///
    /// Reachable from [`cli`] as well, for the two commands that learn
    /// something rather than decide something: an announcement fetched
    /// ([`Mesh::refresh`]) and a peer observed to be alive
    /// ([`Mesh::mark_seen`]) are both facts worth keeping, and neither is a
    /// decision, so neither writes on its own — a live surface marks a peer
    /// seen far too often to touch the disk each time.
    pub(super) fn persist(&self) -> Result<()> {
        if self.store.path().is_none() {
            return Ok(());
        }
        self.store
            .save()
            .context("the decision was applied in memory but could not be written to disk")
    }

    /// Publish this node's presence and capability on the transport.
    pub async fn announce(&self) -> Result<()> {
        self.transport.announce(&self.local_node()).await
    }

    /// Add a peer from a pasted address.
    ///
    /// The whole of discovery. Returns the peer's trust after the call, which
    /// for a peer already in the store is whatever was decided about it
    /// before: adding is not a decision, and re-adding is not a decision
    /// either. The store is written before this returns, so a peer an operator
    /// pasted in is still there after a crash.
    pub fn add_peer(&mut self, address: &str, now: DateTime<Utc>) -> Result<(NodeId, Trust)> {
        let added = self.record_peer(address, now)?;
        // A pasted address is not an approval, but it *is* the difference
        // between a stranger the listener refuses outright and a node it will
        // exchange announcements with. See `Trust::may_contact`.
        self.refresh_consent();
        self.persist()?;
        Ok(added)
    }

    /// Add a peer that another peer mentioned, recording who mentioned it.
    ///
    /// Still not a decision: the new peer lands at [`Trust::Known`] like any
    /// other paste. `via` only draws the graph's observed edge, so an operator
    /// looking at a node they did not add by hand can see where it came from.
    pub fn add_observed_peer(
        &mut self,
        address: &str,
        via: NodeId,
        now: DateTime<Utc>,
    ) -> Result<(NodeId, Trust)> {
        let (id, trust) = self.record_peer(address, now)?;
        self.store.record_observed_via(&id, via);
        self.refresh_consent();
        // Persisted once, after both halves: a file holding the peer without
        // the edge it was learned through is a graph that has forgotten where
        // a node came from.
        self.persist()?;
        Ok((id, trust))
    }

    /// The in-memory half of adding a peer, without the write. Split out so
    /// [`Mesh::add_observed_peer`] can record its edge before the one write
    /// both paths share.
    fn record_peer(&mut self, address: &str, now: DateTime<Utc>) -> Result<(NodeId, Trust)> {
        let node = Node::from_address(address)?;
        if node.id == self.local_id() {
            return Err(anyhow!(
                "that address is this node ({}); a node cannot be its own peer",
                self.local_id().short()
            ));
        }
        let id = node.id;
        let trust = self.store.add(node, now);
        Ok((id, trust))
    }

    /// Record a trust decision, and make it stick.
    ///
    /// Three things that must not come apart, in one call. The decision is
    /// recorded, any move away from [`Trust::Trusted`] revokes through the
    /// transport, and the store is written to disk. The plan's words are that
    /// revocation "must actually drop live subscriptions", and a decision that
    /// is not on disk when the process exits has revoked nothing either: the
    /// peer is contactable again on the next run and the operator is never
    /// told. A caller that has to remember a second step is a caller that will
    /// forget it, so there is no second step.
    ///
    /// Both the revocation and the write are attempted even when the other
    /// fails: a transport error must not leave the decision unwritten, and a
    /// disk error must not leave the peer's stream running. Either failure is
    /// reported, and the in-memory decision stands regardless.
    pub async fn set_trust(&mut self, id: &NodeId, trust: Trust) -> Result<()> {
        self.store.record_trust(id, trust)?;
        // Before the revocation, not after: `revoke` severs what is *live*,
        // and this is what stops the peer opening a new stream a microsecond
        // later. A downgrade that only severed would be a revocation with a
        // race in it.
        self.refresh_consent();
        let local = self.local_id();
        let revoked = if trust.may_send_work() {
            Ok(())
        } else {
            self.transport.revoke(&local, id).await
        };
        let persisted = self.persist();
        revoked?;
        persisted?;
        Ok(())
    }

    /// Drop a peer's record entirely, along with anything live for it.
    ///
    /// `Ok(false)` when there was no such peer, so a caller can say "no peer
    /// matched" rather than reporting a removal that did not happen.
    ///
    /// The revocation is not optional even though the record is going away.
    /// A trusted peer can have a live subscription, and dropping only the row
    /// would leave that stream running against a node this machine no longer
    /// holds any decision about: the events would keep arriving and nothing
    /// would be able to say whose they were. Both halves are attempted even
    /// when the other fails, exactly as in [`Mesh::set_trust`], because a
    /// transport error must not leave the record on disk and a disk error must
    /// not leave the stream running.
    ///
    /// Forgetting is **not** blocking, and the difference matters at exactly
    /// one peer: a forgotten address pasted in again lands at [`Trust::Known`]
    /// like any other paste, so forgetting a [`Trust::Blocked`] peer discards
    /// the decision that was keeping it out. This method cannot tell the two
    /// intentions apart, so the surface that calls it says so out loud.
    pub async fn forget(&mut self, id: &NodeId) -> Result<bool> {
        if !self.store.forget(id) {
            return Ok(false);
        }
        self.refresh_consent();
        let local = self.local_id();
        let revoked = self.transport.revoke(&local, id).await;
        let persisted = self.persist();
        revoked?;
        persisted?;
        Ok(true)
    }

    /// Fetch a peer's announcement and fold it into the store, marking the
    /// peer seen at `now`.
    ///
    /// The whole record, not only the capability: a peer's name is the label
    /// the explorer draws, and this is the only path it has. An address is
    /// pasted ([`Node::from_address`], which carries no name) and a name is
    /// announced, so a mesh that fetched capabilities alone would render every
    /// peer as its own address forever and [`Graph::disambiguate_node_labels`]
    /// would be an answer to a question nothing could ask.
    ///
    /// Refuses a blocked peer: a blocked node is not contacted at all. The
    /// capability is re-normalised on arrival even though the transport is
    /// supposed to have done it, because "the transport is supposed to" is how
    /// unsanitised input gets in when a second transport appears. For the same
    /// reason the answer's id is checked against the peer that was asked
    /// about: a transport that hands back somebody else's announcement would
    /// otherwise rename this peer with another node's claim, and the name is
    /// what a human reads to tell two peers apart.
    pub async fn refresh(&mut self, id: &NodeId, now: DateTime<Utc>) -> Result<Capability> {
        let trust = self
            .store
            .trust_of(id)
            .ok_or_else(|| anyhow!("no peer {} in the store", id.short()))?;
        if !trust.may_contact() {
            return Err(anyhow!(
                "peer {} is blocked; wizard does not contact blocked peers",
                id.short()
            ));
        }
        let mut announced = self.transport.announcement_of(id).await?;
        if announced.id != *id {
            return Err(anyhow!(
                "asked the transport about peer {} and it answered for {}; \
                 a node's announcement is its own or it is nobody's",
                id.short(),
                announced.id.short()
            ));
        }
        announced.caps = announced.caps.normalised();
        let caps = announced.caps.clone();
        self.store.record_announcement(&announced)?;
        self.store.mark_seen(id, now);
        Ok(caps)
    }

    /// Subscribe to a peer's session event stream.
    ///
    /// Trusted peers only. A stream from a merely-known node is an inbound
    /// channel nobody approved, and the events on it would end up on a screen
    /// beside the ones that were.
    ///
    /// One subscription per *node*, carrying every session that node is
    /// running; a watcher that wants one session filters on
    /// [`PeerEvent::session`]. Not one subscription per session, because a
    /// session id is peer-supplied text: subscribing by name would mean asking
    /// a peer to route by a string it chose, and "the session that is called
    /// `main` on that machine right now" is not something this side can pin.
    /// The frame carries the id so a watcher can demux, and
    /// [`Graph::add_session`] draws the ones it has actually seen.
    ///
    /// The decision recorded here is this machine's, about whether it will
    /// *take* a stream. The other half of the question, whether a peer may
    /// watch *this* node, belongs to the peer's own `Mesh` and has nothing to
    /// attach to in this release: see [`Transport`]'s docs, which is where a
    /// network implementation will read it.
    pub async fn subscribe(&mut self, id: &NodeId) -> Result<Subscription> {
        let trust = self
            .store
            .trust_of(id)
            .ok_or_else(|| anyhow!("no peer {} in the store", id.short()))?;
        if !trust.may_send_work() {
            return Err(anyhow!(
                "peer {} is {}, not trusted; trust it first if you want its session stream",
                id.short(),
                trust.label()
            ));
        }
        let local = self.local_id();
        self.transport.subscribe(&local, id).await
    }

    /// Fan one event from a session running *here* out to the peers watching
    /// this node. Returns how many subscriptions took it.
    ///
    /// `&self`, deliberately, where every other mutation here takes
    /// `&mut self`. This is the one mesh operation a live turn calls, several
    /// times a second, and a surface holds its [`Mesh`] behind whatever lock it
    /// uses: needing the exclusive reference to publish would queue a peer's
    /// rendering behind an operator's trust decisions, in a call that has no
    /// business touching them.
    ///
    /// Publishing consults no trust state, because there is none left to
    /// consult by the time an event gets here. A stream exists only because
    /// [`Mesh::subscribe`] approved it, and it stops existing the moment
    /// [`Mesh::set_trust`] moves away from [`Trust::Trusted`]. A second check
    /// here would be the second, differently wrong copy of the policy.
    pub fn publish(&self, session: &str, at: DateTime<Utc>, what: PeerEventKind) -> usize {
        self.transport
            .publish(&PeerEvent::new(self.local_id(), session, at, what))
    }

    /// Publish one of a local session's agent events to whoever is watching.
    ///
    /// The tee an interactive surface hangs off its own event stream: the same
    /// [`AgentEvent`] it is about to render, on its way to a peer that renders
    /// it the same way. `0` when nothing is watching, and also `0` when the
    /// event does not cross the mesh at all ([`PeerTurn::sanitize`]): the
    /// caller has nothing useful to do about the difference, and a surface that
    /// had to handle "this one is a request, not a report" at every call site
    /// would eventually stop handling it.
    pub fn publish_turn(&self, session: &str, at: DateTime<Utc>, event: &AgentEvent) -> usize {
        match PeerTurn::sanitize(event) {
            Some(turn) => self.publish(session, at, PeerEventKind::Turn(turn)),
            None => 0,
        }
    }

    /// The inbound gate: may this peer have one unit of work?
    ///
    /// Checks the recorded decision first, then the peer's own budget. Counts
    /// the request when it allows it, so calling this is the admission, not a
    /// preview of one.
    pub fn admit(&mut self, id: &NodeId, now: DateTime<Utc>) -> Admission {
        let Some(trust) = self.store.trust_of(id) else {
            return Admission::Refused(format!(
                "node {} is not a peer of this machine; add its address first",
                id.short()
            ));
        };
        if !trust.may_accept_work() {
            return Admission::Refused(format!(
                "peer {} is {}, not trusted; wizard runs work only for trusted peers",
                id.short(),
                trust.label()
            ));
        }
        match self.store.try_admit(id, now) {
            Ok(()) => Admission::Allowed,
            Err(exceeded) => Admission::Refused(format!("peer {}: {exceeded}", id.short())),
        }
    }

    /// Attribute API spend to a peer's daily budget.
    pub fn charge(&mut self, id: &NodeId, usd: f64, now: DateTime<Utc>) {
        self.store.charge(id, usd, now);
    }

    /// Count one delegation to a peer; the weight on the graph's delegation
    /// edge.
    pub fn record_delegation(&mut self, id: &NodeId) {
        self.store.record_delegation(id);
    }

    /// Note that a peer was heard from.
    pub fn mark_seen(&mut self, id: &NodeId, now: DateTime<Utc>) {
        self.store.mark_seen(id, now);
    }

    /// The graph to draw, from cached state, at `now`.
    pub fn graph(&self, now: DateTime<Utc>) -> Graph {
        Graph::build(&self.local_node(), &self.store, now)
    }
}

// ---------------------------------------------------------------------------
// The graph the explorer draws
// ---------------------------------------------------------------------------

/// One endpoint in the explorer's graph.
///
/// Capability vertices are shared: two peers that both offer `qwen3.6:27b`
/// point at one vertex, which is what turns a list of peers into a graph worth
/// looking at.
///
/// Session vertices are **not**, and the asymmetry is the point. A capability
/// with the same name on two nodes really is the same capability, so collapsing
/// it says something true. A session with the same name on two nodes is two
/// sessions: the name is peer-supplied text chosen by the far end, `main` is
/// what everybody calls their session, and a shared vertex would draw two
/// machines collaborating on one session that does not exist. So a session is
/// keyed by the node it runs on as well as by its name.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Vertex {
    /// A mesh node (this one, or a peer).
    Node(NodeId),
    /// A named capability: a model, tool, skill or subagent.
    Capability(CapabilityKind, String),
    /// A session running on one named node.
    Session(NodeId, String),
}

/// What kind of relationship an edge records. The five the plan names.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum EdgeKind {
    /// This machine has the far end in its peer store.
    Peer,
    /// The far end was learned about through the near end, rather than pasted
    /// in by hand.
    Observed,
    /// Work has been delegated along this edge. [`Edge::weight`] is how often.
    Delegation,
    /// A node is running a session.
    Session,
    /// A node advertises a capability.
    Capability,
}

impl EdgeKind {
    /// Lower-case label, for the explorer's legend.
    pub fn label(self) -> &'static str {
        match self {
            EdgeKind::Peer => "peer",
            EdgeKind::Observed => "observed",
            EdgeKind::Delegation => "delegation",
            EdgeKind::Session => "session",
            EdgeKind::Capability => "capability",
        }
    }
}

/// One edge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Edge {
    pub from: Vertex,
    pub to: Vertex,
    pub kind: EdgeKind,
    /// How much of the relationship there is: the delegation count for
    /// [`EdgeKind::Delegation`], `1` for the kinds that either exist or do
    /// not.
    pub weight: u32,
}

/// What a vertex is, for the renderer that has to decide how to draw it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VertexDetail {
    /// This machine.
    Local,
    /// A peer, with everything the explorer needs to colour it honestly.
    Peer {
        trust: Trust,
        presence: Presence,
        address: String,
    },
    Capability(CapabilityKind),
    Session,
}

/// A vertex with everything needed to draw it, so the renderer never has to
/// go back to the store mid-frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GraphNode {
    pub vertex: Vertex,
    /// The text to draw. For a node vertex this is unique across the graph:
    /// see [`Graph::disambiguate_node_labels`], which is what stops two peers
    /// from rendering as the same machine.
    pub label: String,
    pub detail: VertexDetail,
}

/// A snapshot of the mesh as a graph.
///
/// Built from cached state alone: no network call, no clock read of its own
/// (`now` is passed in), so it renders identically with the network down and
/// can be regenerated deterministically in a test.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Graph {
    pub nodes: Vec<GraphNode>,
    pub edges: Vec<Edge>,
}

impl Graph {
    /// Build the graph for `local` plus everything in `store`, as of `now`.
    pub fn build(local: &Node, store: &PeerStore, now: DateTime<Utc>) -> Self {
        let mut graph = Graph::default();
        let mut seen: BTreeSet<Vertex> = BTreeSet::new();

        graph.push_node(
            &mut seen,
            Vertex::Node(local.id),
            local.label(),
            VertexDetail::Local,
        );
        graph.add_capability_edges(&mut seen, local.id, &local.caps);

        for peer in store.iter() {
            if peer.id() == local.id {
                // A record claiming to be this node is not a peer record. See
                // [`Mesh::new`], which drops it from the store outright; this
                // is the same refusal for a `Graph` built from a store that
                // never passed through a `Mesh`. Drawing it would silently
                // discard the local vertex's detail (the vertex is already
                // there) and leave a self-loop behind.
                continue;
            }
            let vertex = Vertex::Node(peer.id());
            graph.push_node(
                &mut seen,
                vertex.clone(),
                peer.node.label(),
                VertexDetail::Peer {
                    trust: peer.trust,
                    presence: peer.presence(now),
                    address: peer.node.addr(),
                },
            );
            graph.edges.push(Edge {
                from: Vertex::Node(local.id),
                to: vertex.clone(),
                kind: EdgeKind::Peer,
                weight: 1,
            });
            if let Some(via) = peer.observed_via {
                // Only when the referrer is itself on the graph: an edge to a
                // vertex nobody drew is a dangling line.
                if via == local.id || store.get(&via).is_some() {
                    graph.edges.push(Edge {
                        from: Vertex::Node(via),
                        to: vertex.clone(),
                        kind: EdgeKind::Observed,
                        weight: 1,
                    });
                }
            }
            if peer.delegations > 0 {
                graph.edges.push(Edge {
                    from: Vertex::Node(local.id),
                    to: vertex,
                    kind: EdgeKind::Delegation,
                    weight: peer.delegations,
                });
            }
            graph.add_capability_edges(&mut seen, peer.id(), &peer.node.caps);
        }
        graph.disambiguate_node_labels();
        graph
    }

    /// Record that `node` is running `session`. The session stream is live
    /// state, so it is added by whoever is watching it rather than read out of
    /// the store.
    ///
    /// Does nothing when `node` is not already on the graph. Same rule as the
    /// dropped `observed` edge in [`Graph::build`]: an edge to a vertex nobody
    /// drew is a dangling line, and this is the path that reaches it, because
    /// a [`Subscription`] holds buffered events that outlive the peer being
    /// blocked, forgotten, or reloaded out of the store.
    pub fn add_session(&mut self, node: NodeId, session: &PeerText) {
        if session.is_empty() {
            return;
        }
        let from = Vertex::Node(node);
        if !self.nodes.iter().any(|drawn| drawn.vertex == from) {
            return;
        }
        let vertex = Vertex::Session(node, session.as_str().to_string());
        let mut seen: BTreeSet<Vertex> = self.nodes.iter().map(|n| n.vertex.clone()).collect();
        self.push_node(
            &mut seen,
            vertex.clone(),
            session.as_str().to_string(),
            VertexDetail::Session,
        );
        let edge = Edge {
            from,
            to: vertex,
            kind: EdgeKind::Session,
            weight: 1,
        };
        if !self.edges.contains(&edge) {
            self.edges.push(edge);
        }
    }

    /// Every edge of one kind.
    pub fn edges_of(&self, kind: EdgeKind) -> impl Iterator<Item = &Edge> {
        self.edges.iter().filter(move |edge| edge.kind == kind)
    }

    /// Give every node vertex a label no other node vertex has.
    ///
    /// A node's label is the name it chose for itself, and a peer chooses its
    /// own name. Two peers can therefore be called the same thing: by accident
    /// (two machines both called `workshop`), or on purpose, which is the
    /// whole of the impersonation attack this graph exists to defeat. The
    /// sanitiser closes the invisible-character half of it, but nothing
    /// string-shaped closes the rest: `аlice` in Cyrillic renders like `alice`
    /// in Latin and always will.
    ///
    /// So when two node labels collide, both grow their address. The plan's
    /// words are that a graph that lies about who is online is worse than a
    /// plain one that does not; a graph that lies about *who is who* is worse
    /// still, and drawing two dots a human cannot tell apart is that lie.
    ///
    /// The **full** address, not [`NodeId::short`], because short is a prefix
    /// and its own documentation says prefixes collide. This is the one place
    /// in the renderer where the label has to be the identity, so it is the
    /// one place that cannot use an abbreviation of it.
    fn disambiguate_node_labels(&mut self) {
        let mut counts: BTreeMap<String, usize> = BTreeMap::new();
        for node in &self.nodes {
            if matches!(node.vertex, Vertex::Node(_)) {
                *counts.entry(node.label.clone()).or_default() += 1;
            }
        }
        for node in &mut self.nodes {
            if let Vertex::Node(id) = node.vertex {
                // A node that announced no name is labelled by its own
                // identity already, and two identities cannot collide. Growing
                // that label would append the whole address to a prefix of
                // itself, which reads as two machines rather than one.
                if node.label == id.short() {
                    continue;
                }
                if counts.get(&node.label).copied().unwrap_or(0) > 1 {
                    node.label = format!("{} ({})", node.label, id.address());
                }
            }
        }
    }

    fn push_node(
        &mut self,
        seen: &mut BTreeSet<Vertex>,
        vertex: Vertex,
        label: String,
        detail: VertexDetail,
    ) {
        if seen.insert(vertex.clone()) {
            self.nodes.push(GraphNode {
                vertex,
                label,
                detail,
            });
        }
    }

    fn add_capability_edges(
        &mut self,
        seen: &mut BTreeSet<Vertex>,
        node: NodeId,
        caps: &Capability,
    ) {
        for kind in CapabilityKind::ALL {
            for entry in caps.entries(kind) {
                let name = entry.as_str().to_string();
                let vertex = Vertex::Capability(kind, name.clone());
                self.push_node(seen, vertex.clone(), name, VertexDetail::Capability(kind));
                self.edges.push(Edge {
                    from: Vertex::Node(node),
                    to: vertex,
                    kind: EdgeKind::Capability,
                    weight: 1,
                });
            }
        }
    }
}

#[cfg(test)]
mod tests {

    /// A run of blank lines is a run even when the blank lines contain spaces.
    ///
    /// The doc on `sanitize_body` says "a peer cannot scroll a watcher's screen
    /// with nothing but line breaks". The counter used to reset on any
    /// non-newline, so one space between them defeated the cap entirely and
    /// `" \n"` repeated produced as many lines as the character budget allowed
    /// — which is the exact thing the sentence promises cannot happen.
    #[test]
    fn a_peer_cannot_scroll_a_screen_with_padded_blank_lines() {
        let flood = " \n".repeat(200);
        let cleaned = sanitize_body(&flood, 4096);
        let lines = cleaned.matches('\n').count();
        assert!(
            lines <= 2,
            "{lines} newlines survived a padded flood: {cleaned:?}"
        );

        // Tabs are whitespace too, and they map to a space before this runs.
        let tabbed = "\t\n".repeat(200);
        assert!(sanitize_body(&tabbed, 4096).matches('\n').count() <= 2);

        // Real content is untouched: a paragraph break still survives, and text
        // on either side of it still separates.
        let prose = "first paragraph\n\nsecond paragraph";
        assert_eq!(sanitize_body(prose, 4096), prose);
    }
    use super::*;

    fn at(seconds: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(1_800_000_000 + seconds, 0).expect("timestamp")
    }

    /// A mesh over a loopback transport, handing back the transport so a test
    /// can announce peers onto it and publish their events.
    fn mesh(seed: u8) -> (Mesh, Arc<LoopbackTransport>) {
        let transport = Arc::new(LoopbackTransport::new());
        let mesh = Mesh::new(
            Identity::from_seed([seed; 32]),
            PeerStore::ephemeral(),
            transport.clone(),
        );
        (mesh, transport)
    }

    /// [`Subscription::recv`] with a deadline.
    ///
    /// The revocation tests assert that a stream has *ended*. Awaiting one
    /// that has not ended parks the test forever, and a hung test reads in CI
    /// as an infrastructure problem rather than as the revocation bug it
    /// actually is. Five seconds is far longer than an in-process channel
    /// needs and short enough to be a test failure.
    async fn recv_within(subscription: &mut Subscription, what: &str) -> Option<PeerEvent> {
        tokio::time::timeout(std::time::Duration::from_secs(5), subscription.recv())
            .await
            .unwrap_or_else(|_| panic!("{what}: the subscription was still open after 5s"))
    }

    /// The encoded form of an event, which is the only equality a wire type
    /// has: [`AgentEvent`] is deliberately not `PartialEq`, and it should not
    /// grow the impl to please a test.
    fn encoded(event: &PeerEvent) -> serde_json::Value {
        serde_json::to_value(event).expect("a peer event encodes")
    }

    /// Announce a peer identity onto `transport` and return its address.
    async fn announce(transport: &LoopbackTransport, seed: u8, caps: Capability) -> String {
        announce_as(transport, seed, "peer", caps).await
    }

    /// [`announce`] for a peer that calls itself something in particular. The
    /// name is the far end's own choice, which is the whole reason it is
    /// [`PeerText`] and the whole reason the graph cannot take it on trust.
    async fn announce_as(
        transport: &LoopbackTransport,
        seed: u8,
        name: &str,
        caps: Capability,
    ) -> String {
        let identity = Identity::from_seed([seed; 32]);
        transport
            .announce(&identity.announce(name, caps))
            .await
            .expect("announce");
        identity.id().address()
    }

    /// One representative of every class of code point that a renderer is
    /// entitled to draw as nothing, with the category that gets it there.
    ///
    /// The categories matter more than the characters: a filter written
    /// against a remembered list of "the zero-width ones" catches the first
    /// four rows and nothing else. `char::is_control` is `Cc` alone and
    /// `char::is_whitespace` is `White_Space`, so *every* row here used to
    /// walk through [`PeerText::sanitize`] untouched.
    const INVISIBLE_CLASSES: &[(&str, char)] = &[
        ("Cf zero-width space", '\u{200b}'),
        ("Cf zero-width non-joiner", '\u{200c}'),
        ("Cf zero-width joiner", '\u{200d}'),
        ("Cf word joiner", '\u{2060}'),
        ("Cf invisible times", '\u{2062}'),
        ("Cf byte-order mark", '\u{feff}'),
        ("Cf soft hyphen", '\u{00ad}'),
        ("Cf mongolian vowel separator", '\u{180e}'),
        ("Cf arabic number sign", '\u{0600}'),
        ("Cf interlinear annotation anchor", '\u{fff9}'),
        ("Cf musical symbol begin beam", '\u{1d173}'),
        ("Cf bidi: left-to-right mark", '\u{200e}'),
        ("Cf bidi: arabic letter mark", '\u{061c}'),
        ("Cf bidi: right-to-left override", '\u{202e}'),
        ("Cf bidi: first strong isolate", '\u{2066}'),
        ("Cf tag block: language tag", '\u{e0001}'),
        ("Cf tag block: tag latin small letter a", '\u{e0061}'),
        ("Cn tag block: reserved", '\u{e0080}'),
        (
            "Mn default-ignorable: combining grapheme joiner",
            '\u{034f}',
        ),
        ("Mn default-ignorable: variation selector 16", '\u{fe0f}'),
        ("Mn default-ignorable: variation selector 17", '\u{e0100}'),
        (
            "Mn default-ignorable: mongolian free variation selector",
            '\u{180b}',
        ),
        ("Mn default-ignorable: khmer vowel inherent aq", '\u{17b4}'),
        ("Lo default-ignorable: hangul filler", '\u{3164}'),
        ("Lo default-ignorable: hangul choseong filler", '\u{115f}'),
        ("Lo default-ignorable: halfwidth hangul filler", '\u{ffa0}'),
        ("Cn default-ignorable: reserved", '\u{2065}'),
        ("Cn default-ignorable: reserved", '\u{fff0}'),
        ("Co private use: basic multilingual plane", '\u{e000}'),
        ("Co private use: plane 15", '\u{f0000}'),
        ("Co private use: plane 16", '\u{100000}'),
        ("Cn noncharacter: arabic presentation forms", '\u{fdd0}'),
        ("Cn noncharacter: end of plane", '\u{ffff}'),
    ];

    #[test]
    fn peer_text_neutralises_everything_it_promises_to() {
        // Escape sequences and control characters.
        let text = PeerText::sanitize("\u{1b}[2Jgpt\u{0007}-5\tturbo\r\n");
        assert_eq!(text.as_str(), "[2Jgpt -5 turbo");
        assert!(!text.as_str().contains('\u{1b}'));
        // Bidi overrides: what renders must be what is stored. The whole
        // trojan-source set, including U+061C, which is the one a hand-written
        // list of "the bidi characters" leaves out.
        let text = PeerText::sanitize("innocent\u{202e}suoiciffm");
        assert_eq!(text.as_str(), "innocentsuoiciffm", "{text:?}");
        for bidi in [
            '\u{061c}', '\u{200e}', '\u{200f}', '\u{202a}', '\u{202b}', '\u{202c}', '\u{202d}',
            '\u{202e}', '\u{2066}', '\u{2067}', '\u{2068}', '\u{2069}',
        ] {
            assert!(is_invisible(bidi), "U+{:04X}", bidi as u32);
        }
        // Length, on a character boundary, with the elision visible.
        let text = PeerText::sanitize(&"é".repeat(PeerText::MAX_CHARS * 2));
        assert_eq!(text.as_str().chars().count(), PeerText::MAX_CHARS);
        assert!(text.as_str().ends_with('…'), "{text:?}");
        // Nothing but formatting is nothing.
        assert!(PeerText::sanitize("  \u{202e}\u{0000} ").is_empty());
        assert!(PeerText::default().is_empty());
    }

    #[test]
    fn every_class_of_invisible_character_is_stripped_not_spaced() {
        for (class, invisible) in INVISIBLE_CLASSES {
            // Gone, not turned into a space: `read\u{200b}file` already reads
            // as `readfile` to a human, so spacing it would invent a word
            // break nobody saw and hand a peer a way to forge one.
            assert_eq!(
                PeerText::sanitize(&format!("read{invisible}file")).as_str(),
                "readfile",
                "{class} (U+{:04X})",
                *invisible as u32
            );
            // A name that is nothing but invisible characters is nothing, so
            // `Node::label` falls back to the address instead of drawing an
            // unlabelled dot.
            assert!(
                PeerText::sanitize(&invisible.to_string().repeat(8)).is_empty(),
                "{class} (U+{:04X})",
                *invisible as u32
            );
            // And it cannot be smuggled past the constructor by decoding a
            // record instead of building one.
            let json = serde_json::to_string(&format!("wiz{invisible}ard")).expect("encode");
            let decoded: PeerText = serde_json::from_str(&json).expect("decode");
            assert_eq!(
                decoded.as_str(),
                "wizard",
                "{class} (U+{:04X})",
                *invisible as u32
            );
        }
        // Visible text is left alone, invisible or not: a sanitiser that eats
        // ordinary names is a sanitiser somebody turns off. Non-Latin scripts,
        // emoji, punctuation and combining accents all survive.
        for kept in [
            "ラボ",
            "café",
            "работа",
            "gpt-5.3-codex",
            "wizard ⚙",
            "e\u{0301}",
        ] {
            assert_eq!(PeerText::sanitize(kept).as_str(), kept, "{kept:?}");
        }
    }

    #[tokio::test]
    async fn two_peers_cannot_hold_visually_identical_names() {
        // The acceptance test. A peer chooses its own name, so a peer can
        // choose *another peer's* name plus something that draws no pixels,
        // and the explorer would then show a human two entries it cannot tell
        // apart while claiming to show them an unambiguous trust state.
        //
        // Two halves, because neither closes it alone. The sanitiser deletes
        // everything invisible, so the two names collapse into one value
        // rather than staying two values that render alike. The graph then
        // refuses to draw one label twice, so the collision that is left (and
        // there is always one left: two machines may honestly share a name)
        // is resolved by the only thing that cannot be forged, the address.
        for (class, invisible) in INVISIBLE_CLASSES {
            let where_ = format!("{class} (U+{:04X})", *invisible as u32);
            let impostor = format!("alice{invisible}");
            assert_eq!(
                PeerText::sanitize(&impostor),
                PeerText::sanitize("alice"),
                "{where_}"
            );
            assert_eq!(
                PeerText::sanitize(&format!("al{invisible}ice")),
                PeerText::sanitize("alice"),
                "{where_}"
            );
            // Same defeat, one level down: a capability list de-duplicates by
            // `PeerText`, so this is what stops one peer from filling all 64
            // slots with entries that render as the same model name.
            let caps = Capability::advertise(
                &["qwen3.6:27b", &format!("qwen3.6:27b{invisible}")],
                &[],
                &[],
                &[],
                false,
            );
            assert_eq!(caps.models.len(), 1, "{where_}");

            let (mut mesh, transport) = mesh(21);
            mesh.set_local("alice", Capability::none());
            let honest = announce_as(&transport, 22, "alice", Capability::none()).await;
            let forged = announce_as(&transport, 23, &impostor, Capability::none()).await;
            let (honest_id, _) = mesh.add_peer(&honest, at(0)).expect("add");
            let (forged_id, _) = mesh.add_peer(&forged, at(0)).expect("add");
            // A paste is an address and nothing else; the name is a claim the
            // far end makes, and this is where it arrives.
            mesh.refresh(&honest_id, at(0)).await.expect("refresh");
            mesh.refresh(&forged_id, at(0)).await.expect("refresh");
            mesh.set_trust(&honest_id, Trust::Trusted)
                .await
                .expect("trust");
            mesh.set_trust(&forged_id, Trust::Trusted)
                .await
                .expect("trust");

            // Both are trusted, and both are called `alice`: nothing here
            // pretends a peer can be stopped from choosing a name.
            assert_eq!(
                mesh.store().get(&forged_id).expect("peer").node.name,
                mesh.store().get(&honest_id).expect("peer").node.name,
                "{where_}"
            );

            // What is stopped is two trusted entries a human cannot tell
            // apart. Three node vertices (this machine is called `alice` too),
            // three labels, all different, each carrying its own address in
            // full rather than a prefix of one.
            let graph = mesh.graph(at(1));
            let labels: Vec<&str> = graph
                .nodes
                .iter()
                .filter(|node| matches!(node.vertex, Vertex::Node(_)))
                .map(|node| node.label.as_str())
                .collect();
            assert_eq!(labels.len(), 3, "{where_}");
            let unique: BTreeSet<&str> = labels.iter().copied().collect();
            assert_eq!(unique.len(), 3, "{where_}: {labels:?}");
            for (id, label) in [
                (mesh.local_id(), "local"),
                (honest_id, "honest"),
                (forged_id, "forged"),
            ] {
                let drawn = graph
                    .nodes
                    .iter()
                    .find(|node| node.vertex == Vertex::Node(id))
                    .map(|node| node.label.clone())
                    .unwrap_or_else(|| panic!("{where_}: no {label} vertex"));
                assert!(
                    drawn.contains(&id.address()),
                    "{where_}: the {label} label must carry the whole address, got {drawn:?}"
                );
                assert!(drawn.starts_with("alice ("), "{where_}: {drawn:?}");
            }
        }
    }

    #[tokio::test]
    async fn a_label_grows_an_address_only_when_it_would_otherwise_repeat() {
        // The other side of the disambiguation: it must not fire on every
        // node, or the explorer draws 50 addresses and no names, and the
        // operator stops reading any of them.
        let (mut mesh, transport) = mesh(24);
        mesh.set_local("here", Capability::none());
        let alone = announce_as(&transport, 25, "workshop", Capability::none()).await;
        let (alone_id, _) = mesh.add_peer(&alone, at(0)).expect("add");
        mesh.refresh(&alone_id, at(0)).await.expect("refresh");
        let label = |graph: &Graph, id: NodeId| {
            graph
                .nodes
                .iter()
                .find(|node| node.vertex == Vertex::Node(id))
                .map(|node| node.label.clone())
                .expect("vertex")
        };
        assert_eq!(label(&mesh.graph(at(0)), alone_id), "workshop");

        // A peer that announced no name at all is labelled by its address
        // already, and two of those cannot collide: the label *is* the
        // identity, so it is left alone rather than having the address
        // appended to a prefix of itself.
        let nameless = announce_as(&transport, 26, "", Capability::none()).await;
        let (nameless_id, _) = mesh.add_peer(&nameless, at(0)).expect("add");
        mesh.refresh(&nameless_id, at(0)).await.expect("refresh");
        assert_eq!(label(&mesh.graph(at(0)), nameless_id), nameless_id.short());

        // …and the moment a second `workshop` shows up, both of them grow.
        let twin = announce_as(&transport, 27, "workshop", Capability::none()).await;
        let (twin_id, _) = mesh.add_peer(&twin, at(0)).expect("add");
        mesh.refresh(&twin_id, at(0)).await.expect("refresh");
        let graph = mesh.graph(at(0));
        assert_eq!(
            label(&graph, alone_id),
            format!("workshop ({alone})"),
            "an honest name collision is disambiguated too: nothing here \
             assumes a duplicate name is an attack"
        );
        assert_eq!(label(&graph, twin_id), format!("workshop ({twin})"));
        assert_eq!(label(&graph, nameless_id), nameless_id.short());
        assert_eq!(label(&graph, mesh.local_id()), "here");
    }

    #[test]
    fn peer_text_is_sanitised_when_it_is_decoded_too() {
        // The bypass that matters: building the struct by decoding a record
        // rather than by calling the constructor.
        let decoded: PeerText = serde_json::from_str("\"\\u001b[31mred\\u0007\"").expect("decode");
        assert_eq!(decoded.as_str(), "[31mred");
        // And it round-trips as the cleaned text, so a re-save cannot restore
        // the original bytes.
        let json = serde_json::to_string(&decoded).expect("encode");
        assert_eq!(json, "\"[31mred\"");
    }

    #[tokio::test]
    async fn a_pasted_address_is_known_not_trusted_and_cannot_be_this_node() {
        let (mut mesh, transport) = mesh(1);
        let address = announce(&transport, 2, Capability::none()).await;

        let (id, trust) = mesh.add_peer(&address, at(0)).expect("add");
        assert_eq!(trust, Trust::Known);
        assert_eq!(mesh.store().trust_of(&id), Some(Trust::Known));
        assert_eq!(mesh.store().presence(&id, at(0)), Some(Presence::Unseen));

        // Work is refused in both directions until a human says otherwise.
        assert!(!mesh.admit(&id, at(0)).is_allowed());
        assert!(mesh.subscribe(&id).await.is_err());

        // Adding this node to its own peer list would put a self-loop in the
        // graph and let the local node be treated as a peer.
        let err = mesh
            .add_peer(&mesh.local_id().address(), at(0))
            .expect_err("self");
        assert!(format!("{err:#}").contains("its own peer"), "{err:#}");
        // A pasted typo is refused rather than silently added.
        assert!(mesh.add_peer("wiz1-not-an-address", at(0)).is_err());
        assert_eq!(mesh.store().len(), 1);
    }

    #[tokio::test]
    async fn admission_needs_trust_then_budget() {
        let (mut mesh, transport) = mesh(3);
        let address = announce(&transport, 4, Capability::none()).await;
        let (id, _) = mesh.add_peer(&address, at(0)).expect("add");

        let refusal = mesh.admit(&id, at(0));
        assert!(
            refusal.reason().expect("refused").contains("not trusted"),
            "{refusal:?}"
        );

        mesh.set_trust(&id, Trust::Trusted).await.expect("trust");
        for i in 0..Limits::default().requests_per_minute {
            assert!(
                mesh.admit(&id, at(i as i64)).is_allowed(),
                "request {i} is inside the default budget"
            );
        }
        let refusal = mesh.admit(&id, at(1));
        assert!(
            refusal.reason().expect("refused").contains("rate limit"),
            "a trusted peer is still metered: {refusal:?}"
        );

        // Cost is metered too, and it is the limit that outlives the minute.
        mesh.charge(&id, 5.0, at(1));
        let refusal = mesh.admit(&id, at(300));
        assert!(
            refusal.reason().expect("refused").contains("cost limit"),
            "{refusal:?}"
        );

        // A node nobody added is refused by name, not by budget.
        let stranger = Identity::from_seed([9u8; 32]).id();
        let refusal = mesh.admit(&stranger, at(0));
        assert!(
            refusal.reason().expect("refused").contains("not a peer"),
            "{refusal:?}"
        );
    }

    #[tokio::test]
    async fn revoking_a_peer_drops_its_live_subscription() {
        // The acceptance test for "revocation must actually drop live
        // subscriptions": one call, and the stream is over.
        let (mut mesh, transport) = mesh(5);
        let address = announce(&transport, 6, Capability::none()).await;
        let (id, _) = mesh.add_peer(&address, at(0)).expect("add");
        mesh.set_trust(&id, Trust::Trusted).await.expect("trust");

        let mut subscription = mesh.subscribe(&id).await.expect("subscribe");
        assert_eq!(transport.subscriber_count(&id), 1);
        let event = PeerEvent::new(id, "s-1", at(1), PeerEventKind::SessionStarted);
        assert_eq!(transport.publish(&event), 1);
        let got = recv_within(&mut subscription, "before revoke")
            .await
            .expect("event");
        assert_eq!(encoded(&got), encoded(&event));

        mesh.set_trust(&id, Trust::Blocked).await.expect("revoke");

        assert!(
            recv_within(&mut subscription, "after revoke")
                .await
                .is_none(),
            "the stream ends on revocation, not at some later timeout"
        );
        assert!(subscription.is_closed());
        assert_eq!(transport.subscriber_count(&id), 0);
        assert_eq!(
            transport.publish(&PeerEvent::new(
                id,
                "s-1",
                at(2),
                PeerEventKind::SessionEnded
            )),
            0
        );
        // And the decision stuck: no new stream, no contact, no work.
        assert!(mesh.subscribe(&id).await.is_err());
        assert!(mesh.refresh(&id, at(3)).await.is_err());
        assert!(!mesh.admit(&id, at(3)).is_allowed());
        assert_eq!(mesh.store().trust_of(&id), Some(Trust::Blocked));
    }

    #[tokio::test]
    async fn downgrading_to_known_revokes_as_thoroughly_as_blocking() {
        // Trusted -> Known is a revocation too. It would be easy to only
        // handle the Blocked case and leave a stream running for a peer that
        // was merely un-trusted.
        let (mut mesh, transport) = mesh(7);
        let address = announce(&transport, 8, Capability::none()).await;
        let (id, _) = mesh.add_peer(&address, at(0)).expect("add");
        mesh.set_trust(&id, Trust::Trusted).await.expect("trust");
        let mut subscription = mesh.subscribe(&id).await.expect("subscribe");

        mesh.set_trust(&id, Trust::Known).await.expect("downgrade");
        assert!(
            recv_within(&mut subscription, "after downgrade")
                .await
                .is_none()
        );
        assert_eq!(transport.subscriber_count(&id), 0);
        assert!(!mesh.admit(&id, at(1)).is_allowed());

        // …and a downgrade is reversible, because it is a change of mind and
        // not a ban. Revocation drops what is *live*; a revocation that also
        // forgot the peer existed would leave `refresh` and `subscribe`
        // permanently broken for it, with no way back from inside the process
        // (nothing here re-announces a remote node) and no message saying why.
        mesh.set_trust(&id, Trust::Trusted).await.expect("re-trust");
        mesh.refresh(&id, at(2)).await.expect("reachable again");
        let mut resumed = mesh.subscribe(&id).await.expect("subscribe again");
        let event = PeerEvent::new(id, "s-2", at(3), PeerEventKind::SessionStarted);
        assert_eq!(transport.publish(&event), 1);
        let got = recv_within(&mut resumed, "after re-trust")
            .await
            .expect("event");
        assert_eq!(encoded(&got), encoded(&event));
        assert!(mesh.admit(&id, at(3)).is_allowed());
    }

    #[tokio::test]
    async fn a_trusted_peer_can_watch_a_session_and_sees_the_turn_it_ran() {
        // Tier 2's acceptance test. Two nodes on one loopback: one runs a
        // session, the other watches it and gets the same event sequence, as
        // the same type its own agent loop emits.
        use crate::agent::DoneReason;
        use crate::tools::ToolOutput;

        let transport = Arc::new(LoopbackTransport::new());
        let mut watcher = Mesh::new(
            Identity::from_seed([50u8; 32]),
            PeerStore::ephemeral(),
            transport.clone(),
        );
        let mut worker = Mesh::new(
            Identity::from_seed([51u8; 32]),
            PeerStore::ephemeral(),
            transport.clone(),
        );
        watcher.set_local("here", Capability::none());
        worker.set_local(
            "workshop",
            Capability::advertise(&["qwen3.6:27b"], &["read_file"], &[], &[], true),
        );
        worker.announce().await.expect("announce");

        let (worker_id, trust) = watcher
            .add_peer(&worker.local_id().address(), at(0))
            .expect("add");
        assert_eq!(trust, Trust::Known);
        // A merely-known node's stream is an inbound channel nobody approved.
        let refusal = watcher.subscribe(&worker_id).await.expect_err("known");
        assert!(
            format!("{refusal:#}").contains("not trusted"),
            "{refusal:#}"
        );

        watcher
            .set_trust(&worker_id, Trust::Trusted)
            .await
            .expect("trust");
        let mut watching = watcher.subscribe(&worker_id).await.expect("subscribe");

        // The worker runs a turn. This is the same stream its own surface is
        // rendering, teed onto the mesh.
        let turn = [
            AgentEvent::TextDelta("reading ".into()),
            AgentEvent::ToolStarted {
                name: "read_file".into(),
                args: serde_json::json!({ "path": "src/mesh/mod.rs" }),
            },
            AgentEvent::ToolFinished {
                name: "read_file".into(),
                output: ToolOutput::ok("//! The mesh data model"),
            },
            AgentEvent::StepCompleted { step: 1 },
            AgentEvent::Done {
                reason: DoneReason::Completed,
            },
        ];
        assert_eq!(
            worker.publish("session-7", at(1), PeerEventKind::SessionStarted),
            1
        );
        for (step, event) in turn.iter().enumerate() {
            assert_eq!(
                worker.publish_turn("session-7", at(2 + step as i64), event),
                1,
                "step {step} reached the watcher"
            );
        }
        assert_eq!(
            worker.publish("session-7", at(9), PeerEventKind::SessionEnded),
            1
        );

        // The watcher sees the lifecycle around the turn, and the turn itself
        // in order, attributed to the node that ran it.
        let started = recv_within(&mut watching, "the session starting")
            .await
            .expect("event");
        assert_eq!(started.from, worker_id);
        assert_eq!(started.session.as_str(), "session-7");
        assert!(matches!(started.what, PeerEventKind::SessionStarted));
        assert!(
            started.report().is_none(),
            "a lifecycle frame carries no agent event, and says so"
        );

        for expected in &turn {
            let got = recv_within(&mut watching, "a turn event")
                .await
                .expect("event");
            assert_eq!(got.from, worker_id);
            assert_eq!(got.session.as_str(), "session-7");
            let report = got.report().expect("a turn frame carries its report");
            assert_eq!(
                serde_json::to_value(report).expect("encode"),
                serde_json::to_value(expected).expect("encode"),
                "a peer's turn arrives as the same event the local loop emits, \
                 which is the whole of what makes it renderable"
            );
        }
        let ended = recv_within(&mut watching, "the session ending")
            .await
            .expect("event");
        assert!(matches!(ended.what, PeerEventKind::SessionEnded));
        assert_eq!(watching.dropped(), 0, "nothing was lost at this rate");

        // The explorer can draw what it watched, keyed to the node running it.
        let mut graph = watcher.graph(at(10));
        graph.add_session(worker_id, &started.session);
        assert_eq!(graph.edges_of(EdgeKind::Session).count(), 1);

        // The stream is one-way. Watching a peer does not open anything back:
        // nothing subscribed to the watcher, and the watcher published nothing.
        assert_eq!(transport.subscriber_count(&watcher.local_id()), 0);

        // …and un-trusting ends it, mid-session, with no timeout in between.
        watcher
            .set_trust(&worker_id, Trust::Known)
            .await
            .expect("un-trust");
        assert!(recv_within(&mut watching, "after un-trust").await.is_none());
        assert_eq!(
            worker.publish_turn("session-7", at(11), &AgentEvent::TextDelta("unseen".into())),
            0
        );
    }

    #[tokio::test]
    async fn a_watcher_that_stops_reading_drops_events_rather_than_growing() {
        // The bound, from the model's side. A subscription costs at most
        // `SUBSCRIPTION_BUFFER` events however long the publisher keeps going,
        // and the loss is counted rather than hidden: this tree has already had
        // one unbounded queue, and it hung the suite before it took the memory.
        let transport = Arc::new(LoopbackTransport::new());
        let mut watcher = Mesh::new(
            Identity::from_seed([52u8; 32]),
            PeerStore::ephemeral(),
            transport.clone(),
        );
        let worker = Mesh::new(
            Identity::from_seed([53u8; 32]),
            PeerStore::ephemeral(),
            transport.clone(),
        );
        worker.announce().await.expect("announce");
        let (worker_id, _) = watcher
            .add_peer(&worker.local_id().address(), at(0))
            .expect("add");
        watcher
            .set_trust(&worker_id, Trust::Trusted)
            .await
            .expect("trust");
        let mut watching = watcher.subscribe(&worker_id).await.expect("subscribe");

        let chatter = transport::SUBSCRIPTION_BUFFER * 20;
        for i in 0..chatter {
            worker.publish_turn(
                "session-1",
                at(i as i64),
                &AgentEvent::TextDelta(format!("token {i} ")),
            );
        }
        assert!(!watching.is_closed(), "a slow reader is not a dead one");
        assert_eq!(
            watching.dropped() as usize,
            chatter - transport::SUBSCRIPTION_BUFFER
        );

        let mut held = 0;
        while watching.try_recv().is_some() {
            held += 1;
        }
        assert_eq!(
            held,
            transport::SUBSCRIPTION_BUFFER,
            "the queue does not grow to fit the producer"
        );
        // And it recovers: dropping is backpressure, not a fault.
        assert_eq!(
            worker.publish_turn(
                "session-1",
                at(0),
                &AgentEvent::TextDelta("caught up".into())
            ),
            1
        );
    }

    #[tokio::test]
    async fn refresh_stores_what_a_peer_claims_and_marks_it_seen() {
        let (mut mesh, transport) = mesh(9);
        let caps = Capability::advertise(&["qwen3.6:27b"], &["read_file"], &[], &[], true);
        let address = announce_as(&transport, 10, "workshop", caps.clone()).await;
        let (id, _) = mesh.add_peer(&address, at(0)).expect("add");

        assert_eq!(mesh.store().presence(&id, at(100)), Some(Presence::Unseen));
        assert_eq!(
            mesh.store().get(&id).expect("peer").node.label(),
            id.short(),
            "a pasted address is an identity with nothing attached"
        );
        let fetched = mesh.refresh(&id, at(100)).await.expect("refresh");
        assert_eq!(fetched, caps);
        assert_eq!(
            mesh.store().get(&id).expect("peer").node.label(),
            "workshop",
            "the name is announced, not pasted, so this is the only path it has \
             into the store and into the graph's label"
        );
        assert_eq!(mesh.store().presence(&id, at(100)), Some(Presence::Online));
        assert_eq!(
            mesh.store().presence(&id, at(100_000)),
            Some(Presence::Stale),
            "and it goes stale on its own, with nothing written"
        );

        // What the peer claims does not become a decision about the peer.
        let peer = mesh.store().get(&id).expect("peer");
        assert!(peer.node.caps.accepts_work, "the claim is recorded");
        assert_eq!(peer.trust, Trust::Known, "the claim is not believed");
        assert!(!mesh.admit(&id, at(100)).is_allowed());
    }

    /// A transport that answers every question with one node's announcement,
    /// whoever was asked about.
    struct ConfusedTransport(Node);

    #[async_trait::async_trait]
    impl Transport for ConfusedTransport {
        async fn announce(&self, _node: &Node) -> Result<()> {
            Ok(())
        }

        async fn announcement_of(&self, _id: &NodeId) -> Result<Node> {
            Ok(self.0.clone())
        }

        async fn subscribe(&self, _local: &NodeId, _peer: &NodeId) -> Result<Subscription> {
            Err(anyhow!("this transport carries no streams"))
        }

        fn publish(&self, _event: &PeerEvent) -> usize {
            0
        }

        async fn revoke(&self, _local: &NodeId, _peer: &NodeId) -> Result<()> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn a_transport_that_answers_for_the_wrong_node_renames_nobody() {
        // `refresh` is the one path that writes a peer's name, and the name is
        // what a human reads to tell two peers apart. A transport that hands
        // back somebody else's announcement (a lookup keyed on the wrong
        // thing, a peer answering for its neighbour, a cache serving the
        // previous request) would rewrite this peer's label with another
        // node's claim: the impersonation the graph exists to defeat, arriving
        // through the seam rather than through a name.
        let peer = Identity::from_seed([36u8; 32]);
        let other = Identity::from_seed([37u8; 32]);
        let transport = Arc::new(ConfusedTransport(other.announce(
            "alice",
            Capability::advertise(&["gpt-5.3-codex"], &[], &[], &[], true),
        ))) as Arc<dyn Transport>;
        let mut mesh = Mesh::new(
            Identity::from_seed([38u8; 32]),
            PeerStore::ephemeral(),
            transport,
        );
        let (id, _) = mesh.add_peer(&peer.id().address(), at(0)).expect("add");

        let err = mesh.refresh(&id, at(0)).await.expect_err("the wrong node");
        let message = format!("{err:#}");
        assert!(message.contains(&id.short()), "{message}");
        assert!(message.contains(&other.id().short()), "{message}");

        let stored = mesh.store().get(&id).expect("peer");
        assert!(
            stored.node.name.is_empty(),
            "nothing was renamed: {stored:?}"
        );
        assert!(stored.node.caps.is_empty(), "{stored:?}");
        assert!(!stored.node.caps.accepts_work);
        assert_eq!(
            stored.presence(at(0)),
            Presence::Unseen,
            "and a refusal is not an observation, so nothing was marked seen"
        );
        // The node the transport answered *for* is not created on the way
        // past either: an announcement is not an introduction.
        assert!(mesh.store().get(&other.id()).is_none());
        assert_eq!(mesh.store().len(), 1);
    }

    #[tokio::test]
    async fn the_local_node_advertises_nothing_until_it_is_told_to() {
        let (mut mesh, transport) = mesh(11);
        assert!(!mesh.local_node().caps.accepts_work);
        assert!(mesh.local_node().caps.is_empty());

        mesh.announce().await.expect("announce");
        let announced = transport
            .announcement_of(&mesh.local_id())
            .await
            .expect("fetch");
        assert!(
            !announced.caps.accepts_work,
            "merely running is not consent"
        );

        mesh.set_local(
            "workshop\u{0007}",
            Capability::advertise(&["qwen3.6:27b"], &[], &[], &[], true),
        );
        mesh.announce().await.expect("re-announce");
        assert!(
            transport
                .announcement_of(&mesh.local_id())
                .await
                .expect("fetch")
                .caps
                .accepts_work
        );
        assert_eq!(
            mesh.local_node().label(),
            "workshop",
            "even the local name goes through the sanitiser"
        );
    }

    #[tokio::test]
    async fn the_graph_carries_every_edge_kind_and_tells_the_truth_about_presence() {
        let (mut mesh, transport) = mesh(13);
        mesh.set_local(
            "here",
            Capability::advertise(&["qwen3.6:27b"], &[], &[], &[], false),
        );
        let first = announce(
            &transport,
            14,
            // Shares a model with the local node: one capability vertex, two
            // edges, which is the thing that makes this a graph.
            Capability::advertise(&["qwen3.6:27b"], &["web_search"], &[], &[], true),
        )
        .await;
        let second = announce(&transport, 15, Capability::none()).await;

        let (first_id, _) = mesh.add_peer(&first, at(0)).expect("add");
        let (second_id, _) = mesh
            .add_observed_peer(&second, first_id, at(0))
            .expect("add observed");
        mesh.set_trust(&first_id, Trust::Trusted)
            .await
            .expect("trust");
        mesh.refresh(&first_id, at(10)).await.expect("refresh");
        mesh.record_delegation(&first_id);
        mesh.record_delegation(&first_id);

        let mut graph = mesh.graph(at(20));
        graph.add_session(first_id, &PeerText::sanitize("session-42"));
        graph.add_session(first_id, &PeerText::sanitize("session-42"));

        assert_eq!(graph.edges_of(EdgeKind::Peer).count(), 2);
        let observed: Vec<_> = graph.edges_of(EdgeKind::Observed).collect();
        assert_eq!(observed.len(), 1);
        assert_eq!(observed[0].from, Vertex::Node(first_id));
        assert_eq!(observed[0].to, Vertex::Node(second_id));
        let delegation: Vec<_> = graph.edges_of(EdgeKind::Delegation).collect();
        assert_eq!(delegation.len(), 1);
        assert_eq!(delegation[0].weight, 2, "delegation edges carry a count");
        assert_eq!(
            graph.edges_of(EdgeKind::Session).count(),
            1,
            "the same session twice is one edge"
        );

        // The shared model is one vertex with an edge from each node.
        let shared = Vertex::Capability(CapabilityKind::Model, "qwen3.6:27b".to_string());
        assert_eq!(
            graph
                .nodes
                .iter()
                .filter(|node| node.vertex == shared)
                .count(),
            1
        );
        assert_eq!(
            graph
                .edges_of(EdgeKind::Capability)
                .filter(|edge| edge.to == shared)
                .count(),
            2
        );

        // Presence is baked into the snapshot, honestly, per node.
        let detail = |id: NodeId| {
            graph
                .nodes
                .iter()
                .find(|node| node.vertex == Vertex::Node(id))
                .map(|node| node.detail.clone())
                .expect("vertex")
        };
        assert_eq!(detail(mesh.local_id()), VertexDetail::Local);
        assert_eq!(
            detail(first_id),
            VertexDetail::Peer {
                trust: Trust::Trusted,
                presence: Presence::Online,
                address: first,
            }
        );
        assert_eq!(
            detail(second_id),
            VertexDetail::Peer {
                trust: Trust::Known,
                presence: Presence::Unseen,
                address: second,
            },
            "a pasted address that never answered renders as unseen, not offline"
        );
        // Same store, later clock: the first peer ages out with nothing
        // written and no network reachable.
        let later = mesh.graph(at(100_000));
        let VertexDetail::Peer { presence, .. } = later
            .nodes
            .iter()
            .find(|node| node.vertex == Vertex::Node(first_id))
            .map(|node| node.detail.clone())
            .expect("vertex")
        else {
            panic!("the peer vertex must stay a peer vertex");
        };
        assert_eq!(presence, Presence::Stale);
    }

    #[tokio::test]
    async fn a_session_belongs_to_one_node_and_needs_that_node_on_the_graph() {
        // `main` is what everybody calls their session. Keyed on the name
        // alone, two nodes running one each would share a vertex and the
        // explorer would draw two machines collaborating on a session that
        // does not exist: beautiful, and a lie about who is doing what.
        let (mut mesh, transport) = mesh(28);
        let first = announce(&transport, 29, Capability::none()).await;
        let second = announce(&transport, 30, Capability::none()).await;
        let (first_id, _) = mesh.add_peer(&first, at(0)).expect("add");
        let (second_id, _) = mesh.add_peer(&second, at(0)).expect("add");

        let main = PeerText::sanitize("main");
        let mut graph = mesh.graph(at(0));
        graph.add_session(first_id, &main);
        graph.add_session(second_id, &main);
        graph.add_session(first_id, &main);

        let sessions: Vec<&Edge> = graph.edges_of(EdgeKind::Session).collect();
        assert_eq!(
            sessions.len(),
            2,
            "one edge each, and no third for the repeat"
        );
        assert_eq!(sessions[0].from, Vertex::Node(first_id));
        assert_eq!(sessions[1].from, Vertex::Node(second_id));
        assert_ne!(
            sessions[0].to, sessions[1].to,
            "two nodes running a same-named session are two sessions"
        );
        assert_eq!(
            graph
                .nodes
                .iter()
                .filter(|node| node.detail == VertexDetail::Session)
                .count(),
            2
        );

        // The peer is blocked and forgotten, but a `Subscription` buffers up
        // to SUBSCRIPTION_BUFFER events, so the explorer can still be holding
        // one for a node that is no longer on the graph. `Graph::build` drops
        // an observed edge whose referrer is absent for exactly this reason;
        // this is the same rule on the path that is written to at runtime.
        let mut graph = mesh.graph(at(0));
        let stranger = Identity::from_seed([31u8; 32]).id();
        graph.add_session(stranger, &main);
        graph.add_session(mesh.local_id(), &PeerText::sanitize(""));
        assert_eq!(
            graph.edges_of(EdgeKind::Session).count(),
            0,
            "an edge to a vertex nobody drew is a dangling line"
        );
        assert!(
            graph
                .nodes
                .iter()
                .all(|node| node.detail != VertexDetail::Session),
            "and it must not leave the orphaned session vertex behind either"
        );
        // Every edge on the graph has both endpoints on the graph, which is
        // the property a renderer resolving endpoints by lookup depends on.
        graph.add_session(mesh.local_id(), &main);
        let drawn: BTreeSet<&Vertex> = graph.nodes.iter().map(|node| &node.vertex).collect();
        for edge in &graph.edges {
            assert!(drawn.contains(&edge.from), "dangling from: {edge:?}");
            assert!(drawn.contains(&edge.to), "dangling to: {edge:?}");
        }
    }

    #[tokio::test]
    async fn a_trust_decision_is_on_disk_before_the_call_returns() {
        // The forgettable second step. `set_trust` fuses the recording and the
        // revocation because a caller that has to remember one will forget it;
        // persistence was left as precisely that step, on the same method, for
        // the same security-critical decision. An operator who pastes a
        // hostile address and blocks it, on a machine that then crashes, gets
        // the peer back on the next run with no sign that anything was lost.
        let dir = tempfile::tempdir().expect("tempdir");
        let transport = Arc::new(LoopbackTransport::new());
        let address = announce(&transport, 33, Capability::none()).await;
        let observed = announce(&transport, 34, Capability::none()).await;

        let (id, observed_id) = {
            let mut mesh = Mesh::new(
                Identity::from_seed([32u8; 32]),
                PeerStore::load(dir.path()).expect("load"),
                transport.clone(),
            );
            let (id, _) = mesh.add_peer(&address, at(0)).expect("add");
            let (observed_id, _) = mesh
                .add_observed_peer(&observed, id, at(0))
                .expect("add observed");
            mesh.set_trust(&id, Trust::Trusted).await.expect("trust");
            mesh.set_trust(&id, Trust::Blocked).await.expect("block");
            // No `mesh.save()` anywhere, and no clean shutdown: this is the
            // crash.
            (id, observed_id)
        };

        let mesh = Mesh::new(
            Identity::from_seed([32u8; 32]),
            PeerStore::load(dir.path()).expect("reload"),
            transport,
        );
        assert_eq!(
            mesh.store().trust_of(&id),
            Some(Trust::Blocked),
            "the operator's decision has to outlive the process that made it"
        );
        // Pasting a peer is not a decision, but losing it is still losing
        // work, and the edge it was learned through goes with it.
        assert_eq!(mesh.store().trust_of(&observed_id), Some(Trust::Known));
        assert_eq!(
            mesh.store().get(&observed_id).expect("peer").observed_via,
            Some(id)
        );
    }

    #[tokio::test]
    async fn a_stored_record_claiming_to_be_this_node_is_not_a_peer() {
        // `add_peer` refuses this node's own address, but the store is a file:
        // it can be hand-edited, restored from another machine's backup, or
        // written by a transport that folds an observed set into it. The
        // record would otherwise be consulted by `admit` (a planted trust
        // decision about the local node) and drawn as a self-loop.
        let dir = tempfile::tempdir().expect("tempdir");
        let identity = Identity::from_seed([35u8; 32]);
        std::fs::create_dir_all(peer::mesh_dir(dir.path())).expect("mkdir");
        std::fs::write(
            peer::store_path(dir.path()),
            serde_json::json!({
                "version": 1,
                "peers": [{
                    "node": { "id": identity.id().address() },
                    "trust": "trusted",
                    "added_at": at(0),
                }],
            })
            .to_string(),
        )
        .expect("write");

        let store = PeerStore::load(dir.path()).expect("load");
        assert_eq!(store.trust_of(&identity.id()), Some(Trust::Trusted));

        let mut mesh = Mesh::new(
            identity,
            store,
            Arc::new(LoopbackTransport::new()) as Arc<dyn Transport>,
        );
        let local = mesh.local_id();
        assert!(mesh.store().is_empty(), "not a peer record, so not kept");
        let refusal = mesh.admit(&local, at(0));
        assert!(
            refusal.reason().expect("refused").contains("not a peer"),
            "refused by name rather than by a trust decision somebody planted: {refusal:?}"
        );

        // And a graph built straight from such a store, without a `Mesh` in
        // the way, draws neither the self-loop nor a local vertex stripped of
        // its detail.
        let mut planted = PeerStore::ephemeral();
        planted.add(Node::new(local), at(0));
        let graph = Graph::build(&mesh.local_node(), &planted, at(0));
        assert_eq!(graph.nodes.len(), 1);
        assert_eq!(graph.nodes[0].detail, VertexDetail::Local);
        assert!(graph.edges.is_empty(), "{:?}", graph.edges);
    }

    #[test]
    fn a_fifty_node_synthetic_graph_builds_from_cached_state_alone() {
        // The GUI's performance bar is 50 synthetic nodes at 60fps, and the
        // explorer rebuilds this snapshot from the store. No transport, no
        // clock of its own, no file.
        let now = at(0);
        let store = peer::synthetic_store(50, 1, now);
        let local = Identity::from_seed([0u8; 32]).announce("local", Capability::none());
        let graph = Graph::build(&local, &store, now);

        assert_eq!(graph.edges_of(EdgeKind::Peer).count(), 50);
        assert!(graph.edges_of(EdgeKind::Observed).count() > 0);
        assert!(graph.edges_of(EdgeKind::Delegation).count() > 0);
        assert!(graph.edges_of(EdgeKind::Capability).count() > 0);
        assert_eq!(graph.nodes[0].vertex, Vertex::Node(local.id));
        // Deterministic: the same store and clock give the same picture, so a
        // layout bug is reproducible.
        assert_eq!(Graph::build(&local, &store, now), graph);
        // Every presence state is represented, so the renderer's three cases
        // are all exercised by the bar.
        for state in [Presence::Online, Presence::Stale, Presence::Unseen] {
            assert!(
                graph.nodes.iter().any(|node| matches!(
                    &node.detail,
                    VertexDetail::Peer { presence, .. } if *presence == state
                )),
                "no synthetic vertex is {}",
                state.label()
            );
        }
    }
}
