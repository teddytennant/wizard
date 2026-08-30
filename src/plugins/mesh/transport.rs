//! The transport seam, and the two implementations behind it.
//!
//! ## What is here
//!
//! [`Transport`] is the whole surface the model needs: announce presence,
//! fetch a peer's announcement, subscribe to a peer's session events, publish
//! this node's own, and drop everything live between this node and a peer.
//! Five operations, deliberately: every one of them corresponds to something
//! the graph explorer draws or something the trust model has to be able to
//! undo.
//!
//! [`LoopbackTransport`] implements it in-process. It is what most of the mesh's
//! tests drive and what the GUI's own node talks to when the only node in the
//! mesh is this one. It opens no socket, speaks no wire format, and reaches no
//! network.
//!
//! [`super::quic::QuicTransport`] is the other implementation: QUIC, with mutual
//! TLS whose certificates are the node identities. It is the one that crosses a
//! machine boundary, and its listener is off unless `[mesh] listen` says
//! otherwise.
//!
//! ## What crosses
//!
//! A [`PeerEvent`] is a frame: which node it came from, which of that node's
//! sessions it belongs to, when this machine saw it, and one of four things
//! that happened. Three of those are session lifecycle, which the agent loop
//! has nothing to say about. The fourth carries a [`PeerTurn`], and a
//! `PeerTurn` is an [`AgentEvent`]: the same type the local agent loop emits,
//! so a remote node's turn *is* a local event stream and every surface that can
//! already render one renders a peer's for free.
//!
//! That is why the frame is not itself an event enum. An earlier draft of this
//! file mirrored the agent's events with a small enum of its own, because
//! `AgentEvent` could be neither cloned nor serialised at the time. It can be
//! now, and mirroring a growing enum is a debt that pays out as a peer's turn
//! rendering as "something happened" forever. What survives from that draft is
//! only what `AgentEvent` genuinely does not carry: the origin node (set here,
//! never taken from the sender's claim about itself), the session id that lets
//! one subscription carry several sessions, the local observation clock, and
//! the three lifecycle variants.
//!
//! ## The obligations
//!
//! This is a seam, and these are the obligations any implementation of it takes
//! on, written down where the implementer will be reading. They were written
//! before there was a network transport to hold to them, as the specification
//! for whoever built one; [`super::quic`] is that implementation, and its own
//! module header answers this list item by item with the place each one
//! happens. Read the two together before writing a third transport.
//!
//! A note on the first and the fifth, because both are stated below in terms of
//! a *signed announcement over a plaintext socket*, which is what a transport
//! looked like when this list was written. The QUIC transport meets them with
//! mutual TLS instead: the certificate's key **is** the node id, so the
//! handshake proves the identity and the channel encryption and the signature
//! check are one act rather than three steps with two places to forget one.
//! That is a stronger answer to the same obligation, not a different
//! obligation, and `NodeId::verify` is still the call that decides it —
//! [`super::x509::identity_of`] runs it over the certificate's own contents.
//! A transport that cannot do that has to do what these bullets literally say.
//!
//! - **The identity is the key.** A node's id must be verified, not accepted.
//!   An announcement carries a signature over its own contents; check it with
//!   [`NodeId::verify`] (`verify_strict`, no bypass) before the record reaches
//!   the store, exactly as [`crate::sync`] checks a bundle manifest.
//! - **Everything inbound is untrusted text.** Names and capability entries
//!   arrive as [`super::PeerText`], and a peer's turn arrives as [`PeerTurn`];
//!   neither is a `String` for a reason. See the [`super`] and [`super::turn`]
//!   module docs.
//! - **Bound the message before you decode it.** [`PeerTurn`] bounds the text,
//!   the breadth and the depth of an event it has *already decoded*, which is
//!   the most it can do: by then the bytes are in memory. Capping the frame is
//!   the transport's job, at the socket, before the decoder is handed anything.
//! - **Revocation is not advisory.** [`Transport::revoke`] must sever live
//!   subscriptions, not stop renewing them. A revoked peer whose event stream
//!   keeps arriving until a timeout is a peer that was not revoked, and so is
//!   one that keeps *receiving* this node's stream. Both directions, now, and
//!   no further than that: see the method's own docs.
//! - **The publisher consents too.** [`super::Mesh::subscribe`] is the
//!   *watcher's* decision: whether this machine will take a stream. A network
//!   transport has an inbound half as well, a peer asking to watch this node,
//!   and that request must be checked against this machine's own peer store
//!   before a single event is written to it, or trust means "whoever asked
//!   first". The loopback has no inbound path at all — its subscribers are in
//!   this process and were approved by their own [`super::Mesh`] — so the check
//!   has nothing to attach to there and is absent rather than stubbed. It is
//!   the first thing a network transport owes, and [`super::consent`] is the
//!   seam it attaches to: one question, asked of whatever holds the answer,
//!   because a transport can neither borrow the store nor own a copy of it.
//! - **A peer's clock is a peer's claim.** An announcement's `last_seen` is
//!   whatever the far end wrote; the observation is that the announcement
//!   arrived *here*, *now*. [`super::PeerStore::add`] enforces this rather
//!   than trusting an implementation to remember it, and
//!   [`PeerEvent::at`] is the local clock for the same reason.
//! - **A slow consumer must not stall the transport.** [`Transport::publish`]
//!   is not `async` on purpose: there is no await in it to block on, so a
//!   stalled reader cannot become the producer's problem. Delivery is
//!   lossy-by-design under backpressure (see [`LoopbackTransport::publish`]);
//!   a network implementation should queue into its own bounded per-peer buffer
//!   and drop or coalesce for the same reason.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use anyhow::{Result, anyhow};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::agent::AgentEvent;

