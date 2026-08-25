//! The bytes on the wire: one frame format, versioned from its first byte.
//!
//! # The shape
//!
//! ```text
//! 0        1        2        3        4        5        6
//! +--------+--------+--------+--------+--------+--------+========+
//! |version | kind   |           length (big-endian)      | body   |
//! +--------+--------+--------+--------+--------+--------+========+
//! ```
//!
//! Six bytes of header and then exactly `length` bytes of body. That is the
//! whole format.
//!
//! # Why the version is the first byte and not a field in the body
//!
//! A wire format shipped once is supported forever, so the first thing a
//! reader needs is permission to stop. [`WIRE_VERSION`] is the first octet of
//! every frame, before the kind, before the length, before anything that has to
//! be interpreted: a version 2 frame arriving at a version 1 reader is refused
//! after one byte, with a message naming both numbers, rather than being
//! decoded as whatever a version 1 reader happens to make of it.
//!
//! The version appears a second time in [`ALPN`], which is negotiated during
//! the TLS handshake. That is not redundancy for its own sake. ALPN failure is
//! a *connection* refusal, so two nodes running incompatible versions never
//! establish a session at all and neither one has to reason about a half-spoken
//! protocol; the header byte is what catches the case ALPN cannot, which is a
//! peer that negotiated `wizard-mesh/1` and then wrote something else.
//!
//! # Why the body is JSON, when the design this was modelled on says bincode
//!
//! Because [`PeerTurn`](super::turn::PeerTurn) cannot be decoded from bincode,
//! and that is not a preference.
//!
//! `PeerTurn`'s [`Deserialize`](serde::Deserialize) impl is the sanitising
//! boundary: it decodes into a [`serde_json::Value`] first, cleans every string,
//! bounds the breadth and the depth, and only then builds an
//! [`AgentEvent`](crate::agent::AgentEvent). Decoding into a `Value` calls
//! `deserialize_any`, which a **self-describing** format answers and a
//! non-self-describing one cannot: bincode does not write field names or types,
//! so there is nothing for `deserialize_any` to dispatch on. A bincode body
//! would mean the sanitiser had to be rewritten variant by variant, which the
//! [`turn`](super::turn) module docs explain at length is a debt that pays out
//! as a peer's turn rendering as "something happened" forever.
//!
//! There is a second reason and it is worth writing down. bincode 1.x is
//! unmaintained (RUSTSEC-2025-0141) and this project's `deny.toml` carries an
//! exception for it whose entire justification is that it is *reached only
//! through syntect, which decodes assets compiled into the binary, never
//! untrusted input*. Putting a peer's frames through it would make that
//! sentence false, and the honest response to that would be to delete the
//! exception and take the advisory, not to quietly widen it.
//!
//! So the framing is binary — a fixed six-byte header, a hard length cap, no
//! delimiter scanning, no text parsing before the bound is applied — and the
//! body inside it is JSON. The part of "binary framing" that matters for safety
//! is the framing.
//!
//! # The cap, and where it is applied
//!
//! [`MAX_BODY`] is enforced in [`read_frame`] on the *header*, before a single
//! body byte is read and before any buffer is allocated for one. This is the
//! transport's half of the bound that [`PeerTurn`](super::turn::PeerTurn)
//! cannot provide: `PeerTurn` bounds the text, breadth and depth of an event it
//! has already decoded, by which time the bytes are in memory. A peer must not
//! be able to make this process allocate without bound, and the only place that
//! can be enforced is here.

use anyhow::{Result, anyhow, bail};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// The protocol version, and the first byte of every frame.
pub const WIRE_VERSION: u8 = 1;

/// The ALPN protocol identifier, negotiated in the TLS handshake.
///
/// Carries the same version the header does, so incompatible nodes fail to
/// connect rather than failing to understand each other. See the module docs.
pub const ALPN: &[u8] = b"wizard-mesh/1";

/// Bytes of header before the body: version, kind, and a 32-bit length.
pub const HEADER_LEN: usize = 6;

