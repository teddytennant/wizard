//! Node identity: the ed25519 keypair this machine *is*, and the address every
//! other node knows it by.
//!
//! The address is not assigned, looked up, or registered anywhere: it is a
//! reversible encoding of the public key itself. That single property is what
//! makes the mesh serverless. There is no registry to resolve a name in,
//! because the name *is* the key, so "add a peer" is a paste and a human
//! decision rather than a query to somebody's directory service.
//!
//! ## Why this is not `crate::sync`'s key
//!
//! [`crate::sync`] already does ed25519 the way this module wants it done:
//! seeds straight from the OS RNG, the key file written through
//! [`crate::platform::secrets`], `verify_strict` on the way back in, and no
//! flag anywhere that skips verification. This module reuses those primitives
//! (deliberately, down to the fingerprint format) but keeps its own key file,
//! because the two keys have opposite exposure:
//!
//! - The sync key signs bundles of the user's own `~/.wizard` for the user's
//!   own other machines. Its public half is compared out of band, once, with a
//!   person the user already is.
//! - The node key *is* the mesh address. Its public half gets pasted into
//!   chat windows and rendered in a graph explorer on somebody else's screen.
//!
//! Sharing one key between those would mean publishing the identity that
//! authorises writes into `~/.wizard` in order to say hello, and rotating a
//! leaked node key would invalidate every pinned sync trust line. Two files,
//! one set of primitives.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use base64::Engine as _;
use base64::engine::general_purpose::{
    STANDARD_NO_PAD as BASE64_NO_PAD, URL_SAFE_NO_PAD as BASE64_URL,
};
use chrono::{DateTime, Utc};
use ed25519_dalek::{Signature, Signer as _, SigningKey, VerifyingKey};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use super::PeerText;
use super::capability::Capability;
use crate::platform::secrets;

/// Prefix every mesh address carries. It exists so a pasted address is
/// recognisable as one in a chat log, and so a truncated or half-copied
/// address fails to parse instead of decoding to a different key.
pub const ADDRESS_PREFIX: &str = "wiz1";

/// The encoded body of an address: 32 key bytes in unpadded base64url.
/// (32 bytes → ceil(32 * 4 / 3) = 43 characters.)
const ADDRESS_BODY_LEN: usize = 43;

/// `~/.wizard/node.key`: base64 of this node's 32-byte ed25519 seed, 0600.
pub fn key_path(wizard_dir: &Path) -> PathBuf {
    wizard_dir.join("node.key")
}

// ---------------------------------------------------------------------------
// NodeId
// ---------------------------------------------------------------------------

/// A node's identity: its ed25519 public key.
///
/// Constructing one always goes through `VerifyingKey`, so a `NodeId` that
/// exists is a point on the curve. Garbage that happens to be 32 bytes long
/// is rejected at the paste, not at the first signature check.
#[derive(Clone, Copy)]
pub struct NodeId(VerifyingKey);

impl NodeId {
    /// Wrap a verifying key that has already been parsed.
    pub fn from_verifying_key(key: VerifyingKey) -> Self {
        Self(key)
    }

    /// Parse 32 raw public-key bytes. Fails when they are not a valid ed25519
    /// public key.
    pub fn from_bytes(bytes: [u8; 32]) -> Result<Self> {
        VerifyingKey::from_bytes(&bytes)
            .map(Self)
            .map_err(|_| anyhow!("not a valid ed25519 public key"))
    }

    /// The raw 32 public-key bytes.
    pub fn to_bytes(self) -> [u8; 32] {
        self.0.to_bytes()
    }

    /// The verifying key, for signature checks.
    pub fn verifying_key(&self) -> &VerifyingKey {
        &self.0
    }

    /// This node's address: the identity, encoded for a human to copy.
    ///
    /// base64url with no padding, so the whole address survives a double
    /// click, a URL, a shell argument and a JSON string without an escape or
    /// a selection boundary anywhere in it.
    pub fn address(&self) -> String {
        format!("{ADDRESS_PREFIX}{}", BASE64_URL.encode(self.0.to_bytes()))
    }

