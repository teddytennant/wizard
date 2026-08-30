//! Mutual TLS for the mesh, with the peer store in place of a certificate
//! authority.
//!
//! # Authentication and encryption are one act
//!
//! This is the property the whole transport is built around. A QUIC connection
//! cannot be established without a TLS handshake, the handshake cannot complete
//! without both ends proving possession of the private key behind the
//! certificate they presented, and a mesh certificate's key *is* the node id
//! ([`super::x509`]). So by the time there is a connection to write a byte on,
//! both ends have proved who they are, and there is no separate signature check
//! to remember, to get wrong, or to skip.
//!
//! Compare the alternative, which was the shape the [`Transport`](super::Transport)
//! docs were written against: a plaintext socket carrying signed announcements.
//! There the identity check is a thing the application does after the fact, on
//! each message, and every path that forgets it is a hole. Here it is the
//! connection's precondition.
//!
//! # What replaces the CA
//!
//! Nothing, on the TLS side. There is no root store, no chain building, no path
//! validation, and no `WebPkiServerVerifier` anywhere in this module. Every
//! certificate in the mesh is self-signed by the node it names, so a chain is
//! always exactly one certificate long and an intermediate is a certificate
//! nobody asked for. Both verifiers here refuse one outright rather than
//! ignoring it: a presented chain that is longer than the identity it proves is
//! a peer trying something.
//!
//! What *does* the deciding is:
//!
//! - **Outbound** ([`PinnedPeer`]): this node dialled a specific id, and the
//!   certificate that comes back must carry that id. This is the first
//!   obligation on the transport — "a node's id is verified, not accepted" —
//!   and it is where a substituted endpoint is caught. The requested server
//!   name is deliberately ignored; see [`PinnedPeer::verify_server_cert`].
//! - **Inbound** ([`MeshPeers`]): the certificate must be a well-formed mesh
//!   identity *and* that identity must be a peer this machine has decided
//!   something other than [`Trust::Blocked`](super::peer::Trust::Blocked) about
//!   ([`Consent`](super::consent::Consent)). A stranger is refused during the
//!   handshake, before it can allocate a stream, ask a question, or learn this
//!   node's name.
//!
//! That last one is deliberately harsher than it needs to be for the release's
//! feature set, and the harshness is the point: a mesh listener on a laptop is
//! reachable by everything else on the coffee-shop wifi, and "tell me your name
//! and what models you run" is not a question a stranger gets to ask. The
//! bootstrap flow is unaffected, because it already requires both machines to
//! paste each other's address before anything happens.
//!
//! # One algorithm, on purpose
//!
//! [`SCHEMES`] is `[ED25519]` and nothing else, on both ends. A mesh node's key
//! is ed25519 because its *address* is, so every other signature scheme in TLS
//! is an algorithm no legitimate peer can use and a negotiation nobody needs to
//! have. Advertising one algorithm is one fewer downgrade to reason about.

use std::sync::Arc;

use anyhow::{Context, Result};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{CryptoProvider, verify_tls12_signature, verify_tls13_signature};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::server::danger::{ClientCertVerified, ClientCertVerifier};
use rustls::{DigitallySignedStruct, DistinguishedName, Error as TlsError, SignatureScheme};

use super::consent::Consent;
use super::node::{Identity, NodeId};
use super::wire::ALPN;
use super::x509;

/// The signature schemes this mesh offers and accepts: exactly one.
const SCHEMES: &[SignatureScheme] = &[SignatureScheme::ED25519];

/// The crypto backend, which is ring for the reason `Cargo.toml` gives: this
/// binary ships static against musl and aws-lc-rs is a C library.
fn provider() -> Arc<CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

