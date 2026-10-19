//! Size limits: oversized input is rejected, never truncated, on encode and
//! on decode.

mod common;

use common::{offer, pop_all, raw_frame, sample_hello};
use hop_proto::{
    Decoded, FileEntry, FileMessage, FrameDecoder, KIND_CORE, KIND_FILE, MAX_CLIPBOARD_TEXT,
    MAX_DATAGRAM_LEN, MAX_FILE_NAME_BYTES, MAX_FRAME_LEN, MAX_OFFER_FILES, Message, ProtoError,
    decode_datagram, encode_datagram, encode_frame,
};
use hop_types::{CoreMessage, MotionMark};

fn clipboard(text: String) -> Message {
    Message::Core(CoreMessage::ClipboardText {
        generation: 1,
        text,
    })
}

fn header(kind: u16, len: u32) -> Vec<u8> {
    let mut header = kind.to_be_bytes().to_vec();
    header.extend_from_slice(&len.to_be_bytes());
    header
}

#[test]
fn limits_have_the_documented_values() {
    assert_eq!(MAX_FRAME_LEN, 1024 * 1024 + 64 * 1024);
    assert_eq!(MAX_DATAGRAM_LEN, 1200);
    assert_eq!(MAX_CLIPBOARD_TEXT, 1024 * 1024);
    assert_eq!(MAX_OFFER_FILES, 1000);
    assert_eq!(MAX_FILE_NAME_BYTES, 1024);
}

/// The bytes a misbehaving peer would send for `message`, skipping the
/// encode-side checks.
fn raw_file_frame(message: &FileMessage) -> Vec<u8> {
    raw_frame(
        KIND_FILE,
        &rmp_serde::to_vec_named(message).expect("encodes"),
    )
}

#[test]
fn an_offer_at_both_limits_fits_in_one_frame() {
    let message = Message::File(offer(MAX_OFFER_FILES, MAX_FILE_NAME_BYTES));
    let frame = encode_frame(&message).expect("within the limits");
    assert!(frame.len() - 6 <= MAX_FRAME_LEN);
    let mut decoder = FrameDecoder::new();
    decoder.push(&frame).expect("valid");
    assert_eq!(decoder.pop(), Some(Ok(Decoded::Message(message))));
}

#[test]
fn an_offer_of_too_many_files_is_refused_on_encode_and_decode() {
    let too_many = offer(MAX_OFFER_FILES + 1, 4);
    let expected = ProtoError::FileOfferTooLarge {
        files: MAX_OFFER_FILES + 1,
        longest_name: 4,
    };
    let message = Message::File(too_many.clone());
    assert_eq!(encode_frame(&message), Err(expected.clone()));
    assert_eq!(encode_datagram(&message), Err(expected.clone()));

    let heartbeat = Message::Core(CoreMessage::Heartbeat { seq: 3 });
    let mut stream = raw_file_frame(&too_many);
    stream.extend(encode_frame(&heartbeat).expect("encodes"));
    let mut decoder = FrameDecoder::new();
    decoder.push(&stream).expect("framing is fine");
    assert_eq!(
        pop_all(&mut decoder),
        vec![Err(expected), Ok(Decoded::Message(heartbeat))]
    );
}

#[test]
fn a_file_name_over_the_limit_is_refused_on_encode_and_decode() {
    let long = offer(2, MAX_FILE_NAME_BYTES + 1);
    let expected = ProtoError::FileOfferTooLarge {
        files: 2,
        longest_name: MAX_FILE_NAME_BYTES + 1,
    };
    assert_eq!(
        encode_frame(&Message::File(long.clone())),
        Err(expected.clone())
    );
    let mut decoder = FrameDecoder::new();
    decoder
        .push(&raw_file_frame(&long))
        .expect("framing is fine");
    assert_eq!(decoder.pop(), Some(Err(expected)));
}

#[test]
fn the_name_limit_counts_bytes_not_characters() {
    let named = |name: String| {
        Message::File(FileMessage::Offer {
            id: 1,
            files: vec![FileEntry { name, size: 1 }],
        })
    };
    // "é" is two bytes in UTF-8.
    assert!(encode_frame(&named("é".repeat(MAX_FILE_NAME_BYTES / 2))).is_ok());
    assert_eq!(
        encode_frame(&named("é".repeat(MAX_FILE_NAME_BYTES / 2 + 1))),
        Err(ProtoError::FileOfferTooLarge {
            files: 1,
            longest_name: MAX_FILE_NAME_BYTES + 2,
        })
    );
}

