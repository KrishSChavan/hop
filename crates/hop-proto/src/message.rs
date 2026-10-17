//! Message kinds, the [`Message`] enum and the MessagePack body codec shared
//! by stream frames and datagrams.

use hop_types::CoreMessage;
use serde::de::DeserializeOwned;

use crate::ProtoError;
use crate::hello::{self, Hello};

/// Kind number of [`Message::Hello`].
pub const KIND_HELLO: u16 = 1;

/// Kind number of [`Message::Core`].
pub const KIND_CORE: u16 = 2;

/// Largest clipboard text, in UTF-8 bytes, that a `ClipboardText` message
/// may carry: 1 MiB. Longer text is rejected on encode and on decode, never
/// truncated.
pub const MAX_CLIPBOARD_TEXT: usize = 1024 * 1024;

/// How deeply maps and arrays may nest in a body. The deepest Hop messages
/// today (`Input` with its events, `Hello` with its displays) nest five
/// levels, so 32 leaves room to grow while keeping hostile input from
/// exhausting the stack.
const MAX_DEPTH: usize = 32;

/// A message Hop sends between two devices.
#[derive(Clone, Debug, PartialEq)]
pub enum Message {
    /// The first message on every connection, in both directions.
    Hello(Hello),
    /// A control hand-off or clipboard message.
    Core(CoreMessage),
}

impl Message {
    /// The kind number this message travels under.
    pub fn kind(&self) -> u16 {
        match self {
            Message::Hello(_) => KIND_HELLO,
            Message::Core(_) => KIND_CORE,
        }
    }

    /// Appends this message's MessagePack body to `out`, after checking the
    /// content limits so Hop never sends what its peer would refuse.
    pub(crate) fn write_body(&self, out: &mut Vec<u8>) -> Result<(), ProtoError> {
        let written = match self {
            Message::Hello(hello) => rmp_serde::encode::write_named(out, hello),
            Message::Core(core) => {
                check_clipboard(core)?;
                rmp_serde::encode::write_named(out, core)
            }
        };
        written.map_err(|err| ProtoError::Encode {
            kind: self.kind(),
            reason: err.to_string(),
        })
    }
}

impl From<Hello> for Message {
    fn from(hello: Hello) -> Self {
        Message::Hello(hello)
    }
}

impl From<CoreMessage> for Message {
    fn from(core: CoreMessage) -> Self {
        Message::Core(core)
    }
}

/// The result of decoding one frame or datagram.
#[derive(Clone, Debug, PartialEq)]
pub enum Decoded {
    /// A message this build understands.
    Message(Message),
    /// A message of a kind this build does not know, probably from a newer
    /// peer. Its body has been consumed; skip it.
    Unknown {
        /// The unknown kind number.
        kind: u16,
    },
}

/// Decodes the body of one frame or datagram according to its kind.
pub(crate) fn decode_message(kind: u16, body: &[u8]) -> Result<Decoded, ProtoError> {
    let message = match kind {
        KIND_HELLO => Message::Hello(hello::decode_hello(body)?),
        KIND_CORE => {
            let core: CoreMessage = decode_body(kind, body)?;
            check_clipboard(&core)?;
            Message::Core(core)
        }
        _ => return Ok(Decoded::Unknown { kind }),
    };
    Ok(Decoded::Message(message))
}

/// Decodes exactly one MessagePack value from `body`.
///
/// Unknown map keys are skipped and missing `#[serde(default)]` fields take
/// their defaults (serde's derive does both); this function adds the depth
/// limit and rejects trailing bytes.
pub(crate) fn decode_body<T: DeserializeOwned>(kind: u16, body: &[u8]) -> Result<T, ProtoError> {
    let malformed = |reason: String| ProtoError::Decode { kind, reason };
    // A reading (not borrowing) deserializer is used because it can report
    // how much input is left. It never allocates more than the input holds,
    // so a hostile length prefix cannot cause a huge allocation.
    let mut de = rmp_serde::Deserializer::new(body);
    de.set_max_depth(MAX_DEPTH);
    let value = T::deserialize(&mut de).map_err(|err| malformed(err.to_string()))?;
    let trailing = de.get_ref().len();
    if trailing > 0 {
        return Err(malformed(format!(
            "{trailing} unexpected bytes after the message"
        )));
    }
    Ok(value)
}

