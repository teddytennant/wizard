//! The tee: a live session's events on their way to the peers watching this
//! node.
//!
//! [`crate::mesh::Mesh::publish_turn`] has been built and tested since tier 2
//! landed and nothing called it, which is the difference between a mesh that
//! works and a mesh that is reachable. This is the call site: one hook off
//! [`App::handle_agent_event`](super::App::handle_agent_event), where the same
//! [`AgentEvent`] the TUI is about to render goes to whoever is watching.
//!
//! # Why the tee exists only when `[mesh] listen` is on
//!
//! A peer watches this node by *dialling* it, and a connection this node opened
//! is never used to serve requests back the other way (see
//! [`crate::mesh::quic`]). So with `listen = false` there is no way for anybody
//! to subscribe, and a tee would be a UDP socket bound, an identity file
//! minted and an announcement assembled on every launch, in service of a
//! stream nobody can open.
//!
//! [`MeshTee::join`] therefore answers `None` for the default install, and the
//! surface holds an `Option`. That is the whole of the default-off posture on
//! this side: a mesh that opened a socket because the TUI started would be the
//! security surface `[mesh] listen` exists to keep shut.
//!
//! # The trap this module exists to not fall into
//!
//! [`Mesh::new`] and [`Mesh::with_consent`] compile identically and behave
//! identically over the loopback. Over a socket they do not: `Mesh::new` builds
//! a private [`TrustLedger`] that nothing else holds, so the transport's copy
//! stays empty, and an empty ledger refuses everybody. The failure is silent
//! and it looks exactly like having no peers — the handshake is refused, the
//! subscriber count stays zero, and every publish returns 0.
//!
//! So [`MeshTee::assemble`] builds **one** ledger and hands it to both halves,
//! and [`the_transport_and_the_mesh_share_one_consent_ledger`](tests) is a test
//! over two real sockets that fails if it ever stops doing so.
//!
//! # A decision made elsewhere does not reach this tee
//!
//! [`Mesh::set_trust`] revokes immediately for the `Mesh` that records it, and
//! across the socket for the machine it is about. It does not reach *another
//! process on this machine*: a `wizard peers trust <peer> known` typed in a
//! second terminal writes the store and refreshes that process's ledger, and
//! this one goes on holding the copy it loaded at [`MeshTee::join`].
//!
//! Named rather than papered over. The decision is on disk before that command
//! returns and binds every process started afterwards; what is missing is a
//! reason for a live session to re-read `peers.json`, and adding one is a
//! design question (how often, and what a mid-stream reload does to a
//! subscription) rather than a line of code. Until then the way to stop a
//! running session reaching a peer is to end the session, or to revoke from the
//! peer's own machine — which works, because it closes the connection.
//!
//! # What crosses
//!
//! Not this module's decision, deliberately. Whether a variant may cross at all
//! is [`AgentEvent::is_request`], an exhaustive match next to the variants;
//! what a crossing event looks like is
//! [`PeerTurn::sanitize`](crate::mesh::PeerTurn::sanitize). A second policy
//! here — a list of variants worth forwarding, a size check, a "skip the noisy
//! ones" filter — would be a second thing to keep in step with the enum, and
//! the one that was already tried (a negative match on the serde tag) is why
//! `is_request` exists.

use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::{Context, Result};
use chrono::Utc;

use crate::agent::AgentEvent;
use crate::config::{Config, MeshConfig};
use crate::mesh::consent::TrustLedger;
use crate::mesh::discovery::Discovery;
use crate::mesh::quic::QuicTransport;
use crate::mesh::transport::PeerEventKind;
use crate::mesh::{Capability, Identity, Mesh, NodeId, PeerStore, Transport};

