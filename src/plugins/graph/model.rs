//! The explorer's model: the mesh as something drawable, derived from cached
//! peer state and a clock.
//!
//! [`crate::plugins::mesh::Graph`] already turns a store into vertices and edges. This
//! is the layer above it, and it exists because a renderer needs three things
//! that a wire-shaped snapshot does not owe anybody:
//!
//! 1. **Liveness, decided once, honestly.** [`Liveness`] folds the peer's
//!    [`Presence`] together with the recorded [`Trust`] decision, because a
//!    blocked peer is not reachable no matter how recent its last announcement
//!    was, and this machine is not "unseen" merely because it never observed
//!    itself over the network. The plan's line is that "a graph that is
//!    beautiful and lies about who is online is worse than a plain one that
//!    does not", so the model refuses to hand the renderer a state it would
//!    have to reason about. There is one clock read in the whole module and it
//!    is a parameter.
//! 2. **A label that cannot take the screen hostage.** A peer picks its own
//!    name. [`PeerText`] already strips the control characters and the bidi
//!    overrides, and this layer *still* does not trust what it is handed:
//!    [`bounded_label`] neutralises invisible formatting characters again,
//!    caps the label in display columns *and* in characters, and every node
//!    carries a [`DisplayName::discriminator`] taken from its key fingerprint.
//!    Two peers that call themselves the same thing are told apart by
//!    something a peer cannot choose.
//! 3. **Indices.** The layout runs every frame over `n²` pairs; it wants
//!    `Vec<Point>` and `usize` endpoints, not a map keyed by an enum holding a
//!    `String`. Selection still travels as a [`NodeKey`], which survives a
//!    rebuild, and [`MeshGraph::index_of`] is the one bridge between the two.
//!
//! Capability edges are the odd one of the plan's five kinds: "shared model or
//! skill, used for clustering rather than drawn". They are in [`MeshGraph`]
//! like the rest, as links whose [`Link::is_drawn`] is false, so the layout
//! pulls peers that offer the same model together and the renderer never draws
//! a line for them.

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use unicode_width::UnicodeWidthChar as _;

use crate::plugins::mesh::{
    Capability, CapabilityKind, EdgeKind, Node, NodeId, PeerStore, PeerText, Presence, Trust,
};

/// Widest a node label may render, in terminal/display columns.
///
/// Much shorter than [`PeerText::MAX_CHARS`] on purpose: 64 characters is a
/// reasonable bound on a name in a log line and a ridiculous one on a label
/// floating next to a dot in a graph, where it would cover its neighbours.
pub const MAX_LABEL_COLUMNS: usize = 24;

/// Characters of key fingerprint carried beside every node label.
///
/// Eight base64 characters is 48 bits of a SHA-256 over the public key. That
/// is not a collision-resistant identifier and it is not meant to be one: it
/// is enough that a person looking at two nodes with the same name can see
/// they are different nodes, and the full [`NodeId::fingerprint`] is one field
/// away in the inspector.
pub const DISCRIMINATOR_CHARS: usize = 8;

/// Separates a label from its discriminator in [`DisplayName::rendered`].
///
/// Stripped out of labels by [`bounded_label`], so the last separator in a
/// rendered name always introduces the real fingerprint. Without that, a peer
/// could name itself `workshop · a1b2c3d4` and wear somebody else's
/// discriminator.
pub const DISCRIMINATOR_SEPARATOR: char = '·';

/// What a session is called when nothing of its reported id survives
/// [`bounded_label`].
const SESSION_FALLBACK_LABEL: &str = "session";

// ---------------------------------------------------------------------------
// Liveness
// ---------------------------------------------------------------------------

/// What the model is willing to claim about a node being reachable *now*.
///
/// [`Presence`] answers this from a peer record and a clock. This answers it
/// for a vertex about to be drawn, which needs two states a timestamp cannot
/// produce: the local node is never *observed* (it does not announce to
/// itself), and a blocked peer is one this machine will not contact whatever
/// its last announcement said.
///
/// Ordered by increasing confidence, the same way [`Trust`] is ordered by
/// increasing permission, so `>=` reads the way it looks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Liveness {
    /// Blocked. Not reachable, whatever the timestamps say.
    Unreachable,
    /// In the store, never heard from. A pasted address that has not answered.
    Unseen,
    /// Heard from once, but not inside the freshness window.
    Stale,
    /// Heard from inside the freshness window, and contactable.
    Live,
    /// This machine. Running by definition; never "seen", never stale.
    Here,
}

impl Liveness {
    /// Whether the explorer may draw this node as up.
    ///
    /// The single predicate the renderer is meant to use, so that "never
    /// present an unreachable peer as live" is one function to audit rather
    /// than a match arm at every call site.
    pub fn is_live(self) -> bool {
        matches!(self, Liveness::Live | Liveness::Here)
    }

    /// Lower-case label, for the explorer's legend.
    pub fn label(self) -> &'static str {
        match self {
            Liveness::Unreachable => "unreachable",
            Liveness::Unseen => "unseen",
            Liveness::Stale => "stale",
            Liveness::Live => "live",
            Liveness::Here => "here",
        }
    }

    /// The liveness of a peer, from the decision about it and what the store
    /// last observed.
    ///
    /// Trust is consulted *first*: [`Trust::may_contact`] rather than a match
    /// on [`Trust::Blocked`], so that a fourth trust state which may not be
    /// contacted inherits this rule instead of quietly rendering as online.
    pub fn of_peer(trust: Trust, presence: Presence) -> Self {
        if !trust.may_contact() {
            return Liveness::Unreachable;
        }
        match presence {
            Presence::Online => Liveness::Live,
            Presence::Stale => Liveness::Stale,
            Presence::Unseen => Liveness::Unseen,
        }
    }
}

// ---------------------------------------------------------------------------
// Identity of a drawn thing
// ---------------------------------------------------------------------------

/// A stable handle on one drawn node.
///
/// Selection, pinning and hit-testing all travel as this rather than as an
/// index, because the graph is rebuilt from the store every time anything
/// changes and an index means a different node afterwards. There is
/// deliberately no capability variant: a shared model clusters the peers that
/// offer it (see [`EdgeKind::Capability`]) and is never a thing on screen to
/// point at.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum NodeKey {
    /// A mesh node: this machine or a peer.
    Node(NodeId),
    /// A live session stream, keyed by the node running it *and* the session id
    /// that node reported.
    ///
    /// Both halves, for the reason [`crate::plugins::mesh::Vertex`] gives: a session id
    /// is peer-supplied text chosen by the far end, `main` is what everybody
    /// calls their session, and a key made of the name alone would draw two
    /// machines collaborating on one session that does not exist. Worse, the
    /// name is attacker-controlled, so a hostile peer could claim the session
    /// id of a trusted one and hang itself off the same vertex.
    Session(NodeId, String),
}

/// What a drawn node is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeKind {
    /// This machine.
    Local,
    /// A peer from the store.
    Peer,
    /// A session stream running on `owner`.
    Session { owner: NodeId },
}

