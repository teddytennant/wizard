//! The acceptance bar the plan says is the one most likely to be faked:
//! **revoking from the inspector actually drops the connection.**
//!
//! It is a separate integration test rather than a unit test beside the screen
//! for one reason. A unit test can hold a `Mesh` and assert that
//! `Trust::Blocked` was written, and that assertion passes against an
//! implementation that never touches the transport at all — which is exactly
//! the shape of a faked revocation. What cannot be faked is a **live
//! subscription, open across the call**, whose receiver ends. So this file
//! opens one, publishes through it to prove it was carrying traffic, presses
//! the button the inspector presses, and then asserts three separate things:
//!
//! 1. the stream ends — `recv()` returns `None`, promptly, not at some timeout;
//! 2. the transport has no subscriber left, so a later publish reaches nobody;
//! 3. the graph the screen redraws from calls the peer
//!    [`Liveness::Unreachable`], and the paint that graph produces is hollow.
//!
//! The third is the half a mesh-only test would miss: a revocation that severed
//! the stream and left the canvas drawing a green dot would have satisfied the
//! transport and broken the promise this screen is built on.
//!
//! Gated on `--features native` like everything else under `src/native/`, and
//! on `--features graph`, which is the plugin the screen draws from: with
//! either left out this file compiles to nothing rather than to a broken
//! reference.

#![cfg(all(feature = "native", feature = "graph"))]

use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use tokio::sync::Mutex;

use wizard::mesh::{
    Capability, Identity, LoopbackTransport, Mesh, NodeId, PeerEvent, PeerEventKind, PeerStore,
    Subscription, Transport, Trust,
};
use wizard::plugins::native::graph::paint::node_paint;
use wizard::plugins::native::graph::revoke_and_rebuild;
use wizard::plugins::native::theme::Palette;
use wizard::plugins::graph::{Liveness, MeshGraph, NodeKey};

fn at(seconds: i64) -> DateTime<Utc> {
    DateTime::from_timestamp(1_800_000_000 + seconds, 0).expect("timestamp")
}

/// A mesh over a loopback transport, handing the transport back so the test can
/// announce peers onto it and publish their events.
fn mesh(seed: u8) -> (Arc<Mutex<Mesh>>, Arc<LoopbackTransport>) {
    let transport = Arc::new(LoopbackTransport::new());
    let mut mesh = Mesh::new(
        Identity::from_seed([seed; 32]),
        PeerStore::ephemeral(),
        Arc::clone(&transport) as Arc<dyn Transport>,
    );
    mesh.set_local("here", Capability::none());
    (Arc::new(Mutex::new(mesh)), transport)
}

/// Announce an identity onto the transport and return its pasteable address.
async fn announce(transport: &LoopbackTransport, seed: u8) -> String {
    let identity = Identity::from_seed([seed; 32]);
    transport
        .announce(&identity.announce("workshop", Capability::none()))
        .await
        .expect("announce");
    identity.id().address()
}

/// [`Subscription::recv`] with a deadline.
///
/// The assertion here is that a stream has *ended*. Awaiting one that has not
/// parks the test forever, and a hung test reads in CI as an infrastructure
/// problem rather than as the revocation bug it would actually be.
async fn recv_within(subscription: &mut Subscription, what: &str) -> Option<PeerEvent> {
    tokio::time::timeout(Duration::from_secs(5), subscription.recv())
        .await
        .unwrap_or_else(|_| panic!("{what}: the subscription was still open after 5s"))
}

/// The graph as the screen would build it after the call.
fn peer_in(graph: &MeshGraph, id: NodeId) -> &wizard::plugins::graph::GraphNode {
    graph.node(&NodeKey::Node(id)).expect("the peer is drawn")
}

