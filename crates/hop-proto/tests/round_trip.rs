//! Every message survives encoding and decoding, through stream frames and
//! through datagrams, and the bytes on the wire have the documented layout.

mod common;

use std::collections::BTreeSet;

use common::{
    EVERY_FAILURE, EVERY_REFUSAL, FILE_VARIANT_COUNT, VARIANT_COUNT, every_core_message,
    every_file_message, every_message, failure_name, file_variant_name, refusal_name, sample_hello,
    variant_name,
};
use hop_proto::{
    Decoded, FileEntry, FileMessage, FrameDecoder, Hello, KIND_CORE, KIND_FILE, KIND_HELLO,
    KIND_NUDGE, MAX_DATAGRAM_LEN, Message, PROTO_MAJOR, PROTO_MINOR, decode_datagram,
    default_capabilities, encode_datagram, encode_frame,
};
use hop_types::{CoreMessage, DeviceId, Os, RejectReason};

fn through_frame(message: &Message) -> Decoded {
    let frame = encode_frame(message).expect("encodes");
    let mut decoder = FrameDecoder::new();
    decoder.push(&frame).expect("valid frame");
    let decoded = decoder.pop().expect("one frame").expect("decodes");
    assert!(decoder.pop().is_none(), "exactly one frame");
    assert_eq!(decoder.buffered_len(), 0);
    decoded
}

#[test]
fn samples_cover_every_core_message_variant() {
    let names: BTreeSet<_> = every_core_message().iter().map(variant_name).collect();
    assert_eq!(names.len(), VARIANT_COUNT, "missing samples: {names:?}");
}

#[test]
fn samples_cover_every_file_message_variant_refusal_and_failure() {
    let samples = every_file_message();
    let names: BTreeSet<_> = samples.iter().map(file_variant_name).collect();
    assert_eq!(
        names.len(),
        FILE_VARIANT_COUNT,
        "missing samples: {names:?}"
    );
    let refusals: BTreeSet<_> = samples
        .iter()
        .filter_map(|message| match message {
            FileMessage::Answer {
                refusal: Some(refusal),
                ..
            } => Some(refusal_name(*refusal)),
            _ => None,
        })
        .collect();
    let failures: BTreeSet<_> = samples
        .iter()
        .filter_map(|message| match message {
            FileMessage::Outcome {
                failure: Some(failure),
                ..
            } => Some(failure_name(*failure)),
            _ => None,
        })
        .collect();
    // The name functions have no catch-all arm, so a new value fails to
    // build there; these counts catch one missing from the arrays.
    assert_eq!(refusals.len(), EVERY_REFUSAL.len());
    assert_eq!(failures.len(), EVERY_FAILURE.len());
}

#[test]
fn every_message_round_trips_through_a_frame() {
    for message in every_message() {
        assert_eq!(
            through_frame(&message),
            Decoded::Message(message.clone()),
            "{message:?}"
        );
    }
}

#[test]
fn every_message_round_trips_through_a_datagram() {
    // All samples, Hello with two displays included, are small enough.
    for message in every_message() {
        let datagram = encode_datagram(&message).expect("fits in a datagram");
        assert!(datagram.len() <= MAX_DATAGRAM_LEN);
        assert_eq!(
            decode_datagram(&datagram),
            Ok(Decoded::Message(message.clone())),
            "{message:?}"
        );
    }
}

#[test]
fn frame_header_is_kind_then_big_endian_body_length() {
    for message in every_message() {
        let frame = encode_frame(&message).expect("encodes");
        let kind = u16::from_be_bytes([frame[0], frame[1]]);
        let len = u32::from_be_bytes([frame[2], frame[3], frame[4], frame[5]]);
        assert_eq!(kind, message.kind());
        assert_eq!(len as usize, frame.len() - 6);

        // A datagram carries the same body after a bare kind.
        let datagram = encode_datagram(&message).expect("fits");
        assert_eq!(datagram[..2], frame[..2]);
        assert_eq!(datagram[2..], frame[6..]);
    }
}

#[test]
fn kinds_are_one_for_hello_two_for_core_three_for_files_and_four_for_nudges() {
    assert_eq!(KIND_HELLO, 1);
    assert_eq!(KIND_CORE, 2);
    assert_eq!(KIND_FILE, 3);
    assert_eq!(KIND_NUDGE, 4);
    assert_eq!(Message::Hello(sample_hello()).kind(), 1);
    assert_eq!(Message::Core(CoreMessage::Heartbeat { seq: 0 }).kind(), 2);
    for file in every_file_message() {
        assert_eq!(Message::File(file).kind(), 3);
    }
    assert_eq!(Message::Nudge.kind(), 4);
}