/// Recover a peer id from the certificate chain a TLS peer presented.
///
/// One certificate, self-signed, ed25519, verifying under its own key. Anything
/// else is not a mesh identity. Intermediates are refused rather than skipped:
/// there is no authority in this mesh for an intermediate to have come from, so
/// one arriving means either a misconfiguration or an attempt to confuse a
/// verifier about which certificate is the subject.
fn identity_of_chain(
    end_entity: &CertificateDer<'_>,
    intermediates: &[CertificateDer<'_>],
) -> Result<NodeId, TlsError> {
    if !intermediates.is_empty() {
        return Err(TlsError::General(format!(
            "a mesh peer presented {} intermediate certificates; every mesh identity is \
             self-signed and a chain is always one certificate long",
            intermediates.len()
        )));
    }
    x509::identity_of(end_entity).map_err(|why| TlsError::General(format!("{why:#}")))
}

// ---------------------------------------------------------------------------
// Outbound: the peer must be the peer that was dialled
// ---------------------------------------------------------------------------

/// The client-side verifier: the far end must be exactly the node id this
/// connection was opened to reach.
#[derive(Debug)]
pub struct PinnedPeer {
    expect: NodeId,
    provider: Arc<CryptoProvider>,
}

impl PinnedPeer {
    /// A verifier that accepts one identity and no other.
    pub fn new(expect: NodeId) -> Self {
        Self {
            expect,
            provider: provider(),
        }
    }
}

impl ServerCertVerifier for PinnedPeer {
    /// Check that the certificate belongs to the node that was dialled.
    ///
    /// `server_name` is ignored, and that is not laxity. A mesh address is a
    /// public key, not a host: the DNS name in the certificate is a rendering
    /// of the same key this method is about to compare, so checking it would be
    /// checking the same fact twice through a weaker representation. What the
    /// connection has to prove is possession of the private half, and rustls
    /// proves that against this certificate's key in `CertificateVerify` — the
    /// one thing a name check could never establish.
    ///
    /// Nor is the validity window consulted, for the reason [`super::x509`]
    /// gives: nothing about a mesh identity expires, because there is no issuer
    /// to decline to renew it, and revocation is a decision on this machine
    /// that takes effect immediately.
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, TlsError> {
        let found = identity_of_chain(end_entity, intermediates)?;
        if found != self.expect {
            return Err(TlsError::General(format!(
                "dialled mesh node {} and {} answered; a node's address is its key, so this is \
                 a different machine and not a renamed one",
                self.expect.short(),
                found.short()
            )));
        }
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TlsError> {
        verify_tls12_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TlsError> {
        verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        SCHEMES.to_vec()
    }
}

// ---------------------------------------------------------------------------
// Inbound: the peer must be a peer
// ---------------------------------------------------------------------------

/// The server-side verifier: the far end must present a mesh identity this
/// machine has a record of and has not blocked.
///
/// This is the publisher's consent, applied at the earliest moment it can be:
/// during the handshake, before a stream exists to carry a request on.
/// [`super::quic`] checks again, per request, against the stronger condition a
/// subscription needs ([`Trust::may_send_work`](super::peer::Trust::may_send_work)); this one is the floor.
pub struct MeshPeers {
    consent: Arc<dyn Consent>,
    provider: Arc<CryptoProvider>,
    /// Empty, and required by the trait. There are no certificate authorities
    /// in this mesh, so there is no hint to give a client about whose
    /// certificate to send: it has exactly one, and it is its own.
    hints: Vec<DistinguishedName>,
}

impl MeshPeers {
    /// A verifier that admits the peers `consent` knows about.
    pub fn new(consent: Arc<dyn Consent>) -> Self {
        Self {
            consent,
            provider: provider(),
            hints: Vec::new(),
        }
    }
}

/// The name only. rustls requires its verifiers to be `Debug`, and this one
/// holds the list of machines the operator trusts; printing it into a
/// connection-failure log would be a leak with a stack trace attached.
impl std::fmt::Debug for MeshPeers {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("MeshPeers")
    }
}

impl ClientCertVerifier for MeshPeers {
    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        &self.hints
    }

    /// Every connection presents a certificate, and it is not optional.
    ///
    /// Both of these are the trait's defaults, written out because they are the
    /// difference between a mutually authenticated mesh and a server that
    /// accepts anonymous clients. A default that changes underneath this would
    /// change the security model silently.
    fn offer_client_auth(&self) -> bool {
        true
    }

    fn client_auth_mandatory(&self) -> bool {
        true
    }

    /// Admit a peer this machine has a record of.
    ///
    /// A node that is not in the peer store at all is refused, not merely
    /// unprivileged. Discovery is a paste and a human decision
    /// ([`super::peer`]), so a node nobody added has no business learning that
    /// this one exists, let alone what it is called or what it can do.
    /// [`Trust::Blocked`](super::peer::Trust::Blocked) is refused for the stronger reason that a blocked
    /// peer is not contacted *at all*, in either direction.
    fn verify_client_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        _now: UnixTime,
    ) -> Result<ClientCertVerified, TlsError> {
        let id = identity_of_chain(end_entity, intermediates)?;
        match self.consent.decision(&id) {
            Some(trust) if trust.may_contact() => Ok(ClientCertVerified::assertion()),
            Some(trust) => Err(TlsError::General(format!(
                "mesh node {} is {} on this machine",
                id.short(),
                trust.label()
            ))),
            None => Err(TlsError::General(format!(
                "mesh node {} is not a peer of this machine; its address has to be added here \
                 before it can connect",
                id.short()
            ))),
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TlsError> {
        verify_tls12_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TlsError> {
        verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        SCHEMES.to_vec()
    }
}

// ---------------------------------------------------------------------------
// Configurations
// ---------------------------------------------------------------------------

/// TLS 1.3 only.
///
/// QUIC requires it, so this is not a restriction the mesh is choosing on top
/// of the transport — but it is written down rather than inherited, because the
/// verifiers above have a TLS 1.2 code path that the trait obliges them to
/// implement and nothing else says out loud that it is unreachable.
const VERSIONS: &[&rustls::SupportedProtocolVersion] = &[&rustls::version::TLS13];

/// This node's TLS material, derived once from its identity.
///
/// Derived once because [`x509::certificate_for`] is a pure function of the key
/// and a node presents the same certificate for its whole life; and held here
/// rather than passing an [`Identity`] around because a transport builds a
/// fresh client configuration for every peer it dials, and handing the private
/// key to that many call sites is a wider door than it needs.
pub struct Credentials {
    certificate: CertificateDer<'static>,
    /// PKCS#8 v1, as [`x509::certificate_for`] produced it. Kept as bytes so a
    /// fresh `PrivateKeyDer` can be handed to each configuration rustls builds.
    key: Vec<u8>,
    id: NodeId,
}

impl Credentials {
    /// The certificate and key for `identity`.
    pub fn for_identity(identity: &Identity) -> Self {
        let (certificate, key) = x509::certificate_for(identity);
        Self {
            certificate,
            key: key.secret_pkcs8_der().to_vec(),
            id: identity.id(),
        }
    }

    /// The node these credentials are for.
    pub fn id(&self) -> NodeId {
        self.id
    }

    fn chain(&self) -> Vec<CertificateDer<'static>> {
        vec![self.certificate.clone()]
    }

    fn private_key(&self) -> rustls::pki_types::PrivateKeyDer<'static> {
        rustls::pki_types::PrivatePkcs8KeyDer::from(self.key.clone()).into()
    }
}

/// The secret is never printed, not even as a length.
impl std::fmt::Debug for Credentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Credentials")
            .field("id", &self.id.address())
            .field("key", &"<redacted>")
            .finish()
    }
}

