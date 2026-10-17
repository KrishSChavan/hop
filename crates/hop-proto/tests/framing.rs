//! `FrameDecoder` reassembles frames however the stream splits them, skips
//! unknown kinds and survives malformed bodies.

mod common;

use common::{every_message, pop_all, raw_frame};
use hop_proto::{Decoded, FrameDecoder, KIND_CORE, KIND_HELLO, Message, ProtoError, encode_frame};
use hop_types::CoreMessage;

fn stream_of(messages: &[Message]) -> Vec<u8> {
    messages
        .iter()
        .flat_map(|message| encode_frame(message).expect("encodes"))
        .collect()
}

fn decoded(messages: &[Message]) -> Vec<Result<Decoded, ProtoError>> {
    messages
        .iter()
        .cloned()
        .map(|message| Ok(Decoded::Message(message)))
        .collect()
}

#[test]
fn one_byte_at_a_time() {
    let messages = every_message();
    let stream = stream_of(&messages);
    let mut decoder = FrameDecoder::new();
    let mut out = Vec::new();
    for byte in &stream {
        decoder
            .push(std::slice::from_ref(byte))
            .expect("valid stream");
        out.extend(pop_all(&mut decoder));
    }
    assert_eq!(out, decoded(&messages));
    assert_eq!(decoder.buffered_len(), 0);
}

#[test]
fn several_frames_in_one_chunk() {
    let messages = every_message();
    let mut decoder = FrameDecoder::new();
    decoder.push(&stream_of(&messages)).expect("valid stream");
    assert_eq!(pop_all(&mut decoder), decoded(&messages));
    assert_eq!(decoder.buffered_len(), 0);
}

#[test]
fn two_frames_split_at_every_position() {
    let messages = vec![
        Message::Core(CoreMessage::Heartbeat { seq: 1 }),
        Message::Core(CoreMessage::ClipboardText {
            generation: 2,
            text: "two".to_owned(),
        }),
    ];
    let stream = stream_of(&messages);
    for split in 0..=stream.len() {
        let (first, second) = stream.split_at(split);
        let mut decoder = FrameDecoder::new();
        decoder.push(first).expect("valid");
        let mut out = pop_all(&mut decoder);
        decoder.push(second).expect("valid");
        out.extend(pop_all(&mut decoder));
        assert_eq!(out, decoded(&messages), "split at {split}");
    }
}

#[test]
fn nothing_pops_until_the_frame_is_complete() {
    let frame = encode_frame(&CoreMessage::Heartbeat { seq: 9 }.into()).expect("encodes");
    let mut decoder = FrameDecoder::new();
    assert!(decoder.pop().is_none());
    decoder.push(&[]).expect("empty pushes are fine");
    decoder.push(&frame[..frame.len() - 1]).expect("valid");
    assert!(decoder.pop().is_none());
    assert_eq!(decoder.buffered_len(), frame.len() - 1);
    decoder.push(&frame[frame.len() - 1..]).expect("valid");
    assert!(matches!(decoder.pop(), Some(Ok(Decoded::Message(_)))));
    assert!(decoder.pop().is_none());
}

#[test]
fn popped_frames_are_released() {
    let first = encode_frame(&CoreMessage::Heartbeat { seq: 1 }.into()).expect("encodes");
    let second = encode_frame(&CoreMessage::Heartbeat { seq: 2 }.into()).expect("encodes");
    let mut decoder = FrameDecoder::new();
    decoder.push(&first).expect("valid");
    decoder.push(&second[..3]).expect("valid");
    assert!(matches!(decoder.pop(), Some(Ok(_))));
    assert_eq!(decoder.buffered_len(), 3);
    decoder.push(&second[3..]).expect("valid");
    assert_eq!(decoder.buffered_len(), second.len());
    assert_eq!(
        decoder.pop(),
        Some(Ok(Decoded::Message(
            CoreMessage::Heartbeat { seq: 2 }.into()
        )))
    );
    assert_eq!(decoder.buffered_len(), 0);
}

#[test]
fn unknown_kinds_are_reported_and_skipped() {
    let heartbeat = Message::Core(CoreMessage::Heartbeat { seq: 5 });
    let mut stream = raw_frame(77, b"anything at all, even \xff\xc1 junk");
    stream.extend(raw_frame(0, &[]));
    stream.extend(raw_frame(u16::MAX, &[0x90]));
    stream.extend(encode_frame(&heartbeat).expect("encodes"));
    let mut decoder = FrameDecoder::new();
    decoder.push(&stream).expect("valid framing");
    assert_eq!(
        pop_all(&mut decoder),
        vec![
            Ok(Decoded::Unknown { kind: 77 }),
            Ok(Decoded::Unknown { kind: 0 }),
            Ok(Decoded::Unknown { kind: u16::MAX }),
            Ok(Decoded::Message(heartbeat)),
        ]
    );
}

#[test]
fn a_malformed_body_is_an_error_and_the_next_frame_still_decodes() {
    let heartbeat = Message::Core(CoreMessage::Heartbeat { seq: 5 });
    let valid_body = &encode_frame(&heartbeat).expect("encodes")[6..];
    let mut with_trailing_byte = valid_body.to_vec();
    with_trailing_byte.push(0x00);

    let bad_bodies: Vec<(u16, Vec<u8>)> = vec![
        (KIND_CORE, vec![]),                   // no value at all
        (KIND_CORE, vec![0xc1]),               // a byte MessagePack never uses
        (KIND_CORE, valid_body[..5].to_vec()), // cut short
        (KIND_CORE, with_trailing_byte),       // two values
        (KIND_CORE, vec![0x2a]),               // the number 42, not a message
        (KIND_HELLO, vec![0x80]),              // an empty map: fields missing
    ];
    for (kind, body) in bad_bodies {
        let mut stream = raw_frame(kind, &body);
        stream.extend(encode_frame(&heartbeat).expect("encodes"));
        let mut decoder = FrameDecoder::new();
        decoder.push(&stream).expect("framing is fine");
        let out = pop_all(&mut decoder);
        assert_eq!(out.len(), 2, "{body:x?}");
        assert!(
            matches!(&out[0], Err(ProtoError::Decode { kind: k, .. }) if *k == kind),
            "{body:x?}: {:?}",
            out[0]
        );
        assert_eq!(out[1], Ok(Decoded::Message(heartbeat.clone())));
    }
}

#[test]
fn an_unknown_core_variant_is_a_decode_error() {
    // Rule 3 of the evolution rules: a new variant needs a capability first.
    let mut body = vec![0x81, 0xa8];
    body.extend_from_slice(b"teleport");
    body.push(0x80);
    let mut decoder = FrameDecoder::new();
    decoder
        .push(&raw_frame(KIND_CORE, &body))
        .expect("framing is fine");
    assert!(matches!(
        decoder.pop(),
        Some(Err(ProtoError::Decode {
            kind: KIND_CORE,
            ..
        }))
    ));
}
