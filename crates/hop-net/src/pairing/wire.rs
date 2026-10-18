//! The `hop-pair/1` messages and the key confirmation.
//!
//! Each message is one frame on the single bidirectional stream the joiner
//! opens:
//!
//! ```text
//! kind: u8 | len: u16, big-endian | body: len bytes      (at most 1 KiB in all)
//! ```
//!
//! | Kind | Message | Body |
//! |---|---|---|
//! | 1 | SPAKE2 | the 33-byte SPAKE2 message |
//! | 2 | Confirm | 32-byte HMAC, 1-byte OS, then the name in UTF-8 |
//! | 3 | Accepted | empty: the host's user said yes and the host saved |

use hmac::{Hmac, Mac};
use hop_types::Os;
use quinn::{RecvStream, SendStream};
use sha2::Sha256;
use zeroize::Zeroizing;

use super::{PairError, PairInfo};
use crate::CloseCode;
use crate::sanitize::display_bytes;

/// The largest pairing frame, header included.
const MAX_FRAME: usize = 1024;

/// Header bytes: the kind and the length.
const HEADER: usize = 3;

/// A SPAKE2 message for the Ed25519 group: a side byte and a point.
const SPAKE_MESSAGE_LEN: usize = 33;

/// An HMAC-SHA256 tag.
const MAC_LEN: usize = 32;

/// The TLS exporter label of RFC 9266, the TLS 1.3 channel binding.
const EXPORTER_LABEL: &[u8] = b"EXPORTER-Channel-Binding";

/// The joiner's (side A's) confirmation label.
pub(crate) const LABEL_JOINER: &[u8] = b"hop pair confirm A";

/// The host's (side B's) confirmation label.
pub(crate) const LABEL_HOST: &[u8] = b"hop pair confirm B";

/// The kinds of pairing frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Kind {
    Spake = 1,
    Confirm = 2,
    Accepted = 3,
}

/// A pairing step that failed: the error for the caller, and the close
/// code to send (none when the connection is already gone).
#[derive(Debug)]
pub(crate) struct Failure {
    pub(crate) error: PairError,
    pub(crate) close: Option<CloseCode>,
}

impl Failure {
    pub(crate) fn wrong_code() -> Failure {
        Failure {
            error: PairError::WrongCode,
            close: Some(CloseCode::WrongCode),
        }
    }

    fn protocol(why: &str) -> Failure {
        Failure {
            error: PairError::Protocol(why.to_owned()),
            close: Some(CloseCode::Protocol),
        }
    }

    pub(crate) fn connection(error: quinn::ConnectionError) -> Failure {
        let error = match error {
            quinn::ConnectionError::ApplicationClosed(close) => {
                error_for_close(close.error_code.into_inner())
            }
            other => PairError::Connection(display_bytes(other.to_string().as_bytes(), 80)),
        };
        Failure { error, close: None }
    }
}

/// The error a joiner or host reports when the other side closed with
/// `code`.
pub(crate) fn error_for_close(code: u64) -> PairError {
    match CloseCode::from_code(code) {
        Some(CloseCode::Busy) => PairError::Busy,
        Some(CloseCode::WrongCode) => PairError::WrongCode,
        Some(CloseCode::Expired) => PairError::Expired,
        Some(CloseCode::Declined) => PairError::Declined,
        Some(CloseCode::Protocol) => {
            PairError::Protocol("the other side refused a message".to_owned())
        }
        _ => PairError::Connection(format!("closed by the other side (code {code})")),
    }
}

/// Writes one frame.
pub(crate) async fn write_frame(
    send: &mut SendStream,
    kind: Kind,
    body: &[u8],
) -> Result<(), Failure> {
    let len = u16::try_from(body.len())
        .ok()
        .filter(|len| usize::from(*len) + HEADER <= MAX_FRAME)
        .ok_or_else(|| Failure::protocol("message too large"))?;
    let mut frame = Zeroizing::new(Vec::with_capacity(HEADER + body.len()));
    frame.push(kind as u8);
    frame.extend_from_slice(&len.to_be_bytes());
    frame.extend_from_slice(body);
    send.write_all(&frame).await.map_err(|error| match error {
        quinn::WriteError::ConnectionLost(error) => Failure::connection(error),
        _ => Failure::protocol("the stream was closed"),
    })
}