#[test]
fn answers_outcomes_and_empty_offers_have_no_file_limits_to_break() {
    for message in [
        FileMessage::Offer {
            id: 0,
            files: vec![],
        },
        FileMessage::Answer {
            id: u64::MAX,
            accepted: true,
            refusal: None,
        },
        FileMessage::Outcome {
            id: u64::MAX,
            saved: u32::MAX,
            failure: None,
        },
    ] {
        let datagram = encode_datagram(&Message::File(message.clone())).expect("fits");
        assert_eq!(
            decode_datagram(&datagram),
            Ok(Decoded::Message(Message::File(message)))
        );
    }
}

#[test]
fn a_nudge_with_a_body_is_refused() {
    // A nudge is the kind alone; anything after it is malformed (and a
    // receiver drops a bad datagram).
    let mut datagram = hop_proto::KIND_NUDGE.to_be_bytes().to_vec();
    datagram.push(0xc0);
    assert!(matches!(
        decode_datagram(&datagram),
        Err(ProtoError::Decode {
            kind: hop_proto::KIND_NUDGE,
            ..
        })
    ));
}

#[test]
fn an_oversized_frame_is_rejected_as_soon_as_its_header_arrives() {
    let too_long = u32::try_from(MAX_FRAME_LEN + 1).expect("fits");
    let header = header(KIND_CORE, too_long);
    let mut decoder = FrameDecoder::new();
    decoder.push(&header[..5]).expect("header not complete yet");
    let expected = ProtoError::FrameTooLarge {
        len: MAX_FRAME_LEN + 1,
        max: MAX_FRAME_LEN,
    };
    assert_eq!(decoder.push(&header[5..]), Err(expected.clone()));
    assert_eq!(decoder.buffered_len(), 0, "nothing of the frame is kept");

    // The stream cannot be resynchronized: later bytes are refused and
    // dropped.
    assert_eq!(decoder.push(&vec![0; 64 * 1024]), Err(expected));
    assert_eq!(decoder.buffered_len(), 0);
    assert!(decoder.pop().is_none());
}

#[test]
fn the_largest_declared_length_is_rejected_without_waiting_for_the_body() {
    let mut decoder = FrameDecoder::new();
    let mut chunk = header(KIND_CORE, u32::MAX);
    chunk.extend_from_slice(&[0xab; 1000]); // the start of the "body"
    assert_eq!(
        decoder.push(&chunk),
        Err(ProtoError::FrameTooLarge {
            len: u32::MAX as usize,
            max: MAX_FRAME_LEN,
        })
    );
    assert_eq!(decoder.buffered_len(), 0);
}

#[test]
fn a_frame_of_exactly_the_maximum_length_is_accepted() {
    let max = u32::try_from(MAX_FRAME_LEN).expect("fits");
    let mut decoder = FrameDecoder::new();
    decoder.push(&header(77, max)).expect("allowed");
    decoder.push(&vec![0; MAX_FRAME_LEN]).expect("allowed");
    assert_eq!(decoder.pop(), Some(Ok(Decoded::Unknown { kind: 77 })));
}

#[test]
fn frames_before_an_oversized_header_can_still_be_popped() {
    let heartbeat = Message::Core(CoreMessage::Heartbeat { seq: 1 });
    let mut chunk = encode_frame(&heartbeat).expect("encodes");
    chunk.extend(header(KIND_CORE, u32::MAX));
    chunk.extend_from_slice(&[0; 100]);
    let mut decoder = FrameDecoder::new();
    assert!(matches!(
        decoder.push(&chunk),
        Err(ProtoError::FrameTooLarge { .. })
    ));
    assert_eq!(pop_all(&mut decoder), vec![Ok(Decoded::Message(heartbeat))]);
    assert_eq!(decoder.buffered_len(), 0);
}

#[test]
fn encoding_refuses_a_body_over_the_frame_limit() {
    let mut hello = sample_hello();
    hello.device_name = "x".repeat(MAX_FRAME_LEN);
    assert!(matches!(
        encode_frame(&Message::Hello(hello)),
        Err(ProtoError::FrameTooLarge { len, max: MAX_FRAME_LEN }) if len > MAX_FRAME_LEN
    ));
}

#[test]
fn clipboard_text_at_the_limit_travels_in_a_frame() {
    let message = clipboard("a".repeat(MAX_CLIPBOARD_TEXT));
    let frame = encode_frame(&message).expect("within the limit");
    assert!(frame.len() - 6 <= MAX_FRAME_LEN);
    let mut decoder = FrameDecoder::new();
    decoder.push(&frame).expect("valid");
    assert_eq!(decoder.pop(), Some(Ok(Decoded::Message(message))));
}