/// Longest body this node will read from a peer.
///
/// The number is derived rather than picked. One frame carries at most one
/// [`PeerEvent`](super::PeerEvent), whose text is bounded by
/// [`PeerTurn::MAX_TEXT`](super::turn::PeerTurn::MAX_TEXT) at 16,384
/// characters. JSON's worst case for a character is the six bytes of a
/// `\uXXXX` escape, which puts the text at 96 KiB even for an event made
/// entirely of control characters, and the structure around it —
/// [`MAX_ITEMS`](super::turn::PeerTurn::MAX_ITEMS) entries per container,
/// member names capped at [`PeerText::MAX_CHARS`](super::PeerText::MAX_CHARS) —
/// is what the remaining 160 KiB is for.
///
/// So a legitimate frame is comfortably inside this and a hostile one is
/// refused at the header. It is a bound on what this process will *allocate to
/// decode*, not on what it will hold: a decoded event is bounded much more
/// tightly by `PeerTurn`, and a subscription's queue is bounded by
/// [`SUBSCRIPTION_BUFFER`](super::transport::SUBSCRIPTION_BUFFER) events on top
/// of that.
pub const MAX_BODY: usize = 256 * 1024;

/// What a frame is for.
///
/// The three message kinds P2 puts in scope, each as a request and a reply, plus
/// a refusal. Delegated work (a task request, a bid, a result) is **not** here
/// and is not a gap: tier 3 is cut from this release, and a wire format that
/// carried a task nothing would run is a wire format that has to keep carrying
/// it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Kind {
    /// Liveness, outbound. Empty body.
    Ping = 1,
    /// Liveness, inbound. Empty body.
    Pong = 2,
    /// Ask a peer for its announcement. Empty body.
    WhoAreYou = 3,
    /// A node's own record: its name and its capability, as JSON.
    Announcement = 4,
    /// Ask to watch a peer's sessions. Empty body.
    Watch = 5,
    /// The subscription is open. Empty body, and sent before any event.
    ///
    /// An explicit acknowledgement rather than silence, because silence and a
    /// granted-but-idle subscription are the same bytes. Without it
    /// [`Mesh::subscribe`](super::Mesh::subscribe) would return `Ok` for a
    /// stream the far end had refused, and the refusal would surface later as
    /// a subscription that mysteriously ended — which is the shape of bug an
    /// operator cannot debug from either side.
    Watching = 6,
    /// One [`PeerEvent`](super::PeerEvent) from a watched session, as JSON.
    /// Repeats until the stream ends.
    Event = 7,
    /// Why the request was not granted, as a JSON string. A refusal a peer can
    /// read beats a stream that simply stops: the difference between "you are
    /// not trusted here" and "the network went away" is the difference between
    /// an operator fixing it and an operator guessing.
    Refused = 8,
}

impl Kind {
    /// The kind for a wire byte, or `None` for one this version does not have.
    ///
    /// `None` rather than a default: an unknown kind is a peer speaking a
    /// protocol this node does not, and guessing at it is how a format acquires
    /// behaviour nobody designed.
    pub fn from_byte(byte: u8) -> Option<Self> {
        match byte {
            1 => Some(Kind::Ping),
            2 => Some(Kind::Pong),
            3 => Some(Kind::WhoAreYou),
            4 => Some(Kind::Announcement),
            5 => Some(Kind::Watch),
            6 => Some(Kind::Watching),
            7 => Some(Kind::Event),
            8 => Some(Kind::Refused),
            _ => None,
        }
    }

    /// Lower-case label, for a log line.
    pub fn label(self) -> &'static str {
        match self {
            Kind::Ping => "ping",
            Kind::Pong => "pong",
            Kind::WhoAreYou => "who_are_you",
            Kind::Announcement => "announcement",
            Kind::Watch => "watch",
            Kind::Watching => "watching",
            Kind::Event => "event",
            Kind::Refused => "refused",
        }
    }
}

/// One decoded frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    pub kind: Kind,
    /// The body, already known to be at most [`MAX_BODY`] bytes.
    pub body: Vec<u8>,
}

