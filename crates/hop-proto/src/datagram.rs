//! Datagram encoding: `kind: u16 BE | body`, the body running to the end.

use crate::ProtoError;
use crate::message::{Decoded, Message, decode_message};

/// Largest datagram, in bytes, kind included: 1200.
///
/// 1200 bytes is the smallest UDP payload QUIC requires every path to carry
/// (RFC 9000). QUIC's own headers take part of each packet, so the transport
/// should still check its connection's current datagram limit. A `Motion`
/// message needs well under 100 bytes, so it fits either way.
pub const MAX_DATAGRAM_LEN: usize = 1200;

/// Bytes before the body: the kind.
const KIND_LEN: usize = 2;

/// Encodes `message` as one datagram.
///
/// Fails if the datagram would be longer than [`MAX_DATAGRAM_LEN`]: send
/// such a message as a stream frame instead.
pub fn encode_datagram(message: &Message) -> Result<Vec<u8>, ProtoError> {
    let mut datagram = Vec::with_capacity(64);
    datagram.extend_from_slice(&message.kind().to_be_bytes());
    message.write_body(&mut datagram)?;
    if datagram.len() > MAX_DATAGRAM_LEN {
        return Err(ProtoError::DatagramTooLarge {
            len: datagram.len(),
            max: MAX_DATAGRAM_LEN,
        });
    }
    Ok(datagram)
}

/// Decodes one datagram.
///
/// A datagram of a kind this build does not know comes back as
/// [`Decoded::Unknown`].
pub fn decode_datagram(datagram: &[u8]) -> Result<Decoded, ProtoError> {
    if datagram.len() > MAX_DATAGRAM_LEN {
        return Err(ProtoError::DatagramTooLarge {
            len: datagram.len(),
            max: MAX_DATAGRAM_LEN,
        });
    }
    let Some((&kind, body)) = datagram.split_first_chunk::<KIND_LEN>() else {
        return Err(ProtoError::DatagramTooShort {
            len: datagram.len(),
        });
    };
    decode_message(u16::from_be_bytes(kind), body)
}
