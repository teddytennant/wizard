//! mDNS on the LAN: the second way a node learns where a peer is, and the
//! second way only.
//!
//! # What this does
//!
//! Two things, both narrow:
//!
//! 1. **Advertises** this node on the local link as `_wizard-mesh._udp.local.`,
//!    with its address in a TXT record and the port its listener is bound to.
//! 2. **Browses** for the same service and, for a node this machine has
//!    *already added as a peer*, records where it answered
//!    ([`super::quic::QuicTransport::add_route`]).
//!
//! # What this does not do, and why each one is a refusal rather than a gap
//!
//! - **It does not add peers.** A node found on the LAN does not enter the peer
//!   store, does not become [`Trust::Known`](super::Trust::Known), and is not
//!   contactable. Discovery is a paste and a human decision
//!   ([`super::peer`]); a coffee-shop network that could write into somebody's
//!   peer store would make that sentence false. What browsing produces for a
//!   node nobody added is one entry in [`Discovery::found`], which is a list a
//!   surface may *show* — "these machines nearby are running Wizard" — and
//!   which grants nothing.
//! - **It does not grant trust.** Trust is three-state, human, and on disk. An
//!   mDNS packet is none of those things.
//! - **It does not authenticate anything.** mDNS is unauthenticated by
//!   construction: any machine on the link can claim any TXT record. That is
//!   fine here precisely because a route carries no authority — a wrong or
//!   forged route sends the first QUIC packet to the wrong place, the handshake
//!   refuses the identity that answers ([`super::tls::PinnedPeer`]), and the
//!   dial fails. A hostile LAN can stop two nodes finding each other; it cannot
//!   make one talk to the wrong node.
//! - **It does not reach past the local link.** No gossip, no DHT, no
//!   rendezvous server. The design this is modelled on has a gossip protocol
//!   for exactly that and it is out of scope: it would mean peer records
//!   propagating between machines, and the whole trust model here rests on
//!   records arriving one at a time in front of a person.
//!
//! # It is off by default
//!
//! `[mesh] mdns` defaults to `false`, like `[mesh] listen`. Advertising is
//! broadcasting this machine's name and public key to every device on the
//! network, which is a disclosure some people will want and nobody should get
//! without asking. See [`crate::config::MeshConfig`].

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use anyhow::{Context, Result};
use mdns_sd::{ServiceDaemon, ServiceEvent, ServiceInfo};
use tokio::task::JoinHandle;

use super::node::NodeId;
use super::quic::QuicTransport;
use super::wire::WIRE_VERSION;

/// The DNS-SD service type. `_udp` because QUIC is UDP, and the version is
/// deliberately *not* in it: a node running a different wire version should be
/// visible and then refused with a message, not invisible.
pub const SERVICE_TYPE: &str = "_wizard-mesh._udp.local.";

/// TXT key carrying the node's mesh address. The whole identity, not a prefix:
/// [`NodeId::short`] is documented as a prefix and prefixes collide.
pub const KEY_ADDRESS: &str = "addr";

/// TXT key carrying the wire version the node speaks.
pub const KEY_VERSION: &str = "wire";

/// Nodes [`Discovery::found`] will remember at once.
///
/// The list is fed by anything on the local link, so it needs a bound like
/// everything else a peer can grow. Past it the oldest-inserted entry is
/// dropped, because a list that stopped updating would go stale silently while
/// a list that forgets is at least honest about being a window.
pub const MAX_FOUND: usize = 64;

/// One node seen on the local link.
///
/// **Not a peer.** It is an observation: a machine on this network answered to
/// the mesh service type and claimed this address. Nothing has been verified,
/// because nothing about mDNS can be.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Found {
    /// The address the node claimed. Parsed, so it is at least a real ed25519
    /// public key rather than arbitrary text, and therefore safe to render.
    pub id: NodeId,
    /// Where it answered.
    pub at: SocketAddr,
    /// The wire version it claimed, when it claimed one.
    pub wire: Option<u8>,
    /// Whether this machine already has a peer record for it. The one fact a
    /// surface needs to tell "a stranger on the wifi" from "the laptop I added
    /// last week has moved".
    pub is_peer: bool,
}