/// Reads one frame, which must be of `kind`. The length is checked from
/// the header before the body is read. Bodies are wiped when dropped.
pub(crate) async fn read_frame(
    recv: &mut RecvStream,
    kind: Kind,
) -> Result<Zeroizing<Vec<u8>>, Failure> {
    let mut header = [0u8; HEADER];
    recv.read_exact(&mut header).await.map_err(read_failure)?;
    let [found, l0, l1] = header;
    if found != kind as u8 {
        return Err(Failure::protocol("unexpected message"));
    }
    let len = usize::from(u16::from_be_bytes([l0, l1]));
    if len + HEADER > MAX_FRAME {
        return Err(Failure::protocol("message too large"));
    }
    let mut body = Zeroizing::new(vec![0u8; len]);
    recv.read_exact(&mut body).await.map_err(read_failure)?;
    if kind == Kind::Spake && body.len() != SPAKE_MESSAGE_LEN {
        return Err(Failure::protocol("malformed SPAKE2 message"));
    }
    Ok(body)
}

fn read_failure(error: quinn::ReadExactError) -> Failure {
    match error {
        quinn::ReadExactError::ReadError(quinn::ReadError::ConnectionLost(error)) => {
            Failure::connection(error)
        }
        _ => Failure::protocol("the stream ended early"),
    }
}

/// E: 32 bytes of this connection's TLS exporter, label
/// `EXPORTER-Channel-Binding`, empty context (RFC 9266). Both sides of one
/// TLS session get the same value; two sessions (a relay's) never do.
pub(crate) fn channel_binding(conn: &quinn::Connection) -> Result<Zeroizing<[u8; 32]>, Failure> {
    let mut binding = Zeroizing::new([0u8; 32]);
    conn.export_keying_material(binding.as_mut(), EXPORTER_LABEL, b"")
        .map_err(|_| Failure::protocol("no TLS exporter"))?;
    Ok(binding)
}

/// `HMAC-SHA256(key, label || binding)`.
pub(crate) fn confirmation(
    key: &[u8],
    label: &[u8],
    binding: &[u8; 32],
) -> Zeroizing<[u8; MAC_LEN]> {
    let mut tag = Zeroizing::new([0u8; MAC_LEN]);
    // HMAC accepts keys of any length, so this never fails; an empty tag
    // would simply never verify.
    if let Ok(mut mac) = Hmac::<Sha256>::new_from_slice(key) {
        mac.update(label);
        mac.update(binding);
        tag.copy_from_slice(&mac.finalize().into_bytes());
    }
    tag
}

/// Checks a received confirmation in constant time.
pub(crate) fn verify_confirmation(
    key: &[u8],
    label: &[u8],
    binding: &[u8; 32],
    tag: &[u8],
) -> bool {
    match Hmac::<Sha256>::new_from_slice(key) {
        Ok(mut mac) => {
            mac.update(label);
            mac.update(binding);
            mac.verify_slice(tag).is_ok()
        }
        Err(_) => false,
    }
}

/// A Confirm body: the tag, this laptop's OS and its name.
pub(crate) fn encode_confirm(tag: &[u8; MAC_LEN], me: &PairInfo) -> Vec<u8> {
    let room = MAX_FRAME - HEADER - MAC_LEN - 1;
    let mut name = me.name.as_str();
    while name.len() > room {
        let mut cut = room.min(name.len().saturating_sub(1));
        while !name.is_char_boundary(cut) {
            cut -= 1;
        }
        name = name.get(..cut).unwrap_or_default();
    }
    let mut body = Vec::with_capacity(MAC_LEN + 1 + name.len());
    body.extend_from_slice(tag);
    body.push(os_byte(me.os));
    body.extend_from_slice(name.as_bytes());
    body
}