    /// Parse an address back into the identity it encodes.
    ///
    /// Surrounding whitespace is tolerated because this is a paste target.
    /// Everything else is strict: the prefix, the length, the alphabet, and
    /// the curve check. A mesh whose addresses can be mistyped into *some
    /// other valid node* is a mesh where a typo is a security incident.
    pub fn parse_address(raw: &str) -> Result<Self> {
        let trimmed = raw.trim();
        let body = trimmed.strip_prefix(ADDRESS_PREFIX).ok_or_else(|| {
            anyhow!("a mesh address starts with `{ADDRESS_PREFIX}` (got {trimmed:?})")
        })?;
        if body.len() != ADDRESS_BODY_LEN {
            return Err(anyhow!(
                "a mesh address is {} characters after `{ADDRESS_PREFIX}`, this one has {} \
                 (it looks truncated, or run together with something else)",
                ADDRESS_BODY_LEN,
                body.len()
            ));
        }
        let bytes = BASE64_URL
            .decode(body)
            .map_err(|_| anyhow!("mesh address {trimmed:?} is not valid base64url"))?;
        let raw: [u8; 32] = bytes
            .as_slice()
            .try_into()
            .map_err(|_| anyhow!("mesh address {trimmed:?} does not decode to 32 bytes"))?;
        Self::from_bytes(raw)
            .map_err(|_| anyhow!("mesh address {trimmed:?} is not a valid ed25519 public key"))
    }

    /// OpenSSH-style fingerprint, identical in shape to `wizard sync key`'s so
    /// the two can be compared out of band the same way.
    pub fn fingerprint(&self) -> String {
        use sha2::{Digest, Sha256};
        let mut hasher = Sha256::new();
        hasher.update(self.0.to_bytes());
        format!("SHA256:{}", BASE64_NO_PAD.encode(hasher.finalize()))
    }

    /// A short label for a graph node or a log line. Never use it to *identify*
    /// a peer: it is a prefix, and prefixes collide.
    pub fn short(&self) -> String {
        let address = self.address();
        address.chars().take(ADDRESS_PREFIX.len() + 8).collect()
    }

    /// Verify `signature` over `message`.
    ///
    /// `verify_strict` and nothing else: it rejects small-order and
    /// non-canonical public keys, which is what stops one signature from
    /// verifying under two different identities. There is deliberately no
    /// lenient variant and no bypass flag here, exactly as in [`crate::sync`].
    pub fn verify(&self, message: &[u8], signature: &Signature) -> Result<()> {
        self.0
            .verify_strict(message, signature)
            .map_err(|_| anyhow!("signature does not verify against {}", self.short()))
    }
}

/// The transcript renderer's view of a peer, which is these two strings and
/// nothing else.
///
/// One line each, and that is the entire dependency `src/app/transcript.rs`
/// has on the mesh. It used to take a `NodeId` directly, which made a core
/// renderer name a plugin; the trait is core's, the type is here, and what
/// crosses is text derived from a public key.
///
/// The trait methods deliberately mirror the inherent ones rather than
/// wrapping something else: the marker on every line of a watched session has
/// to be the same short form the graph and the log lines print, or a reader
/// comparing two surfaces would see two different names for one machine.
impl crate::app::PeerAddress for NodeId {
    fn short(&self) -> String {
        NodeId::short(self)
    }

    fn address(&self) -> String {
        NodeId::address(self)
    }
}

impl PartialEq for NodeId {
    fn eq(&self, other: &Self) -> bool {
        self.0.to_bytes() == other.0.to_bytes()
    }
}

impl Eq for NodeId {}

impl std::hash::Hash for NodeId {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.0.to_bytes().hash(state);
    }
}

impl PartialOrd for NodeId {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for NodeId {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.0.to_bytes().cmp(&other.0.to_bytes())
    }
}

/// The address, which is the whole of a node's public identity. Safe to print:
/// unlike [`Identity`], there is no secret in here.
impl std::fmt::Debug for NodeId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.address())
    }
}

impl std::fmt::Display for NodeId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.address())
    }
}

/// Serialised as the address, so the peer store on disk is the same text a
/// human pastes, and so *every* deserialisation runs the curve check.
impl Serialize for NodeId {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.address())
    }
}