impl Frame {
    /// The body as JSON.
    ///
    /// Every decoder this reaches sanitises on the way in — [`super::PeerText`]
    /// and [`super::turn::PeerTurn`] both do it inside `Deserialize` — so a
    /// value that comes back from here has already been through the boundary.
    pub fn decode<T: serde::de::DeserializeOwned>(&self) -> Result<T> {
        serde_json::from_slice(&self.body)
            .map_err(|why| anyhow!("a peer's {} frame did not decode: {why}", self.kind.label()))
    }
}

/// Encode one frame.
///
/// Errors rather than truncating when the body is too long. This runs on the
/// *sending* side, where an oversized body is a local bug rather than an
/// attack, and a truncated frame would arrive as a decode failure on somebody
/// else's machine with nothing to say about where it came from.
pub fn encode(kind: Kind, body: &[u8]) -> Result<Vec<u8>> {
    if body.len() > MAX_BODY {
        bail!(
            "a {} frame is {} bytes, past the {MAX_BODY}-byte limit",
            kind.label(),
            body.len()
        );
    }
    let mut out = Vec::with_capacity(HEADER_LEN + body.len());
    out.push(WIRE_VERSION);
    out.push(kind as u8);
    out.extend_from_slice(&(body.len() as u32).to_be_bytes());
    out.extend_from_slice(body);
    Ok(out)
}

/// Encode a frame whose body is JSON.
pub fn encode_json<T: serde::Serialize>(kind: Kind, value: &T) -> Result<Vec<u8>> {
    let body = serde_json::to_vec(value)
        .map_err(|why| anyhow!("a {} frame did not encode: {why}", kind.label()))?;
    encode(kind, &body)
}

/// Write one frame.
pub async fn write_frame<W: AsyncWrite + Unpin>(
    writer: &mut W,
    kind: Kind,
    body: &[u8],
) -> Result<()> {
    let frame = encode(kind, body)?;
    writer.write_all(&frame).await?;
    Ok(())
}

/// Write one frame whose body is JSON.
pub async fn write_json<W: AsyncWrite + Unpin, T: serde::Serialize>(
    writer: &mut W,
    kind: Kind,
    value: &T,
) -> Result<()> {
    let frame = encode_json(kind, value)?;
    writer.write_all(&frame).await?;
    Ok(())
}

