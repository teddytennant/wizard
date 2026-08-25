//! The certificate a node presents, and the identity recovered from one.
//!
//! # Why the certificate *is* the identity
//!
//! The mesh has no certificate authority and will never have one. A node's
//! name is its ed25519 public key ([`super::node`]), so a certificate that
//! carried a *different* key and vouched for the node id in some field would
//! reintroduce exactly the indirection the address format exists to remove:
//! two things that can disagree, and a rule about which one wins.
//!
//! So the certificate carries the node key itself, in its
//! `subjectPublicKeyInfo`, and it is signed by that key. Recovering a peer id
//! from a certificate is therefore not a lookup, a parse of a name, or a trust
//! decision: it is reading the key out of the one field TLS itself proves
//! possession of.
//!
//! That last clause is the whole security argument and it is worth stating
//! precisely. During the handshake rustls verifies the peer's `CertificateVerify`
//! message against the public key it extracts from this certificate. If this
//! module's parser and rustls' parser could be made to disagree about *which*
//! key that is, a peer holding key `B` could present a certificate that rustls
//! reads as `B` (so the handshake succeeds) and that this module reads as `A`
//! (so the connection is filed under somebody else's identity). DER is
//! canonical and [`read`] refuses everything non-canonical, which makes that
//! disagreement very hard to construct — but "very hard" is not the property
//! this wants.
//!
//! So [`identity_of`] closes it outright: it verifies the certificate's own
//! self-signature, over the exact `tbsCertificate` bytes, with the key it just
//! extracted, through [`NodeId::verify`] (`verify_strict`, no bypass, the same
//! call [`crate::sync`] makes on a bundle manifest). An attacker who does not
//! hold `A`'s private key cannot produce a certificate that verifies under `A`,
//! whatever else the bytes are shaped like. The transport's obligation that "a
//! node's id must be verified, not accepted" is met by a signature check, and
//! the thing signed is the certificate itself.
//!
//! # Why the DER is written and read here rather than by a library
//!
//! Both directions are small, fixed and total. One key algorithm (Ed25519), one
//! subject, two extensions, no chains, no CAs, no revocation lists, no
//! algorithm agility. A certificate-generation crate brings a general X.509
//! writer, an ASN.1 framework and a parser combinator library behind it — nine
//! packages, none of which this file would use more than a corner of, all of
//! which would sit in the supply chain of a binary whose whole selling point is
//! that it is one static file.
//!
//! What is here instead is about two hundred lines of DER, exercised in both
//! directions by this module's own tests *and* end to end by a real QUIC
//! handshake against rustls and webpki in [`super::quic`]'s tests. A DER writer
//! that a different implementation's parser accepts is a DER writer that works;
//! that is a stronger check than any unit test of the bytes.
//!
//! # What is deliberately not here
//!
//! **Expiry as a revocation mechanism.** The certificate is valid from 2000 to
//! 2099 and is a pure function of the key, so a node presents the same bytes on
//! every run. Nothing about a mesh peer expires: the identity is permanent
//! because it is the key, and the way to stop trusting one is
//! `wizard peers trust <address> blocked`, which is a decision on this machine
//! and takes effect immediately ([`super::Transport::revoke`]) rather than at
//! the end of a validity window. A short-lived certificate would add a clock
//! dependency to the handshake and buy nothing, because there is no issuer to
//! decline to renew it.
//!
//! **Names that mean anything.** The subject and the `subjectAltName` are both
//! the node's own address, which is the public key rendered as text. They are
//! there because a certificate with no subject at all is an unusual shape that
//! some parser will one day object to, not because anything reads them. Nothing
//! in this crate makes a decision from a certificate's name; see
//! [`super::tls`], whose verifiers ignore the requested server name on purpose.

use anyhow::{Result, anyhow, bail};
use ed25519_dalek::Signature;
use rustls::pki_types::{CertificateDer, PrivatePkcs8KeyDer};

use super::node::{Identity, NodeId};

// ---------------------------------------------------------------------------
// ASN.1 tags and object identifiers
// ---------------------------------------------------------------------------

const TAG_BOOLEAN: u8 = 0x01;
const TAG_INTEGER: u8 = 0x02;
const TAG_BIT_STRING: u8 = 0x03;
const TAG_OCTET_STRING: u8 = 0x04;
const TAG_OID: u8 = 0x06;
const TAG_UTF8_STRING: u8 = 0x0c;
const TAG_UTC_TIME: u8 = 0x17;
const TAG_GENERALIZED_TIME: u8 = 0x18;
const TAG_SEQUENCE: u8 = 0x30;
const TAG_SET: u8 = 0x31;