// ---------------------------------------------------------------------------
// Names
// ---------------------------------------------------------------------------

/// A name that is safe to put on screen next to a dot.
///
/// Two parts, and the second is the one that matters. `label` is what the peer
/// calls itself, bounded; `discriminator` is a prefix of its key fingerprint,
/// which the peer cannot choose. [`DisplayName::rendered`] always joins both,
/// unconditionally, because the alternative (show the fingerprint only when a
/// clash is detected) is only as good as the clash detector, and
/// [`confusable_fold`] is an approximation by construction: Unicode has more
/// ways to draw a letter than any short table holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DisplayName {
    label: String,
    discriminator: String,
    ambiguous: bool,
}

impl DisplayName {
    /// The peer's own name, bounded. Never unique, never trusted.
    pub fn label(&self) -> &str {
        &self.label
    }

    /// The key fingerprint prefix. Not chosen by the peer.
    pub fn discriminator(&self) -> &str {
        &self.discriminator
    }

    /// Whether another node in the same graph carries a label that could be
    /// mistaken for this one.
    ///
    /// Advisory: a renderer can lean on it to emphasise the discriminator, and
    /// nothing about safety may depend on it, since the fold behind it is
    /// incomplete. The discriminator is present either way.
    pub fn is_ambiguous(&self) -> bool {
        self.ambiguous
    }

    /// The full label to draw: name, separator, fingerprint prefix.
    pub fn rendered(&self) -> String {
        format!(
            "{} {DISCRIMINATOR_SEPARATOR} {}",
            self.label, self.discriminator
        )
    }

    /// A name for a node, from what it calls itself and the key it cannot
    /// choose.
    ///
    /// The fallback to a short address is this layer's own, not a borrowed
    /// one: [`Node::label`] already refuses to hand back an empty label, and
    /// it decides that by asking whether [`PeerText`] kept anything. A name
    /// made of characters that got past the mesh sanitiser but not past
    /// [`bounded_label`] is non-empty there and empty here, and the node still
    /// has to be something a person can point at.
    fn for_node(raw: &str, id: NodeId) -> Self {
        let mut label = bounded_label(raw);
        if label.is_empty() {
            label = bounded_label(&id.short());
        }
        Self {
            label,
            discriminator: discriminator_of(id),
            ambiguous: false,
        }
    }

    /// A name for a session.
    ///
    /// The discriminator is the *owner's* fingerprint, not a hash of the
    /// session id: two sessions that look alike are interesting exactly when
    /// they are running on different machines, and two identically named
    /// sessions on one machine are one vertex anyway.
    fn for_session(raw: &str, owner: NodeId) -> Self {
        let mut label = bounded_label(raw);
        if label.is_empty() {
            label = SESSION_FALLBACK_LABEL.to_string();
        }
        Self {
            label,
            discriminator: discriminator_of(owner),
            ambiguous: false,
        }
    }
}

/// The first [`DISCRIMINATOR_CHARS`] characters of a node's fingerprint body.
fn discriminator_of(id: NodeId) -> String {
    let fingerprint = id.fingerprint();
    let body = fingerprint
        .split_once(':')
        .map(|(_, body)| body)
        .unwrap_or(&fingerprint);
    body.chars().take(DISCRIMINATOR_CHARS).collect()
}

/// Cut a peer-supplied string down to something that fits beside a dot.
///
/// Deliberately repeats work [`PeerText::sanitize`] has already done. This
/// layer is handed names from a store file, from a wire decode and from
/// whatever a future transport builds by hand, and "the layer below sanitises"
/// is exactly the assumption that turns one missed category into a rendering
/// bug on somebody else's screen. Cheap, idempotent, and it removes a class
/// (invisible formatting) that a length cap alone would not.
///
/// Both bounds are load-bearing. The column bound stops a wide name from
/// covering its neighbours; the character bound stops a name made entirely of
/// zero-width combining marks, which is nought columns wide and unbounded in
/// characters, from smearing over them anyway.
fn bounded_label(raw: &str) -> String {
    let mut cleaned = String::with_capacity(raw.len().min(MAX_LABEL_COLUMNS * 4));
    for ch in raw.chars() {
        // Neutralised to a space rather than dropped, matching the mesh
        // sanitiser: deleting a separator silently joins two words.
        if ch == DISCRIMINATOR_SEPARATOR
            || ch.is_control()
            || ch.is_whitespace()
            || is_invisible_format(ch)
        {
            cleaned.push(' ');
        } else {
            cleaned.push(ch);
        }
    }
    let collapsed = cleaned.split_whitespace().collect::<Vec<_>>().join(" ");
    truncate_to_columns(&collapsed, MAX_LABEL_COLUMNS)
}

/// Truncate to `columns` display columns and `columns` characters, whichever
/// bites first, marking the cut with an ellipsis that is itself inside the
/// budget.
fn truncate_to_columns(text: &str, columns: usize) -> String {
    let mut width = 0usize;
    let mut chars = 0usize;
    let mut fits = true;
    for ch in text.chars() {
        width += ch.width().unwrap_or(0);
        chars += 1;
        if width > columns || chars > columns {
            fits = false;
            break;
        }
    }
    if fits {
        return text.to_string();
    }
    let budget = columns.saturating_sub(1);
    let mut out = String::new();
    let (mut width, mut chars) = (0usize, 0usize);
    for ch in text.chars() {
        let next = ch.width().unwrap_or(0);
        if width + next > budget || chars + 1 > budget {
            break;
        }
        out.push(ch);
        width += next;
        chars += 1;
    }
    out.push('…');
    out
}

/// Characters that occupy no space and change how their neighbours render.
///
/// The `Cf` general category plus the zero-width fillers and variation
/// selectors that behave the same way on screen. A reviewer found exactly this
/// class getting past a sanitiser in [`crate::plugins::mesh`]; the fix belongs there,
/// and it also belongs here, because a label this module builds must be
/// bounded by this module's own rules rather than by the state of somebody
/// else's table.
fn is_invisible_format(ch: char) -> bool {
    matches!(ch,
        '\u{00ad}'                  // SOFT HYPHEN
        | '\u{034f}'                // COMBINING GRAPHEME JOINER
        | '\u{0600}'..='\u{0605}'   // Arabic number signs
        | '\u{061c}'                // ARABIC LETTER MARK
        | '\u{06dd}' | '\u{070f}' | '\u{08e2}'
        | '\u{115f}' | '\u{1160}'   // Hangul choseong/jungseong fillers
        | '\u{180b}'..='\u{180f}'   // Mongolian variation selectors, vowel separator
        | '\u{200b}'..='\u{200f}'   // zero width space through RLM
        | '\u{202a}'..='\u{202e}'   // bidi embeddings and overrides
        | '\u{2060}'..='\u{2064}'   // word joiner, invisible operators
        | '\u{2066}'..='\u{206f}'   // bidi isolates, deprecated format characters
        | '\u{3164}'                // HANGUL FILLER
        | '\u{fe00}'..='\u{fe0f}'   // variation selectors
        | '\u{feff}'                // ZERO WIDTH NO-BREAK SPACE / BOM
        | '\u{ffa0}'                // HALFWIDTH HANGUL FILLER
        | '\u{fff9}'..='\u{fffb}'   // interlinear annotation
        | '\u{110bd}' | '\u{110cd}' // Kaithi number signs
        | '\u{13430}'..='\u{1343f}' // Egyptian hieroglyph format controls
        | '\u{1bca0}'..='\u{1bca3}' // shorthand format controls
        | '\u{1d173}'..='\u{1d17a}' // musical beams and slurs
        | '\u{e0000}'..='\u{e007f}' // the Tag block
        | '\u{e0100}'..='\u{e01ef}' // variation selectors supplement
    )
}