use super::PeerText;
use super::node::{Node, NodeId};
use super::turn::PeerTurn;

/// Events buffered per subscription before delivery starts being dropped.
///
/// A subscriber that is this far behind is not going to catch up by being
/// given more memory; the graph explorer only ever renders the latest state
/// anyway, and a transcript that is 64 events behind a peer's turn has already
/// lost the race it was watching.
///
/// This is the bound, and it is a count rather than a rate: 64 events, each
/// carrying at most [`PeerTurn::MAX_TEXT`] characters of text, is the whole of
/// what one subscription can cost this process. Past it the events are dropped
/// and counted ([`Subscription::dropped`]) rather than queued, because a queue
/// that grows to fit its producer is not a queue, it is a leak with a name.
pub const SUBSCRIPTION_BUFFER: usize = 64;

// ---------------------------------------------------------------------------
// Events
// ---------------------------------------------------------------------------

/// What happened on a peer's session.
///
/// Four variants, and the asymmetry between them is the point. Three are
/// session lifecycle, which the agent loop knows nothing about: it reports what
/// a *turn* did, and a session outlives its turns. The fourth is the turn
/// itself, carried whole rather than summarised.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PeerEventKind {
    /// A session began on the peer.
    SessionStarted,
    /// One event from a turn in that session: the peer's agent loop reporting
    /// what it just did.
    Turn(PeerTurn),
    /// The session ended. Not the same as an [`AgentEvent::Done`], which ends a
    /// turn: a session takes many turns and ends when the far end says so.
    SessionEnded,
    /// The peer re-advertised itself; the capability graph needs refetching.
    CapabilityChanged,
}

impl PeerEventKind {
    /// Lower-case label, for a log line or a graph legend.
    ///
    /// Deliberately does not name the agent event inside a
    /// [`PeerEventKind::Turn`]. That would be a second match over a growing
    /// enum, which is the thing this type stopped doing; a legend wants to know
    /// that a turn is happening, and a transcript renders the event itself.
    pub fn label(&self) -> &'static str {
        match self {
            PeerEventKind::SessionStarted => "session_started",
            PeerEventKind::Turn(_) => "turn",
            PeerEventKind::SessionEnded => "session_ended",
            PeerEventKind::CapabilityChanged => "capability_changed",
        }
    }
}

/// One event from a peer's session stream.
///
/// The frame around a [`PeerTurn`], carrying the four things an
/// [`AgentEvent`] structurally cannot:
///
/// - **who** it came from, set by the transport from the connection it arrived
///   on and never from the sender's claim about itself;
/// - **which session**, so one subscription can carry a node that is running
///   three of them and a watcher can demux;
/// - **when this machine saw it**, because a peer's clock is a peer's claim;
/// - **what kind of thing** it is, so a session starting and a session ending
///   have somewhere to live.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PeerEvent {
    /// Which node this came from. Set by the transport, never by the sender's
    /// own claim about itself.
    pub from: NodeId,
    /// The peer's session id. Peer-supplied, hence [`PeerText`].
    pub session: PeerText,
    /// When the *local* machine observed it. A peer's clock is a peer's claim.
    pub at: DateTime<Utc>,
    /// What happened.
    pub what: PeerEventKind,
}

