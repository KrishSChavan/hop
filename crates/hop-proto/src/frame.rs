//! Stream framing: `kind: u16 BE | len: u32 BE | body`.

use crate::ProtoError;
use crate::message::{Decoded, Message, decode_message};

/// Largest body, in bytes, that one stream frame may carry: 1 MiB plus
/// 64 KiB.
///
/// The extra 64 KiB is room for the message around the largest clipboard
/// text ([`MAX_CLIPBOARD_TEXT`](crate::MAX_CLIPBOARD_TEXT), 1 MiB), so text
/// that is within its own limit always fits in a frame.
pub const MAX_FRAME_LEN: usize = 1024 * 1024 + 64 * 1024;

/// Bytes before the body: two for the kind, four for the length.
const HEADER_LEN: usize = 6;

/// Encodes `message` as one stream frame.
///
/// Fails if a clipboard text is over its limit or the body would be longer
/// than [`MAX_FRAME_LEN`].
pub fn encode_frame(message: &Message) -> Result<Vec<u8>, ProtoError> {
    let mut frame = Vec::with_capacity(64);
    frame.extend_from_slice(&message.kind().to_be_bytes());
    // The length is not known until the body is written, so reserve its
    // place and fill it in afterwards. This avoids copying the body.
    frame.extend_from_slice(&[0; 4]);
    message.write_body(&mut frame)?;

    let body_len = frame.len().saturating_sub(HEADER_LEN);
    let too_large = ProtoError::FrameTooLarge {
        len: body_len,
        max: MAX_FRAME_LEN,
    };
    if body_len > MAX_FRAME_LEN {
        return Err(too_large);
    }
    let len_bytes = u32::try_from(body_len)
        .map_err(|_| too_large)?
        .to_be_bytes();
    if let Some(len_field) = frame.get_mut(2..HEADER_LEN) {
        len_field.copy_from_slice(&len_bytes);
    }
    Ok(frame)
}

/// Reassembles stream frames from the pieces a stream delivers.
///
/// [`push`](Self::push) whatever bytes arrive, in pieces of any size, then
/// [`pop`](Self::pop) complete frames until it returns `None`:
///
/// ```
/// # use hop_proto::{FrameDecoder, ProtoError};
/// # fn handle(_: hop_proto::Decoded) {}
/// # fn on_bytes(decoder: &mut FrameDecoder, chunk: &[u8]) -> Result<(), ProtoError> {
/// decoder.push(chunk)?; // an error here means: close the connection
/// while let Some(frame) = decoder.pop() {
///     match frame {
///         Ok(decoded) => handle(decoded),
///         Err(err) => eprintln!("skipping a bad frame: {err}"),
///     }
/// }
/// # Ok(())
/// # }
/// ```
///
/// A frame whose header declares a body longer than [`MAX_FRAME_LEN`] is
/// rejected by `push` as soon as its six-byte header arrives, and none of its
/// body is kept. The stream cannot be split into frames after that, so the
/// decoder stays failed: every later `push` returns the same error and drops
/// its bytes, and `pop` only returns the complete frames that arrived before
/// the oversized header.
///
/// A frame whose body does not decode is consumed and returned as an error
/// by `pop`, and the frames after it decode normally.
#[derive(Debug, Default)]
pub struct FrameDecoder {
    /// Received bytes. Everything before `read` has been popped.
    buf: Vec<u8>,
    /// Start of the first frame not yet popped.
    read: usize,
    /// Start of the first frame whose header `push` has not yet seen in
    /// full together with its body. Every frame between `read` and `scanned`
    /// is complete and has a valid length.
    scanned: usize,
    /// Set once a header declared an oversized body.
    failed: Option<ProtoError>,
}

impl FrameDecoder {
    /// Creates an empty decoder.
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds bytes received from the stream.
    ///
    /// Returns [`ProtoError::FrameTooLarge`] if a frame header in the bytes
    /// received so far declares a body longer than [`MAX_FRAME_LEN`]; see the
    /// type's documentation.
    pub fn push(&mut self, bytes: &[u8]) -> Result<(), ProtoError> {
        if let Some(err) = &self.failed {
            return Err(err.clone());
        }
        // Drop popped frames before growing the buffer, so it only ever
        // holds unread bytes and old frames are not copied around forever.
        if self.read > 0 {
            self.buf.drain(..self.read);
            self.scanned = self.scanned.saturating_sub(self.read);
            self.read = 0;
        }
        self.buf.extend_from_slice(bytes);
        self.check_headers()
    }