impl<'de> Deserialize<'de> for NodeId {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        NodeId::parse_address(&raw).map_err(serde::de::Error::custom)
    }
}

// ---------------------------------------------------------------------------
// Identity
// ---------------------------------------------------------------------------

/// This machine's mesh keypair.
pub struct Identity {
    signing: SigningKey,
}

impl Identity {
    /// Load `~/.wizard/node.key`, generating it on first use.
    ///
    /// The seed comes straight from the OS RNG and is written through
    /// [`secrets::write_private_atomic`], which creates the parent private,
    /// creates the file 0600 before a byte enters it, fsyncs, and renames. A
    /// filesystem that cannot make the parent owner-only fails the call rather
    /// than writing a private key other local users can read: the same strict
    /// policy `wizard sync` applies to its own key, and for the same reason.
    pub fn load_or_generate(wizard_dir: &Path) -> Result<Self> {
        let path = key_path(wizard_dir);
        match std::fs::read_to_string(&path) {
            Ok(raw) => {
                let bytes = base64::engine::general_purpose::STANDARD
                    .decode(raw.trim())
                    .with_context(|| format!("decoding {}", path.display()))?;
                let seed: [u8; 32] = bytes.as_slice().try_into().map_err(|_| {
                    anyhow!(
                        "{} does not hold a base64 32-byte seed; move it aside to mint a \
                         new node identity (every peer will have to re-add you)",
                        path.display()
                    )
                })?;
                Ok(Self {
                    signing: SigningKey::from_bytes(&seed),
                })
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                let mut seed = [0u8; 32];
                getrandom::fill(&mut seed)
                    .map_err(|err| anyhow!("gathering node key randomness: {err}"))?;
                let encoded = base64::engine::general_purpose::STANDARD.encode(seed);
                secrets::write_private_atomic(&path, format!("{encoded}\n").as_bytes())
                    .with_context(|| format!("writing {}", path.display()))?;
                Ok(Self {
                    signing: SigningKey::from_bytes(&seed),
                })
            }
            Err(err) => Err(anyhow!(err).context(format!("reading {}", path.display()))),
        }
    }

    /// An identity from a caller-supplied seed. For tests and for the
    /// deterministic synthetic mesh; never for a real node, whose seed must
    /// come from the OS RNG.
    pub fn from_seed(seed: [u8; 32]) -> Self {
        Self {
            signing: SigningKey::from_bytes(&seed),
        }
    }

    /// This node's public identity.
    pub fn id(&self) -> NodeId {
        NodeId(self.signing.verifying_key())
    }

    /// Sign `message` as this node.
    pub fn sign(&self, message: &[u8]) -> Signature {
        self.signing.sign(message)
    }

    /// This identity as a PKCS#8 v1 private key, for the TLS stack.
    ///
    /// The one place in the crate that hands the secret half out, and it is
    /// `pub(super)` so that place is inside [`super`]. It exists because the
    /// node key *is* the mesh's TLS key ([`super::x509`]): rustls needs the
    /// private half in the encoding its backend parses, and the alternative
    /// was a general `seed()` accessor, which is a much wider door for one
    /// caller to walk through.
    ///
    /// Version 1 (RFC 5208), which is the seed alone with no public half
    /// attached. rustls' ring backend derives the public key from the seed, so
    /// there is no second copy of it here to disagree with the certificate's.
    ///
    /// The prefix is fixed: `SEQUENCE { INTEGER 0, SEQUENCE { OID 1.3.101.112 },
    /// OCTET STRING { OCTET STRING(32) } }`, whose lengths are all constant
    /// because an ed25519 seed is always 32 bytes.
    pub(super) fn pkcs8_seed(&self) -> Vec<u8> {
        const PREFIX: [u8; 16] = [
            0x30, 0x2e, // SEQUENCE (46 bytes)
            0x02, 0x01, 0x00, // INTEGER 0 — PKCS#8 version 1
            0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, // SEQUENCE { OID 1.3.101.112 }
            0x04, 0x22, // OCTET STRING (34 bytes)
            0x04, 0x20, // OCTET STRING (32 bytes): the seed itself
        ];
        let mut der = Vec::with_capacity(PREFIX.len() + 32);
        der.extend_from_slice(&PREFIX);
        der.extend_from_slice(&self.signing.to_bytes());
        der
    }

