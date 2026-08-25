//! The peer store: who this machine has been told about, what it decided
//! about them, and how long ago it last heard from them.
//!
//! Three properties the graph explorer depends on, in the order they matter:
//!
//! 1. **The decision is a human's and it is on disk.** Trust is three-state
//!    ([`Trust`]), it is never inferred from behaviour, and a peer that was
//!    added is *not* thereby trusted. Re-adding a blocked peer does not
//!    un-block it; nothing a peer says about itself moves the dial.
//! 2. **Staleness is reported, never rounded off.** The plan asks the graph to
//!    render from cached state with the network down, and "a graph that is
//!    beautiful and lies about who is online is worse than a plain one that
//!    does not". So [`Peer::presence`] takes the current time as an argument
//!    and can answer [`Presence::Unseen`]: a pasted address that has never
//!    answered renders as never-seen, not as offline and not as a dot like
//!    every other dot.
//! 3. **The file is the record.** It survives restarts, it is written the same
//!    way a secret is (see [`PeerStore::save`]), and it holds nothing a peer
//!    can grow without bound.
//!
//! Discovery is manual: an address is pasted in. There is no DHT, no bootstrap
//! list and no rendezvous server, so the store never grows except by somebody
//! deciding it should.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use chrono::{DateTime, TimeDelta, Utc};
use clap::ValueEnum;
use serde::{Deserialize, Serialize};

use super::PeerText;
use super::capability::{Capability, LimitExceeded, Limits, Meter};
use super::node::{Identity, Node, NodeId};
use crate::platform::secrets;

/// On-disk format version for `peers.json`. Bump on an incompatible change.
const STORE_VERSION: u32 = 1;

/// A peer heard from within this many seconds is [`Presence::Online`].
///
/// Presence is a claim about *now*, so the window is short: a node that
/// announced two minutes ago may have been powered off one minute ago, and
/// drawing it as online would be the exact lie the plan calls out.
pub const FRESH_SECS: i64 = 90;

/// How far into the future a `last_seen` may sit before it is treated as
/// stale rather than as freshly seen.
///
/// Peer clocks are not this machine's clock. A little skew is ordinary and
/// should not flicker a node offline; a `last_seen` in the middle of next year
/// would otherwise pin a node "online" forever, which is the failure mode that
/// actually matters.
pub const SKEW_GRACE_SECS: i64 = 300;

/// `~/.wizard/mesh/`: the peer store lives here, beside nothing else yet.
pub fn mesh_dir(wizard_dir: &Path) -> PathBuf {
    wizard_dir.join("mesh")
}

/// `~/.wizard/mesh/peers.json`.
pub fn store_path(wizard_dir: &Path) -> PathBuf {
    mesh_dir(wizard_dir).join("peers.json")
}

// ---------------------------------------------------------------------------
// Trust
// ---------------------------------------------------------------------------

/// The recorded human decision about one peer.
///
/// Shaped after [`crate::trust::Status`], which answers the same question for
/// project files: a decision, made by a person, recorded on disk, defaulting
/// to no. The difference is that a project's decision has two answers and an
/// "unknown"; a peer's has three real answers, because [`Trust::Blocked`] must
/// survive the peer coming back with a new announcement, and "not decided yet"
/// must not.
/// `clap::ValueEnum` is derived here rather than mirrored into a separate CLI
/// type, for the same reason [`crate::config::Mode`] derives it: a second
/// enum on the argument-parsing side is a place for a fourth state (or a
/// missing third) to appear, and this one decides whether another machine may
/// run work here. Its value strings are the variant names lowercased, which is
/// exactly what [`Trust::label`] prints, so what `wizard peers list` shows and
/// what `wizard peers trust` accepts cannot drift apart.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, ValueEnum,
)]
#[serde(rename_all = "snake_case")]
#[clap(rename_all = "snake_case")]
pub enum Trust {
    /// Explicitly refused. Nothing is fetched from it, nothing is sent to it,
    /// and any live subscription is dropped (see [`super::Mesh::set_trust`]).
    Blocked,
    /// Known to exist, not approved for anything. The state a pasted address
    /// lands in, and the default for any record that does not say otherwise.
    #[default]
    Known,
    /// Approved by a human, with limits.
    Trusted,
}

impl Trust {
    /// May this machine send work to the peer?
    pub fn may_send_work(self) -> bool {
        matches!(self, Trust::Trusted)
    }

    /// May this machine run work the peer submits?
    ///
    /// The same answer as [`Trust::may_send_work`] today, and a separate
    /// method on purpose: they are different questions with different blast
    /// radii, and the day one of them grows a middle ground the other must not
    /// follow it by accident.
    pub fn may_accept_work(self) -> bool {
        matches!(self, Trust::Trusted)
    }

    /// May this machine talk to the peer at all (fetch its capability,
    /// subscribe to its events)?
    pub fn may_contact(self) -> bool {
        !matches!(self, Trust::Blocked)
    }