/// Reads a Confirm body: the tag, the OS and the name (lossily decoded;
/// the caller cleans it for display).
pub(crate) fn parse_confirm(body: &[u8]) -> Result<([u8; MAC_LEN], Os, String), Failure> {
    let (tag, rest) = body
        .split_first_chunk::<MAC_LEN>()
        .ok_or_else(|| Failure::protocol("malformed confirmation"))?;
    let (&os, name) = rest
        .split_first()
        .ok_or_else(|| Failure::protocol("malformed confirmation"))?;
    let os = os_from_byte(os).ok_or_else(|| Failure::protocol("unknown OS"))?;
    Ok((*tag, os, String::from_utf8_lossy(name).into_owned()))
}

fn os_byte(os: Os) -> u8 {
    match os {
        Os::MacOs => 1,
        Os::Windows => 2,
        Os::Linux => 3,
    }
}

fn os_from_byte(byte: u8) -> Option<Os> {
    match byte {
        1 => Some(Os::MacOs),
        2 => Some(Os::Windows),
        3 => Some(Os::Linux),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_confirmation_is_bound_to_the_tls_session() {
        let key = [7u8; 32];
        let binding = [1u8; 32];
        let tag = confirmation(&key, LABEL_JOINER, &binding);
        assert!(verify_confirmation(
            &key,
            LABEL_JOINER,
            &binding,
            tag.as_ref()
        ));
        // Another TLS session (a relay's) has another exporter value: the
        // same key and label no longer verify.
        assert!(!verify_confirmation(
            &key,
            LABEL_JOINER,
            &[2u8; 32],
            tag.as_ref()
        ));
        // The two sides' labels differ, so one side's tag never passes as
        // the other's.
        assert!(!verify_confirmation(
            &key,
            LABEL_HOST,
            &binding,
            tag.as_ref()
        ));
        assert!(!verify_confirmation(
            &[8u8; 32],
            LABEL_JOINER,
            &binding,
            tag.as_ref()
        ));
        assert!(!verify_confirmation(
            &key,
            LABEL_JOINER,
            &binding,
            &tag[..31]
        ));
    }

    #[test]
    fn the_confirmation_is_hmac_sha256_over_label_and_binding() {
        // RFC 4231 test case 2 has key "Jefe" and data
        // "what do ya want for nothing?": split the data into a label and a
        // 32-byte "binding" so the vector checks the construction.
        let data = b"what do ya want for nothing?";
        let mut padded = [0u8; 32];
        padded[..data.len() - 4].copy_from_slice(&data[4..]);
        let mut mac = Hmac::<Sha256>::new_from_slice(b"Jefe").unwrap();
        mac.update(&data[..4]);
        mac.update(&padded);
        let expected = mac.finalize().into_bytes();
        let tag = confirmation(b"Jefe", &data[..4], &padded);
        assert_eq!(tag.as_ref(), expected.as_slice());
    }

    #[test]
    fn confirm_bodies_round_trip() {
        let tag = [9u8; MAC_LEN];
        let me = PairInfo {
            name: "Krish's PC".to_owned(),
            os: Os::Windows,
        };
        let body = encode_confirm(&tag, &me);
        let (parsed_tag, os, name) = parse_confirm(&body).unwrap();
        assert_eq!(parsed_tag, tag);
        assert_eq!(os, Os::Windows);
        assert_eq!(name, "Krish's PC");
        assert!(parse_confirm(&body[..MAC_LEN]).is_err());
        let mut bad_os = body.clone();
        bad_os[MAC_LEN] = 9;
        assert!(parse_confirm(&bad_os).is_err());
        // A huge name is cut to fit one frame, on a character boundary.
        let long = PairInfo {
            name: "é".repeat(2000),
            os: Os::MacOs,
        };
        let body = encode_confirm(&tag, &long);
        assert!(body.len() + HEADER <= MAX_FRAME);
        assert!(parse_confirm(&body).unwrap().2.chars().all(|c| c == 'é'));
    }

    #[test]
    fn close_codes_map_to_errors() {
        assert!(matches!(error_for_close(16), PairError::Busy));
        assert!(matches!(error_for_close(17), PairError::WrongCode));
        assert!(matches!(error_for_close(18), PairError::Expired));
        assert!(matches!(error_for_close(19), PairError::Declined));
        assert!(matches!(error_for_close(0), PairError::Connection(_)));
    }
}