#[test]
fn clipboard_text_over_the_limit_is_refused_on_encode() {
    let expected = Err(ProtoError::ClipboardTooLarge {
        len: MAX_CLIPBOARD_TEXT + 1,
        max: MAX_CLIPBOARD_TEXT,
    });
    let message = clipboard("a".repeat(MAX_CLIPBOARD_TEXT + 1));
    assert_eq!(encode_frame(&message), expected);
    assert_eq!(encode_datagram(&message), expected);
}

#[test]
fn the_clipboard_limit_counts_bytes_not_characters() {
    // "é" is two bytes in UTF-8.
    let at_limit = clipboard("é".repeat(MAX_CLIPBOARD_TEXT / 2));
    assert!(encode_frame(&at_limit).is_ok());
    let over = clipboard("é".repeat(MAX_CLIPBOARD_TEXT / 2 + 1));
    assert_eq!(
        encode_frame(&over),
        Err(ProtoError::ClipboardTooLarge {
            len: MAX_CLIPBOARD_TEXT + 2,
            max: MAX_CLIPBOARD_TEXT,
        })
    );
}

#[test]
fn clipboard_text_over_the_limit_is_refused_on_decode() {
    // A misbehaving peer could skip the encode-side check; build its bytes
    // directly.
    let oversized = CoreMessage::ClipboardText {
        generation: 1,
        text: "a".repeat(MAX_CLIPBOARD_TEXT + 1),
    };
    let body = rmp_serde::to_vec_named(&oversized).expect("encodes");
    assert!(body.len() <= MAX_FRAME_LEN, "the frame itself is allowed");
    let heartbeat = Message::Core(CoreMessage::Heartbeat { seq: 2 });
    let mut stream = raw_frame(KIND_CORE, &body);
    stream.extend(encode_frame(&heartbeat).expect("encodes"));

    let mut decoder = FrameDecoder::new();
    decoder.push(&stream).expect("framing is fine");
    assert_eq!(
        pop_all(&mut decoder),
        vec![
            Err(ProtoError::ClipboardTooLarge {
                len: MAX_CLIPBOARD_TEXT + 1,
                max: MAX_CLIPBOARD_TEXT,
            }),
            Ok(Decoded::Message(heartbeat)),
        ]
    );
}

#[test]
fn a_motion_datagram_with_extreme_values_fits_with_plenty_of_room() {
    let motion = Message::Core(CoreMessage::Motion {
        seq: u64::MAX,
        mark: MotionMark {
            n: u64::MAX,
            total_dx: f64::MAX,
            total_dy: f64::MIN,
        },
    });
    let datagram = encode_datagram(&motion).expect("fits");
    assert!(
        datagram.len() * 10 < MAX_DATAGRAM_LEN,
        "{} bytes is more than a tenth of the limit",
        datagram.len()
    );
    assert_eq!(decode_datagram(&datagram), Ok(Decoded::Message(motion)));
}

#[test]
fn a_datagram_of_exactly_the_maximum_length_is_accepted() {
    // Find the clipboard text length that makes the datagram exactly full.
    let (message, datagram) = (0..MAX_DATAGRAM_LEN)
        .map(|n| clipboard("a".repeat(n)))
        .filter_map(|message| {
            let datagram = encode_datagram(&message).ok()?;
            Some((message, datagram))
        })
        .find(|(_, datagram)| datagram.len() == MAX_DATAGRAM_LEN)
        .expect("some length fills the datagram exactly");
    assert_eq!(decode_datagram(&datagram), Ok(Decoded::Message(message)));
}

#[test]
fn oversized_datagrams_are_refused() {
    let message = clipboard("a".repeat(MAX_DATAGRAM_LEN));
    assert!(matches!(
        encode_datagram(&message),
        Err(ProtoError::DatagramTooLarge { len, max: MAX_DATAGRAM_LEN }) if len > MAX_DATAGRAM_LEN
    ));

    let mut datagram = KIND_CORE.to_be_bytes().to_vec();
    datagram.resize(MAX_DATAGRAM_LEN + 1, 0);
    assert_eq!(
        decode_datagram(&datagram),
        Err(ProtoError::DatagramTooLarge {
            len: MAX_DATAGRAM_LEN + 1,
            max: MAX_DATAGRAM_LEN,
        })
    );
}

#[test]
fn short_and_empty_datagrams() {
    assert_eq!(
        decode_datagram(&[]),
        Err(ProtoError::DatagramTooShort { len: 0 })
    );
    assert_eq!(
        decode_datagram(&[0x00]),
        Err(ProtoError::DatagramTooShort { len: 1 })
    );
    assert!(matches!(
        decode_datagram(&KIND_CORE.to_be_bytes()),
        Err(ProtoError::Decode {
            kind: KIND_CORE,
            ..
        })
    ));
    assert_eq!(
        decode_datagram(&[0x01, 0x00]),
        Ok(Decoded::Unknown { kind: 256 })
    );
}