/// This node's place on the mesh for the life of one session.
///
/// Holds the [`Mesh`] rather than an `Arc` of one because the surface owns it
/// outright: [`Mesh::publish_turn`] takes `&self` precisely so a live turn can
/// publish several times a second without queueing behind an operator's trust
/// decisions, and nothing else in the TUI touches the mesh at all.
pub struct MeshTee {
    mesh: Mesh,
    /// Kept alongside the `Mesh` for the two questions the trait cannot answer:
    /// where this node is bound, and how many peers are actually watching.
    transport: Arc<QuicTransport>,
    /// mDNS, when `[mesh] mdns` asked for it. Held so it lives as long as the
    /// session does: dropping it would withdraw the advertisement and stop the
    /// browse that fills in peers' routes.
    discovery: Option<Arc<Discovery>>,
    /// The session id every event is stamped with, so one subscription to this
    /// node can carry several sessions and a watcher can demux them.
    session: String,
}

/// Says how many peers are watching and where this node is bound, and names
/// neither them nor this machine's peer list. A `{:?}` of the struct that holds
/// one lands in a log line, and who somebody meshes with is not log material.
impl std::fmt::Debug for MeshTee {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MeshTee")
            .field("node", &self.mesh.local_id().short())
            .field("at", &self.transport.local_addr().ok())
            .field("watchers", &self.transport.subscriber_count())
            .finish()
    }
}

impl MeshTee {
    /// Join the mesh for this session, or answer `None` because `[mesh] listen`
    /// is off and nobody could watch anyway.
    ///
    /// Everything with a cost — minting `~/.wizard/node.key`, reading the peer
    /// store, binding a socket, advertising on the LAN — happens after that
    /// check and not before, so the default install pays none of it.
    pub async fn join(config: &Config, session: &str) -> Result<Option<Self>> {
        if !config.mesh.listen {
            return Ok(None);
        }
        let wizard_dir = Config::wizard_dir()?;
        let identity = Identity::load_or_generate(&wizard_dir)?;
        let store = PeerStore::load(&wizard_dir)?;

        // What this node says it is, and what it says it can do. The model is
        // worth advertising because it is what makes the graph's capability
        // vertices mean anything; `accepts_work` is false because delegated
        // work is not built, so a node that offered itself as compute would be
        // offering something nothing would run.
        let model = config.active().model;
        let caps = Capability::advertise(&[model.as_str()], &[], &[], &[], false);
        Self::assemble(
            identity,
            store,
            &config.mesh,
            &crate::sync::host_name(),
            caps,
            session,
        )
        .await
        .map(Some)
    }

    /// [`MeshTee::join`] with the identity, the store and the advertisement
    /// already decided.
    ///
    /// Split out so a test can build the real thing — a real listener on a real
    /// socket, the real ledger wiring — without a `~/.wizard` to read. The
    /// wiring is the point of the split: this function is what the shared-ledger
    /// test breaks to prove itself.
    async fn assemble(
        identity: Identity,
        store: PeerStore,
        config: &MeshConfig,
        name: &str,
        caps: Capability,
        session: &str,
    ) -> Result<Self> {
        // One ledger, two holders. The transport reads it to decide whether a
        // machine that just completed a handshake may be served; the `Mesh`
        // rewrites it from the peer store inside every call that records a
        // decision. Give the transport its own and it answers `None` to
        // everything, which is deny-by-default applied to peers this operator
        // trusted — silently, and indistinguishably from having no peers.
        let consent = TrustLedger::new();
        let transport = QuicTransport::from_config(&identity, consent.shared(), config)?;
        let mut mesh = Mesh::with_consent(
            identity,
            store,
            Arc::clone(&transport) as Arc<dyn Transport>,
            consent,
        );
        mesh.set_local(name, caps);
        mesh.announce()
            .await
            .context("recording what this node answers a peer's who-are-you with")?;

        let discovery = if config.mdns {
            // A failure here is not a failure to join: mDNS fills in routes,
            // and a node with `[mesh] routes` written down needs none of it.
            match Discovery::start(&transport) {
                Ok(discovery) => Some(discovery),
                Err(why) => {
                    tracing::warn!("mesh: mDNS did not start: {why:#}");
                    None
                }
            }
        } else {
            None
        };

        let tee = Self {
            mesh,
            transport,
            discovery,
            session: session.to_string(),
        };
        tee.mesh
            .publish(&tee.session, Utc::now(), PeerEventKind::SessionStarted);
        Ok(tee)
    }