    /// Lower-case label, for the graph explorer's legend.
    pub fn label(self) -> &'static str {
        match self {
            Trust::Blocked => "blocked",
            Trust::Known => "known",
            Trust::Trusted => "trusted",
        }
    }
}

// ---------------------------------------------------------------------------
// Presence
// ---------------------------------------------------------------------------

/// What the store can honestly say about a peer being reachable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Presence {
    /// Heard from within [`FRESH_SECS`].
    Online,
    /// Heard from once, but not recently. Carries how long ago, so the
    /// explorer can render "3h" rather than a colour.
    Stale,
    /// Never heard from. A pasted address that has not answered yet.
    Unseen,
}

impl Presence {
    /// Lower-case label, for the graph explorer's legend.
    pub fn label(self) -> &'static str {
        match self {
            Presence::Online => "online",
            Presence::Stale => "stale",
            Presence::Unseen => "unseen",
        }
    }
}

// ---------------------------------------------------------------------------
// Peer
// ---------------------------------------------------------------------------

/// One entry in the store: a node, the decision about it, and when it arrived.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Peer {
    /// The node itself. Its `id` is the store's primary key.
    pub node: Node,
    /// The recorded decision. Defaults to [`Trust::Known`] for any record that
    /// omits it, which is the deny-side default: `Known` may not be sent work
    /// and may not submit any.
    #[serde(default)]
    pub trust: Trust,
    /// When this peer was added here.
    pub added_at: DateTime<Utc>,
    /// What this peer is allowed to spend. Per peer, because trust is per
    /// peer.
    #[serde(default)]
    pub limits: Limits,
    /// How many delegations have gone to this peer. The weight on the graph's
    /// delegation edge, and a number an operator can sanity-check a bill
    /// against.
    #[serde(default)]
    pub delegations: u32,
    /// Which peer this one was learned from, when it was not pasted in by
    /// hand. Draws the graph's `observed` edge; `None` for a manual add.
    #[serde(default)]
    pub observed_via: Option<NodeId>,
}

impl Peer {
    /// A newly added peer: known, not trusted, never seen, default limits.
    pub fn new(node: Node, added_at: DateTime<Utc>) -> Self {
        Self {
            node,
            trust: Trust::default(),
            added_at,
            limits: Limits::default(),
            delegations: 0,
            observed_via: None,
        }
    }

    /// This peer's id, which is also its address.
    pub fn id(&self) -> NodeId {
        self.node.id
    }

    /// How long ago this peer was last heard from, or `None` if never.
    ///
    /// Negative deltas (a peer whose clock runs ahead) are reported as zero:
    /// "seen -4s ago" is not a thing to put in front of a person.
    pub fn staleness(&self, now: DateTime<Utc>) -> Option<TimeDelta> {
        self.node.last_seen.map(|seen| {
            let delta = now.signed_duration_since(seen);
            if delta < TimeDelta::zero() {
                TimeDelta::zero()
            } else {
                delta
            }
        })
    }

    /// Whether this peer counts as reachable right now, from cached state
    /// alone. `now` is a parameter so the answer is a pure function of the
    /// record and the clock, testable with a frozen one.
    pub fn presence(&self, now: DateTime<Utc>) -> Presence {
        let Some(seen) = self.node.last_seen else {
            return Presence::Unseen;
        };
        let seconds = now.signed_duration_since(seen).num_seconds();
        if seconds < -SKEW_GRACE_SECS {
            // Further ahead than clock skew explains. Something wrote a
            // timestamp this machine has no reason to believe, so it is not
            // going to claim the peer is up because of it.
            return Presence::Stale;
        }
        if seconds <= FRESH_SECS {
            Presence::Online
        } else {
            Presence::Stale
        }
    }
}

// ---------------------------------------------------------------------------
// Store
// ---------------------------------------------------------------------------

/// The persisted file: a version and the peer list.
#[derive(Serialize, Deserialize)]
struct StoreFile {
    version: u32,
    peers: Vec<Peer>,
}

/// The local peer store.
///
/// Ordered by [`NodeId`] so the file, the iteration order, and the graph's
/// layout are all stable across runs: a graph whose nodes jump every time it
/// is opened is a graph nobody can read.
pub struct PeerStore {
    /// Where [`PeerStore::save`] writes. `None` for an ephemeral store (the
    /// synthetic mesh, tests), whose `save` refuses loudly instead of silently
    /// dropping the write.
    path: Option<PathBuf>,
    peers: BTreeMap<NodeId, Peer>,
    /// Rate/cost accounting, per peer, for this process only. Not part of the
    /// file; see [`Meter`].
    meters: BTreeMap<NodeId, Meter>,
}

/// Where it lives and how much is in it. Not the peers themselves: a store
/// with fifty of them turns one panic message into a screenful, and the
/// interesting facts about an individual peer are on [`Peer`], which prints in
/// full.
impl std::fmt::Debug for PeerStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PeerStore")
            .field("path", &self.path)
            .field("peers", &self.peers.len())
            .finish()
    }
}