/// **The acceptance test.** A trusted peer with a live subscription, revoked
/// through the same function the inspector's button calls.
#[tokio::test(flavor = "multi_thread")]
async fn revoking_from_the_inspector_ends_a_live_subscription_and_redraws_unreachable() {
    let (mesh, transport) = mesh(1);
    let address = announce(&transport, 2).await;
    let id = {
        let mut guard = mesh.lock().await;
        let (id, _) = guard.add_peer(&address, at(0)).expect("add the peer");
        guard
            .set_trust(&id, Trust::Trusted)
            .await
            .expect("trust it");
        // Heard from just now, so nothing about staleness can be the reason it
        // stops drawing live.
        guard.refresh(&id, at(0)).await.expect("refresh");
        id
    };

    // A real stream, carrying real traffic. Without this the test would be
    // asserting that a stream nobody opened is closed.
    let mut subscription = {
        let mut guard = mesh.lock().await;
        guard.subscribe(&id).await.expect("subscribe")
    };
    assert_eq!(transport.subscriber_count(&id), 1);
    let event = PeerEvent::new(id, "s-1", at(1), PeerEventKind::SessionStarted);
    assert_eq!(transport.publish(&event), 1, "the stream is carrying");
    assert!(
        recv_within(&mut subscription, "before the revocation")
            .await
            .is_some(),
        "the stream delivered nothing before it was revoked"
    );

    // The screen's own state before the press: live, and painted as live. If
    // this were not true the assertions after the revocation would prove
    // nothing at all.
    let palette = Palette::from_theme(&wizard::theme::minimal());
    let before = {
        let guard = mesh.lock().await;
        MeshGraph::build(&guard.local_node(), guard.store(), at(2))
    };
    let node = peer_in(&before, id);
    assert_eq!(node.trust, Trust::Trusted);
    assert_eq!(node.liveness, Liveness::Live);
    assert!(node.liveness.is_live());
    let paint = node_paint(node, &palette);
    assert!(paint.solid, "a live peer is drawn solid");
    assert!(!paint.barred);

    // The button.
    let after = revoke_and_rebuild(Arc::clone(&mesh), id, at(3))
        .await
        .expect("the revocation succeeded");

    // 1. The stream ended, and it ended now rather than at some later timeout.
    assert!(
        recv_within(&mut subscription, "after the revocation")
            .await
            .is_none(),
        "the subscription outlived the revocation"
    );
    assert!(subscription.is_closed());

    // 2. Nothing is subscribed any more, so a publish reaches nobody.
    assert_eq!(transport.subscriber_count(&id), 0);
    assert_eq!(
        transport.publish(&PeerEvent::new(
            id,
            "s-1",
            at(4),
            PeerEventKind::SessionEnded
        )),
        0,
        "an event still reached a revoked peer's stream"
    );

    // 3. And the graph the canvas redraws from says so, in both channels.
    let node = peer_in(&after, id);
    assert_eq!(node.trust, Trust::Blocked);
    assert_eq!(node.liveness, Liveness::Unreachable);
    assert!(!node.liveness.is_live());
    // It was heard from a moment ago and the model still refuses to call it
    // live: presence alone would have said Online.
    assert_eq!(node.seen_label(), "3s");
    let paint = node_paint(node, &palette);
    assert!(!paint.solid, "the revoked peer is redrawn hollow");
    assert!(paint.halo.is_none(), "and loses the halo only up nodes get");
    assert_eq!(paint.interior, palette.canvas);
    assert!(paint.barred, "and is struck through");
    assert!(
        !after
            .inspect(&NodeKey::Node(id))
            .expect("inspection")
            .revocable
    );

    // The decision stuck, in the store the next snapshot reads and against the
    // mesh's own gates.
    let mut guard = mesh.lock().await;
    assert_eq!(guard.store().trust_of(&id), Some(Trust::Blocked));
    assert!(guard.subscribe(&id).await.is_err(), "no new stream");
    assert!(guard.refresh(&id, at(5)).await.is_err(), "no contact");
}

/// The same call against a peer that was merely downgraded rather than blocked
/// is not this function's job — but the button must not be *offered* for a peer
/// with no trust to take away, and the model is what decides that.
///
/// Here to keep the two halves of the claim in one file: the previous test
/// proves the button works, and this one proves it is only drawn where working
/// means something.
#[tokio::test]
async fn a_peer_with_no_trust_to_take_away_is_not_revocable() {
    let (mesh, transport) = mesh(3);
    let address = announce(&transport, 4).await;
    let id = {
        let mut guard = mesh.lock().await;
        guard.add_peer(&address, at(0)).expect("add the peer").0
    };

    let graph = {
        let guard = mesh.lock().await;
        MeshGraph::build(&guard.local_node(), guard.store(), at(0))
    };
    // A pasted address lands at Known, which may not be sent work, so there is
    // nothing to revoke.
    assert_eq!(peer_in(&graph, id).trust, Trust::Known);
    assert!(
        !graph
            .inspect(&NodeKey::Node(id))
            .expect("inspection")
            .revocable
    );
    // …and the local node never is, whatever else is true of it.
    let local = graph.nodes()[0].key.clone();
    assert!(!graph.inspect(&local).expect("inspection").revocable);
}
