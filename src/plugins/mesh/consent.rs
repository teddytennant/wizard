//! The publisher's half of the trust decision.
//!
//! # The half that never existed
//!
//! [`Mesh::subscribe`](super::Mesh::subscribe) is the *watcher's* decision:
//! whether this machine will take a stream from a peer. It is checked against
//! this machine's peer store, in one place, and no transport gets to re-decide
//! it.
//!
//! A network transport has an inbound half as well, and until this module there
//! was nothing for it to consult. The [`Transport`](super::Transport) docs say
//! so in as many words: a peer asking to watch this node "must be checked
//! against this machine's own peer store before a single event is written to
//! it, or trust means 'whoever asked first'". The loopback never needed it,
//! because its subscribers are in this process and were approved by their own
//! `Mesh`.
//!
//! # Why it is a trait and not a `&PeerStore`
//!
//! Because of who owns what. A [`Mesh`](super::Mesh) owns its
//! [`PeerStore`](super::PeerStore) *and* holds its transport behind an `Arc`,
//! so a transport that borrowed the store would be a cycle, and one that owned
//! a copy of it would be a second store to keep in step.
//!
//! [`Consent`] is the seam instead: one question, asked of whatever holds the
//! answer. [`TrustLedger`] is the implementation a `Mesh` keeps in step with
//! its own store, refreshed in the same call that records a decision, so there
//! is no second step for a caller to forget — the same fusion
//! [`Mesh::set_trust`](super::Mesh::set_trust) already applies to revoking and
//! persisting.
//!
//! # Deny by default, including before anyone has decided anything
//!
//! An empty [`TrustLedger`] answers `None` to everything, and `None` means "not
//! a peer of this machine". A transport built without a ledger therefore serves
//! nobody rather than everybody, which is the direction this codebase's
//! defaults are supposed to lean and has not always.
//!
//! Note what is *not* here: nothing in this module decides what a given trust
//! state permits. That is [`Trust`]'s own answer
//! ([`Trust::may_contact`](super::Trust::may_contact),
//! [`Trust::may_send_work`](super::Trust::may_send_work)), and a second copy of
//! it living next to the socket is exactly the "differently wrong copy of the
//! policy" the transport docs warn about.

use std::collections::BTreeMap;
use std::sync::{Arc, RwLock, RwLockReadGuard, RwLockWriteGuard};

use super::node::NodeId;
use super::peer::{Peer, Trust};

/// What this machine has decided about the peers that reach it.
///
/// One question, because one question is all a transport is entitled to ask.
/// The answer is the recorded decision or `None` for a node that is not a peer
/// at all, and what each answer permits is [`Trust`]'s to say.
pub trait Consent: Send + Sync {
    /// The decision recorded about `peer`, or `None` when there is no record:
    /// a node nobody added is not a peer, and connecting does not make it one.
    fn decision(&self, peer: &NodeId) -> Option<Trust>;
}

/// A live copy of the local trust decisions, shared with a transport.
///
/// Cheap to clone (one `Arc`), cheap to read (a shared lock), and written only
/// when a human changes their mind about a peer, which is rare enough that the
/// lock is never contended by anything that matters.
///
/// It holds *decisions* and nothing else: no names, no capabilities, no
/// addresses. A transport is entitled to know whether it may talk to a node,
/// and the rest of a peer's record is the peer store's business.
#[derive(Clone, Default)]
pub struct TrustLedger {
    decisions: Arc<RwLock<BTreeMap<NodeId, Trust>>>,
}

impl TrustLedger {
    /// An empty ledger, which refuses everybody.
    pub fn new() -> Self {
        Self::default()
    }

    /// Replace the whole ledger with the decisions in `peers`.
    ///
    /// Wholesale rather than incrementally, because an incremental update has a
    /// removal path to forget and a forgotten removal here is a peer that was
    /// dropped from the store and can still be served. Re-deriving from the
    /// store cannot drift from it.
    pub fn replace<'a>(&self, peers: impl IntoIterator<Item = &'a Peer>) {
        let fresh: BTreeMap<NodeId, Trust> = peers
            .into_iter()
            .map(|peer| (peer.id(), peer.trust))
            .collect();
        *self.write() = fresh;
    }

    /// How many peers the ledger holds a decision about.
    pub fn len(&self) -> usize {
        self.read().len()
    }

    /// Whether the ledger refuses everybody.
    pub fn is_empty(&self) -> bool {
        self.read().is_empty()
    }

    /// This ledger as the seam a transport takes.
    pub fn shared(&self) -> Arc<dyn Consent> {
        Arc::new(self.clone())
    }

    // A poisoned lock is recovered rather than propagated, exactly as
    // `LoopbackTransport` recovers its own: the state behind it is a map of
    // decisions with no invariant a panic could leave half-applied, and a
    // transport that started refusing every peer because something unrelated
    // panicked would be a denial of service with a security story attached.
    fn read(&self) -> RwLockReadGuard<'_, BTreeMap<NodeId, Trust>> {
        self.decisions
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn write(&self) -> RwLockWriteGuard<'_, BTreeMap<NodeId, Trust>> {
        self.decisions
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl Consent for TrustLedger {
    fn decision(&self, peer: &NodeId) -> Option<Trust> {
        self.read().get(peer).copied()
    }
}