impl PeerStore {
    /// Load the store for `wizard_dir`. A missing file is an empty store, not
    /// an error: no peers yet is the normal state of a fresh install.
    pub fn load(wizard_dir: &Path) -> Result<Self> {
        let path = store_path(wizard_dir);
        let peers = match std::fs::read(&path) {
            Ok(raw) => {
                let file: StoreFile = serde_json::from_slice(&raw)
                    .with_context(|| format!("parsing {}", path.display()))?;
                if file.version != STORE_VERSION {
                    return Err(anyhow!(
                        "{} is version {} and this wizard understands version {STORE_VERSION} \
                         (update wizard on this machine)",
                        path.display(),
                        file.version
                    ));
                }
                file.peers
                    .into_iter()
                    .map(|peer| (peer.id(), peer))
                    .collect()
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => BTreeMap::new(),
            Err(err) => return Err(anyhow!(err).context(format!("reading {}", path.display()))),
        };
        Ok(Self {
            path: Some(path),
            peers,
            meters: BTreeMap::new(),
        })
    }

    /// A store with no file behind it, for the synthetic mesh and for tests.
    pub fn ephemeral() -> Self {
        Self {
            path: None,
            peers: BTreeMap::new(),
            meters: BTreeMap::new(),
        }
    }

    /// Write the store back.
    ///
    /// Through [`secrets::write_private_atomic`], for two reasons. The list of
    /// machines a person trusts is not something other local users need to
    /// read, and this file is the *only* thing in `~/.wizard/mesh/`, so unlike
    /// `~/.wizard` itself (which holds `node.key`, but also skills, logs and
    /// everything else) there is nothing here that a strict owner-only
    /// directory could inconvenience. Atomic, so a crash mid-write cannot
    /// leave a truncated trust list that reads back as "no peers are blocked".
    pub fn save(&self) -> Result<()> {
        let path = self.path.as_ref().ok_or_else(|| {
            anyhow!(
                "this peer store is ephemeral (synthetic or test data) and has no file to save to"
            )
        })?;
        let file = StoreFile {
            version: STORE_VERSION,
            peers: self.peers.values().cloned().collect(),
        };
        let bytes = serde_json::to_vec_pretty(&file).context("serialising the peer store")?;
        secrets::write_private_atomic(path, &bytes)
            .with_context(|| format!("writing {}", path.display()))
    }

    /// Where this store persists, if anywhere.
    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    /// Add a node, or fold a fresh announcement into the record already held.
    ///
    /// Returns the peer's trust *after* the call, which for an existing peer
    /// is whatever the human decided earlier. That is the security property
    /// here: an announcement updates what a node claims (name, capability) and
    /// never what this machine decided about it, so re-adding a blocked peer
    /// does not un-block it and a peer cannot promote itself by announcing
    /// again.
    ///
    /// `last_seen` is the exception, and it is not a claim that gets stored.
    /// A peer's clock is a peer's claim, exactly as
    /// [`super::PeerEvent::at`] says: what is recorded is `now`, this
    /// machine's clock, because an announcement arriving is an observation
    /// *this* machine made. Storing the claim instead would let a peer
    /// announce `last_seen` five minutes in the future and sit inside
    /// [`SKEW_GRACE_SECS`] plus [`FRESH_SECS`] of false [`Presence::Online`]
    /// long after it went dark, which is the one lie the presence model exists
    /// to prevent. An announcement carrying no `last_seen` at all (a pasted
    /// address, [`Node::from_address`]) is not an observation and does not
    /// become one.
    pub fn add(&mut self, mut node: Node, now: DateTime<Utc>) -> Trust {
        if node.last_seen.is_some() {
            node.last_seen = Some(now);
        }
        match self.peers.get_mut(&node.id) {
            Some(existing) => {
                existing.node.name = node.name;
                existing.node.caps = node.caps;
                if let Some(seen) = node.last_seen {
                    existing.node.last_seen = Some(seen);
                }
                existing.trust
            }
            None => {
                let peer = Peer::new(node, now);
                let trust = peer.trust;
                self.peers.insert(peer.id(), peer);
                trust
            }
        }
    }

    /// Record the decision about a peer.
    ///
    /// The recording half only. Use [`super::Mesh::set_trust`] unless there is
    /// no live transport in reach: a downgrade also has to drop the peer's
    /// live subscriptions, and a revocation that leaves a stream running is
    /// not a revocation.
    pub fn record_trust(&mut self, id: &NodeId, trust: Trust) -> Result<()> {
        let peer = self
            .peers
            .get_mut(id)
            .ok_or_else(|| anyhow!("no peer {} in the store", id.short()))?;
        peer.trust = trust;
        // A peer that is no longer trusted starts from a clean meter if it is
        // ever trusted again, rather than inheriting a spent budget.
        if !trust.may_accept_work() {
            self.meters.remove(id);
        }
        Ok(())
    }

    /// Replace a peer's spending limits.
    pub fn set_limits(&mut self, id: &NodeId, limits: Limits) -> Result<()> {
        let peer = self
            .peers
            .get_mut(id)
            .ok_or_else(|| anyhow!("no peer {} in the store", id.short()))?;
        peer.limits = limits;
        Ok(())
    }

    /// Note that `id` was heard from at `now`. Unknown ids are ignored: a node
    /// nobody added is not a peer, and announcing does not make it one.
    pub fn mark_seen(&mut self, id: &NodeId, now: DateTime<Utc>) {
        if let Some(peer) = self.peers.get_mut(id) {
            peer.node.last_seen = Some(now);
        }
    }

    /// Fold a peer's fresh announcement into its record: everything the node
    /// *claims*, which is its name and its capability, and nothing else.
    ///
    /// Not `last_seen`, even though the announcement carries a field for it:
    /// that a record arrived is an observation this machine made, so it is
    /// [`PeerStore::mark_seen`]'s to record with the local clock. Not the trust
    /// decision either, for the reason [`PeerStore::add`] gives at length:
    /// nothing a peer says about itself moves that dial.
    ///
    /// Keyed on `announced.id`, and an error for a node that is not already a
    /// peer. An announcement is not an introduction: [`super::Mesh::add_peer`]
    /// is, and a stranger that announces at this machine must not thereby
    /// appear in its store.
    pub fn record_announcement(&mut self, announced: &Node) -> Result<()> {
        let peer = self
            .peers
            .get_mut(&announced.id)
            .ok_or_else(|| anyhow!("no peer {} in the store", announced.id.short()))?;
        peer.node.name = announced.name.clone();
        peer.node.caps = announced.caps.clone();
        Ok(())
    }

    /// Count one delegation to this peer; the weight on the graph's delegation
    /// edge. Saturating, so a long-lived store cannot wrap the counter.
    pub fn record_delegation(&mut self, id: &NodeId) {
        if let Some(peer) = self.peers.get_mut(id) {
            peer.delegations = peer.delegations.saturating_add(1);
        }
    }

    /// Record that `id` was learned from `via`, for the graph's observed edge.
    pub fn record_observed_via(&mut self, id: &NodeId, via: NodeId) {
        if let Some(peer) = self.peers.get_mut(id) {
            peer.observed_via = Some(via);
        }
    }

    /// Forget a peer entirely, including its meter. Returns whether there was
    /// one. Note that forgetting is *not* blocking: the next paste of the same
    /// address lands back at [`Trust::Known`].
    pub fn forget(&mut self, id: &NodeId) -> bool {
        self.meters.remove(id);
        self.peers.remove(id).is_some()
    }

    /// One peer, by id.
    pub fn get(&self, id: &NodeId) -> Option<&Peer> {
        self.peers.get(id)
    }

    /// The recorded decision about a peer. An unknown node is [`Trust::Known`]
    /// only in the sense that it may be contacted; it may not be sent work and
    /// [`super::Mesh::admit`] refuses it by name.
    pub fn trust_of(&self, id: &NodeId) -> Option<Trust> {
        self.peers.get(id).map(|peer| peer.trust)
    }

    /// Presence of one peer from cached state. `None` when it is not a peer.
    pub fn presence(&self, id: &NodeId, now: DateTime<Utc>) -> Option<Presence> {
        self.peers.get(id).map(|peer| peer.presence(now))
    }

    /// Every peer, ordered by id.
    pub fn iter(&self) -> impl Iterator<Item = &Peer> {
        self.peers.values()
    }

    /// How many peers are in the store.
    pub fn len(&self) -> usize {
        self.peers.len()
    }

    /// Whether the store holds no peers.
    pub fn is_empty(&self) -> bool {
        self.peers.is_empty()
    }

    /// How many peers are in each presence state right now. The counts the
    /// explorer puts in its header, computed once rather than per frame.
    pub fn presence_counts(&self, now: DateTime<Utc>) -> (usize, usize, usize) {
        let mut counts = (0, 0, 0);
        for peer in self.peers.values() {
            match peer.presence(now) {
                Presence::Online => counts.0 += 1,
                Presence::Stale => counts.1 += 1,
                Presence::Unseen => counts.2 += 1,
            }
        }
        counts
    }

    /// Charge API spend to a peer's daily budget.
    pub fn charge(&mut self, id: &NodeId, usd: f64, now: DateTime<Utc>) {
        self.meters
            .entry(*id)
            .or_insert_with(|| Meter::new(now))
            .charge(usd, now);
    }

    /// Count one inbound request against a peer's limits.
    ///
    /// Only the limits: whether the peer is allowed to ask at all is
    /// [`Trust`], and both are checked by [`super::Mesh::admit`], which is the
    /// gate callers should use.
    pub fn try_admit(&mut self, id: &NodeId, now: DateTime<Utc>) -> Result<(), LimitExceeded> {
        let limits = self
            .peers
            .get(id)
            .map(|peer| peer.limits)
            .unwrap_or_default();
        self.meters
            .entry(*id)
            .or_insert_with(|| Meter::new(now))
            .try_admit(&limits, now)
    }

    /// This peer's spend today, as accounted in this process.
    pub fn spent_usd(&self, id: &NodeId) -> f64 {
        self.meters.get(id).map(Meter::spent_usd).unwrap_or(0.0)
    }
}

// ---------------------------------------------------------------------------
// Synthetic peers
// ---------------------------------------------------------------------------

/// Domain separator for synthetic seeds. Keeps a synthetic key from ever
/// colliding with a key derived for anything else, now or later.
const SYNTHETIC_DOMAIN: &[u8] = b"wizard-mesh-synthetic-v1";

/// Generate `count` deterministic peers.
///
/// For tests and for the GUI's performance bar, which the plan sets at 50
/// synthetic nodes at 60fps. Deterministic in `seed`, so a layout bug found at
/// one seed can be reproduced at that seed.
///
/// **These are not identities.** Their private keys are a sha256 of this
/// module's domain separator, the seed and the index, so anyone reading this
/// file can sign as any of them. What stops that from mattering is that they
/// never reach a real store: they are handed back rather than written, and
/// [`synthetic_store`] builds a [`PeerStore::ephemeral`], whose `save`
/// refuses. They do carry a spread of trust states, [`Trust::Trusted`]
/// included, because a renderer that has never drawn a trusted node is a
/// renderer that has not been tested; the spread is safe precisely because
/// none of it is persisted.
///
/// The set deliberately covers every state the explorer has to draw: all three
/// presence states, all three trust states, peers with and without an
/// `observed_via` edge, and delegation counts spread across the range so edge
/// weights are visibly different.
pub fn synthetic_peers(count: usize, seed: u64, now: DateTime<Utc>) -> Vec<Peer> {
    use sha2::{Digest, Sha256};

    let mut peers: Vec<Peer> = Vec::with_capacity(count);
    for index in 0..count {
        let mut hasher = Sha256::new();
        hasher.update(SYNTHETIC_DOMAIN);
        hasher.update(seed.to_le_bytes());
        hasher.update((index as u64).to_le_bytes());
        let digest: [u8; 32] = hasher.finalize().into();
        let node_id = Identity::from_seed(digest).id();

        let mut node = Node::new(node_id);
        node.name = PeerText::sanitize(&format!("synthetic-{index:02}"));
        node.caps = Capability::advertise(
            &[if index % 2 == 0 {
                "qwen3.6:27b"
            } else {
                "gpt-5.3-codex"
            }],
            &["read_file", "write_file"],
            if index % 3 == 0 { &["research"] } else { &[] },
            if index % 5 == 0 { &["reviewer"] } else { &[] },
            index % 2 == 0,
        );
        // Presence: most online, every third stale, every seventh never seen.
        node.last_seen = match index % 7 {
            0 => None,
            i if i % 3 == 0 => Some(now - TimeDelta::seconds(FRESH_SECS + 60 * (index as i64 + 1))),
            _ => Some(now - TimeDelta::seconds((index % FRESH_SECS as usize) as i64)),
        };

        let mut peer = Peer::new(node, now - TimeDelta::days(index as i64 % 30));
        peer.trust = match index % 4 {
            0 => Trust::Trusted,
            3 => Trust::Blocked,
            _ => Trust::Known,
        };
        peer.delegations = (index as u32 * 7) % 23;
        // Every other peer past the first was "learned from" its predecessor,
        // so the explorer has observed edges to draw as well as peer edges.
        if index >= 2 && index % 2 == 0 {
            peer.observed_via = Some(peers[index - 1].id());
        }
        peers.push(peer);
    }
    peers
}

/// An ephemeral store pre-loaded with [`synthetic_peers`]. The GUI's own node
/// uses this for its performance bar; nothing here can reach the disk.
pub fn synthetic_store(count: usize, seed: u64, now: DateTime<Utc>) -> PeerStore {
    let mut store = PeerStore::ephemeral();
    for peer in synthetic_peers(count, seed, now) {
        store.peers.insert(peer.id(), peer);
    }
    store
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(seconds: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(1_800_000_000 + seconds, 0).expect("timestamp")
    }

    fn node(byte: u8) -> Node {
        Node::new(Identity::from_seed([byte; 32]).id())
    }

    #[test]
    fn a_pasted_peer_starts_known_and_never_seen() {
        let mut store = PeerStore::ephemeral();
        let node = node(1);
        let id = node.id;
        assert_eq!(store.add(node, at(0)), Trust::Known);

        let peer = store.get(&id).expect("added");
        assert_eq!(peer.trust, Trust::Known);
        assert!(!peer.trust.may_send_work());
        assert!(!peer.trust.may_accept_work());
        assert!(peer.trust.may_contact());
        assert!(!peer.node.caps.accepts_work, "and it claims nothing");
        assert_eq!(peer.presence(at(0)), Presence::Unseen);
        assert_eq!(peer.staleness(at(0)), None);
        assert_eq!(peer.delegations, 0);
        assert_eq!(peer.limits, Limits::default());
    }

    #[test]
    fn trust_transitions_round_trip_through_the_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let blocked = node(2).id;
        let trusted = node(3).id;
        {
            let mut store = PeerStore::load(dir.path()).expect("load empty");
            assert!(store.is_empty());
            store.add(node(2), at(0));
            store.add(node(3), at(0));
            store.record_trust(&blocked, Trust::Blocked).expect("block");
            store.record_trust(&trusted, Trust::Trusted).expect("trust");
            store.mark_seen(&trusted, at(10));
            store.record_delegation(&trusted);
            store.record_delegation(&trusted);
            store.record_observed_via(&trusted, blocked);
            store.save().expect("save");
        }

        let store = PeerStore::load(dir.path()).expect("reload");
        assert_eq!(store.len(), 2);
        assert_eq!(store.trust_of(&blocked), Some(Trust::Blocked));
        let peer = store.get(&trusted).expect("trusted peer");
        assert_eq!(peer.trust, Trust::Trusted);
        assert_eq!(peer.node.last_seen, Some(at(10)));
        assert_eq!(peer.delegations, 2);
        assert_eq!(peer.observed_via, Some(blocked));
        assert_eq!(peer.added_at, at(0));

        // Every transition is reachable, and the last one recorded wins.
        let mut store = store;
        for trust in [Trust::Known, Trust::Trusted, Trust::Blocked, Trust::Known] {
            store.record_trust(&trusted, trust).expect("record");
            assert_eq!(store.trust_of(&trusted), Some(trust));
        }
        store.save().expect("save");
        let store = PeerStore::load(dir.path()).expect("reload");
        assert_eq!(store.trust_of(&trusted), Some(Trust::Known));
    }

