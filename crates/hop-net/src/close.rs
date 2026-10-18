//! The application close codes Hop puts on its QUIC connections.

use std::fmt;

/// Why a Hop connection was closed, as the QUIC application error code
/// carried in the CONNECTION_CLOSE frame (docs/M2-DESIGN.md, "Close codes"
/// and "Pairing").
///
/// Codes 0 to 4 are used on session connections (ALPN `hop/1`), and 16 to
/// 19 on pairing connections (ALPN `hop-pair/1`). The numbers are part of
/// the wire protocol: never renumber them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum CloseCode {
    /// 0: the sender is quitting, or the conversation finished normally.
    Shutdown,
    /// 1: a second connection between the same two devices; the other one
    /// is kept.
    Duplicate,
    /// 2: the two Hop builds cannot talk to each other (different major
    /// protocol versions, or no shared `input` capability).
    Incompatible,
    /// 3: a missing, late, oversized or wrong `Hello`, or a frame that does
    /// not decode.
    Protocol,
    /// 4: a queue filled up, and dropping a reliable message instead could
    /// have left a key held down.
    Overloaded,
    /// 16: the laptop showing the pairing code is busy with another joiner.
    Busy,
    /// 17: the key confirmation failed: the joiner typed a different code.
    WrongCode,
    /// 18: the pairing code expired or was used up, or the joiner stopped
    /// waiting for the answer on the laptop showing the code.
    Expired,
    /// 19: the user on the laptop showing the code said no.
    Declined,
}

impl CloseCode {
    /// The numeric code on the wire.
    pub const fn code(self) -> u32 {
        match self {
            CloseCode::Shutdown => 0,
            CloseCode::Duplicate => 1,
            CloseCode::Incompatible => 2,
            CloseCode::Protocol => 3,
            CloseCode::Overloaded => 4,
            CloseCode::Busy => 16,
            CloseCode::WrongCode => 17,
            CloseCode::Expired => 18,
            CloseCode::Declined => 19,
        }
    }

    /// The close code for a numeric code received from a peer, if Hop
    /// knows it.
    pub fn from_code(code: u64) -> Option<CloseCode> {
        [
            CloseCode::Shutdown,
            CloseCode::Duplicate,
            CloseCode::Incompatible,
            CloseCode::Protocol,
            CloseCode::Overloaded,
            CloseCode::Busy,
            CloseCode::WrongCode,
            CloseCode::Expired,
            CloseCode::Declined,
        ]
        .into_iter()
        .find(|known| u64::from(known.code()) == code)
    }

    /// A short lowercase name for logs.
    pub const fn name(self) -> &'static str {
        match self {
            CloseCode::Shutdown => "shutdown",
            CloseCode::Duplicate => "duplicate",
            CloseCode::Incompatible => "incompatible",
            CloseCode::Protocol => "protocol",
            CloseCode::Overloaded => "overloaded",
            CloseCode::Busy => "busy",
            CloseCode::WrongCode => "wrong code",
            CloseCode::Expired => "expired",
            CloseCode::Declined => "declined",
        }
    }

    /// The code as quinn's variable-length integer.
    pub(crate) fn varint(self) -> quinn::VarInt {
        quinn::VarInt::from_u32(self.code())
    }
}

impl fmt::Display for CloseCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} ({})", self.name(), self.code())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_match_the_design() {
        let expected = [
            (CloseCode::Shutdown, 0),
            (CloseCode::Duplicate, 1),
            (CloseCode::Incompatible, 2),
            (CloseCode::Protocol, 3),
            (CloseCode::Overloaded, 4),
            (CloseCode::Busy, 16),
            (CloseCode::WrongCode, 17),
            (CloseCode::Expired, 18),
            (CloseCode::Declined, 19),
        ];
        for (close, code) in expected {
            assert_eq!(close.code(), code);
            assert_eq!(CloseCode::from_code(u64::from(code)), Some(close));
        }
        assert_eq!(CloseCode::from_code(5), None);
        assert_eq!(CloseCode::from_code(u64::MAX), None);
    }
}