/// Fold a label to something two look-alike names share.
///
/// Case-folded, stripped of everything that is not alphanumeric, with a small
/// table of the homoglyphs that actually get used (Cyrillic and Greek letters
/// that are drawn as Latin ones, and the digit/letter pairs). Deliberately
/// crude: it exists to *flag* a pair for emphasis, and the guarantee that two
/// peers stay distinguishable is [`DisplayName::discriminator`], which does
/// not depend on this table being complete. Over-flagging is harmless; a miss
/// costs emphasis, not safety.
fn confusable_fold(label: &str) -> String {
    let mut folded = String::with_capacity(label.len());
    for ch in label.chars().flat_map(char::to_lowercase) {
        let mapped = match ch {
            // Cyrillic drawn as Latin.
            '\u{0430}' => 'a',
            '\u{0435}' => 'e',
            '\u{043e}' => 'o',
            '\u{0440}' => 'p',
            '\u{0441}' => 'c',
            '\u{0443}' => 'y',
            '\u{0445}' => 'x',
            '\u{0455}' => 's',
            '\u{0456}' => 'i',
            '\u{0458}' => 'j',
            // Greek drawn as Latin.
            '\u{03b1}' => 'a',
            '\u{03b5}' => 'e',
            '\u{03b9}' => 'i',
            '\u{03ba}' => 'k',
            '\u{03bd}' => 'v',
            '\u{03bf}' => 'o',
            '\u{03c1}' => 'p',
            '\u{03c4}' => 't',
            '\u{03c5}' => 'u',
            // Digits drawn as letters.
            '0' => 'o',
            '1' => 'l',
            other => other,
        };
        if mapped.is_alphanumeric() {
            folded.push(mapped);
        }
    }
    folded
}

// ---------------------------------------------------------------------------
// Nodes, links, capabilities
// ---------------------------------------------------------------------------

/// One capability a node advertises.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct CapabilityRef {
    pub kind: CapabilityKind,
    /// The name as advertised. This is the identity two peers *share*, so it
    /// is the sanitised name and not the truncated one: folding it to the
    /// display form first would let two different long names collapse into one
    /// shared capability and cluster peers that have nothing in common.
    pub name: String,
    /// The bounded form to put on screen.
    pub display: String,
}

/// One drawn node, carrying everything a renderer or an inspector needs, so
/// neither has to go back to the store mid-frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GraphNode {
    /// Stable handle. Survives a rebuild; an index does not.
    pub key: NodeKey,
    pub kind: NodeKind,
    pub name: DisplayName,
    /// The recorded decision. Never inferred, never optional: a renderer that
    /// has to ask "and what if there is no trust state" will get that question
    /// wrong. The local node is [`Trust::Trusted`] because it is this machine,
    /// and a session carries the trust of the node running it.
    pub trust: Trust,
    /// Whether this node may be drawn as up. Folds trust and presence together
    /// so the renderer cannot draw a blocked peer green by forgetting a case.
    pub liveness: Liveness,
    /// Seconds since this node was last heard from; `None` for never, and for
    /// the local node, which is not observed over the network.
    pub age_secs: Option<i64>,
    /// The address to copy. `None` for a session, which is not addressable.
    pub address: Option<String>,
    /// The full key fingerprint, for an operator comparing out of band.
    pub fingerprint: Option<String>,
    /// Delegations sent to this node. The weight on its delegation edge.
    pub delegations: u32,
    /// What the node *says* it will do. A claim, not evidence.
    pub accepts_work: bool,
    /// What the node advertises, for the capability filter and the inspector.
    pub caps: Vec<CapabilityRef>,
}

impl GraphNode {
    /// How long ago this node was heard from, as a label to draw.
    ///
    /// Coarse on purpose: "4h" is what an operator needs, and a graph that
    /// re-renders a seconds counter sixty times a second is a graph that
    /// spends its frame budget on arithmetic nobody reads.
    pub fn seen_label(&self) -> String {
        match (self.kind, self.age_secs) {
            (NodeKind::Local, _) => "here".to_string(),
            (_, None) => "never".to_string(),
            (_, Some(secs)) if secs < 60 => format!("{secs}s"),
            (_, Some(secs)) if secs < 3_600 => format!("{}m", secs / 60),
            (_, Some(secs)) if secs < 86_400 => format!("{}h", secs / 3_600),
            (_, Some(secs)) => format!("{}d", secs / 86_400),
        }
    }

    /// Whether this node advertises `name` of `kind`.
    pub fn advertises(&self, kind: CapabilityKind, name: &str) -> bool {
        self.caps
            .iter()
            .any(|cap| cap.kind == kind && cap.name == name)
    }
}

/// One relationship between two drawn nodes, by index.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Link {
    pub from: usize,
    pub to: usize,
    pub kind: EdgeKind,
    /// How much of the relationship there is: the delegation count for
    /// [`EdgeKind::Delegation`], `1` for the kinds that either exist or do not.
    pub weight: u32,
}

impl Link {
    /// Whether the renderer should draw a line for this link.
    ///
    /// False for [`EdgeKind::Capability`], which the plan describes as "used
    /// for clustering rather than drawn": a mesh where every peer offers
    /// `read_file` would otherwise be a solid block of lines conveying
    /// nothing. The layout still pulls on it, which is the point.
    pub fn is_drawn(&self) -> bool {
        !matches!(self.kind, EdgeKind::Capability)
    }
}

// ---------------------------------------------------------------------------
// The graph
// ---------------------------------------------------------------------------

/// The mesh as the explorer holds it: drawn nodes, links between them by
/// index, and an index from the stable key back to the position.
///
/// Built from cached state alone. No network call, no clock of its own, so it
/// renders identically with the network down and a test can freeze time and
/// get the same picture every run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MeshGraph {
    nodes: Vec<GraphNode>,
    links: Vec<Link>,
    index: BTreeMap<NodeKey, usize>,
    caps: BTreeMap<(CapabilityKind, String), Vec<usize>>,
    /// How many links touch each node, in `nodes` order. Held here rather than
    /// counted by the layout because the layout runs sixty times a second and
    /// this changes only when the store does.
    degrees: Vec<u32>,
    generated_at: DateTime<Utc>,
}

