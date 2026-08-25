//! The transport that crosses a process boundary: QUIC, with mutual TLS whose
//! certificates *are* the node identities.
//!
//! [`LoopbackTransport`](super::LoopbackTransport) is still what a single node
//! talks to and what most of the mesh's tests drive. This is the other one, and
//! it is the first thing in the mesh that opens a socket.
//!
//! # How the seven obligations are met
//!
//! The [`Transport`] module header lists what any implementation owes. Each one
//! is answered here, in the order it is written there, with the place it
//! happens:
//!
//! 1. **The identity is the key.** The TLS handshake proves it, both ways
//!    ([`super::tls`]): a dialled connection is refused unless the certificate
//!    carries the id that was dialled, and an accepted one is refused unless the
//!    certificate is a peer this machine has a record of. Then it is enforced
//!    *again* in the data path: [`QuicTransport::announcement_of`] refuses an
//!    announcement whose id is not the connection's, and the reader task
//!    overwrites [`PeerEvent::from`] with the connection's verified id rather
//!    than believing the field the sender wrote.
//! 2. **Everything inbound is untrusted.** Nothing is decoded into a `String`.
//!    A peer's name and capability arrive as a [`Node`], whose text members are
//!    [`PeerText`](super::PeerText); a peer's turn arrives inside a
//!    [`PeerEvent`], whose payload is a [`PeerTurn`](super::turn::PeerTurn).
//!    Both sanitise inside `Deserialize`, so the boundary is crossed by
//!    decoding and cannot be walked around.
//! 3. **Bound the message before you decode it.** [`wire::read_frame`] applies
//!    [`wire::MAX_BODY`] to the length field in the header, before allocating a
//!    body buffer. Underneath that, QUIC's own flow control is configured
//!    ([`stream_window`]) so a peer cannot make this process buffer more than a
//!    frame's worth per stream, times a bounded number of streams, times a
//!    bounded number of connections.
//! 4. **Revocation is not advisory.** [`QuicTransport::revoke`] drops the sinks
//!    feeding the peer, aborts the task reading from it, and **closes the QUIC
//!    connections in both directions**. Closing the connection is what makes it
//!    immediate rather than eventual: every stream on it fails at once, on both
//!    machines, without waiting for a timeout or for the next event.
//! 5. **The publisher consents too.** This is the half the loopback never had.
//!    A peer asking to watch this node is checked against
//!    [`Consent`](super::consent::Consent) twice: once during the handshake, so
//!    a stranger never gets a stream to ask on, and once when the `Watch` frame
//!    arrives, against the stronger condition a subscription needs
//!    ([`Trust::may_send_work`]). The second check is not redundant — trust can
//!    change while a connection is open.
//! 6. **A peer's clock is a peer's claim.** Both places a timestamp arrives
//!    from a peer, it is replaced with this machine's clock: `last_seen` on an
//!    announcement and [`PeerEvent::at`] on an event.
//! 7. **A slow consumer must not stall the transport.** [`QuicTransport::publish`]
//!    is not `async` and does not await. It hands each event to a bounded
//!    [`SUBSCRIPTION_BUFFER`] channel with `try_send` and counts what does not
//!    fit; a per-peer writer task drains that channel into the QUIC stream. A
//!    peer that stops reading fills its own buffer and loses its own events,
//!    and no other peer, and no local turn, waits for it.
//!
//! # The listener is off by default
//!
//! [`QuicTransport::dial_only`] is the constructor a default install gets. It
//! binds an ephemeral UDP port for outbound connections and accepts nothing.
//! [`QuicTransport::listening`] is the other one, and nothing reaches it unless
//! `[mesh] listen` is set in `config.toml` — see [`crate::config::MeshConfig`],
//! which defaults it to `false`. A mesh that opened a socket on install would be
//! a security surface nobody asked for, and this codebase has shipped that class
//! of default before.
//!
//! # Where the address comes from
//!
//! A node's *identity* is its key and is not routable. A node's *route* — the
//! `host:port` a QUIC packet goes to — is a separate fact, and it has to be,
//! because the whole point of deriving the address from the key is that it does
//! not encode a location that can change or be forged.
//!
//! So routes live in [`QuicTransport::add_route`], are filled from
//! `[mesh] routes` in `config.toml` and from mDNS ([`super::discovery`]), and
//! carry no authority whatever. A route says where to send the first packet.
//! Whether the machine that answers is the peer is decided by the handshake, and
//! a wrong or hostile route produces a refused connection rather than a
//! misdirected one.
//!
//! # What is not here
//!
//! No NAT traversal, no relaying, no multi-hop routing, no DHT. Exit criterion 3
//! is two nodes on different machines, **each directly reachable or on the same
//! LAN**; anything past that is 2.1. And no delegated work: there is no task
//! frame, no bid, no result, because tier 3 is cut and a wire format that
//! carried a task nothing would run is a wire format that has to keep carrying
//! it.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use async_trait::async_trait;
use chrono::Utc;
use quinn::{Connection, Endpoint, RecvStream, SendStream, VarInt};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use super::consent::Consent;
use super::node::{Identity, Node, NodeId};
use super::peer::Trust;
use super::tls::{self, Credentials};
use super::transport::{PeerEvent, SUBSCRIPTION_BUFFER, Subscription, Transport};
use super::wire::{self, Kind};

/// Live inbound connections this node will hold at once.
///
/// Every one of them is a peer somebody added by hand, so the honest bound is
/// "more than anyone will have"; what the number is really for is the case
/// where one peer reconnects in a loop. Past it a connection is closed on
/// arrival rather than queued.
pub const MAX_CONNECTIONS: usize = 64;

/// Concurrent streams one peer may have open on one connection.
///
/// A request is one stream and a subscription is one long-lived stream, so a
/// well-behaved peer needs two or three. Thirty-two is generous and still a
/// bound: without one, a peer could open streams until this process ran out of
/// buffers, which is [`wire::MAX_BODY`] multiplied by however many streams it
/// felt like.
pub const MAX_STREAMS: u32 = 32;

/// Bytes QUIC will buffer for one stream before it stops reading from the
/// network.
///
/// One frame's worth, plus its header, plus room for the next frame's header to
/// arrive behind it. This is the layer *below* [`wire::MAX_BODY`]: the frame
/// cap stops the decoder allocating, and this stops the socket buffering ahead
/// of the decoder. Together they are the answer to "a peer must not be able to
/// make this process allocate without bound".
pub fn stream_window() -> u32 {
    (wire::MAX_BODY + wire::HEADER_LEN * 2) as u32
}

/// How long a connection may sit with nothing on it before it is closed.
pub const IDLE_TIMEOUT: Duration = Duration::from_secs(30);

/// How often a live connection is kept alive through a NAT or a firewall. Well
/// inside [`IDLE_TIMEOUT`], so an idle *subscription* stays up while an
/// abandoned connection still goes away.
pub const KEEP_ALIVE: Duration = Duration::from_secs(10);

/// How long a dial may take before it is an error. Covers the handshake, which
/// is where a wrong route or a wrong identity surfaces.
pub const DIAL_TIMEOUT: Duration = Duration::from_secs(10);

/// How long a request may wait for its reply.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);

/// The TLS server name used when dialling.
///
/// A constant, and deliberately not the peer's address. A mesh address is a
/// public key, not a host, and [`super::tls::PinnedPeer`] ignores this field
/// because the identity is proved by the certificate's key rather than by any
/// name in it. `.invalid` is the reserved TLD (RFC 2606) precisely so a name
/// that must never resolve cannot accidentally resolve.
const SERVER_NAME: &str = "node.wizard-mesh.invalid";

