//! The crate's single error type.

use thiserror::Error;

/// Everything that can go wrong while encoding, decoding or negotiating.
///
/// Errors carry plain values and text rather than the MessagePack library's
/// own error types, so callers do not depend on that library and errors can
/// be cloned and compared (tests rely on both).
///
/// What a caller should do depends on the variant:
///
/// - [`FrameTooLarge`](Self::FrameTooLarge) from [`FrameDecoder`] means the
///   stream can no longer be split into frames: close the connection.
/// - [`Incompatible`](Self::Incompatible) means the peer runs a Hop build
///   this one cannot talk to: close the connection and tell the user to
///   update.
/// - Anything else concerns one message. An outgoing message cannot be sent
///   as it is. An incoming frame or datagram was bad but has been consumed,
///   so the caller may skip it and carry on.
///
/// [`FrameDecoder`]: crate::FrameDecoder
#[derive(Clone, Debug, PartialEq, Eq, Error)]
pub enum ProtoError {
    /// A stream frame body is longer than [`MAX_FRAME_LEN`].
    ///
    /// [`MAX_FRAME_LEN`]: crate::MAX_FRAME_LEN
    #[error("frame body of {len} bytes is over the {max}-byte limit")]
    FrameTooLarge {
        /// The body length that was declared (decoding) or produced (encoding).
        len: usize,
        /// The limit, [`MAX_FRAME_LEN`](crate::MAX_FRAME_LEN).
        max: usize,
    },

    /// A datagram is longer than [`MAX_DATAGRAM_LEN`].
    ///
    /// [`MAX_DATAGRAM_LEN`]: crate::MAX_DATAGRAM_LEN
    #[error("datagram of {len} bytes is over the {max}-byte limit")]
    DatagramTooLarge {
        /// The whole datagram's length, kind included.
        len: usize,
        /// The limit, [`MAX_DATAGRAM_LEN`](crate::MAX_DATAGRAM_LEN).
        max: usize,
    },

    /// A datagram is too short to hold even its two-byte kind.
    #[error("datagram of {len} bytes is too short to hold a message kind")]
    DatagramTooShort {
        /// The datagram's length.
        len: usize,
    },

    /// A `ClipboardText` message carries more text than
    /// [`MAX_CLIPBOARD_TEXT`].
    ///
    /// [`MAX_CLIPBOARD_TEXT`]: crate::MAX_CLIPBOARD_TEXT
    #[error("clipboard text of {len} bytes is over the {max}-byte limit")]
    ClipboardTooLarge {
        /// The text's length in UTF-8 bytes.
        len: usize,
        /// The limit, [`MAX_CLIPBOARD_TEXT`](crate::MAX_CLIPBOARD_TEXT).
        max: usize,
    },

    /// A message could not be turned into MessagePack. Hop's own message
    /// types always encode, so this points to a bug.
    #[error("cannot encode a message of kind {kind}: {reason}")]
    Encode {
        /// The kind of the message.
        kind: u16,
        /// What the MessagePack encoder reported.
        reason: String,
    },

    /// A body is not valid MessagePack for its kind: a required field is
    /// missing, a value has the wrong type, an enum variant is unknown,
    /// nesting is too deep, or bytes follow the message.
    #[error("malformed body for message kind {kind}: {reason}")]
    Decode {
        /// The kind the body arrived under.
        kind: u16,
        /// What is wrong with it. It can quote the peer's bytes, so it is
        /// cut to a few hundred bytes and its control characters are
        /// escaped, which keeps it safe to log.
        reason: String,
    },

    /// The two devices speak different major protocol versions.
    #[error(
        "incompatible protocol versions: this device speaks {local_major}.{local_minor}, \
         the peer speaks {remote_major}.{remote_minor} (update Hop on both devices)"
    )]
    Incompatible {
        /// This device's major version.
        local_major: u16,
        /// This device's minor version.
        local_minor: u16,
        /// The peer's major version.
        remote_major: u16,
        /// The peer's minor version.
        remote_minor: u16,
    },
}