impl PeerEvent {
    /// An event as observed locally at `at`.
    pub fn new(from: NodeId, session: &str, at: DateTime<Utc>, what: PeerEventKind) -> Self {
        Self {
            from,
            session: PeerText::sanitize(session),
            at,
            what,
        }
    }

    /// One agent event from a session, cleaned for the mesh.
    ///
    /// `None` when the event does not cross at all. See [`PeerTurn::sanitize`],
    /// and the [`super::turn`] module docs for which ones those are and why an
    /// agent event that asks this machine for something is not one of them.
    pub fn turn(
        from: NodeId,
        session: &str,
        at: DateTime<Utc>,
        event: &AgentEvent,
    ) -> Option<Self> {
        let turn = PeerTurn::sanitize(event)?;
        Some(Self::new(from, session, at, PeerEventKind::Turn(turn)))
    }

    /// The agent event this frame carries, if it carries one.
    ///
    /// The whole of tier 2 on one line: what comes back is the same type the
    /// local agent loop emits, so a surface renders a peer's turn with the code
    /// it already has. It is still a peer's data: display only, never a
    /// prompt, never a command.
    pub fn report(&self) -> Option<&AgentEvent> {
        match &self.what {
            PeerEventKind::Turn(turn) => Some(turn.as_event()),
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------
// Subscription
// ---------------------------------------------------------------------------

/// A live subscription to one peer's session events.
///
/// Dropping it unsubscribes: the sender side notices the closed receiver on
/// its next publish and forgets it. Revocation works from the other end (see
/// [`Transport::revoke`]), which closes the sender and makes
/// [`Subscription::recv`] return `None`.
pub struct Subscription {
    peer: NodeId,
    events: tokio::sync::mpsc::Receiver<PeerEvent>,
    dropped: Arc<AtomicU64>,
}

impl Subscription {
    /// Build a subscription over a channel a transport is feeding.
    ///
    /// The constructor a [`Transport`] implementation outside this module
    /// needs, and the reason the three fields are private: a subscription is a
    /// receiver, the peer it belongs to, and the count of what was lost on the
    /// way. A transport that assembled one out of parts it chose could hand
    /// back a stream labelled with the wrong peer, and every surface that
    /// renders `PeerEvent::from` would then be rendering the transport's
    /// opinion rather than a verified identity.
    ///
    /// `dropped` is shared with whatever is filling `events`, and the contract
    /// is the one [`LoopbackTransport::publish`] keeps: an event that does not
    /// fit the buffer is dropped and counted, never queued. See
    /// [`Subscription::dropped`].
    pub fn from_channel(
        peer: NodeId,
        events: tokio::sync::mpsc::Receiver<PeerEvent>,
        dropped: Arc<AtomicU64>,
    ) -> Self {
        Self {
            peer,
            events,
            dropped,
        }
    }

    /// The peer this subscription is to.
    pub fn peer(&self) -> NodeId {
        self.peer
    }

    /// The next event, or `None` once the stream has ended: the peer was
    /// revoked, or the transport dropped it.
    pub async fn recv(&mut self) -> Option<PeerEvent> {
        self.events.recv().await
    }

    /// The next event if one is already buffered, without waiting.
    ///
    /// What a render loop wants: a frame at 60fps cannot await, and "nothing
    /// new" and "the stream ended" look the same to a renderer that is about
    /// to draw the cached state either way. Use [`Subscription::is_closed`]
    /// when the difference matters.
    pub fn try_recv(&mut self) -> Option<PeerEvent> {
        self.events.try_recv().ok()
    }

    /// Whether the sending half is gone, without waiting for an event. The
    /// explorer polls this to stop drawing a stream that has ended.
    pub fn is_closed(&self) -> bool {
        self.events.is_closed()
    }

    /// How many events this subscription has lost to backpressure since it
    /// opened.
    ///
    /// A counter and not a queue: that is the whole design. When the buffer is
    /// full the event is dropped and this goes up, so the cost of a slow
    /// subscriber is bounded at [`SUBSCRIPTION_BUFFER`] events no matter how
    /// far behind it gets.
    ///
    /// A renderer should show the gap rather than swallow it. A transcript with
    /// a silent hole in it is a lie about what the peer did; "3 events were
    /// dropped" is not.
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }
}

impl std::fmt::Debug for Subscription {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Subscription")
            .field("peer", &self.peer.short())
            .field("closed", &self.is_closed())
            .field("dropped", &self.dropped())
            .finish()
    }
}

// ---------------------------------------------------------------------------
// The seam
// ---------------------------------------------------------------------------

/// Everything the mesh model needs from a way of reaching other nodes.
#[async_trait]
pub trait Transport: Send + Sync {
    /// Publish this node's presence and capability.
    async fn announce(&self, node: &Node) -> Result<()>;