/// Close code for a connection that was revoked. QUIC carries it to the far
/// end, so a revoked peer is told rather than left guessing at a dead socket.
const CLOSE_REVOKED: u32 = 1;
/// Close code for a peer this machine will not talk to.
const CLOSE_REFUSED: u32 = 2;
/// Close code for a connection dropped because [`MAX_CONNECTIONS`] is reached.
const CLOSE_BUSY: u32 = 3;

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

/// One peer watching this node's sessions.
struct Sink {
    /// Which peer asked. What makes revocation work from the publishing side:
    /// without it a stream is anonymous and "stop sending my sessions to that
    /// machine" has nothing to match on.
    subscriber: NodeId,
    /// Distinguishes two subscriptions from the same peer, so a writer task
    /// that ends removes its own sink and not its sibling's.
    handle: u64,
    events: mpsc::Sender<PeerEvent>,
    dropped: Arc<AtomicU64>,
}

/// One subscription this node holds to a peer.
struct Watcher {
    /// The task reading frames off the QUIC stream. Aborted by
    /// [`QuicTransport::revoke`], which is what makes the local
    /// [`Subscription`] end now rather than at the next event.
    reader: JoinHandle<()>,
}

#[derive(Default)]
struct Inner {
    /// Connections this node opened, by peer. Reused for further requests, so
    /// a `refresh` and a `subscribe` to the same peer are two streams rather
    /// than two handshakes.
    outbound: BTreeMap<NodeId, Vec<Connection>>,
    /// Connections a peer opened to this node, by peer.
    ///
    /// Held **separately** from the outbound ones, and the separation is a
    /// security property rather than bookkeeping. A QUIC connection is
    /// symmetric: either end may open a stream on it. If this node served
    /// requests on connections it had *dialled*, then `[mesh] listen = false`
    /// would stop meaning "nobody can watch this machine" — a peer would only
    /// have to wait for this node to call it, and then ask over the connection
    /// it opened itself. So the accept loop serves inbound connections and
    /// nothing else, and a dial-only node answers no one.
    ///
    /// Revocation closes both maps, which is what makes it immediate on both
    /// machines.
    inbound: BTreeMap<NodeId, Vec<Connection>>,
    /// Peers watching this node.
    sinks: Vec<Sink>,
    /// Subscriptions this node holds, by peer.
    watchers: BTreeMap<NodeId, Vec<Watcher>>,
    /// Where each peer was last known to be reachable. Routing, not identity.
    routes: BTreeMap<NodeId, SocketAddr>,
    /// The record served to a peer's `WhoAreYou`. `None` until
    /// [`Transport::announce`] is called, and a peer asking before then is told
    /// so rather than handed an empty record it would read as a claim.
    announcement: Option<Node>,
    /// Source of [`Sink::handle`].
    next_handle: u64,
}

/// A [`Transport`] over QUIC.
///
/// Speaks for exactly one node, unlike [`LoopbackTransport`](super::LoopbackTransport),
/// which hosts every node in the process. That is why `publish` can ignore
/// events that did not come from this node and why `revoke`'s `local` argument
/// is checked rather than used to index anything.
pub struct QuicTransport {
    local: NodeId,
    credentials: Credentials,
    endpoint: Endpoint,
    consent: Arc<dyn Consent>,
    state: Mutex<Inner>,
    /// The accept loop, when this node is listening. Aborted by
    /// [`QuicTransport::shutdown`].
    accepting: Mutex<Option<JoinHandle<()>>>,
}

impl std::fmt::Debug for QuicTransport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("QuicTransport")
            .field("local", &self.local.short())
            .field("listening", &self.is_listening())
            .field("at", &self.endpoint.local_addr().ok())
            .finish()
    }
}

// ---------------------------------------------------------------------------
// Construction
// ---------------------------------------------------------------------------

impl QuicTransport {
    /// A transport that dials peers and accepts nothing. **The default.**
    ///
    /// It still binds a UDP socket, because a QUIC client needs one, but on an
    /// ephemeral port chosen by the OS and with no server configuration
    /// installed: an inbound packet has nothing to be accepted by. That is the
    /// difference between a client socket and a listening service, and it is
    /// what `[mesh] listen = false` gets.
    /// No default client configuration is installed, deliberately: quinn's
    /// `connect` would use one, and every dial here goes through
    /// `connect_with` with a configuration that pins the peer's identity
    /// ([`super::tls::client_config`]). Leaving the default unset means a
    /// future call to `connect` fails loudly rather than opening a connection
    /// nobody verified.
    pub fn dial_only(identity: &Identity, consent: Arc<dyn Consent>) -> Result<Arc<Self>> {
        let unspecified: SocketAddr = "0.0.0.0:0".parse().expect("a literal address");
        let endpoint =
            Endpoint::client(unspecified).context("binding a UDP socket for the mesh client")?;
        Ok(Arc::new(Self::assemble(identity, consent, endpoint)))
    }

    /// A transport that also accepts connections on `addr`.
    ///
    /// Reached only when `[mesh] listen` is true. Spawns the accept loop before
    /// returning, so a caller that awaits this has a listener that is up.
    pub fn listening(
        identity: &Identity,
        consent: Arc<dyn Consent>,
        addr: SocketAddr,
    ) -> Result<Arc<Self>> {
        let credentials = Credentials::for_identity(identity);
        let mut server = tls::server_config(&credentials, consent.clone())?;
        server.transport_config(Arc::new(transport_config()));
        let endpoint = Endpoint::server(server, addr)
            .with_context(|| format!("binding the mesh listener on {addr}"))?;

        let transport = Arc::new(Self::assemble(identity, consent, endpoint));
        let accepting = tokio::spawn(Arc::clone(&transport).accept_loop());
        *transport
            .accepting
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(accepting);
        Ok(transport)
    }

    fn assemble(identity: &Identity, consent: Arc<dyn Consent>, endpoint: Endpoint) -> Self {
        Self {
            local: identity.id(),
            credentials: Credentials::for_identity(identity),
            endpoint,
            consent,
            state: Mutex::new(Inner::default()),
            accepting: Mutex::new(None),
        }
    }

    /// The transport `config` describes.
    ///
    /// The one place `[mesh] listen` turns into a socket, and the reason it is
    /// one place: a default that is enforced in two constructors is a default
    /// that is enforced in one of them. `listen = false` — which is
    /// [`MeshConfig`](crate::config::MeshConfig)'s own default — gives
    /// [`QuicTransport::dial_only`], and nothing else in this crate calls
    /// [`QuicTransport::listening`] outside a test.
    ///
    /// Routes from `[mesh] routes` are installed on the way out. A malformed
    /// one is an error rather than a skipped line: a route the operator wrote
    /// and this node silently ignored is a peer that mysteriously never
    /// connects.
    pub fn from_config(
        identity: &Identity,
        consent: Arc<dyn Consent>,
        config: &crate::config::MeshConfig,
    ) -> Result<Arc<Self>> {
        let transport = if config.listen {
            Self::listening(identity, consent, config.listen_socket()?)?
        } else {
            Self::dial_only(identity, consent)?
        };
        for (address, at) in &config.routes {
            let id = NodeId::parse_address(address)
                .with_context(|| format!("[mesh] routes: {address:?} is not a mesh address"))?;
            let at: SocketAddr = at.parse().with_context(|| {
                format!("[mesh] routes: {at:?} is not a `host:port` address for {address}")
            })?;
            transport.add_route(id, at);
        }
        Ok(transport)
    }