    /// Returns the next complete frame, decoded, or `None` if no complete
    /// frame has arrived yet.
    ///
    /// A frame of a kind this build does not know comes back as
    /// [`Decoded::Unknown`]. A frame whose body is malformed or over a
    /// content limit comes back as an error; it has been consumed, so the
    /// caller may skip it and keep popping.
    pub fn pop(&mut self) -> Option<Result<Decoded, ProtoError>> {
        if self.read >= self.scanned {
            return None;
        }
        let (kind, body_len) = parse_header(self.buf.get(self.read..)?)?;
        let body_start = self.read.checked_add(HEADER_LEN)?;
        let body_end = body_start.checked_add(body_len)?;
        let body = self.buf.get(body_start..body_end)?;
        let decoded = decode_message(kind, body);
        self.read = body_end;
        if self.read == self.buf.len() {
            // Everything has been popped: reuse the buffer from the start.
            self.buf.clear();
            self.read = 0;
            self.scanned = 0;
        }
        Some(decoded)
    }

    /// How many received bytes the decoder is holding: complete frames not
    /// yet popped plus the start of an incomplete one.
    pub fn buffered_len(&self) -> usize {
        self.buf.len().saturating_sub(self.read)
    }

    /// Moves `scanned` past every complete frame and rejects the first
    /// header that declares an oversized body.
    fn check_headers(&mut self) -> Result<(), ProtoError> {
        loop {
            let Some((_, body_len)) = self.buf.get(self.scanned..).and_then(parse_header) else {
                return Ok(());
            };
            if body_len > MAX_FRAME_LEN {
                // Keep the complete frames before the bad header and drop
                // the header and whatever part of the body came with it.
                self.buf.truncate(self.scanned);
                let err = ProtoError::FrameTooLarge {
                    len: body_len,
                    max: MAX_FRAME_LEN,
                };
                self.failed = Some(err.clone());
                return Err(err);
            }
            let frame_end = self.scanned + HEADER_LEN + body_len;
            if frame_end > self.buf.len() {
                return Ok(());
            }
            self.scanned = frame_end;
        }
    }
}

/// Reads the kind and body length from the start of `bytes`, if a whole
/// header is there.
fn parse_header(bytes: &[u8]) -> Option<(u16, usize)> {
    let &[k0, k1, l0, l1, l2, l3] = bytes.first_chunk::<HEADER_LEN>()?;
    let kind = u16::from_be_bytes([k0, k1]);
    // A u32 always fits in usize on the 32- and 64-bit targets Hop builds
    // for; saturating keeps an impossible overflow rejected, not wrapped.
    let body_len = usize::try_from(u32::from_be_bytes([l0, l1, l2, l3])).unwrap_or(usize::MAX);
    Some((kind, body_len))
}

#[cfg(test)]
mod tests {
    use hop_types::CoreMessage;

    use super::*;

    fn heartbeat(seq: u64) -> Vec<u8> {
        encode_frame(&Message::Core(CoreMessage::Heartbeat { seq })).expect("encodes")
    }

    #[test]
    fn popped_bytes_are_dropped_on_the_next_push() {
        let mut stream = heartbeat(1);
        let second = heartbeat(2);
        stream.extend_from_slice(&second[..4]);
        let mut decoder = FrameDecoder::new();
        decoder.push(&stream).expect("valid");
        assert!(matches!(decoder.pop(), Some(Ok(_))));
        assert_eq!(decoder.buf.len(), stream.len(), "nothing is moved by pop");

        decoder.push(&second[4..]).expect("valid");
        assert_eq!(decoder.buf, second, "the popped frame is gone");
        assert_eq!((decoder.read, decoder.scanned), (0, second.len()));
        assert!(matches!(decoder.pop(), Some(Ok(_))));
        assert!(decoder.buf.is_empty());
        assert_eq!((decoder.read, decoder.scanned), (0, 0));
    }

    #[test]
    fn headers_are_checked_once_each_frame_is_complete() {
        let mut decoder = FrameDecoder::new();
        let frame = heartbeat(1);
        decoder.push(&frame[..HEADER_LEN]).expect("valid");
        assert_eq!(decoder.scanned, 0, "the body is still missing");
        decoder.push(&frame[HEADER_LEN..]).expect("valid");
        assert_eq!(decoder.scanned, frame.len());
    }
}