    /// Hand one of this session's events to whoever is watching. How many took
    /// it, which is `0` when nobody is and also `0` when the event does not
    /// cross the mesh at all.
    ///
    /// The caller has nothing useful to do about the difference, which is why
    /// the two answers are the same one: a surface that had to handle "this one
    /// is a request, not a report" at the call site would eventually stop
    /// handling it, and the decision belongs to `AgentEvent::is_request` where
    /// it is exhaustive.
    pub fn publish(&self, event: &AgentEvent) -> usize {
        self.mesh.publish_turn(&self.session, Utc::now(), event)
    }

    /// This node's address, the text another machine pastes into
    /// `wizard peers add`.
    pub fn address(&self) -> String {
        self.mesh.local_id().address()
    }

    /// This node's id.
    pub fn local_id(&self) -> NodeId {
        self.mesh.local_id()
    }

    /// Where the listener is bound, with any `0` port resolved to what the OS
    /// chose.
    pub fn listening_at(&self) -> Result<SocketAddr> {
        self.transport.local_addr()
    }

    /// How many peers are watching this node right now.
    pub fn watchers(&self) -> usize {
        self.transport.subscriber_count()
    }

    /// Say the session ended, stop advertising, and close the socket.
    ///
    /// Consuming rather than `Drop`, because closing a QUIC endpoint politely
    /// means telling the far end, and that is an async conversation a
    /// destructor cannot have. A watcher whose peer exits without this sees the
    /// stream end at the connection's idle timeout instead of now, which is a
    /// worse answer rather than a wrong one.
    pub async fn leave(self) {
        self.mesh
            .publish(&self.session, Utc::now(), PeerEventKind::SessionEnded);
        if let Some(discovery) = &self.discovery {
            discovery.stop();
        }
        self.transport.shutdown().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::ConsoleGate;
    use crate::mesh::{PeerEvent, Subscription, Trust};
    use std::time::Duration;

    fn identity(byte: u8) -> Identity {
        Identity::from_seed([byte; 32])
    }

    /// A `[mesh]` section that listens on a loopback port the OS picks.
    fn listening_config() -> MeshConfig {
        MeshConfig {
            listen: true,
            listen_addr: "127.0.0.1:0".to_string(),
            mdns: false,
            routes: Default::default(),
        }
    }

    /// Await something, or fail rather than hang. Several assertions here are
    /// that a stream *ended*; awaiting one that has not parks the test forever,
    /// and a hung test reads in CI as infrastructure rather than as the bug.
    async fn within<T>(what: &str, future: impl std::future::Future<Output = T>) -> T {
        tokio::time::timeout(Duration::from_secs(20), future)
            .await
            .unwrap_or_else(|_| panic!("{what}: still waiting after 20s"))
    }

    /// A watcher: its own transport, its own store, its own opinion, dialling
    /// the tee it is pointed at.
    async fn watcher(seed: u8, publisher: &MeshTee) -> (Arc<QuicTransport>, Mesh) {
        let identity = identity(seed);
        let consent = TrustLedger::new();
        let transport =
            QuicTransport::dial_only(&identity, consent.shared()).expect("a dialling transport");
        transport.add_route(
            publisher.local_id(),
            publisher.listening_at().expect("the tee is bound"),
        );
        let mut mesh = Mesh::with_consent(
            identity,
            PeerStore::ephemeral(),
            Arc::clone(&transport) as Arc<dyn Transport>,
            consent,
        );
        mesh.add_peer(&publisher.address(), Utc::now())
            .expect("paste the publisher's address");
        mesh.set_trust(&publisher.local_id(), Trust::Trusted)
            .await
            .expect("the watcher decides to take this node's stream");
        (transport, mesh)
    }

    /// The text of a `TextDelta`, or a panic naming what arrived instead.
    fn delta_text(event: &PeerEvent) -> String {
        match event.report() {
            Some(AgentEvent::TextDelta(text)) => text.clone(),
            other => panic!("expected a text delta, got {other:?}"),
        }
    }

    async fn next(subscription: &mut Subscription, what: &str) -> Option<PeerEvent> {
        within(what, subscription.recv()).await
    }

    #[tokio::test]
    async fn the_default_install_joins_nothing_and_binds_nothing() {
        // `[mesh] listen` is false by default, and with it false a peer has no
        // way to subscribe, so there is nothing for a tee to feed. Answering
        // `None` before any of the cost — the key file, the store, the socket —
        // is what makes the default posture free rather than merely quiet.
        let config = Config::default();
        assert!(!config.mesh.listen, "the shipped default");
        let tee = MeshTee::join(&config, "session-1")
            .await
            .expect("no listener is not an error");
        assert!(tee.is_none());
    }

    /// The trap named in `src/mesh/consent.rs` and in this module's header, over
    /// two real sockets.
    ///
    /// `Mesh::new` and `Mesh::with_consent` compile the same and, over the
    /// loopback, behave the same. Over QUIC the first one leaves the transport
    /// holding an empty ledger, and an empty ledger refuses everybody: the
    /// handshake is refused, the subscriber count stays zero, and every publish
    /// returns 0 — a mesh that silently serves nobody, which looks exactly like
    /// a mesh with no peers.
    ///
    /// Both halves are asserted. The first is that `MeshTee::assemble` serves a
    /// peer it trusts, which fails the moment the ledger stops being shared.
    /// The second builds the wrong wiring on purpose and shows what it costs,
    /// so the first assertion cannot be read as "networking works".
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn the_transport_and_the_mesh_share_one_consent_ledger() {
        // --- The wiring under test ------------------------------------------
        let mut tee = MeshTee::assemble(
            identity(201),
            PeerStore::ephemeral(),
            &listening_config(),
            "workshop",
            Capability::none(),
            "session-7",
        )
        .await
        .expect("the tee joins");

        let (watcher_transport, mut watcher_mesh) = watcher(202, &tee).await;
        let watcher_id = identity(202).id();

        // The publisher's own half of the decision: this operator says that
        // machine may watch this one. It goes through `Mesh`, which is what
        // refreshes the ledger the transport reads.
        tee.mesh
            .add_peer(&watcher_id.address(), Utc::now())
            .expect("paste the watcher's address");
        tee.mesh
            .set_trust(&watcher_id, Trust::Trusted)
            .await
            .expect("the publisher consents to being watched");

        let mut watching = within(
            "subscribing to a tee whose ledger is shared",
            watcher_mesh.subscribe(&tee.local_id()),
        )
        .await
        .expect(
            "a trusted peer must be able to watch: an empty ledger on the transport would \
             refuse it here, and would look exactly like having no peers",
        );

        assert_eq!(
            tee.publish(&AgentEvent::TextDelta("reading src/app/tee.rs".into())),
            1,
            "the tee's own count of who took it"
        );
        assert_eq!(
            delta_text(
                &next(&mut watching, "the tee's first event")
                    .await
                    .expect("an event")
            ),
            "reading src/app/tee.rs"
        );
        assert_eq!(tee.watchers(), 1);

        // --- The same thing, wired the wrong way -----------------------------
        //
        // Identical except for `Mesh::new`, which builds a ledger nothing else
        // holds. Nothing errors, nothing logs, and the peer simply never gets
        // in.
        let unshared_identity = identity(203);
        let unshared_transport = QuicTransport::from_config(
            &unshared_identity,
            TrustLedger::new().shared(),
            &listening_config(),
        )
        .expect("a listener");
        let mut unshared = Mesh::new(
            identity(203),
            PeerStore::ephemeral(),
            Arc::clone(&unshared_transport) as Arc<dyn Transport>,
        );
        unshared.set_local("workshop", Capability::none());
        unshared.announce().await.expect("announce");
        unshared
            .add_peer(&watcher_id.address(), Utc::now())
            .expect("paste");
        unshared
            .set_trust(&watcher_id, Trust::Trusted)
            .await
            .expect("the same decision, recorded in the same way");

        watcher_transport.add_route(
            unshared_identity.id(),
            unshared_transport.local_addr().expect("bound"),
        );
        watcher_mesh
            .add_peer(&unshared_identity.id().address(), Utc::now())
            .expect("paste");
        watcher_mesh
            .set_trust(&unshared_identity.id(), Trust::Trusted)
            .await
            .expect("trust");
        let refused = within(
            "subscribing to a mesh whose ledger nothing shares",
            watcher_mesh.subscribe(&unshared_identity.id()),
        )
        .await
        .expect_err(
            "a transport holding its own empty ledger serves nobody, however the operator \
             decided",
        );
        assert!(
            !format!("{refused:#}").is_empty(),
            "and the refusal is where the trap hides: it is indistinguishable from a peer \
             that is simply not there"
        );
        assert_eq!(unshared_transport.subscriber_count(), 0);

        unshared_transport.shutdown().await;
        watcher_transport.shutdown().await;
        tee.leave().await;
    }

    /// A revocation on the publishing side kills the watcher's stream, over a
    /// real socket, and the watcher can tell that it ended.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn un_trusting_a_watcher_ends_its_stream_now() {
        let mut tee = MeshTee::assemble(
            identity(205),
            PeerStore::ephemeral(),
            &listening_config(),
            "workshop",
            Capability::none(),
            "session-9",
        )
        .await
        .expect("the tee joins");
        let (watcher_transport, mut watcher_mesh) = watcher(206, &tee).await;
        let watcher_id = identity(206).id();
        tee.mesh
            .add_peer(&watcher_id.address(), Utc::now())
            .expect("paste");
        tee.mesh
            .set_trust(&watcher_id, Trust::Trusted)
            .await
            .expect("consent");

        let mut watching = within("subscribing", watcher_mesh.subscribe(&tee.local_id()))
            .await
            .expect("a subscription");
        assert_eq!(tee.publish(&AgentEvent::TextDelta("working".into())), 1);
        assert_eq!(
            delta_text(
                &next(&mut watching, "the first event")
                    .await
                    .expect("an event")
            ),
            "working"
        );

        tee.mesh
            .set_trust(&watcher_id, Trust::Known)
            .await
            .expect("the operator changes their mind");
        assert!(
            next(&mut watching, "the stream after the revocation")
                .await
                .is_none(),
            "the stream ends now, not at the idle timeout"
        );
        assert!(watching.is_closed());
        assert_eq!(
            tee.publish(&AgentEvent::TextDelta("gone".into())),
            0,
            "and nothing this session does reaches the revoked peer any more"
        );

        watcher_transport.shutdown().await;
        tee.leave().await;
    }