#[derive(Default)]
struct Inner {
    /// Insertion order, for the [`MAX_FOUND`] eviction.
    order: Vec<NodeId>,
    found: BTreeMap<NodeId, Found>,
}

/// A live mDNS presence: what this node advertises, and what it has seen.
pub struct Discovery {
    daemon: ServiceDaemon,
    /// The registered service's full name, when advertising. `None` when this
    /// node browses without announcing itself, which is what a node with no
    /// listener does: it has no port to advertise.
    registered: Option<String>,
    browsing: Mutex<Option<JoinHandle<()>>>,
    state: Arc<Mutex<Inner>>,
}

impl std::fmt::Debug for Discovery {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Discovery")
            .field("advertising", &self.registered.is_some())
            .field("found", &self.found().len())
            .finish()
    }
}

impl Discovery {
    /// Start mDNS for `transport`.
    ///
    /// Advertises when the transport is listening, because a node with no
    /// listener has no port worth publishing; browses either way, because a
    /// dial-only node still needs to know where its peers are.
    ///
    /// Routes learned here are installed on `transport`, which is the only
    /// effect this has on anything. Everything else it discovers lands in
    /// [`Discovery::found`], where it is a list to look at.
    pub fn start(transport: &Arc<QuicTransport>) -> Result<Arc<Self>> {
        let daemon = ServiceDaemon::new().context("starting the mDNS daemon for the mesh")?;
        let registered = if transport.is_listening() {
            let addr = transport.local_addr()?;
            let service = advertisement(transport.local_id(), addr.port())?;
            let fullname = service.get_fullname().to_string();
            daemon
                .register(service)
                .context("advertising this node on the local network")?;
            Some(fullname)
        } else {
            None
        };

        let discovery = Arc::new(Self {
            daemon,
            registered,
            browsing: Mutex::new(None),
            state: Arc::new(Mutex::new(Inner::default())),
        });

        let events = discovery
            .daemon
            .browse(SERVICE_TYPE)
            .context("browsing the local network for mesh nodes")?;
        let state = Arc::clone(&discovery.state);
        let transport = Arc::clone(transport);
        let local = transport.local_id();
        let browsing = tokio::spawn(async move {
            while let Ok(event) = events.recv_async().await {
                let ServiceEvent::ServiceResolved(service) = event else {
                    continue;
                };
                let Some((id, at, wire)) = read_advertisement(&service) else {
                    continue;
                };
                if id == local {
                    // This node's own advertisement, echoed back. Recording a
                    // route to itself would let `connect` dial the local
                    // socket, and a node cannot be its own peer.
                    continue;
                }
                absorb(&state, &transport, id, at, wire);
            }
        });
        *discovery
            .browsing
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(browsing);
        Ok(discovery)
    }

    /// Every node seen on the local link since this started, newest state per
    /// node, ordered by id.
    ///
    /// A list to render, never a list to act on. See the module docs: an entry
    /// here is a claim an unauthenticated packet made.
    pub fn found(&self) -> Vec<Found> {
        self.lock().found.values().cloned().collect()
    }

    /// Whether this node is advertising itself, as opposed to only listening
    /// for others.
    pub fn is_advertising(&self) -> bool {
        self.registered.is_some()
    }