    /// The [`Node`] record this identity announces: its own capability, no
    /// `last_seen` (a node does not observe itself over the network).
    ///
    /// The capability is normalised on the way in even though it is this
    /// machine's own, so that "every [`Capability`] inside a [`Node`] has been
    /// through the ingest rules" is true without a caller having to remember
    /// it.
    pub fn announce(&self, name: &str, caps: Capability) -> Node {
        Node {
            id: self.id(),
            name: PeerText::sanitize(name),
            caps: caps.normalised(),
            last_seen: None,
        }
    }
}

/// Public half only. A `{:?}` of a struct holding an [`Identity`] must not put
/// the seed into a log file or a panic message.
impl std::fmt::Debug for Identity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Identity")
            .field("id", &self.id().address())
            .field("seed", &"<redacted>")
            .finish()
    }
}

// ---------------------------------------------------------------------------
// Node
// ---------------------------------------------------------------------------

/// One node in the mesh, local or remote.
///
/// The v2 plan writes this record as `{ id, addr, name, caps, last_seen }`.
/// `addr` is [`Node::addr`], a method, not a field: the address is a pure
/// function of `id`, and a struct that stores both can be handed a pair that
/// disagree (by a hand-edited peer file, by a peer announcing somebody else's
/// address, or by a future wire format that forgets to check). A derived
/// address cannot lie about which key it belongs to, and that is the one
/// property the whole serverless model rests on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Node {
    /// The public key. Also the address, also the primary key in the store.
    pub id: NodeId,
    /// Display name the node calls itself. Peer-supplied, hence [`PeerText`].
    #[serde(default)]
    pub name: PeerText,
    /// What the node says it can do. Advertised, never verified, and
    /// `accepts_work: false` unless it says otherwise.
    #[serde(default)]
    pub caps: Capability,
    /// When this node was last *observed*, not when it was added. `None` means
    /// never: a pasted address that has not answered yet renders as unseen,
    /// which is the difference between a graph that is honest and a graph that
    /// is decorative.
    #[serde(default)]
    pub last_seen: Option<DateTime<Utc>>,
}

impl Node {
    /// A node record for a freshly pasted address: identity only, nothing
    /// claimed, nothing seen.
    pub fn from_address(address: &str) -> Result<Self> {
        Ok(Self::new(NodeId::parse_address(address)?))
    }

    /// A node record with no claims attached.
    pub fn new(id: NodeId) -> Self {
        Self {
            id,
            name: PeerText::default(),
            caps: Capability::default(),
            last_seen: None,
        }
    }

    /// The address to paste. Derived from [`Node::id`]; see the type docs.
    pub fn addr(&self) -> String {
        self.id.address()
    }