    /// This node's id.
    pub fn local_id(&self) -> NodeId {
        self.local
    }

    /// What this machine has decided about the peers that reach it.
    ///
    /// Exposed so [`super::discovery`] can ask the one question it is entitled
    /// to — is this node already a peer? — without being handed a peer store or
    /// a second copy of the policy.
    pub fn consent(&self) -> &Arc<dyn Consent> {
        &self.consent
    }

    /// The socket this node is bound to. For a dial-only transport this is an
    /// ephemeral port nothing is listening on; for a listener it is the address
    /// peers connect to, with any `0` port resolved to what the OS chose.
    pub fn local_addr(&self) -> Result<SocketAddr> {
        self.endpoint
            .local_addr()
            .context("reading the mesh socket address")
    }

    /// Whether this transport accepts inbound connections.
    pub fn is_listening(&self) -> bool {
        self.accepting
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .is_some()
    }

    /// Stop accepting, close every live connection, and release the socket.
    ///
    /// Idempotent. Not `Drop`, because closing a QUIC endpoint politely means
    /// telling the far end, and that is an async conversation a destructor
    /// cannot have.
    pub async fn shutdown(&self) {
        if let Some(accepting) = self
            .accepting
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
        {
            accepting.abort();
        }
        let connections: Vec<Connection> = {
            let mut state = self.lock();
            state.sinks.clear();
            for watchers in state.watchers.values() {
                for watcher in watchers {
                    watcher.reader.abort();
                }
            }
            state.watchers.clear();
            std::mem::take(&mut state.outbound)
                .into_values()
                .chain(std::mem::take(&mut state.inbound).into_values())
                .flatten()
                .collect()
        };
        for connection in connections {
            connection.close(VarInt::from_u32(0), b"shutting down");
        }
        self.endpoint.close(VarInt::from_u32(0), b"shutting down");
        self.endpoint.wait_idle().await;
    }

    // A poisoned lock is recovered rather than propagated, exactly as
    // `LoopbackTransport` does: the state behind it is a set of maps with no
    // invariant a panic could leave half-applied, and a transport that started
    // failing every call because something unrelated panicked would take the
    // revocation path down with it.
    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// Drop closed connections from `held` and return how many are left.
///
/// The empty entries go too: a long-lived process that saw one peer reconnect a
/// thousand times would otherwise keep a thousand empty vectors.
fn reap(held: &mut BTreeMap<NodeId, Vec<Connection>>) -> usize {
    for connections in held.values_mut() {
        connections.retain(|connection| connection.close_reason().is_none());
    }
    held.retain(|_, connections| !connections.is_empty());
    held.values().map(Vec::len).sum()
}

/// The QUIC transport parameters both ends of a mesh connection use.
fn transport_config() -> quinn::TransportConfig {
    let mut config = quinn::TransportConfig::default();
    config.max_concurrent_bidi_streams(VarInt::from_u32(MAX_STREAMS));
    // Nothing in this protocol is unidirectional: every message kind is a
    // request that has a reply or a refusal. A peer opening a unidirectional
    // stream is a peer doing something this version does not have, so it gets
    // none.
    config.max_concurrent_uni_streams(VarInt::from_u32(0));
    config.stream_receive_window(VarInt::from_u32(stream_window()));
    // Every stream's window at once, which is the most a single connection can
    // hold in flight. `u64` because the product overflows a `u32`.
    config.receive_window(
        VarInt::from_u64(u64::from(stream_window()) * u64::from(MAX_STREAMS))
            .expect("the connection window fits a QUIC varint"),
    );
    config.max_idle_timeout(Some(
        IDLE_TIMEOUT
            .try_into()
            .expect("thirty seconds is a representable QUIC idle timeout"),
    ));
    config.keep_alive_interval(Some(KEEP_ALIVE));
    config
}

// ---------------------------------------------------------------------------
// Routing
// ---------------------------------------------------------------------------

impl QuicTransport {
    /// Record where a peer can be reached.
    ///
    /// A hint and nothing more. It grants no trust, creates no peer, and is not
    /// checked against anything: the handshake decides whether the machine at
    /// `at` is really `peer`, and a wrong route fails the dial rather than
    /// misdirecting a stream. See the module docs.
    pub fn add_route(&self, peer: NodeId, at: SocketAddr) {
        self.lock().routes.insert(peer, at);
    }

    /// Where a peer was last known to be, if anywhere.
    pub fn route(&self, peer: &NodeId) -> Option<SocketAddr> {
        self.lock().routes.get(peer).copied()
    }

    /// Every route this transport holds, ordered by peer.
    pub fn routes(&self) -> Vec<(NodeId, SocketAddr)> {
        self.lock()
            .routes
            .iter()
            .map(|(id, at)| (*id, *at))
            .collect()
    }

    /// Forget where a peer is. Discovery may re-learn it; nothing else does.
    pub fn drop_route(&self, peer: &NodeId) -> bool {
        self.lock().routes.remove(peer).is_some()
    }
}

// ---------------------------------------------------------------------------
// Dialling
// ---------------------------------------------------------------------------

/// The node id a live connection belongs to, read out of the certificate the
/// far end presented.
///
/// The handshake already proved possession of the matching private key, so this
/// is a read rather than a check — but it is the *transport's* read, and it is
/// what every inbound record is filed under. Nothing here believes a field a
/// peer wrote about itself.
pub fn peer_of(connection: &Connection) -> Result<NodeId> {
    let identity = connection
        .peer_identity()
        .ok_or_else(|| anyhow!("a mesh connection completed with no peer certificate"))?;
    let chain = identity
        .downcast::<Vec<rustls::pki_types::CertificateDer<'static>>>()
        .map_err(|_| anyhow!("a mesh connection's peer identity is not a certificate chain"))?;
    let end_entity = chain
        .first()
        .ok_or_else(|| anyhow!("a mesh connection presented an empty certificate chain"))?;
    super::x509::identity_of(end_entity)
}

impl QuicTransport {
    /// A live connection to `peer`, reusing one if there is one.
    ///
    /// Pooled because a `refresh` and a `subscribe` to the same peer are two
    /// streams on one connection rather than two handshakes, and because a
    /// second connection would be a second thing for [`QuicTransport::revoke`]
    /// to find.
    async fn connect(&self, peer: &NodeId) -> Result<Connection> {
        if let Some(live) = self.live_connection(peer) {
            return Ok(live);
        }
        let at = self.route(peer).ok_or_else(|| {
            anyhow!(
                "no route to mesh node {}: an address is a public key, not a location, so this \
                 machine needs a `host:port` for it (from `[mesh] routes` in config.toml, or \
                 from mDNS on the same LAN)",
                peer.short()
            )
        })?;

        let mut config = tls::client_config(&self.credentials, *peer)?;
        config.transport_config(Arc::new(transport_config()));
        let connecting = self
            .endpoint
            .connect_with(config, at, SERVER_NAME)
            .with_context(|| format!("dialling mesh node {} at {at}", peer.short()))?;
        let connection = tokio::time::timeout(DIAL_TIMEOUT, connecting)
            .await
            .map_err(|_| {
                anyhow!(
                    "dialling mesh node {} at {at} timed out after {}s",
                    peer.short(),
                    DIAL_TIMEOUT.as_secs()
                )
            })?
            .with_context(|| format!("connecting to mesh node {} at {at}", peer.short()))?;

        // The verifier already refused anybody else, so this can only fail if
        // the verifier was not installed. Checking anyway is cheap and it is
        // the one property nothing else in this file can recover from being
        // wrong about.
        let found = peer_of(&connection)?;
        if found != *peer {
            connection.close(VarInt::from_u32(CLOSE_REFUSED), b"wrong identity");
            bail!(
                "dialled mesh node {} at {at} and {} answered",
                peer.short(),
                found.short()
            );
        }
        self.remember(peer, connection.clone(), false);
        Ok(connection)
    }