/// How many peers, and nothing about which. A ledger's contents are the list of
/// machines somebody trusts, and a `{:?}` of a struct holding one should not
/// put that in a log line.
impl std::fmt::Debug for TrustLedger {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TrustLedger")
            .field("peers", &self.len())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugins::mesh::node::{Identity, Node};
    use chrono::{DateTime, Utc};

    fn at(seconds: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(1_800_000_000 + seconds, 0).expect("timestamp")
    }

    fn peer(byte: u8, trust: Trust) -> Peer {
        let mut peer = Peer::new(Node::new(Identity::from_seed([byte; 32]).id()), at(0));
        peer.trust = trust;
        peer
    }

    #[test]
    fn an_empty_ledger_refuses_everybody() {
        // The default this module exists to get right: a transport built
        // without a ledger serves nobody, not everybody.
        let ledger = TrustLedger::new();
        assert!(ledger.is_empty());
        assert_eq!(ledger.decision(&peer(1, Trust::Trusted).id()), None);
        assert_eq!(TrustLedger::default().len(), 0);
    }

    #[test]
    fn the_ledger_answers_with_the_recorded_decision_and_nothing_more() {
        let peers = [
            peer(2, Trust::Trusted),
            peer(3, Trust::Known),
            peer(4, Trust::Blocked),
        ];
        let ledger = TrustLedger::new();
        ledger.replace(peers.iter());
        assert_eq!(ledger.len(), 3);
        assert_eq!(ledger.decision(&peers[0].id()), Some(Trust::Trusted));
        assert_eq!(ledger.decision(&peers[1].id()), Some(Trust::Known));
        assert_eq!(ledger.decision(&peers[2].id()), Some(Trust::Blocked));
        // A node nobody added is not a peer, and it is `None` rather than a
        // trust state, so a caller cannot confuse "not decided" with "decided
        // to be the default".
        assert_eq!(ledger.decision(&peer(5, Trust::Trusted).id()), None);
    }

    #[test]
    fn replacing_the_ledger_removes_what_is_no_longer_in_the_store() {
        // The reason `replace` is wholesale: a peer that was forgotten must
        // stop being served, and an incremental update is where that gets
        // missed.
        let peers = [peer(6, Trust::Trusted), peer(7, Trust::Trusted)];
        let ledger = TrustLedger::new();
        ledger.replace(peers.iter());
        assert_eq!(ledger.decision(&peers[1].id()), Some(Trust::Trusted));

        ledger.replace(peers[..1].iter());
        assert_eq!(ledger.decision(&peers[0].id()), Some(Trust::Trusted));
        assert_eq!(
            ledger.decision(&peers[1].id()),
            None,
            "forgotten is refused"
        );
    }

    #[test]
    fn every_clone_sees_the_same_decisions() {
        // The transport holds one of these and a `Mesh` writes to it; if a
        // clone kept its own copy, revoking would revoke nothing.
        let ledger = TrustLedger::new();
        let held: Arc<dyn Consent> = ledger.shared();
        let peers = [peer(8, Trust::Trusted)];
        ledger.replace(peers.iter());
        assert_eq!(held.decision(&peers[0].id()), Some(Trust::Trusted));
        ledger.replace(std::iter::empty());
        assert_eq!(held.decision(&peers[0].id()), None);
    }

    #[test]
    fn a_debug_print_does_not_list_who_is_trusted() {
        let peers = [peer(9, Trust::Trusted)];
        let ledger = TrustLedger::new();
        ledger.replace(peers.iter());
        let rendered = format!("{ledger:?}");
        assert!(rendered.contains("peers: 1"), "{rendered}");
        assert!(
            !rendered.contains(&peers[0].id().address()),
            "who this machine trusts is nobody else's business: {rendered}"
        );
    }
}