impl MeshGraph {
    /// Build the graph for `local` plus everything in `store`, as of `now`.
    ///
    /// Node order is the local node first, then the store's own order (which
    /// is by [`NodeId`], so it is stable across runs). That stability is what
    /// lets the layout keep a node where the operator last saw it.
    pub fn build(local: &Node, store: &PeerStore, now: DateTime<Utc>) -> Self {
        let mut graph = Self {
            nodes: Vec::with_capacity(store.len() + 1),
            links: Vec::new(),
            index: BTreeMap::new(),
            caps: BTreeMap::new(),
            degrees: Vec::new(),
            generated_at: now,
        };

        graph.push(GraphNode {
            key: NodeKey::Node(local.id),
            kind: NodeKind::Local,
            name: DisplayName::for_node(local.name.as_str(), local.id),
            // This machine. Not a decision anybody recorded, and not one
            // anybody can change from the outside.
            trust: Trust::Trusted,
            liveness: Liveness::Here,
            age_secs: None,
            address: Some(local.addr()),
            fingerprint: Some(local.id.fingerprint()),
            delegations: 0,
            accepts_work: local.caps.accepts_work,
            caps: capability_refs(&local.caps),
        });

        // A store that holds this machine's own address is not something
        // [`crate::plugins::mesh::Mesh::add_peer`] can produce, and it is one hand edit
        // of `peers.json` away. Drawing it would put the local node on screen
        // twice, hang a self-loop off it, and give it a peer's trust state
        // beside its own.
        let peers = || store.iter().filter(|peer| peer.id() != local.id);

        for peer in peers() {
            let id = peer.id();
            graph.push(GraphNode {
                key: NodeKey::Node(id),
                kind: NodeKind::Peer,
                name: DisplayName::for_node(peer.node.name.as_str(), id),
                trust: peer.trust,
                liveness: Liveness::of_peer(peer.trust, peer.presence(now)),
                age_secs: peer.staleness(now).map(|delta| delta.num_seconds()),
                address: Some(peer.node.addr()),
                fingerprint: Some(id.fingerprint()),
                delegations: peer.delegations,
                accepts_work: peer.node.caps.accepts_work,
                caps: capability_refs(&peer.node.caps),
            });
        }

        // Edges, in a second pass: an observed edge points at whoever
        // introduced the peer, and that node may come later in store order.
        let local_index = 0;
        for peer in peers() {
            let Some(&to) = graph.index.get(&NodeKey::Node(peer.id())) else {
                continue;
            };
            graph.links.push(Link {
                from: local_index,
                to,
                kind: EdgeKind::Peer,
                weight: 1,
            });
            if let Some(via) = peer.observed_via {
                // Only when the referrer is itself drawn: an edge to a vertex
                // nobody put on screen is a line into empty space.
                if let Some(&from) = graph.index.get(&NodeKey::Node(via)) {
                    graph.links.push(Link {
                        from,
                        to,
                        kind: EdgeKind::Observed,
                        weight: 1,
                    });
                }
            }
            if peer.delegations > 0 {
                graph.links.push(Link {
                    from: local_index,
                    to,
                    kind: EdgeKind::Delegation,
                    weight: peer.delegations,
                });
            }
        }

        graph.index_capabilities();
        graph.link_shared_capabilities();
        graph.flag_confusable_labels();
        graph.recount_degrees();
        graph
    }

    /// Record that `owner` is running `session`. Returns whether that changed
    /// the graph, so a caller stepping a layout knows whether to re-sync.
    ///
    /// Session streams are live state held by whoever is watching them, not
    /// something in the store, so they are added after the build. An unknown
    /// owner is ignored for the same reason a dangling observed edge is: a
    /// session hanging off nothing is a line into empty space.
    pub fn add_session(&mut self, owner: NodeId, session: &PeerText) -> bool {
        if session.is_empty() {
            return false;
        }
        let Some(&from) = self.index.get(&NodeKey::Node(owner)) else {
            return false;
        };
        let key = NodeKey::Session(owner, session.as_str().to_string());
        let mut changed = false;
        let to = match self.index.get(&key) {
            Some(&existing) => existing,
            None => {
                let host = &self.nodes[from];
                let node = GraphNode {
                    key: key.clone(),
                    kind: NodeKind::Session { owner },
                    name: DisplayName::for_session(session.as_str(), owner),
                    // A session is exactly as trusted, and exactly as live, as
                    // the node running it. It cannot be more: everything known
                    // about it was reported by that node, so a session drawn
                    // live beside a stale owner would be inventing freshness.
                    trust: host.trust,
                    liveness: host.liveness,
                    age_secs: host.age_secs,
                    address: None,
                    fingerprint: None,
                    delegations: 0,
                    accepts_work: false,
                    caps: Vec::new(),
                };
                self.push(node);
                changed = true;
                self.nodes.len() - 1
            }
        };
        let link = Link {
            from,
            to,
            kind: EdgeKind::Session,
            weight: 1,
        };
        if !self.links.contains(&link) {
            self.links.push(link);
            changed = true;
        }
        if changed {
            self.flag_confusable_labels();
            self.recount_degrees();
        }
        changed
    }

    /// Every drawn node, in layout order.
    pub fn nodes(&self) -> &[GraphNode] {
        &self.nodes
    }

    /// Every link, drawn and clustering alike.
    pub fn links(&self) -> &[Link] {
        &self.links
    }

    /// How many links touch each node, in [`MeshGraph::nodes`] order.
    ///
    /// The layout divides by this: a node that fifty springs pull on is fifty
    /// times as hard to move as one with a single link, and treating them
    /// alike is what makes the local hub oscillate forever instead of
    /// settling. See [`crate::plugins::graph::layout`]'s notes on stability.
    pub fn degrees(&self) -> &[u32] {
        &self.degrees
    }

    /// Only the links a renderer should draw a line for.
    pub fn drawn_links(&self) -> impl Iterator<Item = &Link> {
        self.links.iter().filter(|link| link.is_drawn())
    }

    /// Every link of one kind.
    pub fn links_of(&self, kind: EdgeKind) -> impl Iterator<Item = &Link> {
        self.links.iter().filter(move |link| link.kind == kind)
    }

    /// How many nodes will be drawn.
    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    /// Whether there is nothing to draw. Never true for a graph built from a
    /// store: the local node is always in it, even with no peers at all.
    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    /// The clock this snapshot was taken against. Every liveness state in it
    /// is relative to this instant and to nothing else.
    pub fn generated_at(&self) -> DateTime<Utc> {
        self.generated_at
    }

    /// Where a key sits in [`MeshGraph::nodes`], for the layout.
    pub fn index_of(&self, key: &NodeKey) -> Option<usize> {
        self.index.get(key).copied()
    }

    /// One node, by key.
    pub fn node(&self, key: &NodeKey) -> Option<&GraphNode> {
        self.index_of(key).map(|index| &self.nodes[index])
    }