    /// Which events cross is `AgentEvent::is_request`'s decision and this
    /// module does not re-make it — including for the three console variants,
    /// which landed after the boundary did.
    #[test]
    fn the_console_variants_cross_as_reports_and_carry_no_writable_ticket() {
        // A console *output* is plainly a report: it is what the peer's command
        // printed, and while a command is blocked on a question it is the most
        // interesting thing on the peer's stream.
        //
        // A console *open* is the sharper call, because something is waiting on
        // it — and it is a report by the same rule that makes `PlanReady` one.
        // The question ("a command on that machine is asking somebody
        // something") is a fact about the peer's turn and a watcher should see
        // it. What a watcher must not have is the ability to *answer*, and that
        // is taken away by voiding the ticket rather than by dropping the
        // event: a claimed console gate is a writer into a shell on the
        // publisher's machine, which is the single most dangerous thing any
        // gate hands out.
        let (gate, host) = ConsoleGate::open();
        for event in [
            AgentEvent::ConsoleOpened {
                command: "sudo rm -rf /".into(),
                gate,
            },
            AgentEvent::ConsoleOutput {
                gate,
                chunk: "Do you want to continue? [Y/n] ".into(),
            },
            AgentEvent::ConsoleWaiting { gate },
            AgentEvent::ConsoleClosed { gate },
        ] {
            assert!(
                !event.is_request(),
                "a console event reports what a command did: {event:?}"
            );
            let crossed = crate::mesh::PeerTurn::sanitize(&event)
                .unwrap_or_else(|| panic!("{event:?} should cross the mesh"));
            let delivered = match crossed.as_event() {
                AgentEvent::ConsoleOpened { gate, .. }
                | AgentEvent::ConsoleOutput { gate, .. }
                | AgentEvent::ConsoleWaiting { gate }
                | AgentEvent::ConsoleClosed { gate } => *gate,
                other => panic!("{other:?}"),
            };
            assert!(
                delivered.claim().is_none(),
                "watching a peer's session must never become typing into a peer's shell"
            );
        }
        // And the desk still holds the real one: what was voided is the copy
        // that crossed, not the console the publisher's own surface will drive.
        assert!(gate.claim().is_some(), "the local console was collateral");
        assert!(host.attended());
    }