    /// Fetch the record a peer advertises. The result is the peer's claim
    /// about itself and nothing more.
    ///
    /// The whole [`Node`] rather than only its [`Capability`], because a
    /// node's name is peer-supplied text as well and an announcement is the
    /// only thing that carries it: a pasted address is an identity with
    /// nothing attached ([`Node::from_address`]), so a transport that handed
    /// back capabilities alone would leave every peer rendering as its own
    /// address forever, and the graph's whole name-collision problem would be
    /// one the explorer could never actually reach.
    ///
    /// An implementation must answer for the node it was asked about.
    /// [`super::Mesh::refresh`] checks rather than trusting it to, because a
    /// transport that answered with somebody else's announcement would rename
    /// one peer with another's claim.
    async fn announcement_of(&self, id: &NodeId) -> Result<Node>;

    /// Subscribe `local` to `peer`'s session events.
    ///
    /// Implementations do not consult the trust store: whether this machine
    /// *wants* a stream from this peer is [`super::Mesh::subscribe`]'s
    /// decision, kept in one place so a second transport cannot ship a second
    /// (differently wrong) copy of the policy.
    ///
    /// `local` is passed rather than remembered, exactly as [`Transport::announce`]
    /// takes the node to announce. A network transport belongs to one node and
    /// could have kept it, but the loopback hosts several in one process, and
    /// [`Transport::revoke`] has to be able to find the streams flowing *toward*
    /// a peer as well as the ones flowing from it. A subscription that did not
    /// record who asked for it could not be revoked from the publishing side.
    async fn subscribe(&self, local: &NodeId, peer: &NodeId) -> Result<Subscription>;

    /// Deliver one of this node's own session events to whoever is subscribed
    /// to `event.from`. Returns how many subscriptions took it.
    ///
    /// Not `async`, and that is the design rather than an omission: a publish
    /// that could await is a publish that a stalled reader can block, and this
    /// runs inside a live turn. An implementation hands the event to a bounded
    /// buffer and returns; anything slower belongs on the far side of that
    /// buffer.
    fn publish(&self, event: &PeerEvent) -> usize;

    /// Drop everything *live* between `local` and `peer`: subscriptions,
    /// streams, connections. Idempotent, and never an error for a peer that has
    /// nothing live, because it runs on the revocation path where failing is
    /// not an option.
    ///
    /// **Both directions.** The stream this node is receiving from the peer,
    /// and the stream the peer is receiving from this node. Severing only the
    /// inbound half would leave a peer that was just un-trusted still watching
    /// this machine's own sessions, which is the more expensive half of the
    /// mistake: the first leaks a screen, the second leaks a workspace.
    ///
    /// Live state only, and scoped to this pair. An implementation must not
    /// forget that the node exists: trust moves both ways, and a peer
    /// downgraded from [`super::Trust::Trusted`] to [`super::Trust::Known`] has
    /// not been banished, it has been un-approved. Discarding its address or
    /// its announcement here makes that ordinary change of mind unrecoverable
    /// without a restart. Nor may it touch a stream between two *other* nodes,
    /// which one process hosting several of them can reach and no revocation
    /// ever meant.
    async fn revoke(&self, local: &NodeId, peer: &NodeId) -> Result<()>;
}

// ---------------------------------------------------------------------------
// Loopback
// ---------------------------------------------------------------------------

/// One live stream: where the events go, and who asked for them.
///
/// The subscriber's id is what makes revocation work from the publishing side.
/// Without it a stream is anonymous, and "stop sending my sessions to that
/// peer" has nothing to match on.
struct Sink {
    /// The node that asked for this stream.
    subscriber: NodeId,
    events: tokio::sync::mpsc::Sender<PeerEvent>,
    /// Events this sink could not take because its buffer was full. Shared
    /// with the [`Subscription`], which is the only thing that reads it.
    dropped: Arc<AtomicU64>,
}

/// What the loopback knows.
#[derive(Default)]
struct Inner {
    /// Nodes that have announced themselves here.
    nodes: BTreeMap<NodeId, Node>,
    /// Live streams, keyed by the node whose events they carry.
    streams: BTreeMap<NodeId, Vec<Sink>>,
}

/// An in-process [`Transport`]: announcements land in a map, events fan out
/// over channels, nothing leaves the process.
///
/// This is the whole of the mesh's networking in this release, and it is
/// genuinely useful rather than a stub: the GUI's own node announces into it,
/// the graph explorer reads its own capability back out of it, and a second
/// [`super::Mesh`] sharing one of these is two nodes watching each other's
/// sessions, which is what the tier 2 tests drive.
#[derive(Default)]
pub struct LoopbackTransport {
    state: Mutex<Inner>,
}

impl LoopbackTransport {
    /// A loopback with nothing announced on it.
    pub fn new() -> Self {
        Self::default()
    }