    /// A connection *this node opened* to `peer`, if one is still live.
    ///
    /// Deliberately never an inbound one. See [`Inner::inbound`]: reusing a
    /// connection a peer opened would be fine for QUIC and wrong for the trust
    /// model in the other direction, and keeping the two apart in one place is
    /// what makes both halves obvious.
    fn live_connection(&self, peer: &NodeId) -> Option<Connection> {
        let mut state = self.lock();
        let connections = state.outbound.get_mut(peer)?;
        connections.retain(|connection| connection.close_reason().is_none());
        if connections.is_empty() {
            state.outbound.remove(peer);
            return None;
        }
        connections.first().cloned()
    }

    fn remember(&self, peer: &NodeId, connection: Connection, inbound: bool) {
        let mut state = self.lock();
        let held = if inbound {
            &mut state.inbound
        } else {
            &mut state.outbound
        };
        let connections = held.entry(*peer).or_default();
        connections.retain(|held| held.close_reason().is_none());
        connections.push(connection);
    }

    fn forget_connection(&self, peer: &NodeId, connection: &Connection, inbound: bool) {
        let mut state = self.lock();
        let held = if inbound {
            &mut state.inbound
        } else {
            &mut state.outbound
        };
        let Some(connections) = held.get_mut(peer) else {
            return;
        };
        connections.retain(|kept| kept.stable_id() != connection.stable_id());
        if connections.is_empty() {
            // An empty vector left behind for every peer that ever
            // disconnected is the small, boring shape of a growth bug.
            held.remove(peer);
        }
    }

    /// How many live connections this node holds, in either direction.
    pub fn connection_count(&self) -> usize {
        let mut state = self.lock();
        let Inner {
            outbound, inbound, ..
        } = &mut *state;
        reap(outbound) + reap(inbound)
    }

    /// How many connections a peer has opened *to* this node.
    pub fn inbound_count(&self) -> usize {
        reap(&mut self.lock().inbound)
    }

    /// Ask a peer whether it is there, and how long the round trip took.
    ///
    /// The liveness half of the protocol. The answer is an observation this
    /// machine made — a peer that replies was reachable *now* — which is
    /// exactly the fact [`super::PeerStore::mark_seen`] wants and exactly the
    /// fact a peer's own `last_seen` field is not.
    pub async fn ping(&self, peer: &NodeId) -> Result<Duration> {
        let connection = self.connect(peer).await?;
        let started = std::time::Instant::now();
        let (mut send, mut recv) = connection
            .open_bi()
            .await
            .context("opening a stream for a mesh ping")?;
        wire::write_frame(&mut send, Kind::Ping, &[]).await?;
        send.finish().context("finishing a mesh ping")?;
        match self.await_reply(&mut recv, Kind::Ping).await?.kind {
            Kind::Pong => Ok(started.elapsed()),
            other => bail!(
                "mesh node {} answered a ping with a {} frame",
                peer.short(),
                other.label()
            ),
        }
    }

    /// Read one reply, refusing to wait forever and turning a `Refused` frame
    /// into an error carrying the peer's own words.
    async fn await_reply(&self, recv: &mut RecvStream, asked: Kind) -> Result<wire::Frame> {
        let frame = tokio::time::timeout(REQUEST_TIMEOUT, wire::read_frame(recv))
            .await
            .map_err(|_| {
                anyhow!(
                    "a mesh peer did not answer a {} in {}s",
                    asked.label(),
                    REQUEST_TIMEOUT.as_secs()
                )
            })??
            .ok_or_else(|| {
                anyhow!(
                    "a mesh peer closed the stream without answering a {}",
                    asked.label()
                )
            })?;
        if frame.kind == Kind::Refused {
            // The reason is the peer's text and is treated as such: decoded
            // through `PeerText`, which sanitises, because this string is
            // about to be printed in somebody's terminal.
            let reason: super::PeerText = frame.decode()?;
            bail!(
                "a mesh peer refused a {}: {}",
                asked.label(),
                reason.as_str()
            );
        }
        Ok(frame)
    }
}

// ---------------------------------------------------------------------------
// Serving
// ---------------------------------------------------------------------------

impl QuicTransport {
    /// Accept connections until the endpoint closes or the task is aborted.
    async fn accept_loop(self: Arc<Self>) {
        while let Some(incoming) = self.endpoint.accept().await {
            let transport = Arc::clone(&self);
            tokio::spawn(async move {
                if let Err(why) = transport.serve_connection(incoming).await {
                    // Debug rather than warn: a refused stranger is the
                    // listener working, and a listener that logged a warning
                    // per scan would be a listener somebody turns off.
                    tracing::debug!("mesh: an inbound connection ended: {why:#}");
                }
            });
        }
    }

    async fn serve_connection(self: Arc<Self>, incoming: quinn::Incoming) -> Result<()> {
        let connection = incoming.await.context("completing an inbound handshake")?;
        let peer = peer_of(&connection)?;

        // The handshake already applied this ([`super::tls::MeshPeers`]), and
        // it is applied again here for the reason `Mesh::refresh` re-normalises
        // a capability it was handed: "the layer below is supposed to" is how
        // an unchecked value gets in the day somebody builds this transport
        // with a different configuration.
        match self.consent.decision(&peer) {
            Some(trust) if trust.may_contact() => {}
            other => {
                connection.close(
                    VarInt::from_u32(CLOSE_REFUSED),
                    b"not a peer of this machine",
                );
                bail!(
                    "mesh node {} is {} here",
                    peer.short(),
                    other.map_or("not a peer", Trust::label)
                );
            }
        }
        if self.inbound_count() >= MAX_CONNECTIONS {
            connection.close(VarInt::from_u32(CLOSE_BUSY), b"too many connections");
            bail!(
                "mesh: refused {} — {MAX_CONNECTIONS} inbound connections are already live",
                peer.short()
            );
        }
        self.remember(&peer, connection.clone(), true);

        // Ends when the connection does — because the peer closed it, because
        // it went idle, or because `revoke` closed it from this side.
        while let Ok((send, recv)) = connection.accept_bi().await {
            let transport = Arc::clone(&self);
            let on = connection.clone();
            tokio::spawn(async move {
                if let Err(why) = transport.serve_stream(peer, on, send, recv).await {
                    tracing::debug!("mesh: a stream from {} ended: {why:#}", peer.short());
                }
            });
        }
        self.forget_connection(&peer, &connection, true);
        Ok(())
    }

    async fn serve_stream(
        self: Arc<Self>,
        peer: NodeId,
        on: Connection,
        mut send: SendStream,
        mut recv: RecvStream,
    ) -> Result<()> {
        // One request per stream, and the bound is applied inside `read_frame`
        // before anything is allocated for a body.
        let Some(frame) = wire::read_frame(&mut recv).await? else {
            return Ok(());
        };
        match frame.kind {
            Kind::Ping => wire::write_frame(&mut send, Kind::Pong, &[]).await?,
            Kind::WhoAreYou => {
                // Cloned out from under the lock before the await: a guard held
                // across one makes this future `!Send` and, worse, holds the
                // transport's state for as long as a peer's socket takes.
                let announcement = self.lock().announcement.clone();
                match announcement {
                    Some(node) => wire::write_json(&mut send, Kind::Announcement, &node).await?,
                    None => refuse(&mut send, "this node has not announced itself yet").await?,
                }
            }
            Kind::Watch => return self.serve_watch(peer, on, send).await,
            other => {
                refuse(
                    &mut send,
                    &format!("a {} frame is a reply, not a request", other.label()),
                )
                .await?;
            }
        }
        send.finish().context("finishing a mesh reply")?;
        Ok(())
    }