    /// The tee is hung off the surface, not off the turn.
    ///
    /// [`super::App::handle_agent_event`] is the one place every agent event on
    /// this surface passes through — a turn's stream, a session-start hook, a
    /// background task reporting in, a subagent run — so a watcher sees exactly
    /// what the local transcript shows. A tee attached to the turn task instead
    /// would show a watcher a session that went silent between turns.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn the_surface_publishes_every_event_it_renders() {
        let tee = MeshTee::assemble(
            identity(209),
            PeerStore::ephemeral(),
            &listening_config(),
            "workshop",
            Capability::none(),
            "session-4",
        )
        .await
        .expect("the tee joins");
        let (watcher_transport, mut watcher_mesh) = watcher(210, &tee).await;
        let watcher_id = identity(210).id();
        let mut app = super::super::App::new(Config::default());
        {
            // The publisher's own half of the decision, before the tee moves
            // into the surface.
            let mut tee = tee;
            tee.mesh
                .add_peer(&watcher_id.address(), Utc::now())
                .expect("paste");
            tee.mesh
                .set_trust(&watcher_id, Trust::Trusted)
                .await
                .expect("consent");
            let node = tee.local_id();
            let mut watching = within("subscribing", watcher_mesh.subscribe(&node))
                .await
                .expect("a subscription");
            app.mesh = Some(tee);

            // Not a turn: a background task reporting in between turns, which
            // is the case a turn-scoped tee would have missed.
            app.handle_agent_event(AgentEvent::Notice("compacting history".into()));
            let event = next(&mut watching, "the surface's event")
                .await
                .expect("an event");
            assert!(
                matches!(event.report(), Some(AgentEvent::Notice(text)) if text == "compacting history"),
                "{event:?}"
            );
            // And the local transcript shows the same thing, from the same
            // value: one event, two readers, no second filter.
            assert_eq!(app.transcript.len(), 1);
        }
        watcher_transport.shutdown().await;
        if let Some(tee) = app.mesh.take() {
            tee.leave().await;
        }
    }

    /// The one variant that does not cross, checked through the tee rather than
    /// through the boundary, so the tee cannot quietly grow a second policy.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_slash_command_never_leaves_this_machine() {
        let mut tee = MeshTee::assemble(
            identity(207),
            PeerStore::ephemeral(),
            &listening_config(),
            "workshop",
            Capability::none(),
            "session-3",
        )
        .await
        .expect("the tee joins");
        let (watcher_transport, mut watcher_mesh) = watcher(208, &tee).await;
        let watcher_id = identity(208).id();
        tee.mesh
            .add_peer(&watcher_id.address(), Utc::now())
            .expect("paste");
        tee.mesh
            .set_trust(&watcher_id, Trust::Trusted)
            .await
            .expect("consent");
        let mut watching = within("subscribing", watcher_mesh.subscribe(&tee.local_id()))
            .await
            .expect("a subscription");

        assert_eq!(
            tee.publish(&AgentEvent::CommandRequested(
                "/model attacker-choice".into()
            )),
            0,
            "a request is another machine driving this one's menu"
        );
        // Nothing was queued behind it either: the next report is the next
        // thing on the stream, so there is no frame carrying the command at
        // all.
        assert_eq!(tee.publish(&AgentEvent::TextDelta("after".into())), 1);
        assert_eq!(
            delta_text(
                &next(&mut watching, "the next report")
                    .await
                    .expect("an event")
            ),
            "after"
        );

        watcher_transport.shutdown().await;
        tee.leave().await;
    }
}