    /// How many nodes are in one liveness state, for the header count.
    pub fn count_of(&self, liveness: Liveness) -> usize {
        self.nodes
            .iter()
            .filter(|node| node.liveness == liveness)
            .count()
    }

    /// Every capability anybody advertises, with the nodes advertising it.
    ///
    /// Ordered, so the filter's list does not reshuffle between frames.
    pub fn capabilities(&self) -> impl Iterator<Item = (CapabilityKind, &str, &[usize])> {
        self.caps
            .iter()
            .map(|((kind, name), nodes)| (*kind, name.as_str(), nodes.as_slice()))
    }

    /// The nodes advertising one capability. Empty when nobody does.
    pub fn advertisers(&self, kind: CapabilityKind, name: &str) -> &[usize] {
        // The key is borrowed as a pair, which a BTreeMap cannot look up
        // without allocating the owned tuple, so this walks the (small,
        // ordered) index instead of building a String per frame.
        self.caps
            .iter()
            .find(|((k, n), _)| *k == kind && n == name)
            .map(|(_, nodes)| nodes.as_slice())
            .unwrap_or(&[])
    }

    /// Everything an inspector panel shows for one node.
    pub fn inspect(&self, key: &NodeKey) -> Option<Inspection<'_>> {
        let index = self.index_of(key)?;
        let node = &self.nodes[index];
        let sessions = self
            .links_of(EdgeKind::Session)
            .filter(|link| link.from == index)
            .map(|link| &self.nodes[link.to])
            .collect();
        let introduced = self
            .links_of(EdgeKind::Observed)
            .filter(|link| link.from == index)
            .map(|link| &self.nodes[link.to])
            .collect();
        let introduced_by = self
            .links_of(EdgeKind::Observed)
            .find(|link| link.to == index)
            .map(|link| &self.nodes[link.from]);
        Some(Inspection {
            node,
            sessions,
            introduced,
            introduced_by,
            // There is trust to take away only from a peer that has some. A
            // Known peer's "revoke" would be a button that does nothing.
            revocable: node.kind == NodeKind::Peer && node.trust.may_send_work(),
        })
    }

    fn push(&mut self, node: GraphNode) {
        self.index.insert(node.key.clone(), self.nodes.len());
        self.nodes.push(node);
    }

    fn index_capabilities(&mut self) {
        for (index, node) in self.nodes.iter().enumerate() {
            for cap in &node.caps {
                self.caps
                    .entry((cap.kind, cap.name.clone()))
                    .or_default()
                    .push(index);
            }
        }
    }

    /// Add the undrawn clustering links.
    ///
    /// A star through the lowest-indexed advertiser rather than every pair:
    /// `m - 1` links pull a group together as well as `m * (m - 1) / 2` do,
    /// and on a mesh where fifty peers all offer `read_file` the difference is
    /// 49 links against 1225, every frame, inside a 16ms budget.
    fn link_shared_capabilities(&mut self) {
        for nodes in self.caps.values() {
            let Some((&hub, rest)) = nodes.split_first() else {
                continue;
            };
            for &member in rest {
                self.links.push(Link {
                    from: hub,
                    to: member,
                    kind: EdgeKind::Capability,
                    weight: 1,
                });
            }
        }
    }

    /// Recount how many links touch each node.
    ///
    /// Every link kind counts, the undrawn capability ones included: a node
    /// held by twenty invisible clustering springs is exactly as hard to move
    /// as one held by twenty visible ones, and the layout is asking about
    /// inertia rather than about what is on screen.
    fn recount_degrees(&mut self) {
        self.degrees = vec![0; self.nodes.len()];
        for link in &self.links {
            for end in [link.from, link.to] {
                if let Some(degree) = self.degrees.get_mut(end) {
                    *degree += 1;
                }
            }
        }
    }

    /// Mark every label that another node's label could be mistaken for.
    fn flag_confusable_labels(&mut self) {
        let folds: Vec<String> = self
            .nodes
            .iter()
            .map(|node| confusable_fold(node.name.label()))
            .collect();
        let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
        for fold in &folds {
            *counts.entry(fold.as_str()).or_default() += 1;
        }
        let ambiguous: Vec<bool> = folds
            .iter()
            .map(|fold| counts.get(fold.as_str()).copied().unwrap_or(0) > 1)
            .collect();
        for (node, flag) in self.nodes.iter_mut().zip(ambiguous) {
            node.name.ambiguous = flag;
        }
    }
}

/// What an inspector panel shows for one node.
///
/// Borrowed from the graph rather than copied out of it: the panel is redrawn
/// from the same snapshot the canvas is drawing, so the two cannot disagree
/// about whether a peer is live.
#[derive(Debug)]
pub struct Inspection<'a> {
    pub node: &'a GraphNode,
    /// Sessions running on this node, as the explorer has been told.
    pub sessions: Vec<&'a GraphNode>,
    /// Peers this node introduced.
    pub introduced: Vec<&'a GraphNode>,
    /// The peer that introduced this one, if it was not pasted in by hand.
    pub introduced_by: Option<&'a GraphNode>,
    /// Whether a revoke control applies: this is a peer and it is trusted.
    pub revocable: bool,
}

fn capability_refs(caps: &Capability) -> Vec<CapabilityRef> {
    let mut refs = Vec::with_capacity(caps.len());
    for kind in CapabilityKind::ALL {
        for entry in caps.entries(kind) {
            let name = entry.as_str().to_string();
            refs.push(CapabilityRef {
                display: bounded_label(&name),
                kind,
                name,
            });
        }
    }
    refs
}

#[cfg(test)]
mod tests {
    use chrono::TimeDelta;

    use super::*;
    use crate::plugins::mesh::peer::{FRESH_SECS, synthetic_store};
    use crate::plugins::mesh::{Capability, Identity, Peer, PeerStore};

    fn at(seconds: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(1_800_000_000 + seconds, 0).expect("timestamp")
    }

    fn id_of(byte: u8) -> NodeId {
        Identity::from_seed([byte; 32]).id()
    }

    fn local() -> Node {
        Identity::from_seed([0u8; 32]).announce("here", Capability::none())
    }

    /// A peer with a name, added at `at(0)`.
    fn named_peer(byte: u8, name: &str) -> Peer {
        let mut node = Node::new(id_of(byte));
        node.name = PeerText::sanitize(name);
        Peer::new(node, at(0))
    }

    fn store_of(peers: Vec<Peer>) -> PeerStore {
        let mut store = PeerStore::ephemeral();
        for peer in peers {
            let trust = peer.trust;
            let delegations = peer.delegations;
            let observed_via = peer.observed_via;
            let last_seen = peer.node.last_seen;
            let id = peer.id();
            store.add(peer.node.clone(), at(0));
            store.record_trust(&id, trust).expect("trust");
            for _ in 0..delegations {
                store.record_delegation(&id);
            }
            if let Some(via) = observed_via {
                store.record_observed_via(&id, via);
            }
            if let Some(seen) = last_seen {
                store.mark_seen(&id, seen);
            }
        }
        store
    }