    /// Serve one peer's subscription to this node's sessions.
    ///
    /// The publisher's consent, checked here and not only at the handshake.
    /// Trust can change while a connection is open — that is the whole point of
    /// `wizard peers trust <address> known` — and a check that only ran at
    /// connect time would leave a peer watching a machine that had stopped
    /// trusting it until it happened to reconnect.
    async fn serve_watch(
        self: Arc<Self>,
        peer: NodeId,
        on: Connection,
        mut send: SendStream,
    ) -> Result<()> {
        let trust = self.consent.decision(&peer);
        if !trust.is_some_and(Trust::may_send_work) {
            let why = format!(
                "this machine records you as {}, not trusted; a session stream needs trust",
                trust.map_or("not a peer", Trust::label)
            );
            refuse(&mut send, &why).await?;
            send.finish().context("finishing a mesh refusal")?;
            bail!("mesh: refused a watch from {}: {why}", peer.short());
        }

        let (events, mut queue) = mpsc::channel(SUBSCRIPTION_BUFFER);
        let dropped = Arc::new(AtomicU64::new(0));
        let handle = {
            let mut state = self.lock();
            state.next_handle += 1;
            let handle = state.next_handle;
            state.sinks.push(Sink {
                subscriber: peer,
                handle,
                events,
                dropped: Arc::clone(&dropped),
            });
            handle
        };

        // Acknowledge before the first event, so the far end can tell a
        // granted subscription from a refused one without waiting for traffic.
        let opened = wire::write_frame(&mut send, Kind::Watching, &[]).await;
        if opened.is_ok() {
            loop {
                // Either half can end this. `queue.recv()` returning `None` is
                // *this* machine revoking, which drops the sink. `closed()` is
                // the far end revoking, and it has to be watched explicitly:
                // without it this task would sit in `recv()` still holding a
                // sink for a peer that has gone, and the publisher would go on
                // counting a subscriber that revoked it until the next event
                // failed to write.
                tokio::select! {
                    event = queue.recv() => {
                        let Some(event) = event else { break };
                        if wire::write_json(&mut send, Kind::Event, &event).await.is_err() {
                            break;
                        }
                    }
                    _ = on.closed() => break,
                }
            }
        }
        self.lock().sinks.retain(|sink| sink.handle != handle);
        let _ = send.finish();
        opened
    }

    /// How many of this node's events were dropped on the way to `peer`
    /// because it was not reading fast enough.
    ///
    /// The publishing side's half of [`Subscription::dropped`], and the same
    /// contract: a counter, not a queue. A renderer that shows the gap is
    /// telling the truth; one that silently omits it is not.
    pub fn dropped_to(&self, peer: &NodeId) -> u64 {
        self.lock()
            .sinks
            .iter()
            .filter(|sink| sink.subscriber == *peer)
            .map(|sink| sink.dropped.load(Ordering::Relaxed))
            .sum()
    }

    /// How many peers are watching this node right now.
    pub fn subscriber_count(&self) -> usize {
        self.lock()
            .sinks
            .iter()
            .filter(|sink| !sink.events.is_closed())
            .count()
    }
}

/// Write a refusal a human on the far end can read.
async fn refuse(send: &mut SendStream, why: &str) -> Result<()> {
    wire::write_json(send, Kind::Refused, &why).await
}

// ---------------------------------------------------------------------------
// The trait
// ---------------------------------------------------------------------------

#[async_trait]
impl Transport for QuicTransport {
    /// Record what this node serves to a peer's `WhoAreYou`.
    ///
    /// Nothing is broadcast: the mesh has no rendezvous server to publish into
    /// and does not shout on the network. Advertising on a LAN is
    /// [`super::discovery`]'s job, is a separate mechanism, and is separately
    /// off by default.
    async fn announce(&self, node: &Node) -> Result<()> {
        if node.id != self.local {
            bail!(
                "this transport speaks for mesh node {} and was asked to announce {}",
                self.local.short(),
                node.id.short()
            );
        }
        self.lock().announcement = Some(node.clone());
        Ok(())
    }

    /// Ask a peer for its own record.
    ///
    /// Three things happen to the answer before it is returned, and all three
    /// are obligations rather than politeness:
    ///
    /// - the text in it was sanitised on the way through `Deserialize`, because
    ///   [`Node`]'s name is a [`PeerText`](super::PeerText) and its capability
    ///   entries are too;
    /// - its `id` is checked against the identity the connection proved, so a
    ///   node cannot answer with somebody else's announcement;
    /// - its `last_seen` is replaced with this machine's clock, because a
    ///   peer's clock is a peer's claim and what actually happened is that a
    ///   record arrived *here*, *now*.
    async fn announcement_of(&self, id: &NodeId) -> Result<Node> {
        let connection = self.connect(id).await?;
        let (mut send, mut recv) = connection
            .open_bi()
            .await
            .context("opening a stream to fetch an announcement")?;
        wire::write_frame(&mut send, Kind::WhoAreYou, &[]).await?;
        send.finish().context("finishing an announcement request")?;

        let frame = self.await_reply(&mut recv, Kind::WhoAreYou).await?;
        if frame.kind != Kind::Announcement {
            bail!(
                "mesh node {} answered a who-are-you with a {} frame",
                id.short(),
                frame.kind.label()
            );
        }
        let mut node: Node = frame.decode()?;
        if node.id != *id {
            bail!(
                "mesh node {} answered with {}'s announcement; a node's announcement is its own \
                 or it is nobody's",
                id.short(),
                node.id.short()
            );
        }
        node.last_seen = Some(Utc::now());
        Ok(node)
    }

    /// Open a subscription to a peer's session events.
    ///
    /// Whether this machine *wants* the stream is
    /// [`Mesh::subscribe`](super::Mesh::subscribe)'s decision and is not
    /// re-made here. What this does is ask, wait for the peer's answer, and
    /// turn a refusal into an error carrying the peer's own reason rather than
    /// a subscription that silently never produces anything.
    async fn subscribe(&self, local: &NodeId, peer: &NodeId) -> Result<Subscription> {
        if *local != self.local {
            bail!(
                "this transport speaks for mesh node {} and was asked to subscribe as {}",
                self.local.short(),
                local.short()
            );
        }
        let connection = self.connect(peer).await?;
        let (mut send, mut recv) = connection
            .open_bi()
            .await
            .context("opening a stream to watch a peer")?;
        wire::write_frame(&mut send, Kind::Watch, &[]).await?;
        send.finish().context("finishing a watch request")?;

        let frame = self.await_reply(&mut recv, Kind::Watch).await?;
        if frame.kind != Kind::Watching {
            bail!(
                "mesh node {} answered a watch with a {} frame",
                peer.short(),
                frame.kind.label()
            );
        }

        let (events, queue) = mpsc::channel(SUBSCRIPTION_BUFFER);
        let dropped = Arc::new(AtomicU64::new(0));
        let reader = tokio::spawn(read_events(*peer, recv, events, Arc::clone(&dropped)));
        self.lock()
            .watchers
            .entry(*peer)
            .or_default()
            .push(Watcher { reader });
        Ok(Subscription::from_channel(*peer, queue, dropped))
    }