    /// Stop browsing, withdraw this node's advertisement, and shut the daemon
    /// down. Idempotent.
    pub fn stop(&self) {
        if let Some(browsing) = self
            .browsing
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
        {
            browsing.abort();
        }
        if let Some(fullname) = &self.registered {
            // Withdrawing tells the link this node is gone rather than leaving
            // a stale record for its TTL, which is the difference between a
            // peer that reconnects and one that dials a port nobody holds.
            let _ = self.daemon.unregister(fullname);
        }
        let _ = self.daemon.shutdown();
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// The service record this node publishes.
fn advertisement(id: NodeId, port: u16) -> Result<ServiceInfo> {
    let address = id.address();
    // The instance name has to be unique on the link, and an address is the one
    // thing about a node that provably is. A human-chosen name would collide
    // and, worse, would be a second identity for the node to be known by.
    let host = format!("{}.local.", id.short());
    let properties = [
        (KEY_ADDRESS, address.as_str()),
        (KEY_VERSION, &WIRE_VERSION.to_string()),
    ];
    let service = ServiceInfo::new(SERVICE_TYPE, &address, &host, "", port, &properties[..])
        .context("building this node's mDNS advertisement")?
        // Let the daemon fill in this machine's addresses and keep them
        // current, rather than freezing whatever the interfaces looked like at
        // startup: a laptop changes networks.
        .enable_addr_auto();
    Ok(service)
}

/// The identity, address and claimed wire version in a resolved service, or
/// `None` when it is not a mesh advertisement this node can use.
///
/// Every field is validated rather than trusted. The address must parse as a
/// real ed25519 public key ([`NodeId::parse_address`]), which is what stops an
/// arbitrary string from a hostile link ever reaching a renderer.
fn read_advertisement(
    service: &mdns_sd::ResolvedService,
) -> Option<(NodeId, SocketAddr, Option<u8>)> {
    let claimed = service.txt_properties.get_property_val_str(KEY_ADDRESS)?;
    let id = NodeId::parse_address(claimed).ok()?;
    let wire = service
        .txt_properties
        .get_property_val_str(KEY_VERSION)
        .and_then(|raw| raw.parse::<u8>().ok());
    // The first address, preferring IPv4: a link-local IPv6 address needs a
    // scope id to dial and `SocketAddr` does not carry one usefully here.
    let ip = service
        .addresses
        .iter()
        .find(|scoped| scoped.is_ipv4())
        .or_else(|| service.addresses.iter().next())?
        .to_ip_addr();
    Some((id, SocketAddr::new(ip, service.port), wire))
}

/// Record one sighting, and install a route for it only if it is a peer.
///
/// Split out from the browse loop so the rule that matters — **a route is
/// installed for a peer and for nobody else** — is a function with a test
/// rather than a branch inside a task nothing can drive.
fn absorb(
    state: &Mutex<Inner>,
    transport: &QuicTransport,
    id: NodeId,
    at: SocketAddr,
    wire: Option<u8>,
) {
    let is_peer = transport.consent().decision(&id).is_some();
    if is_peer {
        transport.add_route(id, at);
    }
    let mut inner = state.lock().unwrap_or_else(PoisonError::into_inner);
    if inner
        .found
        .insert(
            id,
            Found {
                id,
                at,
                wire,
                is_peer,
            },
        )
        .is_none()
    {
        inner.order.push(id);
        while inner.order.len() > MAX_FOUND {
            let oldest = inner.order.remove(0);
            inner.found.remove(&oldest);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugins::mesh::consent::TrustLedger;
    use crate::plugins::mesh::node::{Identity, Node};
    use crate::plugins::mesh::peer::{Peer, Trust};
    use chrono::{DateTime, Utc};

    fn identity(byte: u8) -> Identity {
        Identity::from_seed([byte; 32])
    }

    fn at(seconds: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(1_800_000_000 + seconds, 0).expect("timestamp")
    }

    fn socket(port: u16) -> SocketAddr {
        SocketAddr::new("192.0.2.10".parse().expect("a literal address"), port)
    }

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

    #[tokio::test]
    async fn a_sighting_routes_a_peer_and_only_records_a_stranger() {
        // The rule this module exists to keep: mDNS tells a node where its
        // peers are. It does not tell it who its peers are.
        let peer = identity(2);
        let stranger = identity(3);
        let transport =
            QuicTransport::dial_only(&identity(1), ledger(&[(&peer, Trust::Known)]).shared())
                .expect("dial-only");
        let state = Mutex::new(Inner::default());

        absorb(&state, &transport, peer.id(), socket(4242), Some(1));
        absorb(&state, &transport, stranger.id(), socket(4243), Some(1));

        assert_eq!(transport.route(&peer.id()), Some(socket(4242)));
        assert_eq!(
            transport.route(&stranger.id()),
            None,
            "a node nobody added must not become routable by shouting on a network"
        );

        let found = { state.lock().expect("lock").found.clone() };
        assert!(found[&peer.id()].is_peer);
        assert!(!found[&stranger.id()].is_peer);
        assert_eq!(found[&stranger.id()].at, socket(4243));
        assert_eq!(found[&stranger.id()].wire, Some(1));
        transport.shutdown().await;
    }

    #[tokio::test]
    async fn a_peer_that_moved_gets_its_new_route() {
        let peer = identity(4);
        let transport =
            QuicTransport::dial_only(&identity(5), ledger(&[(&peer, Trust::Trusted)]).shared())
                .expect("dial-only");
        let state = Mutex::new(Inner::default());
        absorb(&state, &transport, peer.id(), socket(1), None);
        absorb(&state, &transport, peer.id(), socket(2), None);
        assert_eq!(transport.route(&peer.id()), Some(socket(2)));
        let found = { state.lock().expect("lock").found.len() };
        assert_eq!(found, 1, "one node, one entry");
        transport.shutdown().await;
    }

    #[tokio::test]
    async fn the_found_list_is_bounded_because_a_link_can_be_hostile() {
        let transport =
            QuicTransport::dial_only(&identity(6), TrustLedger::new().shared()).expect("dial-only");
        let state = Mutex::new(Inner::default());
        for byte in 0..=255u8 {
            // Not every byte is a valid seed collision-free, but every one
            // gives a distinct key, which is all this needs.
            absorb(
                &state,
                &transport,
                Identity::from_seed([byte; 32]).id(),
                socket(u16::from(byte) + 1000),
                None,
            );
        }
        let (found, order) = {
            let inner = state.lock().expect("lock");
            (inner.found.len(), inner.order.len())
        };
        assert_eq!(found, MAX_FOUND);
        assert_eq!(order, MAX_FOUND);
        transport.shutdown().await;
    }

    #[test]
    fn the_advertisement_carries_the_whole_address_and_the_wire_version() {
        let service = advertisement(identity(7).id(), 4242).expect("advertisement");
        assert_eq!(
            service.get_property_val_str(KEY_ADDRESS),
            Some(identity(7).id().address().as_str()),
            "the whole address: a short form is a prefix, and prefixes collide"
        );
        assert_eq!(
            service.get_property_val_str(KEY_VERSION),
            Some(WIRE_VERSION.to_string().as_str())
        );
        assert_eq!(service.get_port(), 4242);
        assert!(
            service.get_fullname().ends_with(SERVICE_TYPE),
            "{}",
            service.get_fullname()
        );
    }

    #[tokio::test]
    async fn a_dial_only_node_browses_without_advertising() {
        // Nothing to advertise: a node with no listener has no port a peer
        // could reach it on, and publishing one would be an invitation to a
        // connection that cannot be accepted.
        let transport =
            QuicTransport::dial_only(&identity(8), TrustLedger::new().shared()).expect("dial-only");
        let Ok(discovery) = Discovery::start(&transport) else {
            // No multicast in this sandbox. Skipping beats asserting something
            // the environment cannot produce; the pure rules above are what
            // this module's behaviour actually rests on.
            transport.shutdown().await;
            return;
        };
        assert!(!discovery.is_advertising());
        assert!(discovery.found().is_empty());
        assert!(format!("{discovery:?}").contains("advertising: false"));
        discovery.stop();
        discovery.stop();
        transport.shutdown().await;
    }
}