/// The QUIC client configuration for dialling exactly `peer`.
///
/// One configuration per peer, which is not an inefficiency worth removing: the
/// pinned identity is the security property, and a shared client config would
/// mean the check had to move somewhere it could be forgotten.
pub fn client_config(credentials: &Credentials, peer: NodeId) -> Result<quinn::ClientConfig> {
    let mut config = rustls::ClientConfig::builder_with_provider(provider())
        .with_protocol_versions(VERSIONS)
        .context("building the mesh TLS client configuration")?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(PinnedPeer::new(peer)))
        .with_client_auth_cert(credentials.chain(), credentials.private_key())
        .context("installing this node's mesh certificate")?;
    config.alpn_protocols = vec![ALPN.to_vec()];

    let crypto = quinn::crypto::rustls::QuicClientConfig::try_from(config)
        .context("the mesh TLS client configuration is not usable for QUIC")?;
    Ok(quinn::ClientConfig::new(Arc::new(crypto)))
}

/// The QUIC server configuration for this node's listener.
pub fn server_config(
    credentials: &Credentials,
    consent: Arc<dyn Consent>,
) -> Result<quinn::ServerConfig> {
    let mut config = rustls::ServerConfig::builder_with_provider(provider())
        .with_protocol_versions(VERSIONS)
        .context("building the mesh TLS server configuration")?
        .with_client_cert_verifier(Arc::new(MeshPeers::new(consent)))
        .with_single_cert(credentials.chain(), credentials.private_key())
        .context("installing this node's mesh certificate")?;
    config.alpn_protocols = vec![ALPN.to_vec()];

    let crypto = quinn::crypto::rustls::QuicServerConfig::try_from(config)
        .context("the mesh TLS server configuration is not usable for QUIC")?;
    Ok(quinn::ServerConfig::with_crypto(Arc::new(crypto)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugins::mesh::consent::TrustLedger;
    use crate::plugins::mesh::node::Node;
    use crate::plugins::mesh::peer::Peer;
    use crate::plugins::mesh::peer::Trust;
    use chrono::{DateTime, Utc};

    fn identity(byte: u8) -> Identity {
        Identity::from_seed([byte; 32])
    }

    fn at(seconds: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(1_800_000_000 + seconds, 0).expect("timestamp")
    }

    fn ledger(entries: &[(u8, Trust)]) -> TrustLedger {
        let peers: Vec<Peer> = entries
            .iter()
            .map(|(byte, trust)| {
                let mut peer = Peer::new(Node::new(identity(*byte).id()), at(0));
                peer.trust = *trust;
                peer
            })
            .collect();
        let ledger = TrustLedger::new();
        ledger.replace(peers.iter());
        ledger
    }

    fn certificate(byte: u8) -> CertificateDer<'static> {
        x509::certificate_for(&identity(byte)).0
    }

    fn any_name() -> ServerName<'static> {
        ServerName::try_from("mesh.invalid").expect("a name")
    }

    #[test]
    fn the_dialled_identity_is_the_only_one_accepted() {
        // Obligation one, at the only place a client can enforce it: the
        // handshake. A machine that answers on the address this node dialled
        // but holds a different key is a different machine.
        let verifier = PinnedPeer::new(identity(1).id());
        verifier
            .verify_server_cert(
                &certificate(1),
                &[],
                &any_name(),
                &[],
                UnixTime::since_unix_epoch(std::time::Duration::from_secs(1_800_000_000)),
            )
            .expect("the node that was dialled");

        let err = verifier
            .verify_server_cert(
                &certificate(2),
                &[],
                &any_name(),
                &[],
                UnixTime::since_unix_epoch(std::time::Duration::from_secs(1_800_000_000)),
            )
            .expect_err("somebody else");
        let message = format!("{err}");
        assert!(message.contains("different machine"), "{message}");
        assert!(message.contains(&identity(1).id().short()), "{message}");
    }

    #[test]
    fn an_intermediate_certificate_is_refused_rather_than_skipped() {
        // There is no authority in this mesh, so a chain is one certificate
        // long. A verifier that skipped past intermediates to the end entity
        // would be a verifier a peer could confuse about which certificate is
        // the subject.
        let verifier = PinnedPeer::new(identity(3).id());
        let err = verifier
            .verify_server_cert(
                &certificate(3),
                &[certificate(4)],
                &any_name(),
                &[],
                UnixTime::since_unix_epoch(std::time::Duration::from_secs(1_800_000_000)),
            )
            .expect_err("a chain");
        assert!(format!("{err}").contains("self-signed"), "{err}");

        let inbound = MeshPeers::new(ledger(&[(3, Trust::Trusted)]).shared());
        assert!(
            inbound
                .verify_client_cert(
                    &certificate(3),
                    &[certificate(4)],
                    UnixTime::since_unix_epoch(std::time::Duration::from_secs(1_800_000_000)),
                )
                .is_err()
        );
    }

    #[test]
    fn a_certificate_that_is_not_a_mesh_identity_is_refused_by_both_ends() {
        let junk = CertificateDer::from(vec![0x30u8, 0x03, 0x02, 0x01, 0x00]);
        let now = UnixTime::since_unix_epoch(std::time::Duration::from_secs(1_800_000_000));
        assert!(
            PinnedPeer::new(identity(5).id())
                .verify_server_cert(&junk, &[], &any_name(), &[], now)
                .is_err()
        );
        assert!(
            MeshPeers::new(ledger(&[(5, Trust::Trusted)]).shared())
                .verify_client_cert(&junk, &[], now)
                .is_err()
        );
    }

    #[test]
    fn the_listener_admits_peers_and_refuses_strangers_and_blocked_nodes() {
        // The publisher's consent, at the earliest point it can be applied. A
        // stranger does not get to learn that this node exists, so the refusal
        // is at the handshake rather than at the first request.
        let verifier = MeshPeers::new(
            ledger(&[(6, Trust::Trusted), (7, Trust::Known), (8, Trust::Blocked)]).shared(),
        );
        let now = UnixTime::since_unix_epoch(std::time::Duration::from_secs(1_800_000_000));

        verifier
            .verify_client_cert(&certificate(6), &[], now)
            .expect("a trusted peer connects");
        verifier
            .verify_client_cert(&certificate(7), &[], now)
            .expect("a known peer connects; what it may then ask for is a separate question");

        let err = verifier
            .verify_client_cert(&certificate(8), &[], now)
            .expect_err("blocked");
        assert!(format!("{err}").contains("blocked"), "{err}");

        let err = verifier
            .verify_client_cert(&certificate(9), &[], now)
            .expect_err("a stranger");
        assert!(format!("{err}").contains("not a peer"), "{err}");
    }

    #[test]
    fn an_empty_ledger_admits_nobody() {
        // Deny by default, all the way down to the socket.
        let verifier = MeshPeers::new(TrustLedger::new().shared());
        let now = UnixTime::since_unix_epoch(std::time::Duration::from_secs(1_800_000_000));
        assert!(
            verifier
                .verify_client_cert(&certificate(10), &[], now)
                .is_err()
        );
    }

    #[test]
    fn client_authentication_is_mandatory_and_one_scheme_is_offered() {
        let verifier = MeshPeers::new(TrustLedger::new().shared());
        assert!(verifier.offer_client_auth());
        assert!(
            verifier.client_auth_mandatory(),
            "a connection that presented no certificate would have no identity to check"
        );
        assert!(verifier.root_hint_subjects().is_empty());
        assert_eq!(verifier.supported_verify_schemes(), SCHEMES.to_vec());
        assert_eq!(
            PinnedPeer::new(identity(11).id()).supported_verify_schemes(),
            SCHEMES.to_vec()
        );
    }

    #[test]
    fn both_configurations_build_and_carry_the_versioned_alpn() {
        // A mismatch here is a connection that never forms, which is the point
        // of putting the version in the ALPN as well as in the frame header.
        let credentials = Credentials::for_identity(&identity(12));
        assert_eq!(credentials.id(), identity(12).id());
        client_config(&credentials, identity(13).id()).expect("client config");
        server_config(&credentials, TrustLedger::new().shared()).expect("server config");
        assert_eq!(ALPN, b"wizard-mesh/1");
    }

    #[test]
    fn credentials_never_print_the_private_key() {
        let credentials = Credentials::for_identity(&identity(14));
        let rendered = format!("{credentials:?}");
        assert!(
            rendered.contains(&identity(14).id().address()),
            "{rendered}"
        );
        assert!(rendered.contains("redacted"), "{rendered}");
        assert!(!rendered.contains("14, 14"), "{rendered}");
    }
}