fn check_clipboard(core: &CoreMessage) -> Result<(), ProtoError> {
    if let CoreMessage::ClipboardText { text, .. } = core
        && text.len() > MAX_CLIPBOARD_TEXT
    {
        return Err(ProtoError::ClipboardTooLarge {
            len: text.len(),
            max: MAX_CLIPBOARD_TEXT,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use serde::{Deserialize, Serialize};

    use super::*;

    /// A message as an older build defines it.
    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    struct Old {
        seq: u64,
        name: String,
    }

    /// The same message after a field was added the way rule 1 says.
    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    struct New {
        seq: u64,
        name: String,
        #[serde(default)]
        tags: Vec<String>,
        #[serde(default)]
        priority: Option<u8>,
    }

    fn encode<T: Serialize>(value: &T) -> Vec<u8> {
        rmp_serde::to_vec_named(value).expect("test values encode")
    }

    #[test]
    fn rule_1_newer_struct_with_extra_fields_decodes_into_the_older_one() {
        let new = New {
            seq: 4,
            name: "mac".to_owned(),
            tags: vec!["a".to_owned(), "b".to_owned()],
            priority: Some(3),
        };
        let old: Old = decode_body(KIND_CORE, &encode(&new)).expect("decodes");
        assert_eq!(
            old,
            Old {
                seq: 4,
                name: "mac".to_owned()
            }
        );
    }

    #[test]
    fn rule_1_older_encoding_missing_defaulted_fields_decodes_into_the_newer_one() {
        let old = Old {
            seq: 4,
            name: "mac".to_owned(),
        };
        let new: New = decode_body(KIND_CORE, &encode(&old)).expect("decodes");
        assert_eq!(
            new,
            New {
                seq: 4,
                name: "mac".to_owned(),
                tags: vec![],
                priority: None,
            }
        );
    }

    #[test]
    fn a_missing_field_without_a_default_is_an_error() {
        #[derive(Serialize)]
        struct SeqOnly {
            seq: u64,
        }
        let result = decode_body::<Old>(KIND_CORE, &encode(&SeqOnly { seq: 1 }));
        assert!(
            matches!(&result, Err(ProtoError::Decode { kind: KIND_CORE, reason }) if reason.contains("name")),
            "{result:?}"
        );
    }

    #[test]
    fn trailing_bytes_are_an_error() {
        let mut body = encode(&Old {
            seq: 1,
            name: String::new(),
        });
        body.push(0xc0);
        assert!(matches!(
            decode_body::<Old>(KIND_CORE, &body),
            Err(ProtoError::Decode { .. })
        ));
    }

    /// `{"seq": 1, "name": "", "junk": [[[...]]]}` with `depth` nested arrays.
    fn nested_junk(depth: usize) -> Vec<u8> {
        let mut body = vec![0x83, 0xa3];
        body.extend_from_slice(b"seq");
        body.extend_from_slice(&[0x01, 0xa4]);
        body.extend_from_slice(b"name");
        body.extend_from_slice(&[0xa0, 0xa4]);
        body.extend_from_slice(b"junk");
        body.extend(std::iter::repeat_n(0x91, depth)); // arrays of one element
        body.push(0xc0); // nil, at the bottom
        body
    }

    #[test]
    fn moderate_nesting_in_an_unknown_field_is_skipped() {
        let old: Old = decode_body(KIND_CORE, &nested_junk(MAX_DEPTH - 2)).expect("decodes");
        assert_eq!(old.seq, 1);
    }

    #[test]
    fn deep_nesting_is_rejected_without_exhausting_the_stack() {
        for depth in [MAX_DEPTH + 1, 100_000] {
            assert!(
                matches!(
                    decode_body::<Old>(KIND_CORE, &nested_junk(depth)),
                    Err(ProtoError::Decode { .. })
                ),
                "depth {depth}"
            );
        }
    }

    #[test]
    fn hostile_length_prefixes_fail_without_huge_allocations() {
        let bodies: [&[u8]; 4] = [
            &[0xdb, 0xff, 0xff, 0xff, 0xff, b'a'], // a 4 GiB string
            &[0xc6, 0xff, 0xff, 0xff, 0xff, 0x00], // 4 GiB of binary
            &[0xdd, 0xff, 0xff, 0xff, 0xff, 0xc0], // 4 billion array items
            &[0xdf, 0xff, 0xff, 0xff, 0xff, 0xc0], // 4 billion map entries
        ];
        for body in bodies {
            assert!(decode_body::<Old>(KIND_CORE, body).is_err(), "{body:x?}");
            assert!(decode_message(KIND_CORE, body).is_err(), "{body:x?}");
            assert!(decode_message(KIND_HELLO, body).is_err(), "{body:x?}");
        }
    }

    #[test]
    fn unknown_kinds_ignore_their_body() {
        assert_eq!(
            decode_message(9, &[0xc1, 0xff]),
            Ok(Decoded::Unknown { kind: 9 })
        );
    }
}
