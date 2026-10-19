//! Hop's wire protocol: how messages between two Hop devices become bytes
//! and back. See docs/ARCHITECTURE.md ("Wire protocol").
//!
//! This crate is pure logic. It never touches the network or the OS: the
//! transport hands it the bytes it received and sends the bytes it returns.
//!
//! # Messages and kinds
//!
//! Every [`Message`] travels under a *kind* number that says how to read its
//! body:
//!
//! | Kind | Message | Carries |
//! |---|---|---|
//! | [`KIND_HELLO`] = 1 | [`Message::Hello`] | Versions, identity, displays and capabilities. The first message on a connection |
//! | [`KIND_CORE`] = 2 | [`Message::Core`] | A [`CoreMessage`]: the control hand-off and the clipboard |
//! | [`KIND_FILE`] = 3 | [`Message::File`] | A [`FileMessage`]: a file transfer's offer, answer or outcome, as stream frames on the control stream, to peers that negotiated `files` |
//! | [`KIND_NUDGE`] = 4 | [`Message::Nudge`] | Nothing (an empty body), as a datagram only: it wakes the peer's Wi-Fi radio, and every build ignores it |
//!
//! A kind this build does not know decodes to [`Decoded::Unknown`], so the
//! caller can skip it. Builds from before kinds 3 and 4 skip those the same
//! way.
//!
//! # Handshake
//!
//! Each side sends a [`Hello`] (see [`Hello::new`]) as its first message,
//! then passes its own and the peer's to [`negotiate`]. That checks that the
//! major versions match and settles on the minor version and the
//! capabilities both sides support ([`Negotiated`]).
//!
//! A `Hello` that does not decode but names a different major version comes
//! back as [`ProtoError::Incompatible`] rather than as a malformed body, so
//! the user is told to update Hop instead of seeing a parse error. For that
//! to keep working, `Hello`'s `proto_major` and `proto_minor` fields must
//! keep their names and types even across major versions.
//!
//! # Framing
//!
//! On a stream, each message is one frame:
//!
//! ```text
//! kind: u16, big-endian | len: u32, big-endian | body: len bytes
//! ```
//!
//! [`encode_frame`] builds a frame, and [`FrameDecoder`] reassembles frames
//! from whatever pieces the stream delivers.
//!
//! In a datagram, the kind is followed by the body, which runs to the end of
//! the datagram:
//!
//! ```text
//! kind: u16, big-endian | body
//! ```
//!
//! See [`encode_datagram`] and [`decode_datagram`]. Datagrams can be lost or
//! reordered, so Hop uses them only for pointer motion
//! ([`CoreMessage::Motion`]), where the latest message wins, and for
//! [`Message::Nudge`], which carries nothing.
//!
//! File data does not travel in frames: after an accepted
//! [`FileMessage::Answer`], the sender writes it on a unidirectional stream
//! of its own, as the [`FileMessage`] docs describe.
//!
//! # Body
//!
//! The body is MessagePack with named fields, as `rmp_serde::to_vec_named`
//! writes it. A struct is a map from field name to value, a unit enum variant
//! is its name as a string, and any other enum variant is a one-entry map
//! from its name to its contents. A body holds exactly one value (trailing
//! bytes are an error), and its maps and arrays nest at most 32 deep, far
//! more than any Hop message needs.
//!
//! # Limits
//!
//! | Limit | Value | Measures |
//! |---|---|---|
//! | [`MAX_FRAME_LEN`] | 1 MiB + 64 KiB | The body of one stream frame |
//! | [`MAX_DATAGRAM_LEN`] | 1200 bytes | A whole datagram, kind included |
//! | [`MAX_CLIPBOARD_TEXT`] | 1 MiB | The UTF-8 text of one `ClipboardText` |
//! | [`MAX_OFFER_FILES`] | 1,000 | The files listed in one [`FileMessage::Offer`] |
//! | [`MAX_FILE_NAME_BYTES`] | 1,024 bytes | The UTF-8 name of one file in an offer |
//!
//! Oversized input is rejected, never truncated. Encoding checks the same
//! limits, so Hop never sends what its peer would refuse. An offer at both
//! file limits still fits in one frame.
//!
//! # Evolving the protocol
//!
//! Devices running different Hop builds must keep working together, so every
//! change to a message follows these rules. The crate's tests check rule 1 in
//! both directions.
//!
//! 1. **Add a field with `#[serde(default)]`.** An older peer skips fields it
//!    does not know, and a newer peer reading an older message gets the
//!    default.
//! 2. **Never remove, rename or retype a field, or change its meaning.** Stop
//!    using it instead.
//! 3. **A new enum variant is not backward compatible.** An older peer cannot
//!    decode a message that contains a variant it does not know. Send a new
//!    [`CoreMessage`] or [`FileMessage`] variant, or a new value of an enum
//!    inside one (such as [`FileRefusal`]), only to a peer that negotiated
//!    the capability that introduced it. [`Hello`] is decoded before any
//!    negotiation, so the enums inside it (such as `Os`) must not gain
//!    variants within a major version.
//! 4. **A new message kind takes the next unused number.** Older peers skip
//!    it.
//! 5. **Versions.** A new feature adds a capability or raises the version of
//!    one. A peer that supports version N must still speak every lower
//!    version, because [`negotiate`] settles on the lower of the two. Raise
//!    [`PROTO_MINOR`] for other backward-compatible changes, and
//!    [`PROTO_MAJOR`] only for a change older peers cannot survive: peers with
//!    different majors refuse to talk.
//!
//! # Example
//!
//! ```
//! use hop_proto::{Decoded, FrameDecoder, Message, encode_frame};
//! use hop_types::CoreMessage;
//!
//! let heartbeat = Message::Core(CoreMessage::Heartbeat { seq: 7 });
//! let bytes = encode_frame(&heartbeat)?;
//!
//! // A stream may deliver those bytes in any pieces.
//! let (first, rest) = bytes.split_at(4);
//! let mut decoder = FrameDecoder::new();
//! decoder.push(first)?;
//! assert!(decoder.pop().is_none(), "the frame is not complete yet");
//! decoder.push(rest)?;
//! assert_eq!(decoder.pop().transpose()?, Some(Decoded::Message(heartbeat)));
//! # Ok::<(), hop_proto::ProtoError>(())
//! ```
//!
//! [`CoreMessage`]: hop_types::CoreMessage
//! [`CoreMessage::Motion`]: hop_types::CoreMessage::Motion

#![forbid(unsafe_code)]
#![warn(missing_docs)]
// Bytes come from another machine, so the library code must not be able to
// panic on them. Tests may unwrap freely.
#![cfg_attr(
    not(test),
    warn(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::indexing_slicing
    )
)]

mod datagram;
mod error;
mod file;
mod frame;
mod hello;
mod message;

pub use datagram::{MAX_DATAGRAM_LEN, decode_datagram, encode_datagram};
pub use error::ProtoError;
pub use file::{
    FileEntry, FileFailure, FileMessage, FileRefusal, MAX_FILE_NAME_BYTES, MAX_OFFER_FILES,
};
pub use frame::{FrameDecoder, MAX_FRAME_LEN, encode_frame};
pub use hello::{
    Capability, Hello, Negotiated, PROTO_MAJOR, PROTO_MINOR, caps, default_capabilities, negotiate,
};
pub use message::{
    Decoded, KIND_CORE, KIND_FILE, KIND_HELLO, KIND_NUDGE, MAX_CLIPBOARD_TEXT, Message,
};