/// Read one frame, or `None` when the stream ended cleanly between frames.
///
/// The bound is applied here and it is applied to the *header*. Six bytes are
/// read into a fixed array on the stack; the version, the kind and the length
/// are all checked against it; and only then is a buffer allocated for the
/// body. A peer that claims a four-gigabyte body gets an error message, not
/// four gigabytes of this process's memory.
///
/// `None` is only for a stream that ended with nothing in hand. A stream that
/// ends *inside* a frame is an error, because "the peer stopped talking" and
/// "the peer sent half a message" are different facts and a reader that
/// conflated them would silently drop the tail of every truncated stream.
pub async fn read_frame<R: AsyncRead + Unpin>(reader: &mut R) -> Result<Option<Frame>> {
    let mut header = [0u8; HEADER_LEN];
    let mut filled = 0usize;
    while filled < HEADER_LEN {
        let read = reader.read(&mut header[filled..]).await?;
        if read == 0 {
            if filled == 0 {
                return Ok(None);
            }
            bail!("a peer's stream ended {filled} bytes into a {HEADER_LEN}-byte frame header");
        }
        filled += read;
    }

    if header[0] != WIRE_VERSION {
        bail!(
            "a peer sent wire version {}; this node speaks version {WIRE_VERSION}",
            header[0]
        );
    }
    let kind = Kind::from_byte(header[1]).ok_or_else(|| {
        anyhow!(
            "a peer sent frame kind {}, which version {WIRE_VERSION} of this protocol does not have",
            header[1]
        )
    })?;
    let len = u32::from_be_bytes([header[2], header[3], header[4], header[5]]) as usize;
    if len > MAX_BODY {
        bail!(
            "a peer's {} frame claims {len} bytes, past the {MAX_BODY}-byte limit",
            kind.label()
        );
    }

    let mut body = vec![0u8; len];
    reader.read_exact(&mut body).await.map_err(|why| {
        anyhow!(
            "a peer's {} frame promised {len} bytes and the stream ended first: {why}",
            kind.label()
        )
    })?;
    Ok(Some(Frame { kind, body }))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A frame written into a buffer and read back out of it, which is the
    /// only round trip a wire format has.
    async fn round_trip(kind: Kind, body: &[u8]) -> Frame {
        let mut buffer = Vec::new();
        write_frame(&mut buffer, kind, body).await.expect("write");
        let mut cursor = std::io::Cursor::new(buffer);
        read_frame(&mut cursor)
            .await
            .expect("read")
            .expect("a frame")
    }

    #[tokio::test]
    async fn every_kind_round_trips() {
        for kind in [
            Kind::Ping,
            Kind::Pong,
            Kind::WhoAreYou,
            Kind::Announcement,
            Kind::Watch,
            Kind::Watching,
            Kind::Event,
            Kind::Refused,
        ] {
            let body = kind.label().as_bytes();
            let frame = round_trip(kind, body).await;
            assert_eq!(frame.kind, kind, "{}", kind.label());
            assert_eq!(frame.body, body, "{}", kind.label());
            // And the byte is stable, because a discriminant that moves is a
            // protocol change that looks like a refactor.
            assert_eq!(Kind::from_byte(kind as u8), Some(kind));
        }
        // The discriminants themselves, written out, so reordering the enum
        // fails here rather than on somebody's LAN.
        assert_eq!(
            [
                Kind::Ping as u8,
                Kind::Pong as u8,
                Kind::WhoAreYou as u8,
                Kind::Announcement as u8,
                Kind::Watch as u8,
                Kind::Watching as u8,
                Kind::Event as u8,
                Kind::Refused as u8,
            ],
            [1, 2, 3, 4, 5, 6, 7, 8]
        );
    }

    #[tokio::test]
    async fn the_version_is_the_first_byte_and_a_mismatch_is_refused() {
        let mut buffer = Vec::new();
        write_frame(&mut buffer, Kind::Ping, b"")
            .await
            .expect("write");
        assert_eq!(buffer[0], WIRE_VERSION);
        assert_eq!(buffer.len(), HEADER_LEN);

        buffer[0] = WIRE_VERSION + 1;
        let mut cursor = std::io::Cursor::new(buffer);
        let err = read_frame(&mut cursor).await.expect_err("wrong version");
        let message = format!("{err:#}");
        assert!(
            message.contains(&(WIRE_VERSION + 1).to_string()),
            "{message}"
        );
        assert!(message.contains(&WIRE_VERSION.to_string()), "{message}");
    }

    #[tokio::test]
    async fn an_unknown_kind_is_refused_rather_than_guessed_at() {
        let mut buffer = Vec::new();
        write_frame(&mut buffer, Kind::Ping, b"")
            .await
            .expect("write");
        buffer[1] = 200;
        let mut cursor = std::io::Cursor::new(buffer);
        let err = read_frame(&mut cursor).await.expect_err("unknown kind");
        assert!(format!("{err:#}").contains("200"), "{err:#}");
        assert_eq!(Kind::from_byte(0), None);
        assert_eq!(Kind::from_byte(9), None);
    }

    #[tokio::test]
    async fn an_oversized_body_is_refused_at_the_header_without_allocating() {
        // The obligation this module exists for: the cap is applied to the
        // length field, not to the bytes that follow it. There are no bytes
        // following it here at all — the frame is six bytes long and claims a
        // gigabyte — so a reader that allocated first would allocate a gigabyte
        // for a six-byte input.
        let mut header = Vec::new();
        header.push(WIRE_VERSION);
        header.push(Kind::Event as u8);
        header.extend_from_slice(&(1_000_000_000u32).to_be_bytes());
        let mut cursor = std::io::Cursor::new(header);
        let err = read_frame(&mut cursor).await.expect_err("too big");
        let message = format!("{err:#}");
        assert!(message.contains("1000000000"), "{message}");
        assert!(message.contains(&MAX_BODY.to_string()), "{message}");

        // The sending side refuses too, so an oversized body is a local error
        // rather than somebody else's decode failure.
        let err = encode(Kind::Event, &vec![0u8; MAX_BODY + 1]).expect_err("too big");
        assert!(format!("{err:#}").contains("past the"), "{err:#}");
    }

    #[tokio::test]
    async fn a_body_exactly_at_the_cap_still_crosses() {
        // A cap that is off by one is a cap that rejects the largest legitimate
        // message, and the failure would only show up under load.
        let body = vec![0x41u8; MAX_BODY];
        let frame = round_trip(Kind::Event, &body).await;
        assert_eq!(frame.body.len(), MAX_BODY);
    }

    #[tokio::test]
    async fn the_cap_has_room_for_the_largest_event_the_turn_boundary_admits() {
        // The derivation in `MAX_BODY`'s docs, checked rather than asserted in
        // prose: the worst-case JSON encoding of a maximal `PeerTurn`'s text
        // has to fit, or the bound would refuse events the layer above allows.
        let worst_case_text = super::super::turn::PeerTurn::MAX_TEXT * 6;
        assert!(
            worst_case_text < MAX_BODY,
            "{worst_case_text} bytes of escaped text does not fit in {MAX_BODY}"
        );
    }

    #[tokio::test]
    async fn a_clean_end_is_none_and_a_torn_one_is_an_error() {
        // Nothing at all: the peer finished and closed.
        let mut empty = std::io::Cursor::new(Vec::new());
        assert!(read_frame(&mut empty).await.expect("clean end").is_none());

        // Half a header.
        let mut torn = std::io::Cursor::new(vec![WIRE_VERSION, Kind::Ping as u8]);
        let err = read_frame(&mut torn).await.expect_err("torn header");
        assert!(format!("{err:#}").contains("frame header"), "{err:#}");

        // A whole header and half a body, which is the case a reader that
        // returned `None` on any short read would silently swallow.
        let mut short = Vec::new();
        short.push(WIRE_VERSION);
        short.push(Kind::Event as u8);
        short.extend_from_slice(&16u32.to_be_bytes());
        short.extend_from_slice(b"only eight");
        let mut cursor = std::io::Cursor::new(short);
        let err = read_frame(&mut cursor).await.expect_err("torn body");
        assert!(format!("{err:#}").contains("stream ended first"), "{err:#}");
    }

    #[tokio::test]
    async fn several_frames_share_one_stream_without_a_delimiter() {
        // The length prefix is what makes this work: there is no sentinel to
        // scan for, so a body may contain any bytes at all, including the ones
        // a delimiter-based format would have to escape.
        let mut buffer = Vec::new();
        for i in 0..4u8 {
            write_frame(
                &mut buffer,
                Kind::Event,
                &[WIRE_VERSION, Kind::Event as u8, i],
            )
            .await
            .expect("write");
        }
        let mut cursor = std::io::Cursor::new(buffer);
        for i in 0..4u8 {
            let frame = read_frame(&mut cursor)
                .await
                .expect("read")
                .expect("a frame");
            assert_eq!(frame.body[2], i);
        }
        assert!(read_frame(&mut cursor).await.expect("clean end").is_none());
    }

    #[tokio::test]
    async fn a_json_body_round_trips_through_the_sanitising_decoders() {
        use crate::plugins::mesh::PeerText;

        let mut buffer = Vec::new();
        write_json(&mut buffer, Kind::Refused, &"not\u{1b}[2J trusted")
            .await
            .expect("write");
        let mut cursor = std::io::Cursor::new(buffer);
        let frame = read_frame(&mut cursor)
            .await
            .expect("read")
            .expect("a frame");
        let reason: PeerText = frame.decode().expect("decode");
        assert!(!reason.as_str().contains('\u{1b}'), "{reason:?}");
        assert!(reason.as_str().contains("trusted"), "{reason:?}");

        // And a body that is not the type it claims to be is an error naming
        // the frame kind, not a panic.
        let frame = Frame {
            kind: Kind::Announcement,
            body: b"{".to_vec(),
        };
        let err = frame.decode::<PeerText>().expect_err("bad json");
        assert!(format!("{err:#}").contains("announcement"), "{err:#}");
    }
}