    #[test]
    fn a_mesh_with_no_peers_is_a_graph_with_this_machine_in_it() {
        // The empty case has to be a picture, not a crash and not a blank: a
        // fresh install has no peers and still opens the explorer.
        let graph = MeshGraph::build(&local(), &PeerStore::ephemeral(), at(0));
        assert_eq!(graph.len(), 1);
        assert!(!graph.is_empty());
        assert!(graph.links().is_empty());
        let node = &graph.nodes()[0];
        assert_eq!(node.kind, NodeKind::Local);
        assert_eq!(node.liveness, Liveness::Here);
        assert!(node.liveness.is_live());
        assert_eq!(node.trust, Trust::Trusted);
        assert_eq!(node.seen_label(), "here");
        assert_eq!(graph.count_of(Liveness::Here), 1);
        assert_eq!(graph.index_of(&NodeKey::Node(local().id)), Some(0));
        assert!(graph.node(&NodeKey::Node(id_of(9))).is_none());
    }

    #[test]
    fn an_unreachable_peer_is_never_drawn_as_live() {
        // A blocked peer that announced one second ago. Presence alone says
        // Online; the model must not, because this machine will not contact
        // it and drawing it up claims a link that does not exist.
        let now = at(10_000);
        let mut peer = named_peer(1, "blocked-but-loud");
        peer.trust = Trust::Blocked;
        peer.node.last_seen = Some(now - TimeDelta::seconds(1));
        let store = store_of(vec![peer]);

        assert_eq!(store.presence(&id_of(1), now), Some(Presence::Online));
        let graph = MeshGraph::build(&local(), &store, now);
        let node = graph.node(&NodeKey::Node(id_of(1))).expect("peer");
        assert_eq!(node.liveness, Liveness::Unreachable);
        assert!(!node.liveness.is_live());
        assert_eq!(node.trust, Trust::Blocked);
        // The observation is still reported: an operator wants to know a
        // blocked peer is still knocking. It just is not "live".
        assert_eq!(node.age_secs, Some(1));
        assert_eq!(node.seen_label(), "1s");
        assert_eq!(graph.count_of(Liveness::Live), 0);
    }

    #[test]
    fn a_stale_peer_is_never_reported_live_at_any_clock() {
        let mut peer = named_peer(2, "workshop");
        peer.trust = Trust::Trusted;
        peer.node.last_seen = Some(at(0));
        let store = store_of(vec![peer]);
        let key = NodeKey::Node(id_of(2));

        // Inside the window: live. One second past it: never live again, at
        // any later clock, with nothing written to the store.
        let fresh = MeshGraph::build(&local(), &store, at(FRESH_SECS));
        assert_eq!(fresh.node(&key).expect("peer").liveness, Liveness::Live);
        for seconds in [FRESH_SECS + 1, 600, 86_400, 86_400 * 400] {
            let graph = MeshGraph::build(&local(), &store, at(seconds));
            let node = graph.node(&key).expect("peer");
            assert_eq!(node.liveness, Liveness::Stale, "at {seconds}s");
            assert!(!node.liveness.is_live(), "at {seconds}s");
        }
        assert_eq!(
            MeshGraph::build(&local(), &store, at(4 * 86_400))
                .node(&key)
                .expect("peer")
                .seen_label(),
            "4d"
        );
    }

    #[test]
    fn liveness_is_never_live_without_presence_and_trust_agreeing() {
        // The invariant, over every combination the synthetic mesh produces,
        // at several clocks: a node the model calls live is a node that was
        // seen inside the window and that this machine is allowed to contact.
        let built = at(0);
        let store = synthetic_store(50, 3, built);
        for offset in [0, 30, FRESH_SECS, 5_000, 400_000] {
            let now = at(offset);
            let graph = MeshGraph::build(&local(), &store, now);
            for node in graph.nodes() {
                let NodeKey::Node(id) = &node.key else {
                    continue;
                };
                if node.kind == NodeKind::Local {
                    continue;
                }
                let presence = store.presence(id, now).expect("peer");
                if node.liveness.is_live() {
                    assert_eq!(presence, Presence::Online, "{node:?} at {offset}s");
                    assert!(node.trust.may_contact(), "{node:?} at {offset}s");
                }
                if !node.trust.may_contact() {
                    assert_eq!(node.liveness, Liveness::Unreachable, "{node:?}");
                }
            }
        }
    }

    #[test]
    fn two_confusable_names_stay_distinguishable() {
        // The attack: a peer names itself with a Cyrillic 'а' so it renders
        // exactly like one the operator already trusts.
        let honest = named_peer(3, "workshop");
        let mut impostor = named_peer(4, "w\u{043e}rkshop");
        impostor.trust = Trust::Blocked;
        let store = store_of(vec![honest, impostor]);
        let graph = MeshGraph::build(&local(), &store, at(0));

        let first = graph.node(&NodeKey::Node(id_of(3))).expect("peer");
        let second = graph.node(&NodeKey::Node(id_of(4))).expect("peer");
        assert_ne!(
            first.name.label(),
            second.name.label(),
            "the two labels differ only by a homoglyph"
        );
        // Told apart by something the peer does not choose.
        assert_ne!(first.name.discriminator(), second.name.discriminator());
        assert_ne!(first.name.rendered(), second.name.rendered());
        assert!(
            first.name.rendered().contains(first.name.discriminator()),
            "{}",
            first.name.rendered()
        );
        // …and flagged, so a renderer can lean on it.
        assert!(first.name.is_ambiguous(), "{first:?}");
        assert!(second.name.is_ambiguous(), "{second:?}");
        // Trust stays unambiguous from the model alone: the look-alike is the
        // blocked one and nothing about the name can change that.
        assert_eq!(first.trust, Trust::Known);
        assert_eq!(second.trust, Trust::Blocked);
        assert_eq!(second.liveness, Liveness::Unreachable);

        // An exact duplicate name is flagged too, and a unique one is not.
        let both = store_of(vec![named_peer(5, "twin"), named_peer(6, "twin")]);
        let graph = MeshGraph::build(&local(), &both, at(0));
        assert!(
            graph
                .node(&NodeKey::Node(id_of(5)))
                .expect("peer")
                .name
                .is_ambiguous()
        );
        let alone = store_of(vec![named_peer(7, "unique")]);
        let graph = MeshGraph::build(&local(), &alone, at(0));
        assert!(
            !graph
                .node(&NodeKey::Node(id_of(7)))
                .expect("peer")
                .name
                .is_ambiguous()
        );
    }