/// `[0] EXPLICIT`, which is where `tbsCertificate.version` lives.
const TAG_CONTEXT_0: u8 = 0xa0;
/// `[2] IMPLICIT IA5String`, which is a `GeneralName.dNSName`.
const TAG_CONTEXT_2: u8 = 0x82;
/// `[3] EXPLICIT`, which is where `tbsCertificate.extensions` lives.
const TAG_CONTEXT_3: u8 = 0xa3;

/// `1.3.101.112`, id-Ed25519 (RFC 8410). The only algorithm this mesh speaks,
/// in the certificate's key *and* in its signature.
const OID_ED25519: &[u8] = &[0x2b, 0x65, 0x70];

/// `2.5.4.3`, id-at-commonName.
const OID_COMMON_NAME: &[u8] = &[0x55, 0x04, 0x03];

/// `2.5.29.17`, id-ce-subjectAltName.
const OID_SUBJECT_ALT_NAME: &[u8] = &[0x55, 0x1d, 0x11];

/// `2.5.29.19`, id-ce-basicConstraints.
const OID_BASIC_CONSTRAINTS: &[u8] = &[0x55, 0x1d, 0x13];

/// Bytes of ed25519 public key, and of the `BIT STRING` that holds one once
/// its unused-bits octet is counted.
const KEY_LEN: usize = 32;

/// Bytes of an ed25519 signature.
const SIG_LEN: usize = 64;

/// Longest certificate this module will look at.
///
/// A peer's certificate arrives before anything else does, from a machine that
/// has proved nothing yet, and [`identity_of`] walks it. rustls already bounds
/// a TLS message, but bounding the input to this parser separately is cheap and
/// means the bound does not depend on somebody else's constant. Sixteen
/// kilobytes is forty times the size of the certificate this module writes.
pub const MAX_CERT_BYTES: usize = 16 * 1024;

/// Domain separator for the serial number. Keeps the derivation from colliding
/// with any other hash of a node key, here or later.
const SERIAL_DOMAIN: &[u8] = b"wizard-mesh-certificate-serial-v1";

// ---------------------------------------------------------------------------
// Writing
// ---------------------------------------------------------------------------

/// Append one DER length octet-string for `len`.
///
/// Short form below 128, minimal long form above it. Minimal matters: DER
/// admits exactly one encoding of a length, and a writer that emitted a padded
/// one would produce certificates that a strict parser (including [`read`])
/// rejects.
fn push_len(out: &mut Vec<u8>, len: usize) {
    if len < 0x80 {
        out.push(len as u8);
        return;
    }
    let bytes = len.to_be_bytes();
    // `len >= 0x80`, so at least one byte is non-zero and `position` always
    // finds it.
    let first = bytes
        .iter()
        .position(|byte| *byte != 0)
        .unwrap_or(bytes.len() - 1);
    let used = bytes.len() - first;
    out.push(0x80 | used as u8);
    out.extend_from_slice(&bytes[first..]);
}

/// Append one tag-length-value.
fn push_tlv(out: &mut Vec<u8>, tag: u8, content: &[u8]) {
    out.push(tag);
    push_len(out, content.len());
    out.extend_from_slice(content);
}

/// A tag-length-value as its own buffer.
fn tlv(tag: u8, content: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(content.len() + 4);
    push_tlv(&mut out, tag, content);
    out
}

/// `AlgorithmIdentifier` for Ed25519: the OID and, per RFC 8410, *no*
/// parameters at all. An explicit `NULL` here is the classic interop bug; the
/// RFC says the field must be absent.
fn ed25519_algorithm() -> Vec<u8> {
    tlv(TAG_SEQUENCE, &tlv(TAG_OID, OID_ED25519))
}

/// `Name` holding a single `CN=<text>`.
fn common_name(text: &str) -> Vec<u8> {
    let mut attribute = tlv(TAG_OID, OID_COMMON_NAME);
    attribute.extend_from_slice(&tlv(TAG_UTF8_STRING, text.as_bytes()));
    let attribute = tlv(TAG_SEQUENCE, &attribute);
    let rdn = tlv(TAG_SET, &attribute);
    tlv(TAG_SEQUENCE, &rdn)
}