    /// The label to render: the node's own name when it has one, otherwise a
    /// short form of the address. Never empty, so a node with a blank or
    /// entirely-control-character name cannot render as an unlabelled dot.
    pub fn label(&self) -> String {
        if self.name.is_empty() {
            self.id.short()
        } else {
            self.name.as_str().to_string()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id_from(byte: u8) -> NodeId {
        Identity::from_seed([byte; 32]).id()
    }

    #[test]
    fn an_address_round_trips_through_its_text_form() {
        let id = id_from(7);
        let address = id.address();
        assert!(address.starts_with(ADDRESS_PREFIX), "{address}");
        assert_eq!(address.len(), ADDRESS_PREFIX.len() + ADDRESS_BODY_LEN);
        assert_eq!(NodeId::parse_address(&address).expect("parse"), id);
        // Pasting picks up whitespace; that must not change the identity.
        assert_eq!(
            NodeId::parse_address(&format!("  {address}\n")).expect("parse"),
            id
        );
    }

    #[test]
    fn a_damaged_address_is_rejected_rather_than_decoded_to_another_node() {
        let address = id_from(9).address();
        let cases = [
            String::new(),
            "wiz1".to_string(),
            address.trim_start_matches(ADDRESS_PREFIX).to_string(),
            address[..address.len() - 1].to_string(),
            format!("{address}A"),
            format!("{address}{address}"),
            // Right shape, wrong alphabet: base64url has no `+` or `/`.
            format!("{ADDRESS_PREFIX}{}", "+".repeat(ADDRESS_BODY_LEN)),
        ];
        for bad in cases {
            assert!(
                NodeId::parse_address(&bad).is_err(),
                "{bad:?} must not parse"
            );
        }

        // A half-copied address is the likeliest paste accident, so its error
        // has to say so rather than complain about base64: the length check
        // exists for that message, and without it this case falls through to
        // the decoder and comes back as gibberish about an alphabet.
        let truncated = &address[..address.len() - 4];
        let err = NodeId::parse_address(truncated).expect_err("truncated");
        let message = format!("{err:#}");
        assert!(message.contains("truncated"), "{message}");
        assert!(message.contains(&ADDRESS_BODY_LEN.to_string()), "{message}");
    }

    #[test]
    fn thirty_two_bytes_that_are_not_a_curve_point_are_refused() {
        // `0x02` repeated does not decompress to a point on the curve (about
        // half of all 32-byte strings do not). An address is only useful if
        // holding one means somebody can hold the matching private key, so
        // this has to fail at parse time rather than at the first signature
        // check, which may be much later or never.
        let bytes = [0x02u8; 32];
        assert!(NodeId::from_bytes(bytes).is_err());
        let address = format!("{ADDRESS_PREFIX}{}", BASE64_URL.encode(bytes));
        assert_eq!(address.len(), ADDRESS_PREFIX.len() + ADDRESS_BODY_LEN);
        let err = NodeId::parse_address(&address).expect_err("not a public key");
        assert!(format!("{err:#}").contains("ed25519"), "{err:#}");
    }

    #[test]
    fn the_key_file_is_private_and_the_identity_survives_a_reload() {
        let dir = tempfile::tempdir().expect("tempdir");
        let first = Identity::load_or_generate(dir.path()).expect("generate");
        let second = Identity::load_or_generate(dir.path()).expect("reload");
        assert_eq!(
            first.id(),
            second.id(),
            "the node id must survive a restart, or every peer has to re-add us"
        );
        assert!(key_path(dir.path()).exists());
        #[cfg(unix)]
        assert!(
            secrets::is_protected(&key_path(dir.path())).expect("stat"),
            "a node private key must not be readable by other local users"
        );
    }

    #[test]
    fn two_machines_get_different_identities() {
        let a = tempfile::tempdir().expect("tempdir");
        let b = tempfile::tempdir().expect("tempdir");
        let first = Identity::load_or_generate(a.path()).expect("generate");
        let second = Identity::load_or_generate(b.path()).expect("generate");
        assert_ne!(
            first.id(),
            second.id(),
            "seeds come from the OS RNG, not from a constant"
        );
    }

    #[test]
    fn a_corrupt_key_file_reports_the_path_instead_of_minting_a_new_identity() {
        // Silently regenerating would change this node's address, and every
        // peer that pinned the old one would see a stranger.
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(key_path(dir.path()), "not base64 at all!!\n").expect("write");
        let err = Identity::load_or_generate(dir.path()).expect_err("corrupt key");
        assert!(format!("{err:#}").contains("node.key"), "{err:#}");

        std::fs::write(key_path(dir.path()), "c2hvcnQ=\n").expect("write");
        let err = Identity::load_or_generate(dir.path()).expect_err("short seed");
        assert!(format!("{err:#}").contains("32-byte"), "{err:#}");
    }

    #[cfg(unix)]
    #[test]
    fn a_key_file_that_cannot_be_read_is_an_error_not_a_new_identity() {
        // Only *absent* means "mint one". A key file that exists but cannot be
        // read (no permission, a directory in its place, a mount that went
        // away) must not be quietly replaced: the old identity is pinned by
        // every peer that added this node, and minting a fresh one strands all
        // of them with no way back and no message saying why.
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().expect("tempdir");
        let original = Identity::load_or_generate(dir.path()).expect("generate");
        let path = key_path(dir.path());
        let before = std::fs::read(&path).expect("read the key");

        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000)).expect("chmod");
        if std::fs::read(&path).is_ok() {
            // Running as root (some CI containers do): mode bits do not apply,
            // so there is no unreadable file here to test against. Skipping
            // beats asserting something this environment cannot produce.
            return;
        }

        let err = Identity::load_or_generate(dir.path()).expect_err("unreadable key");
        assert!(format!("{err:#}").contains("node.key"), "{err:#}");

        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).expect("chmod");
        assert_eq!(
            std::fs::read(&path).expect("read the key"),
            before,
            "the key on disk was not replaced"
        );
        assert_eq!(
            Identity::load_or_generate(dir.path()).expect("reload").id(),
            original.id(),
            "and the node still has the identity its peers pinned"
        );
    }

    #[test]
    fn signatures_verify_only_against_the_signing_node_and_the_signed_bytes() {
        let identity = Identity::from_seed([3u8; 32]);
        let other = Identity::from_seed([4u8; 32]);
        let message = b"announce: wizard mesh";
        let signature = identity.sign(message);

        identity.id().verify(message, &signature).expect("verifies");
        assert!(
            identity
                .id()
                .verify(b"announce: wizard mes", &signature)
                .is_err(),
            "a changed message must not verify"
        );
        assert!(
            other.id().verify(message, &signature).is_err(),
            "another node's key must not verify our signature"
        );
    }

    #[test]
    fn a_node_id_serialises_as_the_address_and_validates_on_the_way_back() {
        let id = id_from(11);
        let json = serde_json::to_string(&id).expect("serialize");
        assert_eq!(json, format!("\"{}\"", id.address()));
        assert_eq!(
            serde_json::from_str::<NodeId>(&json).expect("deserialize"),
            id
        );
        // A store file edited by hand cannot introduce a bogus identity.
        assert!(serde_json::from_str::<NodeId>("\"wiz1nope\"").is_err());
    }

    #[test]
    fn the_address_is_derived_so_a_node_cannot_advertise_someone_elses() {
        let node = Node::new(id_from(13));
        assert_eq!(node.addr(), node.id.address());
        // There is no `addr` field to disagree with `id`, in the struct or in
        // its serialised form.
        let json = serde_json::to_value(&node).expect("serialize");
        assert!(json.get("addr").is_none(), "{json}");
        let restored: Node = serde_json::from_value(json).expect("deserialize");
        assert_eq!(restored.addr(), node.addr());
    }

    #[test]
    fn a_node_always_has_something_to_render() {
        let mut node = Node::new(id_from(17));
        assert_eq!(node.label(), node.id.short());
        assert!(node.label().starts_with(ADDRESS_PREFIX));
        // A name that sanitises away must not leave an unlabelled dot.
        node.name = PeerText::sanitize("\u{202e}\u{0007}");
        assert_eq!(node.label(), node.id.short());
        node.name = PeerText::sanitize("workshop");
        assert_eq!(node.label(), "workshop");
    }

    #[test]
    fn fingerprints_match_the_shape_sync_prints() {
        let fingerprint = id_from(19).fingerprint();
        assert!(fingerprint.starts_with("SHA256:"), "{fingerprint}");
        let body = fingerprint.strip_prefix("SHA256:").expect("prefix");
        assert_eq!(body.len(), 43, "{fingerprint}");
        assert!(!body.ends_with('='), "unpadded: {fingerprint}");
    }

    #[test]
    fn an_identity_debug_print_never_carries_the_seed() {
        let identity = Identity::from_seed([23u8; 32]);
        let rendered = format!("{identity:?}");
        assert!(rendered.contains(&identity.id().address()), "{rendered}");
        assert!(rendered.contains("redacted"), "{rendered}");
        // The seed, in either encoding it is ever written in.
        assert!(
            !rendered.contains(&base64::engine::general_purpose::STANDARD.encode([23u8; 32])),
            "{rendered}"
        );
        assert!(!rendered.contains("17171717"), "{rendered}");
    }
}