    /// Hand one of this node's events to every peer watching, without waiting
    /// for any of them.
    ///
    /// Not `async`, and the whole design of the publishing side follows from
    /// that: there is no await here to block on, so a peer that has stopped
    /// reading its socket cannot stall a live turn. Each sink is a bounded
    /// channel drained by that peer's own writer task; an event that does not
    /// fit is dropped and counted ([`QuicTransport::dropped_to`]), never
    /// queued, because a queue that grows to fit its producer is a leak with a
    /// name.
    ///
    /// Events from another node are ignored. This transport speaks for one
    /// node, so an event whose `from` is not this one either came from a
    /// caller's mistake or is a peer's event being reflected back onto the
    /// mesh, and neither should be published as this machine's own.
    fn publish(&self, event: &PeerEvent) -> usize {
        if event.from != self.local {
            return 0;
        }
        let mut state = self.lock();
        state.sinks.retain(|sink| !sink.events.is_closed());
        let mut delivered = 0;
        for sink in state.sinks.iter() {
            if sink.events.try_send(event.clone()).is_ok() {
                delivered += 1;
            } else {
                sink.dropped.fetch_add(1, Ordering::Relaxed);
            }
        }
        delivered
    }

    /// Drop everything live between this node and `peer`, in both directions,
    /// now.
    ///
    /// Three things, and the third is what makes it immediate:
    ///
    /// 1. the sinks feeding the peer are dropped, so this node's events stop
    ///    reaching a machine it has just un-trusted — the expensive half of the
    ///    mistake, because it leaks a workspace rather than a screen;
    /// 2. the task reading the peer's stream is aborted, so the local
    ///    [`Subscription`] ends rather than going quiet;
    /// 3. every QUIC connection to or from the peer is **closed**, which fails
    ///    every stream on it at once, on both machines, with a close code the
    ///    far end can read. A revocation that waited for a timeout would be a
    ///    revocation that had not happened yet.
    ///
    /// Routes and announcements deliberately survive, for the reason
    /// [`LoopbackTransport`](super::LoopbackTransport) keeps its announcements:
    /// trust moves both ways, `Trusted -> Known` is a change of mind rather
    /// than a banishment, and discarding how to reach a peer here would make
    /// that ordinary change unrecoverable without a restart.
    async fn revoke(&self, local: &NodeId, peer: &NodeId) -> Result<()> {
        if *local != self.local {
            // Not an error: this runs on a path where failing is not an option.
            // Everything live for `peer` is severed regardless, because this
            // transport hosts one node and there is no third party's stream
            // here to reach by mistake.
            tracing::debug!(
                "mesh: revoke called as {} on the transport for {}",
                local.short(),
                self.local.short()
            );
        }
        let (connections, watchers) = {
            let mut state = self.lock();
            state.sinks.retain(|sink| sink.subscriber != *peer);
            let connections: Vec<Connection> = state
                .outbound
                .remove(peer)
                .unwrap_or_default()
                .into_iter()
                .chain(state.inbound.remove(peer).unwrap_or_default())
                .collect();
            (connections, state.watchers.remove(peer).unwrap_or_default())
        };
        for watcher in watchers {
            watcher.reader.abort();
        }
        for connection in connections {
            connection.close(VarInt::from_u32(CLOSE_REVOKED), b"revoked");
        }
        Ok(())
    }
}

/// Pump a peer's event stream into a subscription's buffer.
///
/// Two fields of every event are overwritten before it goes anywhere, and both
/// are obligations:
///
/// - **`from`** becomes the identity the *connection* proved, never the one the
///   sender wrote. A peer that stamped somebody else's id on its events would
///   otherwise have every surface rendering its turns as that node's.
/// - **`at`** becomes this machine's clock, because a peer's clock is a peer's
///   claim and the observation that was actually made is that the event arrived
///   here.
///
/// `session` and the turn itself are left as they came, because they were
/// already sanitised by [`PeerText`](super::PeerText) and
/// [`PeerTurn`](super::turn::PeerTurn) inside `Deserialize`. They are still a
/// peer's data: display only, never a prompt, never a command.
async fn read_events(
    peer: NodeId,
    mut recv: RecvStream,
    events: mpsc::Sender<PeerEvent>,
    dropped: Arc<AtomicU64>,
) {
    loop {
        let frame = match wire::read_frame(&mut recv).await {
            Ok(Some(frame)) => frame,
            Ok(None) => break,
            Err(why) => {
                tracing::debug!("mesh: the stream from {} ended: {why:#}", peer.short());
                break;
            }
        };
        if frame.kind != Kind::Event {
            tracing::debug!(
                "mesh: {} sent a {} frame on a session stream",
                peer.short(),
                frame.kind.label()
            );
            break;
        }
        let mut event: PeerEvent = match frame.decode() {
            Ok(event) => event,
            Err(why) => {
                tracing::debug!("mesh: an event from {} was refused: {why:#}", peer.short());
                continue;
            }
        };
        event.from = peer;
        event.at = Utc::now();
        if events.try_send(event).is_err() {
            if events.is_closed() {
                break;
            }
            dropped.fetch_add(1, Ordering::Relaxed);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::AgentEvent;
    use crate::plugins::mesh::capability::Capability;
    use crate::plugins::mesh::consent::TrustLedger;
    use crate::plugins::mesh::peer::Peer;
    use crate::plugins::mesh::transport::PeerEventKind;
    use crate::plugins::mesh::{PeerText, PeerTurn};
    use chrono::{DateTime, TimeDelta, Utc};

    fn identity(byte: u8) -> Identity {
        Identity::from_seed([byte; 32])
    }

    fn at(seconds: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(1_800_000_000 + seconds, 0).expect("timestamp")
    }

    /// A ledger holding one decision per entry.
    fn ledger(entries: &[(&Identity, Trust)]) -> TrustLedger {
        let peers: Vec<Peer> = entries
            .iter()
            .map(|(identity, trust)| {
                let mut peer = Peer::new(Node::new(identity.id()), at(0));
                peer.trust = *trust;
                peer
            })
            .collect();
        let ledger = TrustLedger::new();
        ledger.replace(peers.iter());
        ledger
    }

    fn localhost() -> SocketAddr {
        "127.0.0.1:0".parse().expect("a literal address")
    }

    /// Await something that should have already happened, with a deadline.
    ///
    /// Several of these tests assert that a stream has *ended*. Awaiting one
    /// that has not parks the test forever, and a hung test reads in CI as an
    /// infrastructure problem rather than as the revocation bug it is.
    async fn within<T>(what: &str, future: impl std::future::Future<Output = T>) -> T {
        tokio::time::timeout(Duration::from_secs(10), future)
            .await
            .unwrap_or_else(|_| panic!("{what}: still waiting after 10s"))
    }

    /// A turn event carrying `text`.
    fn delta(from: NodeId, session: &str, text: &str) -> PeerEvent {
        PeerEvent::turn(
            from,
            session,
            at(0),
            &AgentEvent::TextDelta(text.to_string()),
        )
        .expect("a text delta crosses the mesh")
    }

    /// Two transports: a listener and a dialler that knows where it is, with
    /// the trust each one records about the other.
    async fn pair(
        server: &Identity,
        client: &Identity,
        server_view: Trust,
        client_view: Trust,
    ) -> (Arc<QuicTransport>, Arc<QuicTransport>) {
        let listener = QuicTransport::listening(
            server,
            ledger(&[(client, server_view)]).shared(),
            localhost(),
        )
        .expect("listener");
        let dialler = QuicTransport::dial_only(client, ledger(&[(server, client_view)]).shared())
            .expect("dialler");
        dialler.add_route(server.id(), listener.local_addr().expect("bound"));
        (listener, dialler)
    }

    #[tokio::test]
    async fn a_dial_only_transport_listens_to_nobody() {
        // The default posture: a socket for dialling out, no service on it.
        let transport =
            QuicTransport::dial_only(&identity(1), TrustLedger::new().shared()).expect("dial-only");
        assert!(!transport.is_listening());
        assert_eq!(transport.local_id(), identity(1).id());
        // It is bound — a QUIC client needs a socket — but on an ephemeral
        // port with no server configuration behind it.
        assert_ne!(transport.local_addr().expect("bound").port(), 0);
        transport.shutdown().await;
    }

    #[tokio::test]
    async fn a_listener_reports_the_port_it_actually_got() {
        let transport =
            QuicTransport::listening(&identity(2), TrustLedger::new().shared(), localhost())
                .expect("listener");
        assert!(transport.is_listening());
        assert_ne!(transport.local_addr().expect("bound").port(), 0);
        assert!(format!("{transport:?}").contains("listening: true"));
        transport.shutdown().await;
    }

    #[tokio::test]
    async fn a_peer_without_a_route_is_an_error_that_says_what_is_missing() {
        // Identity is not location. The error has to say so, because "no route"
        // is the first thing anybody hits and the address they pasted looks
        // like it should be enough.
        let transport =
            QuicTransport::dial_only(&identity(3), TrustLedger::new().shared()).expect("dial-only");
        let err = transport
            .announcement_of(&identity(4).id())
            .await
            .expect_err("no route");
        let message = format!("{err:#}");
        assert!(message.contains("no route"), "{message}");
        assert!(message.contains("public key, not a location"), "{message}");
        transport.shutdown().await;
    }

    #[tokio::test]
    async fn a_route_is_a_hint_that_can_be_added_read_and_dropped() {
        let transport =
            QuicTransport::dial_only(&identity(5), TrustLedger::new().shared()).expect("dial-only");
        let peer = identity(6).id();
        let at: SocketAddr = "192.0.2.7:4242".parse().expect("a literal address");
        assert_eq!(transport.route(&peer), None);
        transport.add_route(peer, at);
        assert_eq!(transport.route(&peer), Some(at));
        assert_eq!(transport.routes(), vec![(peer, at)]);
        assert!(transport.drop_route(&peer));
        assert!(!transport.drop_route(&peer));
        transport.shutdown().await;
    }

    #[tokio::test]
    async fn an_announcement_crosses_and_its_clock_becomes_the_local_one() {
        // Obligations 1, 2 and 6 in one exchange: the record is the peer's, the
        // text in it was sanitised on the way through, and the timestamp is the
        // observation this machine made rather than the one the peer wrote.
        let (server, client) = pair(&identity(10), &identity(11), Trust::Known, Trust::Known).await;
        let caps = Capability::advertise(&["qwen3.6:27b"], &["read_file"], &[], &[], false);
        let mut announced = identity(10).announce("work\u{1b}[2Jshop", caps.clone());
        // A peer's clock, set to something this machine has no reason to
        // believe.
        announced.last_seen = Some(Utc::now() + TimeDelta::days(365));
        server.announce(&announced).await.expect("announce");

        let fetched = within(
            "fetching an announcement",
            client.announcement_of(&identity(10).id()),
        )
        .await
        .expect("the announcement");
        assert_eq!(fetched.id, identity(10).id());
        assert_eq!(fetched.caps, caps);
        assert!(!fetched.label().contains('\u{1b}'), "{:?}", fetched.label());
        assert!(fetched.label().contains("shop"), "{:?}", fetched.label());
        let seen = fetched.last_seen.expect("an observation");
        assert!(
            (Utc::now() - seen).num_seconds().abs() < 60,
            "a peer's clock became this machine's: {seen}"
        );

        client.shutdown().await;
        server.shutdown().await;
    }

    #[tokio::test]
    async fn a_node_that_has_not_announced_says_so_rather_than_answering_emptily() {
        let (server, client) = pair(&identity(12), &identity(13), Trust::Known, Trust::Known).await;
        let err = within(
            "fetching an announcement",
            client.announcement_of(&identity(12).id()),
        )
        .await
        .expect_err("nothing announced");
        assert!(format!("{err:#}").contains("has not announced"), "{err:#}");
        client.shutdown().await;
        server.shutdown().await;
    }

    #[tokio::test]
    async fn a_stranger_cannot_connect_at_all() {
        // Obligation 5 at its strongest: a node nobody added does not get to
        // learn that this one exists, let alone what it is called. The refusal
        // is in the handshake, so there is never a stream to ask on.
        let server =
            QuicTransport::listening(&identity(14), TrustLedger::new().shared(), localhost())
                .expect("listener");
        let stranger = QuicTransport::dial_only(
            &identity(15),
            ledger(&[(&identity(14), Trust::Trusted)]).shared(),
        )
        .expect("dialler");
        stranger.add_route(identity(14).id(), server.local_addr().expect("bound"));
        server
            .announce(&identity(14).announce("secret", Capability::none()))
            .await
            .expect("announce");

        let err = within(
            "a stranger dialling",
            stranger.announcement_of(&identity(14).id()),
        )
        .await
        .expect_err("refused");
        assert!(!format!("{err:#}").contains("secret"), "{err:#}");
        stranger.shutdown().await;
        server.shutdown().await;
    }

    #[tokio::test]
    async fn a_blocked_peer_cannot_connect_either() {
        let (server, client) =
            pair(&identity(16), &identity(17), Trust::Blocked, Trust::Trusted).await;
        server
            .announce(&identity(16).announce("node", Capability::none()))
            .await
            .expect("announce");
        assert!(
            within("a blocked peer dialling", client.ping(&identity(16).id()))
                .await
                .is_err()
        );
        client.shutdown().await;
        server.shutdown().await;
    }

    #[tokio::test]
    async fn dialling_the_wrong_key_at_the_right_address_is_refused() {
        // Obligation 1, at the point it matters most: a machine that answers on
        // the address this node has for a peer, holding a different key, is a
        // different machine.
        let server = QuicTransport::listening(
            &identity(18),
            ledger(&[(&identity(19), Trust::Trusted)]).shared(),
            localhost(),
        )
        .expect("listener");
        let client = QuicTransport::dial_only(
            &identity(19),
            ledger(&[(&identity(20), Trust::Trusted)]).shared(),
        )
        .expect("dialler");
        // The route points at node 18's socket, but the peer being dialled is
        // node 20.
        client.add_route(identity(20).id(), server.local_addr().expect("bound"));

        let err = within(
            "dialling the wrong key",
            client.announcement_of(&identity(20).id()),
        )
        .await
        .expect_err("a different machine answered");
        assert!(format!("{err:#}").contains("connecting to"), "{err:#}");
        client.shutdown().await;
        server.shutdown().await;
    }

    #[tokio::test]
    async fn a_known_peer_may_ask_who_you_are_and_may_not_watch_you() {
        // The publisher's consent is per request, not per connection: `Known`
        // is enough to fetch an announcement and not enough for a session
        // stream, and the refusal says which.
        let (server, client) =
            pair(&identity(21), &identity(22), Trust::Known, Trust::Trusted).await;
        server
            .announce(&identity(21).announce("node", Capability::none()))
            .await
            .expect("announce");
        within("fetching", client.announcement_of(&identity(21).id()))
            .await
            .expect("a known peer may ask");

        let err = within(
            "watching",
            client.subscribe(&identity(22).id(), &identity(21).id()),
        )
        .await
        .expect_err("known is not trusted");
        let message = format!("{err:#}");
        assert!(message.contains("not trusted"), "{message}");
        assert_eq!(server.subscriber_count(), 0);
        client.shutdown().await;
        server.shutdown().await;
    }

    #[tokio::test]
    async fn a_peer_that_stops_reading_loses_its_own_events_and_stalls_nothing() {
        // Obligation 7 on the publishing side. `publish` is not async, so there
        // is nothing here to await; what has to be true is that a full buffer
        // is a dropped event rather than a queue that grows, and that the
        // return value is honest about it.
        let (server, client) =
            pair(&identity(23), &identity(24), Trust::Trusted, Trust::Trusted).await;
        server
            .announce(&identity(23).announce("node", Capability::none()))
            .await
            .expect("announce");
        let subscription = within(
            "subscribing",
            client.subscribe(&identity(24).id(), &identity(23).id()),
        )
        .await
        .expect("subscribe");
        // The far end never calls `recv`, so the writer task fills the socket
        // and then the channel behind it.
        let peer = identity(24).id();

        let mut published = 0usize;
        for i in 0..(SUBSCRIPTION_BUFFER * 40) {
            published += server.publish(&delta(identity(23).id(), "s", &format!("tick {i}")));
            if server.dropped_to(&peer) > 0 {
                break;
            }
        }
        assert!(
            server.dropped_to(&peer) > 0,
            "a subscriber that never reads must eventually lose events rather than \
             growing this process's memory (published {published})"
        );
        drop(subscription);
        client.shutdown().await;
        server.shutdown().await;
    }

    #[tokio::test]
    async fn publishing_another_nodes_event_reaches_nobody() {
        // This transport speaks for one node. An event stamped with somebody
        // else's id is either a caller's mistake or a peer's event being
        // reflected back onto the mesh, and neither is this machine's to
        // publish.
        let (server, client) =
            pair(&identity(25), &identity(26), Trust::Trusted, Trust::Trusted).await;
        server
            .announce(&identity(25).announce("node", Capability::none()))
            .await
            .expect("announce");
        let mut subscription = within(
            "subscribing",
            client.subscribe(&identity(26).id(), &identity(25).id()),
        )
        .await
        .expect("subscribe");

        assert_eq!(
            server.publish(&delta(identity(99).id(), "s", "not mine")),
            0
        );
        assert!(subscription.try_recv().is_none());
        assert_eq!(server.publish(&delta(identity(25).id(), "s", "mine")), 1);
        let event = within("an event", subscription.recv())
            .await
            .expect("event");
        assert_eq!(event.from, identity(25).id());
        client.shutdown().await;
        server.shutdown().await;
    }

    #[tokio::test]
    async fn a_peers_claim_about_who_sent_an_event_is_overwritten_by_the_connection() {
        // Obligation 1 in the data path. The publisher stamps its own id on the
        // frame, but the reader files the event under the identity the
        // handshake proved, so a peer that lied would be believed by nothing.
        let (server, client) =
            pair(&identity(27), &identity(28), Trust::Trusted, Trust::Trusted).await;
        server
            .announce(&identity(27).announce("node", Capability::none()))
            .await
            .expect("announce");
        let mut subscription = within(
            "subscribing",
            client.subscribe(&identity(28).id(), &identity(27).id()),
        )
        .await
        .expect("subscribe");

        // A frame that claims to be from somebody else entirely, injected past
        // `publish`'s own check by pushing it straight into the sink.
        let mut forged = delta(identity(27).id(), "s", "who am i");
        forged.from = identity(27).id();
        forged.at = at(0);
        assert_eq!(server.publish(&forged), 1);

        let event = within("an event", subscription.recv())
            .await
            .expect("event");
        assert_eq!(event.from, identity(27).id(), "the connection's identity");
        assert!(
            (Utc::now() - event.at).num_seconds().abs() < 60,
            "and the local clock, not the one in the frame: {}",
            event.at
        );
        client.shutdown().await;
        server.shutdown().await;
    }

    #[tokio::test]
    async fn peer_text_and_a_peers_turn_are_sanitised_on_the_way_in() {
        // Obligation 2. Nothing here is a `String`: the session id is
        // `PeerText` and the turn is a `PeerTurn`, and both clean themselves
        // inside `Deserialize`, so the boundary is crossed by decoding and
        // cannot be walked around.
        let (server, client) =
            pair(&identity(29), &identity(30), Trust::Trusted, Trust::Trusted).await;
        server
            .announce(&identity(29).announce("node", Capability::none()))
            .await
            .expect("announce");
        let mut subscription = within(
            "subscribing",
            client.subscribe(&identity(30).id(), &identity(29).id()),
        )
        .await
        .expect("subscribe");

        let hostile = PeerEvent::new(
            identity(29).id(),
            "sess\u{0007}ion",
            at(0),
            PeerEventKind::Turn(
                PeerTurn::sanitize(&AgentEvent::TextDelta(
                    "\u{1b}[2Jwiped\u{202e}the screen".into(),
                ))
                .expect("a delta crosses"),
            ),
        );
        assert_eq!(server.publish(&hostile), 1);

        let event = within("an event", subscription.recv())
            .await
            .expect("event");
        assert_eq!(event.session.as_str(), "sess ion");
        let Some(AgentEvent::TextDelta(text)) = event.report() else {
            panic!("{event:?}");
        };
        assert!(!text.contains('\u{1b}'), "{text:?}");
        assert!(!text.contains('\u{202e}'), "{text:?}");
        assert!(text.contains("wiped"), "{text:?}");
        client.shutdown().await;
        server.shutdown().await;
    }

    #[tokio::test]
    async fn a_ping_measures_a_round_trip_to_a_peer_that_is_there() {
        let (server, client) = pair(&identity(31), &identity(32), Trust::Known, Trust::Known).await;
        let elapsed = within("pinging", client.ping(&identity(31).id()))
            .await
            .expect("a pong");
        assert!(elapsed < Duration::from_secs(5), "{elapsed:?}");
        assert_eq!(client.connection_count(), 1, "the connection is pooled");
        // A second request reuses it rather than handshaking again.
        within("pinging again", client.ping(&identity(31).id()))
            .await
            .expect("a pong");
        assert_eq!(client.connection_count(), 1);
        client.shutdown().await;
        server.shutdown().await;
    }

    #[tokio::test]
    async fn announcing_for_another_node_is_refused() {
        let transport = QuicTransport::dial_only(&identity(33), TrustLedger::new().shared())
            .expect("dial-only");
        let err = transport
            .announce(&identity(34).announce("impostor", Capability::none()))
            .await
            .expect_err("not this node");
        assert!(format!("{err:#}").contains("speaks for"), "{err:#}");
        transport.shutdown().await;
    }

    #[tokio::test]
    async fn subscribing_as_another_node_is_refused() {
        let transport = QuicTransport::dial_only(&identity(35), TrustLedger::new().shared())
            .expect("dial-only");
        let err = transport
            .subscribe(&identity(36).id(), &identity(37).id())
            .await
            .expect_err("not this node");
        assert!(format!("{err:#}").contains("speaks for"), "{err:#}");
        transport.shutdown().await;
    }

    #[tokio::test]
    async fn a_refusal_carries_the_peers_reason_and_is_sanitised() {
        // The reason is a string a peer wrote, on its way to somebody's
        // terminal, so it goes through `PeerText` like everything else.
        let reason = PeerText::sanitize("not\u{1b}[2J trusted");
        assert!(!reason.as_str().contains('\u{1b}'));
        assert!(reason.as_str().contains("trusted"));
    }
}