/// One `Extension`.
fn extension(oid: &[u8], critical: bool, value: &[u8]) -> Vec<u8> {
    let mut body = tlv(TAG_OID, oid);
    if critical {
        // `DEFAULT FALSE`, so a non-critical extension omits the field
        // entirely rather than encoding `FALSE`. DER forbids encoding a value
        // equal to the default.
        body.extend_from_slice(&tlv(TAG_BOOLEAN, &[0xff]));
    }
    body.extend_from_slice(&tlv(TAG_OCTET_STRING, value));
    tlv(TAG_SEQUENCE, &body)
}

/// A positive 16-byte serial derived from the node key.
///
/// Derived rather than random so the whole certificate is a pure function of
/// the identity: a node that restarts presents byte-identical bytes, which is
/// one fewer thing that can differ between two runs when something goes wrong
/// on a wire. The top bit is cleared because a DER `INTEGER` is signed and a
/// leading `1` bit would make the serial negative, which RFC 5280 forbids.
fn serial_of(id: &NodeId) -> Vec<u8> {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(SERIAL_DOMAIN);
    hasher.update(id.to_bytes());
    let digest = hasher.finalize();
    let mut serial = digest[..16].to_vec();
    serial[0] &= 0x7f;
    // An all-zero leading byte would be a non-minimal INTEGER encoding, and
    // DER requires the minimal one. Setting the low bit keeps it non-zero
    // without touching the sign bit.
    if serial[0] == 0 {
        serial[0] = 0x01;
    }
    serial
}

/// The DER `SubjectPublicKeyInfo` for a node id.
fn public_key_info(id: &NodeId) -> Vec<u8> {
    let mut body = ed25519_algorithm();
    let mut key = Vec::with_capacity(KEY_LEN + 1);
    // A BIT STRING's first content octet counts the unused bits in the last
    // byte. A key is whole bytes, so there are none.
    key.push(0);
    key.extend_from_slice(&id.to_bytes());
    body.extend_from_slice(&tlv(TAG_BIT_STRING, &key));
    tlv(TAG_SEQUENCE, &body)
}

/// The `tbsCertificate`, which is both what gets signed and what a verifier
/// re-reads to recover the key.
fn to_be_signed(id: &NodeId) -> Vec<u8> {
    let address = id.address();
    let mut body = Vec::new();
    // version: [0] EXPLICIT INTEGER 2, which is v3. v3 rather than v1 because
    // v1 has no extensions field to put a subjectAltName in.
    body.extend_from_slice(&tlv(TAG_CONTEXT_0, &tlv(TAG_INTEGER, &[0x02])));
    body.extend_from_slice(&tlv(TAG_INTEGER, &serial_of(id)));
    body.extend_from_slice(&ed25519_algorithm());
    body.extend_from_slice(&common_name(&address));

    // validity. UTCTime through 2049 and GeneralizedTime from 2050, which is
    // RFC 5280's rule and not a choice: a parser reading a two-digit year has
    // to know which century, and the standard answers it by switching types.
    let mut validity = tlv(TAG_UTC_TIME, b"000101000000Z");
    validity.extend_from_slice(&tlv(TAG_GENERALIZED_TIME, b"20991231235959Z"));
    body.extend_from_slice(&tlv(TAG_SEQUENCE, &validity));

    body.extend_from_slice(&common_name(&address));
    body.extend_from_slice(&public_key_info(id));

    // extensions: [3] EXPLICIT SEQUENCE OF Extension.
    //
    // basicConstraints is critical with `cA` absent (DEFAULT FALSE), which
    // says in the strongest available terms that this certificate signs
    // nothing but itself. subjectAltName is not critical, because it is the
    // ordinary shape for a certificate that also has a subject.
    let mut extensions = extension(
        OID_BASIC_CONSTRAINTS,
        true,
        &tlv(TAG_SEQUENCE, &[]), // BasicConstraints ::= SEQUENCE {} — all defaults
    );
    let alt_name = tlv(
        TAG_SEQUENCE,
        &tlv(TAG_CONTEXT_2, address.as_bytes()), // GeneralName ::= dNSName
    );
    extensions.extend_from_slice(&extension(OID_SUBJECT_ALT_NAME, false, &alt_name));
    body.extend_from_slice(&tlv(TAG_CONTEXT_3, &tlv(TAG_SEQUENCE, &extensions)));

    tlv(TAG_SEQUENCE, &body)
}