    /// How many live subscriptions there are to `id`.
    ///
    /// Counts sinks whose receiver is still alive, so a subscription that
    /// was dropped without a revocation does not linger in the count.
    pub fn subscriber_count(&self, id: &NodeId) -> usize {
        self.lock()
            .streams
            .get(id)
            .map(|sinks| sinks.iter().filter(|sink| !sink.events.is_closed()).count())
            .unwrap_or(0)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        // A panic while holding this lock would otherwise poison the whole
        // transport for the rest of the process; the state behind it is a map
        // of records, with no invariant a panic could leave half-applied.
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// Drop every sink under `publisher` that belongs to `subscriber`, and the map
/// entry with it once nothing is left.
///
/// The empty entry matters: a long-lived process that revoked one peer a
/// thousand times would otherwise keep a thousand empty vectors, which is the
/// small, boring shape of the growth bug this module keeps promising not to
/// have.
fn sever(state: &mut Inner, publisher: &NodeId, subscriber: &NodeId) {
    let Some(sinks) = state.streams.get_mut(publisher) else {
        return;
    };
    sinks.retain(|sink| sink.subscriber != *subscriber);
    if sinks.is_empty() {
        state.streams.remove(publisher);
    }
}

#[async_trait]
impl Transport for LoopbackTransport {
    async fn announce(&self, node: &Node) -> Result<()> {
        self.lock().nodes.insert(node.id, node.clone());
        Ok(())
    }

    async fn announcement_of(&self, id: &NodeId) -> Result<Node> {
        self.lock()
            .nodes
            .get(id)
            .cloned()
            .ok_or_else(|| anyhow!("node {} has not announced itself on this mesh", id.short()))
    }

    async fn subscribe(&self, local: &NodeId, peer: &NodeId) -> Result<Subscription> {
        let mut state = self.lock();
        if !state.nodes.contains_key(peer) {
            return Err(anyhow!(
                "node {} has not announced itself on this mesh",
                peer.short()
            ));
        }
        let (tx, rx) = tokio::sync::mpsc::channel(SUBSCRIPTION_BUFFER);
        let dropped = Arc::new(AtomicU64::new(0));
        state.streams.entry(*peer).or_default().push(Sink {
            subscriber: *local,
            events: tx,
            dropped: dropped.clone(),
        });
        Ok(Subscription {
            peer: *peer,
            events: rx,
            dropped,
        })
    }

    /// Deliver `event` to everyone subscribed to `event.from`. Returns how
    /// many subscriptions took it.
    ///
    /// Delivery is lossy under backpressure: a subscriber whose buffer is full
    /// misses this event, has it counted against [`Subscription::dropped`], and
    /// keeps its subscription. Blocking here would let one stalled reader stop
    /// every other node's events, and unbounded buffering would let a chatty
    /// peer grow this process's memory without limit. Closed subscribers are
    /// reaped on the way past.
    fn publish(&self, event: &PeerEvent) -> usize {
        let mut state = self.lock();
        let Some(sinks) = state.streams.get_mut(&event.from) else {
            return 0;
        };
        sinks.retain(|sink| !sink.events.is_closed());
        let mut delivered = 0;
        for sink in sinks.iter() {
            if sink.events.try_send(event.clone()).is_ok() {
                delivered += 1;
            } else {
                sink.dropped.fetch_add(1, Ordering::Relaxed);
            }
        }
        delivered
    }

    async fn revoke(&self, local: &NodeId, peer: &NodeId) -> Result<()> {
        // Dropping the senders closes every receiver: a subscriber awaiting
        // `recv` wakes with `None` now, rather than at the next event that
        // never comes.
        //
        // Both directions. The first call is the stream this node is watching;
        // the second is the stream the peer is watching, which is this
        // machine's own sessions still flowing to somebody it just un-trusted.
        //
        // Neither call touches a stream between two other nodes. One loopback
        // hosts every node in the process, so `streams.remove(peer)` would
        // reach a third party's subscription that this revocation never meant,
        // and a test mesh would then be proving the wrong thing.
        let mut state = self.lock();
        sever(&mut state, peer, local);
        sever(&mut state, local, peer);
        // The peers' announcements deliberately stay. They are not live state:
        // they are the record that a node exists and how to reach it, and
        // deleting one here would make revocation irreversible in-process.
        // Nothing re-announces a *remote* peer (`Mesh::announce` announces only
        // the local node), so a `Trusted -> Known` move, which is an ordinary
        // change of mind and not a ban, would permanently break `refresh` and
        // `subscribe` for that peer with no way back. Whether a revoked peer
        // may be contacted again is [`Trust`]'s answer and `Mesh`'s to enforce;
        // a transport that decides it too is the second, differently wrong copy
        // of the policy this trait's docs warn against.
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugins::mesh::capability::Capability;
    use crate::plugins::mesh::node::Identity;

    fn at(seconds: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(1_800_000_000 + seconds, 0).expect("timestamp")
    }

    fn identity(byte: u8) -> Identity {
        Identity::from_seed([byte; 32])
    }

    /// The encoded form of an event, which is the only equality a wire type
    /// has: [`AgentEvent`] is deliberately not `PartialEq`, and it should not
    /// grow the impl to please a test.
    fn encoded(event: &PeerEvent) -> serde_json::Value {
        serde_json::to_value(event).expect("a peer event encodes")
    }

    /// A turn event carrying `text`.
    fn delta(from: NodeId, session: &str, at: DateTime<Utc>, text: &str) -> PeerEvent {
        PeerEvent::turn(from, session, at, &AgentEvent::TextDelta(text.to_string()))
            .expect("a text delta crosses the mesh")
    }

    /// [`Subscription::recv`] with a deadline.
    ///
    /// Several of these tests assert that a stream has *ended*. Awaiting one
    /// that has not ended parks the test forever, and a hung test reads in CI
    /// as an infrastructure problem rather than as the revocation bug it
    /// actually is. Five seconds is far longer than an in-process channel
    /// needs and short enough to be a test failure.
    async fn recv_within(subscription: &mut Subscription, what: &str) -> Option<PeerEvent> {
        tokio::time::timeout(std::time::Duration::from_secs(5), subscription.recv())
            .await
            .unwrap_or_else(|_| panic!("{what}: the subscription was still open after 5s"))
    }

    #[tokio::test]
    async fn announcing_publishes_a_record_others_can_fetch() {
        let transport = LoopbackTransport::new();
        let peer = identity(1);
        let caps = Capability::advertise(&["qwen3.6:27b"], &["read_file"], &[], &[], true);
        transport
            .announce(&peer.announce("workshop", caps.clone()))
            .await
            .expect("announce");

        let announced = transport
            .announcement_of(&peer.id())
            .await
            .expect("fetch the announcement");
        assert_eq!(announced.caps, caps);
        // The name comes back too, and it is the only path it has: an address
        // is pasted, a name is announced.
        assert_eq!(announced.label(), "workshop");
        assert_eq!(announced.id, peer.id());
        // A node nobody announced is absent, not empty: "advertises nothing"
        // and "is not there" are different answers.
        let err = transport
            .announcement_of(&identity(2).id())
            .await
            .expect_err("absent");
        assert!(format!("{err:#}").contains("has not announced"), "{err:#}");
    }

    #[tokio::test]
    async fn events_reach_every_live_subscriber_and_only_them() {
        let transport = LoopbackTransport::new();
        let watcher = identity(2).id();
        let peer = identity(3);
        let other = identity(4);
        transport
            .announce(&peer.announce("peer", Capability::none()))
            .await
            .expect("announce");
        transport
            .announce(&other.announce("other", Capability::none()))
            .await
            .expect("announce");

        let mut first = transport
            .subscribe(&watcher, &peer.id())
            .await
            .expect("subscribe");
        let mut second = transport
            .subscribe(&watcher, &peer.id())
            .await
            .expect("subscribe");
        let mut elsewhere = transport
            .subscribe(&watcher, &other.id())
            .await
            .expect("subscribe");
        assert_eq!(transport.subscriber_count(&peer.id()), 2);

        let event = delta(peer.id(), "session-7", at(0), "building the mesh");
        assert_eq!(transport.publish(&event), 2);
        let got = recv_within(&mut first, "first subscriber")
            .await
            .expect("event");
        assert_eq!(encoded(&got), encoded(&event));
        let got = recv_within(&mut second, "second subscriber")
            .await
            .expect("event");
        assert_eq!(encoded(&got), encoded(&event));
        assert!(
            elsewhere.try_recv().is_none(),
            "a subscription to another node must not receive it"
        );
        assert!(!elsewhere.is_closed(), "and it is still live, just quiet");

        // The other direction, so the routing is pinned from both ends.
        let theirs = PeerEvent::new(other.id(), "session-1", at(1), PeerEventKind::SessionEnded);
        assert_eq!(transport.publish(&theirs), 1);
        let got = elsewhere.try_recv().expect("event");
        assert_eq!(encoded(&got), encoded(&theirs));
        assert!(first.try_recv().is_none());
    }

    #[tokio::test]
    async fn a_dropped_subscription_stops_counting_and_stops_receiving() {
        let transport = LoopbackTransport::new();
        let watcher = identity(4).id();
        let peer = identity(5);
        transport
            .announce(&peer.announce("peer", Capability::none()))
            .await
            .expect("announce");

        let subscription = transport
            .subscribe(&watcher, &peer.id())
            .await
            .expect("subscribe");
        assert_eq!(transport.subscriber_count(&peer.id()), 1);
        drop(subscription);
        assert_eq!(transport.subscriber_count(&peer.id()), 0);
        assert_eq!(
            transport.publish(&PeerEvent::new(
                peer.id(),
                "s",
                at(1),
                PeerEventKind::SessionEnded
            )),
            0
        );
    }

    #[tokio::test]
    async fn revoking_severs_every_stream_without_stranding_the_peer() {
        let transport = LoopbackTransport::new();
        let watcher = identity(5).id();
        let peer = identity(6);
        let caps = Capability::advertise(&["qwen3.6:27b"], &[], &[], &[], true);
        transport
            .announce(&peer.announce("peer", caps.clone()))
            .await
            .expect("announce");
        let mut first = transport
            .subscribe(&watcher, &peer.id())
            .await
            .expect("subscribe");
        let mut second = transport
            .subscribe(&watcher, &peer.id())
            .await
            .expect("subscribe");

        transport
            .revoke(&watcher, &peer.id())
            .await
            .expect("revoke");

        // Not "no more events eventually": every stream ends now.
        assert!(recv_within(&mut first, "after revoke").await.is_none());
        assert!(recv_within(&mut second, "after revoke").await.is_none());
        assert!(first.is_closed() && second.is_closed());
        assert_eq!(transport.subscriber_count(&peer.id()), 0);
        assert_eq!(
            transport.publish(&PeerEvent::new(
                peer.id(),
                "s",
                at(1),
                PeerEventKind::SessionStarted
            )),
            0
        );
        // Revoking again is not an error: this runs on a path that must not
        // fail.
        transport
            .revoke(&watcher, &peer.id())
            .await
            .expect("idempotent");

        // Live state only. The node still exists here, because trust moves
        // both ways: `Trusted -> Known` is a change of mind, not a banishment,
        // and nothing in this process re-announces a remote peer. Forgetting
        // the announcement would make that ordinary downgrade permanent.
        assert_eq!(
            transport
                .announcement_of(&peer.id())
                .await
                .expect("still here")
                .caps,
            caps
        );
        let mut resumed = transport
            .subscribe(&watcher, &peer.id())
            .await
            .expect("re-subscribe");
        let event = delta(peer.id(), "s", at(2), "trusted again");
        assert_eq!(transport.publish(&event), 1);
        let got = recv_within(&mut resumed, "after re-trust")
            .await
            .expect("event");
        assert_eq!(encoded(&got), encoded(&event));
        // Whether this machine *wants* any of that is `Trust`'s answer and
        // `Mesh`'s to enforce; a transport that decided it too would be the
        // second, differently wrong copy of the policy.
    }

    #[tokio::test]
    async fn revoking_stops_this_node_sending_as_well_as_receiving() {
        // The half a one-directional revocation forgets, and the expensive
        // half: un-trusting a peer that is *watching* this node has to stop
        // this machine's own sessions reaching it. Leaking a screen is the
        // cheaper mistake.
        let transport = LoopbackTransport::new();
        let here = identity(40);
        let peer = identity(41);
        let bystander = identity(42).id();
        for node in [&here, &peer] {
            transport
                .announce(&node.announce("node", Capability::none()))
                .await
                .expect("announce");
        }

        // The peer is watching this node, this node is watching the peer, and
        // an unrelated third node is watching this one too.
        let mut theirs = transport
            .subscribe(&peer.id(), &here.id())
            .await
            .expect("subscribe");
        let mut ours = transport
            .subscribe(&here.id(), &peer.id())
            .await
            .expect("subscribe");
        let mut unrelated = transport
            .subscribe(&bystander, &here.id())
            .await
            .expect("subscribe");
        assert_eq!(transport.subscriber_count(&here.id()), 2);

        transport
            .revoke(&here.id(), &peer.id())
            .await
            .expect("revoke");

        assert!(
            recv_within(&mut theirs, "the peer's view of us")
                .await
                .is_none()
        );
        assert!(
            recv_within(&mut ours, "our view of the peer")
                .await
                .is_none()
        );
        // …and the bystander's stream is untouched. One loopback hosts every
        // node in the process, so a revocation that reached for "every stream
        // from this node" would sever a subscription nobody revoked.
        assert!(!unrelated.is_closed());
        assert_eq!(transport.subscriber_count(&here.id()), 1);
        let event = delta(here.id(), "s", at(1), "still working");
        assert_eq!(transport.publish(&event), 1);
        let got = recv_within(&mut unrelated, "the bystander")
            .await
            .expect("event");
        assert_eq!(encoded(&got), encoded(&event));
    }

    #[tokio::test]
    async fn a_stalled_subscriber_loses_events_instead_of_stalling_the_transport() {
        let transport = LoopbackTransport::new();
        let watcher = identity(6).id();
        let peer = identity(7);
        transport
            .announce(&peer.announce("peer", Capability::none()))
            .await
            .expect("announce");
        let mut slow = transport
            .subscribe(&watcher, &peer.id())
            .await
            .expect("subscribe");

        let event = |i: usize| delta(peer.id(), "s", at(i as i64), &format!("tick {i}"));
        for i in 0..SUBSCRIPTION_BUFFER {
            assert_eq!(transport.publish(&event(i)), 1, "event {i} fits the buffer");
        }
        assert_eq!(slow.dropped(), 0);
        // Past the buffer: dropped for this subscriber, and `publish` returns
        // rather than waiting for a reader that may never come back. The queue
        // does not grow to fit the producer, however long the producer keeps
        // going.
        for i in 0..1_000 {
            assert_eq!(transport.publish(&event(SUBSCRIPTION_BUFFER + i)), 0);
        }
        assert_eq!(
            slow.dropped(),
            1_000,
            "the gap is counted, so a transcript can say so rather than lie by omission"
        );
        // The subscription survives, and still holds exactly the events it was
        // given: the bound is a count, and this is the count.
        assert!(!slow.is_closed());
        let mut held = 0;
        while slow.try_recv().is_some() {
            held += 1;
        }
        assert_eq!(held, SUBSCRIPTION_BUFFER);
    }

    #[tokio::test]
    async fn peer_supplied_event_text_is_sanitised_at_construction() {
        // The session id and the turn's text are both whatever the far end
        // typed. Neither may repaint a terminal or reorder a graph label.
        let event = delta(
            identity(8).id(),
            "sess\u{0007}ion",
            at(0),
            "\u{1b}[2Jwiped\u{202e}the screen",
        );
        assert_eq!(event.session.as_str(), "sess ion");
        let Some(AgentEvent::TextDelta(text)) = event.report() else {
            panic!("{event:?}");
        };
        assert!(!text.contains('\u{1b}'), "{text:?}");
        assert!(!text.contains('\u{202e}'), "{text:?}");
        assert!(text.contains("wiped"), "{text:?}");
    }

    #[tokio::test]
    async fn a_frame_that_is_not_a_report_carries_no_agent_event() {
        // The three lifecycle variants exist because the agent loop has nothing
        // to say about them, and `report` is honest about carrying nothing.
        for kind in [
            PeerEventKind::SessionStarted,
            PeerEventKind::SessionEnded,
            PeerEventKind::CapabilityChanged,
        ] {
            let label = kind.label();
            let event = PeerEvent::new(identity(9).id(), "s", at(0), kind);
            assert!(event.report().is_none(), "{label}");
        }
        // And an agent event that is a request rather than a report does not
        // become a frame at all.
        assert!(
            PeerEvent::turn(
                identity(9).id(),
                "s",
                at(0),
                &AgentEvent::CommandRequested("/model gpt-5.3-codex".into())
            )
            .is_none()
        );
    }
}