/// Locks the wire format of the file messages and the nudge, as above.
#[test]
fn file_messages_and_the_nudge_have_the_documented_bytes() {
    let answer = encode_frame(
        &FileMessage::Answer {
            id: 5,
            accepted: false,
            refusal: Some(hop_proto::FileRefusal::TooLarge),
        }
        .into(),
    )
    .expect("encodes");
    let mut body = vec![0x81, 0xa6]; // one-entry map: the variant
    body.extend_from_slice(b"answer");
    body.extend_from_slice(&[0x83, 0xa2]); // three fields
    body.extend_from_slice(b"id");
    body.extend_from_slice(&[0x05, 0xa8]);
    body.extend_from_slice(b"accepted");
    body.extend_from_slice(&[0xc2, 0xa7]); // false
    body.extend_from_slice(b"refusal");
    body.push(0xa9);
    body.extend_from_slice(b"too_large");
    let mut expected = vec![0x00, 0x03, 0x00, 0x00, 0x00, body.len() as u8];
    expected.extend_from_slice(&body);
    assert_eq!(answer, expected);

    let offer = encode_datagram(
        &FileMessage::Offer {
            id: 1,
            files: vec![FileEntry {
                name: "a".to_owned(),
                size: 2,
            }],
        }
        .into(),
    )
    .expect("fits");
    let mut expected = vec![0x00, 0x03, 0x81, 0xa5];
    expected.extend_from_slice(b"offer");
    expected.extend_from_slice(&[0x82, 0xa2]);
    expected.extend_from_slice(b"id");
    expected.extend_from_slice(&[0x01, 0xa5]);
    expected.extend_from_slice(b"files");
    expected.extend_from_slice(&[0x91, 0x82, 0xa4]); // one entry, two fields
    expected.extend_from_slice(b"name");
    expected.extend_from_slice(&[0xa1, b'a', 0xa4]);
    expected.extend_from_slice(b"size");
    expected.push(0x02);
    assert_eq!(offer, expected);

    // A nudge is its kind and nothing else.
    assert_eq!(encode_datagram(&Message::Nudge), Ok(vec![0x00, 0x04]));
    assert_eq!(
        encode_frame(&Message::Nudge),
        Ok(vec![0x00, 0x04, 0x00, 0x00, 0x00, 0x00])
    );
}

/// Locks the wire format: named fields, snake_case variant names, unit enum
/// values as strings. If this test breaks, peers running older builds will
/// not understand the new bytes.
#[test]
fn heartbeat_and_reject_have_the_documented_bytes() {
    let heartbeat = encode_frame(&CoreMessage::Heartbeat { seq: 7 }.into()).expect("encodes");
    let mut expected = vec![0x00, 0x02, 0x00, 0x00, 0x00, 17];
    expected.push(0x81); // map with one entry: the variant
    expected.push(0xa9); // 9-byte string
    expected.extend_from_slice(b"heartbeat");
    expected.push(0x81); // map with one entry: the fields
    expected.push(0xa3);
    expected.extend_from_slice(b"seq");
    expected.push(0x07);
    assert_eq!(heartbeat, expected);

    let reject = encode_datagram(
        &CoreMessage::EnterReject {
            seq: 1,
            reason: RejectReason::Busy,
        }
        .into(),
    )
    .expect("encodes");
    let mut expected = vec![0x00, 0x02, 0x81, 0xac];
    expected.extend_from_slice(b"enter_reject");
    expected.extend_from_slice(&[0x82, 0xa3]);
    expected.extend_from_slice(b"seq");
    expected.extend_from_slice(&[0x01, 0xa6]);
    expected.extend_from_slice(b"reason");
    expected.push(0xa4);
    expected.extend_from_slice(b"busy");
    assert_eq!(reject, expected);
}

#[test]
fn hello_new_describes_this_build() {
    let displays = vec![common::display(7, 0.0, true)];
    let hello = Hello::new("1.2.3", DeviceId::new("abc"), "Desk PC", displays.clone());
    assert_eq!(hello.proto_major, PROTO_MAJOR);
    assert_eq!(hello.proto_minor, PROTO_MINOR);
    assert_eq!(hello.app_version, "1.2.3");
    assert_eq!(hello.device_id, DeviceId::new("abc"));
    assert_eq!(hello.device_name, "Desk PC");
    assert_eq!(hello.os, Os::current());
    assert_eq!(hello.displays, displays);
    assert_eq!(hello.capabilities, default_capabilities());
}

#[test]
fn messages_convert_from_their_contents() {
    assert_eq!(
        Message::from(sample_hello()),
        Message::Hello(sample_hello())
    );
    let release = CoreMessage::Release { seq: 3 };
    assert_eq!(Message::from(release.clone()), Message::Core(release));
    let outcome = FileMessage::Outcome {
        id: 9,
        saved: 0,
        failure: None,
    };
    assert_eq!(Message::from(outcome.clone()), Message::File(outcome));
}

#[test]
fn every_file_message_knows_its_transfer() {
    for message in every_file_message() {
        let expected = match &message {
            FileMessage::Offer { id, .. }
            | FileMessage::Answer { id, .. }
            | FileMessage::Outcome { id, .. } => *id,
        };
        assert_eq!(message.id(), expected);
    }
}