/// This node's certificate and the private key that signs with it.
///
/// Both are a pure function of `identity`. The private key is PKCS#8 v1 (seed
/// only, no attached public half), which is what rustls' ring backend accepts
/// for Ed25519 and the smallest thing that carries the secret.
pub fn certificate_for(
    identity: &Identity,
) -> (CertificateDer<'static>, PrivatePkcs8KeyDer<'static>) {
    let id = identity.id();
    let tbs = to_be_signed(&id);
    let signature = identity.sign(&tbs);

    let mut certificate = tbs;
    certificate.extend_from_slice(&ed25519_algorithm());
    let mut sig = Vec::with_capacity(SIG_LEN + 1);
    sig.push(0);
    sig.extend_from_slice(&signature.to_bytes());
    certificate.extend_from_slice(&tlv(TAG_BIT_STRING, &sig));
    let certificate = tlv(TAG_SEQUENCE, &certificate);

    (
        CertificateDer::from(certificate),
        PrivatePkcs8KeyDer::from(identity.pkcs8_seed()),
    )
}

// ---------------------------------------------------------------------------
// Reading
// ---------------------------------------------------------------------------

/// A cursor over DER that refuses everything but the canonical encoding.
///
/// Every refusal here is one fewer way for two parsers to disagree about the
/// same bytes, which is the property [`identity_of`] rests on. In particular:
/// indefinite lengths (a BER feature DER removes), non-minimal long-form
/// lengths, high-tag-number form, and any trailing byte after the structure
/// that was asked for.
struct Der<'a> {
    rest: &'a [u8],
}

impl<'a> Der<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { rest: bytes }
    }

    /// Whether anything is left.
    fn is_empty(&self) -> bool {
        self.rest.is_empty()
    }

    /// The next element's tag, without consuming it.
    fn peek(&self) -> Option<u8> {
        self.rest.first().copied()
    }

    /// Split the next element into (whole TLV, its tag, its content),
    /// consuming it.
    fn next_tlv(&mut self) -> Result<(&'a [u8], u8, &'a [u8])> {
        let start = self.rest;
        let (&tag, after_tag) = self
            .rest
            .split_first()
            .ok_or_else(|| anyhow!("truncated DER: expected a tag"))?;
        if tag & 0x1f == 0x1f {
            bail!("DER uses the high-tag-number form, which no field here has");
        }
        let (&first, after_first) = after_tag
            .split_first()
            .ok_or_else(|| anyhow!("truncated DER: expected a length"))?;
        let (len, body) = if first < 0x80 {
            (first as usize, after_first)
        } else {
            let count = (first & 0x7f) as usize;
            if count == 0 {
                bail!("DER length is indefinite, which DER does not allow");
            }
            if count > 4 {
                bail!("DER length claims {count} octets, which is longer than anything here");
            }
            if after_first.len() < count {
                bail!("truncated DER: a {count}-octet length ran off the end");
            }
            let (octets, body) = after_first.split_at(count);
            if octets[0] == 0 {
                bail!("DER length is padded with a leading zero, which is not the minimal form");
            }
            let mut len = 0usize;
            for octet in octets {
                len = (len << 8) | *octet as usize;
            }
            if len < 0x80 {
                bail!("DER length {len} is in long form when the short form encodes it");
            }
            (len, body)
        };
        if body.len() < len {
            bail!(
                "truncated DER: an element claims {len} bytes and {} are left",
                body.len()
            );
        }
        // Where the content begins inside the element that was just read, so
        // `whole` covers the header as well. `body` is a suffix of `start`, so
        // the subtraction is the header length and cannot underflow.
        let header = start.len() - body.len();
        let whole = &start[..header + len];
        let content = &body[..len];
        self.rest = &body[len..];
        Ok((whole, tag, content))
    }

    /// The content of the next element, which must carry `tag`.
    fn take(&mut self, tag: u8, what: &str) -> Result<&'a [u8]> {
        let (_, found, content) = self.next_tlv()?;
        if found != tag {
            bail!("{what}: expected DER tag {tag:#04x}, found {found:#04x}");
        }
        Ok(content)
    }

    /// The next element whole, header included, which must carry `tag`.
    fn take_raw(&mut self, tag: u8, what: &str) -> Result<&'a [u8]> {
        let (whole, found, _) = self.next_tlv()?;
        if found != tag {
            bail!("{what}: expected DER tag {tag:#04x}, found {found:#04x}");
        }
        Ok(whole)
    }

    /// Discard the next element whatever it is.
    fn skip(&mut self, what: &str) -> Result<()> {
        if self.is_empty() {
            bail!("{what}: the structure ended early");
        }
        self.next_tlv()?;
        Ok(())
    }

    /// Refuse anything left over.
    fn finish(self, what: &str) -> Result<()> {
        if !self.rest.is_empty() {
            bail!(
                "{what}: {} trailing bytes after the structure",
                self.rest.len()
            );
        }
        Ok(())
    }
}

