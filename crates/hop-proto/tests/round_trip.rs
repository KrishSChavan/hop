//! Every message survives encoding and decoding, through stream frames and
//! through datagrams, and the bytes on the wire have the documented layout.

mod common;

use std::collections::BTreeSet;

use common::{VARIANT_COUNT, every_core_message, every_message, sample_hello, variant_name};
use hop_proto::{
    Decoded, FrameDecoder, Hello, KIND_CORE, KIND_HELLO, MAX_DATAGRAM_LEN, Message, PROTO_MAJOR,
    PROTO_MINOR, decode_datagram, default_capabilities, encode_datagram, encode_frame,
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
fn kinds_are_one_for_hello_and_two_for_core() {
    assert_eq!(KIND_HELLO, 1);
    assert_eq!(KIND_CORE, 2);
    assert_eq!(Message::Hello(sample_hello()).kind(), 1);
    assert_eq!(Message::Core(CoreMessage::Heartbeat { seq: 0 }).kind(), 2);
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
}