    #[test]
    fn a_peer_cannot_forge_another_peers_discriminator() {
        // Naming yourself after somebody else's fingerprint is the obvious
        // dodge once the discriminator is always rendered. The separator does
        // not survive a label, so the last one in a rendered name is always
        // the real key.
        let real = discriminator_of(id_of(3));
        let forged = format!("workshop {DISCRIMINATOR_SEPARATOR} {real}");
        let store = store_of(vec![named_peer(8, &forged)]);
        let graph = MeshGraph::build(&local(), &store, at(0));
        let node = graph.node(&NodeKey::Node(id_of(8))).expect("peer");
        assert!(
            !node.name.label().contains(DISCRIMINATOR_SEPARATOR),
            "{:?}",
            node.name.label()
        );
        let rendered = node.name.rendered();
        let (_, tail) = rendered
            .rsplit_once(DISCRIMINATOR_SEPARATOR)
            .expect("separator");
        assert_eq!(tail.trim(), node.name.discriminator());
        assert_ne!(tail.trim(), real);
    }

    #[test]
    fn a_hostile_name_cannot_take_the_screen() {
        let cases = [
            // Long: bounded in columns.
            "n".repeat(400),
            // Wide: bounded in columns, not characters.
            "字".repeat(200),
            // Zero-width: bounded in characters, since it is nought columns.
            "a\u{0301}".repeat(200),
            // Invisible formatting, including the Tag block and the BOM.
            format!("wiz{}ard{}", '\u{200b}', '\u{e0041}'),
            "\u{feff}\u{2060}\u{180e}ghost".to_string(),
        ];
        for raw in cases {
            let label = bounded_label(&raw);
            let columns: usize = label.chars().map(|ch| ch.width().unwrap_or(0)).sum();
            assert!(columns <= MAX_LABEL_COLUMNS, "{columns} columns: {label:?}");
            assert!(
                label.chars().count() <= MAX_LABEL_COLUMNS,
                "{} chars: {label:?}",
                label.chars().count()
            );
            assert!(
                !label.chars().any(is_invisible_format),
                "invisible formatting survived: {label:?}"
            );
            assert!(!label.chars().any(char::is_control), "{label:?}");
        }
        assert_eq!(bounded_label("wiz\u{200b}ard"), "wiz ard");
        assert_eq!(bounded_label("\u{feff}\u{2060}\u{180e}ghost"), "ghost");
        assert!(bounded_label(&"n".repeat(400)).ends_with('…'));
    }

    #[test]
    fn a_node_with_nothing_to_show_still_has_a_label() {
        // A name that sanitises away must not leave an unlabelled dot. The
        // fallback is this layer's own, so it holds for the characters the
        // mesh sanitiser keeps as well as the ones it drops.
        let peer = named_peer(9, "\u{202e}\u{0007}\u{200b}");
        let store = store_of(vec![peer]);
        let graph = MeshGraph::build(&local(), &store, at(0));
        let node = graph.node(&NodeKey::Node(id_of(9))).expect("peer");
        assert_eq!(node.name.label(), id_of(9).short());
        assert!(!node.name.label().is_empty());
        assert_eq!(
            node.name.discriminator().chars().count(),
            DISCRIMINATOR_CHARS
        );
        assert!(
            node.name.rendered().contains(node.name.discriminator()),
            "{:?}",
            node.name.rendered()
        );
        assert_eq!(node.fingerprint, Some(id_of(9).fingerprint()));
    }

    #[test]
    fn this_machines_own_address_in_the_store_is_not_drawn_as_a_peer() {
        // Not reachable through `Mesh::add_peer`, which refuses it, and one
        // hand edit of `peers.json` away. Two vertices for one key would make
        // `index_of` point at whichever came last and give this machine a
        // peer's trust state beside its own.
        let local = local();
        let mut store = PeerStore::ephemeral();
        store.add(local.clone(), at(0));
        store
            .record_trust(&local.id, Trust::Blocked)
            .expect("block");

        let graph = MeshGraph::build(&local, &store, at(0));
        assert_eq!(graph.len(), 1);
        assert_eq!(graph.nodes()[0].kind, NodeKind::Local);
        assert_eq!(graph.nodes()[0].trust, Trust::Trusted);
        assert_eq!(graph.nodes()[0].liveness, Liveness::Here);
        assert!(graph.links().is_empty(), "and no self-loop");
    }

    #[test]
    fn every_edge_kind_the_plan_names_is_derivable() {
        let mut first = named_peer(10, "alpha");
        first.trust = Trust::Trusted;
        first.delegations = 4;
        first.node.caps = Capability::advertise(&["qwen3.6:27b"], &["read_file"], &[], &[], true);
        let mut second = named_peer(11, "beta");
        second.observed_via = Some(id_of(10));
        second.node.caps = Capability::advertise(&["qwen3.6:27b"], &[], &[], &[], false);
        let store = store_of(vec![first, second]);

        let mut graph = MeshGraph::build(&local(), &store, at(0));
        assert!(graph.add_session(id_of(10), &PeerText::sanitize("session-42")));
        assert!(
            !graph.add_session(id_of(10), &PeerText::sanitize("session-42")),
            "the same session twice is one vertex"
        );
        assert!(
            !graph.add_session(id_of(99), &PeerText::sanitize("orphan")),
            "a session on a node nobody drew is a line into empty space"
        );
        assert!(!graph.add_session(id_of(10), &PeerText::sanitize("  ")));

        assert_eq!(graph.links_of(EdgeKind::Peer).count(), 2);
        assert_eq!(graph.links_of(EdgeKind::Observed).count(), 1);
        let delegation: Vec<_> = graph.links_of(EdgeKind::Delegation).collect();
        assert_eq!(delegation.len(), 1);
        assert_eq!(delegation[0].weight, 4);
        assert_eq!(graph.links_of(EdgeKind::Session).count(), 1);
        // The shared model clusters the two peers without drawing a line.
        let capability: Vec<_> = graph.links_of(EdgeKind::Capability).collect();
        assert_eq!(capability.len(), 1, "{capability:?}");
        assert!(!capability[0].is_drawn());
        assert_eq!(
            graph.drawn_links().count(),
            graph.links().len() - 1,
            "everything except the capability link is drawn"
        );
        // A capability only one node offers clusters nothing.
        assert_eq!(
            graph.advertisers(CapabilityKind::Tool, "read_file").len(),
            1
        );
        assert_eq!(
            graph
                .advertisers(CapabilityKind::Model, "qwen3.6:27b")
                .len(),
            2
        );
        assert!(graph.advertisers(CapabilityKind::Skill, "nope").is_empty());
        assert!(
            graph
                .capabilities()
                .any(|(kind, name, nodes)| kind == CapabilityKind::Model
                    && name == "qwen3.6:27b"
                    && nodes.len() == 2)
        );
    }