/// A `BIT STRING` holding `len` whole bytes.
fn bit_string_bytes<'a>(content: &'a [u8], len: usize, what: &str) -> Result<&'a [u8]> {
    let (&unused, bits) = content
        .split_first()
        .ok_or_else(|| anyhow!("{what}: an empty BIT STRING"))?;
    if unused != 0 {
        bail!("{what}: a BIT STRING with {unused} unused bits, which no whole-byte value has");
    }
    if bits.len() != len {
        bail!("{what}: expected {len} bytes, found {}", bits.len());
    }
    Ok(bits)
}

/// An `AlgorithmIdentifier` that must be Ed25519 with no parameters.
fn expect_ed25519(content: &[u8], what: &str) -> Result<()> {
    let mut der = Der::new(content);
    let oid = der.take(TAG_OID, what)?;
    if oid != OID_ED25519 {
        bail!(
            "{what}: this mesh speaks ed25519 only, and this certificate names another algorithm"
        );
    }
    // RFC 8410: the parameters field is absent for Ed25519. Present-and-NULL
    // is the common encoder bug, and accepting it would be accepting a second
    // encoding of the same thing.
    der.finish(what)
}

/// The node id a certificate belongs to, or why it is not one.
///
/// Three checks, and all three have to pass:
///
/// 1. the certificate parses as canonical DER with the exact shape
///    [`certificate_for`] writes, in an ed25519 key and an ed25519 signature;
/// 2. the key is a point on the curve ([`NodeId::from_bytes`]);
/// 3. the certificate's signature over its own `tbsCertificate` verifies under
///    that key, through [`NodeId::verify`] and so through `verify_strict`.
///
/// The third is what makes the answer trustworthy rather than merely parsed.
/// See the module docs.
pub fn identity_of(certificate: &CertificateDer<'_>) -> Result<NodeId> {
    let bytes = certificate.as_ref();
    if bytes.len() > MAX_CERT_BYTES {
        bail!(
            "a peer's certificate is {} bytes, past the {MAX_CERT_BYTES}-byte limit",
            bytes.len()
        );
    }

    let mut outer = Der::new(bytes);
    let body = outer.take(TAG_SEQUENCE, "certificate")?;
    outer.finish("certificate")?;

    let mut fields = Der::new(body);
    let tbs = fields.take_raw(TAG_SEQUENCE, "tbsCertificate")?;
    expect_ed25519(
        fields.take(TAG_SEQUENCE, "signatureAlgorithm")?,
        "signatureAlgorithm",
    )?;
    let signature = bit_string_bytes(
        fields.take(TAG_BIT_STRING, "signatureValue")?,
        SIG_LEN,
        "signatureValue",
    )?;
    fields.finish("certificate")?;

    // Re-open the tbsCertificate: `take_raw` handed back the whole element so
    // the signature can be checked over exactly the bytes that were signed,
    // and the walk below reads the same bytes for the key.
    let mut tbs_outer = Der::new(tbs);
    let inner = tbs_outer.take(TAG_SEQUENCE, "tbsCertificate")?;
    tbs_outer.finish("tbsCertificate")?;

    let mut tbs_fields = Der::new(inner);
    if tbs_fields.peek() == Some(TAG_CONTEXT_0) {
        tbs_fields.skip("tbsCertificate.version")?;
    }
    tbs_fields.skip("tbsCertificate.serialNumber")?;
    expect_ed25519(
        tbs_fields.take(TAG_SEQUENCE, "tbsCertificate.signature")?,
        "tbsCertificate.signature",
    )?;
    tbs_fields.skip("tbsCertificate.issuer")?;
    tbs_fields.skip("tbsCertificate.validity")?;
    tbs_fields.skip("tbsCertificate.subject")?;

    let spki = tbs_fields.take(TAG_SEQUENCE, "subjectPublicKeyInfo")?;
    let mut spki_fields = Der::new(spki);
    expect_ed25519(
        spki_fields.take(TAG_SEQUENCE, "subjectPublicKeyInfo.algorithm")?,
        "subjectPublicKeyInfo.algorithm",
    )?;
    let key = bit_string_bytes(
        spki_fields.take(TAG_BIT_STRING, "subjectPublicKey")?,
        KEY_LEN,
        "subjectPublicKey",
    )?;
    spki_fields.finish("subjectPublicKeyInfo")?;

    let key: [u8; KEY_LEN] = key.try_into().expect("checked to be 32 bytes");
    let id = NodeId::from_bytes(key)
        .map_err(|_| anyhow!("a certificate's key is not a point on the ed25519 curve"))?;

    let signature: [u8; SIG_LEN] = signature.try_into().expect("checked to be 64 bytes");
    id.verify(tbs, &Signature::from_bytes(&signature))
        .map_err(|_| {
            anyhow!(
                "a certificate claiming to be {} is not signed by that key",
                id.short()
            )
        })?;
    Ok(id)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity(byte: u8) -> Identity {
        Identity::from_seed([byte; 32])
    }

    #[test]
    fn a_certificate_carries_the_node_key_and_is_signed_by_it() {
        let identity = identity(1);
        let (certificate, _key) = certificate_for(&identity);
        assert_eq!(
            identity_of(&certificate).expect("the identity comes back"),
            identity.id()
        );
    }

    #[test]
    fn the_certificate_is_a_pure_function_of_the_key() {
        // A node that restarts presents the same bytes, and two nodes never
        // present the same bytes. The first makes a wire capture comparable
        // across runs; the second is the whole point.
        let (first, first_key) = certificate_for(&identity(2));
        let (again, again_key) = certificate_for(&identity(2));
        assert_eq!(first.as_ref(), again.as_ref());
        assert_eq!(first_key.secret_pkcs8_der(), again_key.secret_pkcs8_der());

        let (other, _) = certificate_for(&identity(3));
        assert_ne!(first.as_ref(), other.as_ref());
    }

    #[test]
    fn the_certificate_is_small_enough_that_the_cap_is_generous() {
        let (certificate, _) = certificate_for(&identity(4));
        assert!(
            certificate.as_ref().len() < MAX_CERT_BYTES / 8,
            "{} bytes",
            certificate.as_ref().len()
        );
    }

    #[test]
    fn a_certificate_signed_by_another_key_is_refused() {
        // The attack the self-signature check exists for: take a valid
        // certificate and swap the key in it for somebody else's, so the
        // connection would be filed under an identity the presenter does not
        // hold. The bytes still parse; they do not verify.
        let (certificate, _) = certificate_for(&identity(5));
        let mut bytes = certificate.as_ref().to_vec();
        let victim = identity(6).id().to_bytes();
        let key = identity(5).id().to_bytes();
        let at = bytes
            .windows(32)
            .position(|window| window == key)
            .expect("the key is in the certificate");
        bytes[at..at + 32].copy_from_slice(&victim);

        let err = identity_of(&CertificateDer::from(bytes)).expect_err("not signed by that key");
        assert!(format!("{err:#}").contains("not signed by"), "{err:#}");
    }

    #[test]
    fn a_tampered_certificate_body_is_refused() {
        // Any edit at all: the signature is over the whole tbsCertificate, so
        // a changed serial or a changed name fails exactly as a changed key
        // does.
        let (certificate, _) = certificate_for(&identity(7));
        for at in [8usize, 20, 40] {
            let mut bytes = certificate.as_ref().to_vec();
            bytes[at] ^= 0x01;
            assert!(
                identity_of(&CertificateDer::from(bytes)).is_err(),
                "a byte flipped at {at} must not verify"
            );
        }
    }

    #[test]
    fn a_truncated_or_padded_certificate_is_refused() {
        let (certificate, _) = certificate_for(&identity(8));
        let bytes = certificate.as_ref().to_vec();

        let short = bytes[..bytes.len() - 1].to_vec();
        assert!(identity_of(&CertificateDer::from(short)).is_err());

        // Trailing bytes after a complete structure: a parser that stopped at
        // the end of the SEQUENCE and ignored the rest would accept two
        // different byte strings as the same certificate.
        let mut padded = bytes.clone();
        padded.push(0x00);
        let err = identity_of(&CertificateDer::from(padded)).expect_err("trailing bytes");
        assert!(format!("{err:#}").contains("trailing"), "{err:#}");

        assert!(identity_of(&CertificateDer::from(Vec::new())).is_err());
        assert!(identity_of(&CertificateDer::from(vec![0x30])).is_err());
    }

    #[test]
    fn an_oversized_certificate_is_refused_before_it_is_parsed() {
        let bytes = vec![0x30u8; MAX_CERT_BYTES + 1];
        let err = identity_of(&CertificateDer::from(bytes)).expect_err("too big");
        assert!(format!("{err:#}").contains("past the"), "{err:#}");
    }

    #[test]
    fn non_canonical_der_is_refused_rather_than_interpreted() {
        // Every one of these is a second encoding of something the canonical
        // form already has one encoding for, and every one is a way for two
        // parsers to read the same bytes differently.
        let cases: Vec<(&str, Vec<u8>)> = vec![
            // Indefinite length.
            ("indefinite", vec![0x30, 0x80, 0x00, 0x00]),
            // Long form for a length the short form encodes.
            ("long form", vec![0x30, 0x81, 0x01, 0x00]),
            // Long form padded with a leading zero.
            ("padded length", vec![0x30, 0x82, 0x00, 0x01, 0x00]),
            // High-tag-number form.
            ("high tag", vec![0x3f, 0x01, 0x00, 0x00]),
            // A length that runs off the end.
            ("overrun", vec![0x30, 0x20, 0x00]),
        ];
        for (what, bytes) in cases {
            assert!(
                identity_of(&CertificateDer::from(bytes)).is_err(),
                "{what} must not parse"
            );
        }
    }

    #[test]
    fn a_certificate_naming_another_algorithm_is_refused() {
        // Algorithm agility is how a downgrade gets in. There is one algorithm
        // here and a certificate that names any other is not a mesh identity,
        // whatever else is true about it.
        let (certificate, _) = certificate_for(&identity(9));
        let mut bytes = certificate.as_ref().to_vec();
        // `2b 65 70` is id-Ed25519; `2b 65 71` is id-Ed448.
        let at = bytes
            .windows(3)
            .position(|window| window == OID_ED25519)
            .expect("the OID is in the certificate");
        bytes[at + 2] = 0x71;
        let err = identity_of(&CertificateDer::from(bytes)).expect_err("wrong algorithm");
        assert!(format!("{err:#}").contains("ed25519"), "{err:#}");
    }

    #[test]
    fn a_key_that_is_not_a_curve_point_is_refused() {
        // The same rule `NodeId::parse_address` applies to a pasted address:
        // an identity nobody can hold the private half of is not an identity.
        // `0x02` repeated does not decompress to a point.
        let (certificate, _) = certificate_for(&identity(10));
        let mut bytes = certificate.as_ref().to_vec();
        let key = identity(10).id().to_bytes();
        let at = bytes
            .windows(32)
            .position(|window| window == key)
            .expect("the key is in the certificate");
        bytes[at..at + 32].copy_from_slice(&[0x02u8; 32]);
        let err = identity_of(&CertificateDer::from(bytes)).expect_err("not a curve point");
        assert!(format!("{err:#}").contains("curve"), "{err:#}");
    }

    #[test]
    fn lengths_round_trip_through_both_forms() {
        // The writer's own encoding, checked against the reader that has to
        // refuse everything else. 127/128 is the short/long boundary and
        // 255/256 is where the long form grows a byte.
        for len in [0usize, 1, 127, 128, 255, 256, 65_535, 65_536] {
            let content = vec![0x41u8; len];
            let encoded = tlv(TAG_OCTET_STRING, &content);
            let mut der = Der::new(&encoded);
            let read = der
                .take(TAG_OCTET_STRING, "round trip")
                .expect("reads back");
            assert_eq!(read.len(), len, "length {len}");
            der.finish("round trip").expect("nothing left over");
        }
    }

    #[test]
    fn the_private_key_is_the_pkcs8_of_the_node_seed() {
        // rustls' ring backend derives the public half from the seed, so the
        // certificate's key and the signing key cannot come apart.
        let (_, key) = certificate_for(&identity(11));
        let der = key.secret_pkcs8_der();
        assert_eq!(der.len(), 48, "PKCS#8 v1 for ed25519 is 48 bytes");
        assert_eq!(&der[der.len() - 32..], &[11u8; 32], "the seed, at the end");
    }
}