    #[cfg(unix)]
    #[test]
    fn the_store_file_is_not_readable_by_other_local_users() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut store = PeerStore::load(dir.path()).expect("load");
        store.add(node(4), at(0));
        store.save().expect("save");
        assert!(
            secrets::is_protected(&store_path(dir.path())).expect("stat"),
            "who this machine trusts is nobody else's business"
        );
        assert!(
            secrets::is_protected(&mesh_dir(dir.path())).expect("stat"),
            "and neither is the directory it is the only file in"
        );
    }

    #[test]
    fn re_adding_a_blocked_peer_does_not_unblock_it() {
        // The whole point of a three-state decision: a peer that comes back
        // with a shiny new announcement is still blocked.
        let mut store = PeerStore::ephemeral();
        let id = node(5).id;
        store.add(node(5), at(0));
        store.record_trust(&id, Trust::Blocked).expect("block");

        let mut announcement = node(5);
        announcement.name = PeerText::sanitize("totally legit");
        announcement.caps = Capability::advertise(&["gpt-5.3"], &[], &[], &[], true);
        announcement.last_seen = Some(at(9_999));
        assert_eq!(store.add(announcement, at(30)), Trust::Blocked);

        let peer = store.get(&id).expect("still there");
        assert_eq!(peer.trust, Trust::Blocked);
        assert!(!peer.trust.may_accept_work());
        assert!(!peer.trust.may_contact());
        // The claims did update: that is what a re-announcement is for.
        assert_eq!(peer.node.name.as_str(), "totally legit");
        assert!(peer.node.caps.accepts_work, "it can claim what it likes");
        assert_eq!(
            peer.node.last_seen,
            Some(at(30)),
            "…but not the one claim that is an observation: the announcement \
             said at(9999) and what is stored is when this machine heard it"
        );
    }

    #[test]
    fn a_peers_own_clock_never_becomes_this_machines_observation() {
        // Presence is the one thing on a peer record that the graph promises
        // is true *now*, and a peer that could write its own `last_seen` could
        // write itself online. `SKEW_GRACE_SECS` tolerates a future timestamp
        // rather than rejecting it, so a claim of `now + 300` would sit inside
        // the grace window and then inside `FRESH_SECS` on top of it: 390
        // seconds of a machine rendering as up after it went dark.
        let mut store = PeerStore::ephemeral();
        let id = node(16).id;
        let now = at(1_000);

        let mut announcement = node(16);
        announcement.last_seen = Some(now + TimeDelta::seconds(SKEW_GRACE_SECS));
        store.add(announcement, now);
        assert_eq!(store.get(&id).expect("peer").node.last_seen, Some(now));

        // Which is what makes presence age honestly: the peer goes dark right
        // after announcing, and it is stale one second past the window rather
        // than `SKEW_GRACE_SECS` later.
        assert_eq!(store.presence(&id, now), Some(Presence::Online));
        assert_eq!(
            store.presence(&id, now + TimeDelta::seconds(FRESH_SECS + 1)),
            Some(Presence::Stale)
        );

        // A pasted address carries no observation and does not gain one: `add`
        // records what was heard, and nothing has been heard.
        let pasted = node(17).id;
        store.add(node(17), now);
        assert_eq!(store.get(&pasted).expect("peer").node.last_seen, None);
        assert_eq!(store.presence(&pasted, now), Some(Presence::Unseen));
    }

    #[test]
    fn an_announcement_never_erases_a_last_seen_the_store_already_had() {
        let mut store = PeerStore::ephemeral();
        let id = node(6).id;
        store.add(node(6), at(0));
        store.mark_seen(&id, at(50));
        // A fresh paste of the same address carries no observation.
        store.add(node(6), at(60));
        assert_eq!(store.get(&id).expect("peer").node.last_seen, Some(at(50)));
    }

    #[test]
    fn presence_is_reported_from_cached_state_with_a_frozen_clock() {
        let mut store = PeerStore::ephemeral();
        let never = node(7).id;
        let fresh = node(8).id;
        let old = node(9).id;
        for byte in [7u8, 8, 9] {
            store.add(node(byte), at(0));
        }
        let now = at(10_000);
        store.mark_seen(&fresh, now - TimeDelta::seconds(FRESH_SECS - 1));
        store.mark_seen(&old, now - TimeDelta::seconds(FRESH_SECS + 1));

        assert_eq!(store.presence(&never, now), Some(Presence::Unseen));
        assert_eq!(store.presence(&fresh, now), Some(Presence::Online));
        assert_eq!(store.presence(&old, now), Some(Presence::Stale));
        assert_eq!(store.presence_counts(now), (1, 1, 1));

        // Exactly on the boundary is still online; one second past is not.
        store.mark_seen(&fresh, now - TimeDelta::seconds(FRESH_SECS));
        assert_eq!(store.presence(&fresh, now), Some(Presence::Online));
        store.mark_seen(&fresh, now - TimeDelta::seconds(FRESH_SECS + 1));
        assert_eq!(store.presence(&fresh, now), Some(Presence::Stale));

        // The clock moving forward ages a peer out without anything being
        // written: this is exactly the offline-render path.
        assert_eq!(
            store.presence(&old, now + TimeDelta::days(3)),
            Some(Presence::Stale)
        );
        assert_eq!(
            store
                .get(&old)
                .expect("peer")
                .staleness(now + TimeDelta::days(3))
                .expect("seen once")
                .num_days(),
            3
        );
        assert_eq!(store.presence(&node(99).id, now), None);
    }

    #[test]
    fn a_last_seen_from_the_future_does_not_pin_a_peer_online() {
        let mut store = PeerStore::ephemeral();
        let id = node(10).id;
        store.add(node(10), at(0));
        let now = at(1_000);

        // Ordinary skew: still online, and staleness never goes negative.
        store.mark_seen(&id, now + TimeDelta::seconds(SKEW_GRACE_SECS - 1));
        assert_eq!(store.presence(&id, now), Some(Presence::Online));
        assert_eq!(
            store.get(&id).expect("peer").staleness(now),
            Some(TimeDelta::zero())
        );

        // A year in the future is not skew; it is a record this machine has no
        // reason to believe.
        store.mark_seen(&id, now + TimeDelta::days(365));
        assert_eq!(store.presence(&id, now), Some(Presence::Stale));
    }

    #[test]
    fn admission_counts_against_the_peers_own_limits() {
        let mut store = PeerStore::ephemeral();
        let id = node(11).id;
        store.add(node(11), at(0));
        store
            .set_limits(
                &id,
                Limits {
                    requests_per_minute: 2,
                    cost_usd_per_day: 1.0,
                    ..Limits::default()
                },
            )
            .expect("limits");

        store.try_admit(&id, at(0)).expect("first");
        store.try_admit(&id, at(1)).expect("second");
        assert!(store.try_admit(&id, at(2)).is_err(), "third is over budget");

        store.charge(&id, 2.0, at(3));
        assert_eq!(store.spent_usd(&id), 2.0);
        let err = store.try_admit(&id, at(120)).expect_err("cost limit");
        assert!(matches!(err, LimitExceeded::Cost { .. }), "{err:?}");

        // Losing trust clears the meter, so a re-trusted peer is not born
        // already over its budget.
        store.record_trust(&id, Trust::Blocked).expect("block");
        assert_eq!(store.spent_usd(&id), 0.0);
    }

    #[test]
    fn recording_against_a_peer_that_is_not_in_the_store_is_an_error_not_a_no_op() {
        let mut store = PeerStore::ephemeral();
        let stranger = node(12).id;
        let err = store
            .record_trust(&stranger, Trust::Trusted)
            .expect_err("no such peer");
        assert!(format!("{err:#}").contains("no peer"), "{err:#}");
        assert!(store.record_announcement(&node(12)).is_err());
        assert!(store.set_limits(&stranger, Limits::default()).is_err());
        // …while the observation helpers stay quiet, because an unsolicited
        // announcement from a stranger must not create a peer.
        store.mark_seen(&stranger, at(0));
        store.record_delegation(&stranger);
        assert!(store.get(&stranger).is_none());
        assert!(store.is_empty());
    }

    #[test]
    fn forgetting_a_peer_is_not_the_same_as_blocking_it() {
        let mut store = PeerStore::ephemeral();
        let id = node(13).id;
        store.add(node(13), at(0));
        store.record_trust(&id, Trust::Blocked).expect("block");
        assert!(store.forget(&id));
        assert!(!store.forget(&id), "gone is gone");
        // Pasting the address again starts the decision over at Known, which
        // is why the docs say forgetting is not blocking.
        assert_eq!(store.add(node(13), at(1)), Trust::Known);
    }

    #[test]
    fn an_ephemeral_store_refuses_to_save_instead_of_pretending_to() {
        let mut store = PeerStore::ephemeral();
        store.add(node(14), at(0));
        assert!(store.path().is_none());
        let err = store.save().expect_err("nowhere to save");
        assert!(format!("{err:#}").contains("ephemeral"), "{err:#}");
    }

    #[test]
    fn a_store_file_from_the_future_is_refused_rather_than_half_read() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(mesh_dir(dir.path())).expect("mkdir");
        std::fs::write(
            store_path(dir.path()),
            serde_json::json!({ "version": STORE_VERSION + 1, "peers": [] }).to_string(),
        )
        .expect("write");
        let err = PeerStore::load(dir.path()).expect_err("future version");
        let message = format!("{err:#}");
        assert!(message.contains("version"), "{message}");
        assert!(message.contains("update wizard"), "{message}");
    }

    #[test]
    fn a_record_that_omits_trust_reads_back_as_known_not_as_trusted() {
        // The fail-open shape this codebase has shipped before: a field added
        // later must default to the deny side.
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(mesh_dir(dir.path())).expect("mkdir");
        let address = node(15).id.address();
        std::fs::write(
            store_path(dir.path()),
            serde_json::json!({
                "version": STORE_VERSION,
                "peers": [{ "node": { "id": address }, "added_at": at(0) }],
            })
            .to_string(),
        )
        .expect("write");

        let store = PeerStore::load(dir.path()).expect("load");
        let peer = store.get(&node(15).id).expect("peer");
        assert_eq!(peer.trust, Trust::Known);
        assert!(!peer.trust.may_accept_work());
        assert!(!peer.node.caps.accepts_work);
        assert_eq!(peer.limits, Limits::default());
        assert_eq!(peer.presence(at(0)), Presence::Unseen);
    }

    #[test]
    fn a_corrupt_store_file_is_an_error_not_an_empty_peer_list() {
        // Reading a damaged file as "no peers" would silently drop every
        // Blocked decision on the machine.
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(mesh_dir(dir.path())).expect("mkdir");
        std::fs::write(store_path(dir.path()), "{ not json").expect("write");
        let err = PeerStore::load(dir.path()).expect_err("corrupt");
        assert!(format!("{err:#}").contains("peers.json"), "{err:#}");
    }

    #[test]
    fn the_synthetic_mesh_covers_every_state_the_explorer_draws() {
        let now = at(0);
        let peers = synthetic_peers(50, 7, now);
        assert_eq!(peers.len(), 50);

        // Deterministic in the seed, and different across seeds.
        let again = synthetic_peers(50, 7, now);
        assert_eq!(
            peers.iter().map(Peer::id).collect::<Vec<_>>(),
            again.iter().map(Peer::id).collect::<Vec<_>>()
        );
        assert_ne!(
            peers[0].id(),
            synthetic_peers(1, 8, now)[0].id(),
            "a different seed is a different mesh"
        );
        // Distinct identities, not one key repeated.
        let unique: std::collections::BTreeSet<_> = peers.iter().map(Peer::id).collect();
        assert_eq!(unique.len(), peers.len());

        for state in [Presence::Online, Presence::Stale, Presence::Unseen] {
            assert!(
                peers.iter().any(|peer| peer.presence(now) == state),
                "no synthetic peer is {}",
                state.label()
            );
        }
        for trust in [Trust::Trusted, Trust::Known, Trust::Blocked] {
            assert!(
                peers.iter().any(|peer| peer.trust == trust),
                "no synthetic peer is {}",
                trust.label()
            );
        }
        assert!(peers.iter().any(|peer| peer.observed_via.is_some()));
        assert!(peers.iter().any(|peer| peer.delegations > 0));
        assert!(peers.iter().any(|peer| !peer.node.caps.is_empty()));

        // And it cannot be mistaken for real state: the store has no file.
        let store = synthetic_store(50, 7, now);
        assert_eq!(store.len(), 50);
        assert!(store.save().is_err(), "synthetic data must not reach disk");
    }
}