    #[test]
    fn a_session_is_never_more_live_than_the_node_running_it() {
        let mut peer = named_peer(12, "gone");
        peer.trust = Trust::Trusted;
        peer.node.last_seen = Some(at(0));
        let store = store_of(vec![peer]);

        let mut graph = MeshGraph::build(&local(), &store, at(FRESH_SECS + 600));
        assert!(graph.add_session(id_of(12), &PeerText::sanitize("s-1")));
        let session = graph
            .node(&NodeKey::Session(id_of(12), "s-1".to_string()))
            .expect("session");
        assert_eq!(session.liveness, Liveness::Stale);
        assert!(!session.liveness.is_live());
        assert_eq!(session.kind, NodeKind::Session { owner: id_of(12) });
        assert_eq!(session.trust, Trust::Trusted, "it inherits its owner's");
        assert!(session.address.is_none(), "a session is not addressable");

        // And a session on a blocked owner is unreachable, not live.
        let mut blocked = named_peer(13, "blocked");
        blocked.trust = Trust::Blocked;
        blocked.node.last_seen = Some(at(0));
        let store = store_of(vec![blocked]);
        let mut graph = MeshGraph::build(&local(), &store, at(1));
        assert!(graph.add_session(id_of(13), &PeerText::sanitize("s-2")));
        assert_eq!(
            graph
                .node(&NodeKey::Session(id_of(13), "s-2".to_string()))
                .expect("session")
                .liveness,
            Liveness::Unreachable
        );
    }

    #[test]
    fn two_peers_reporting_the_same_session_id_are_two_sessions() {
        // A session id is peer-supplied text, `main` is what everybody calls
        // their session, and one vertex for both would draw two machines
        // collaborating on something that does not exist. It is also the
        // cheapest impersonation on the graph: a peer that names its session
        // after a trusted peer's would hang itself off that peer's vertex.
        let mut honest = named_peer(20, "alpha");
        honest.trust = Trust::Trusted;
        let mut hostile = named_peer(21, "beta");
        hostile.trust = Trust::Blocked;
        let store = store_of(vec![honest, hostile]);
        let mut graph = MeshGraph::build(&local(), &store, at(0));

        let session = PeerText::sanitize("main");
        assert!(graph.add_session(id_of(20), &session));
        assert!(graph.add_session(id_of(21), &session));

        let first = graph
            .node(&NodeKey::Session(id_of(20), "main".to_string()))
            .expect("the trusted peer's session");
        let second = graph
            .node(&NodeKey::Session(id_of(21), "main".to_string()))
            .expect("the blocked peer's session");
        assert_ne!(first.key, second.key, "two sessions, not one");
        assert_eq!(first.kind, NodeKind::Session { owner: id_of(20) });
        assert_eq!(second.kind, NodeKind::Session { owner: id_of(21) });
        // Each carries its own owner's state, which is the whole reason they
        // must not share a vertex.
        assert_eq!(first.trust, Trust::Trusted);
        assert_eq!(second.trust, Trust::Blocked);
        assert_eq!(second.liveness, Liveness::Unreachable);
        assert_eq!(graph.links_of(EdgeKind::Session).count(), 2);
        // Told apart on screen by the owner's fingerprint, since the label is
        // the same word twice.
        assert_eq!(first.name.label(), second.name.label());
        assert_ne!(first.name.discriminator(), second.name.discriminator());
        // Each session hangs off its own owner and nobody else's.
        for (owner, session) in [(id_of(20), first), (id_of(21), second)] {
            let inspection = graph.inspect(&NodeKey::Node(owner)).expect("inspection");
            assert_eq!(inspection.sessions.len(), 1);
            assert_eq!(inspection.sessions[0].key, session.key);
        }
    }

    #[test]
    fn a_degree_is_counted_for_every_link_that_touches_a_node() {
        // The layout divides by this, so an undercount is a node that jitters
        // and an overcount is one that will not move.
        let mut peer = named_peer(22, "alpha");
        peer.delegations = 3;
        let store = store_of(vec![peer]);
        let mut graph = MeshGraph::build(&local(), &store, at(0));
        assert_eq!(graph.degrees().len(), graph.len());
        // The peer edge and the delegation edge, at both ends.
        assert_eq!(graph.degrees(), &[2, 2]);

        assert!(graph.add_session(id_of(22), &PeerText::sanitize("s-1")));
        assert_eq!(
            graph.degrees(),
            &[2, 3, 1],
            "a session adds one at each end"
        );
        assert!(
            !graph.add_session(id_of(22), &PeerText::sanitize("s-1")),
            "the same session twice changes nothing"
        );
        assert_eq!(graph.degrees(), &[2, 3, 1]);

        // Every link is counted exactly twice, capability links included.
        let now = at(0);
        let graph = MeshGraph::build(&local(), &synthetic_store(20, 2, now), now);
        assert_eq!(
            graph
                .degrees()
                .iter()
                .map(|degree| *degree as usize)
                .sum::<usize>(),
            graph.links().len() * 2
        );
    }

    #[test]
    fn the_inspector_has_what_the_panel_needs() {
        let mut first = named_peer(14, "alpha");
        first.trust = Trust::Trusted;
        first.node.caps = Capability::advertise(&["qwen3.6:27b"], &[], &["research"], &[], true);
        let mut second = named_peer(15, "beta");
        second.observed_via = Some(id_of(14));
        let store = store_of(vec![first, second]);
        let mut graph = MeshGraph::build(&local(), &store, at(0));
        graph.add_session(id_of(14), &PeerText::sanitize("s-9"));

        let inspection = graph
            .inspect(&NodeKey::Node(id_of(14)))
            .expect("inspection");
        assert_eq!(inspection.node.trust, Trust::Trusted);
        assert!(inspection.revocable, "a trusted peer has trust to revoke");
        assert_eq!(inspection.sessions.len(), 1);
        assert_eq!(inspection.introduced.len(), 1);
        assert!(inspection.introduced_by.is_none());
        assert!(
            inspection
                .node
                .advertises(CapabilityKind::Skill, "research")
        );
        assert!(
            !inspection
                .node
                .advertises(CapabilityKind::Model, "research")
        );
        assert!(inspection.node.accepts_work, "the claim is recorded");

        let second = graph
            .inspect(&NodeKey::Node(id_of(15)))
            .expect("inspection");
        assert!(!second.revocable, "a known peer has nothing to revoke");
        assert_eq!(
            second.introduced_by.map(|node| node.name.label()),
            Some("alpha")
        );
        assert!(graph.inspect(&NodeKey::Node(id_of(99))).is_none());
    }

    #[test]
    fn the_same_store_and_clock_build_the_same_graph() {
        // The layout is snapshot-tested against this, so the input has to be
        // reproducible before the output can be.
        let now = at(1_234);
        let store = synthetic_store(50, 5, now);
        let graph = MeshGraph::build(&local(), &store, now);
        assert_eq!(MeshGraph::build(&local(), &store, now), graph);
        assert_eq!(graph.len(), 51, "the local node plus fifty peers");
        assert_eq!(graph.links_of(EdgeKind::Peer).count(), 50);
        assert!(graph.links_of(EdgeKind::Capability).count() > 0);
        for state in [Liveness::Live, Liveness::Stale, Liveness::Unseen] {
            assert!(graph.count_of(state) > 0, "no synthetic node is {state:?}");
        }
        assert_eq!(graph.count_of(Liveness::Here), 1);
    }
}
