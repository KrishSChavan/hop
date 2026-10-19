//! Version and capability negotiation between two `Hello`s.

mod common;

use common::{raw_frame, sample_hello};
use hop_proto::{
    Capability, FrameDecoder, Hello, KIND_HELLO, Negotiated, PROTO_MAJOR, PROTO_MINOR, ProtoError,
    caps, decode_datagram, default_capabilities, negotiate,
};
use serde::Serialize;

fn hello(major: u16, minor: u16, capabilities: &[(&str, u16)]) -> Hello {
    Hello {
        proto_major: major,
        proto_minor: minor,
        capabilities: capabilities
            .iter()
            .map(|&(name, version)| Capability::new(name, version))
            .collect(),
        ..sample_hello()
    }
}

#[test]
fn default_capabilities_are_input_keymap_clipboard_text_and_files() {
    assert_eq!(
        default_capabilities(),
        vec![
            Capability::new("input", 1),
            Capability::new("keymap", 1),
            Capability::new("clipboard.text", 1),
            Capability::new("files", 1),
        ]
    );
    assert_eq!(caps::INPUT, "input");
    assert_eq!(caps::KEYMAP, "keymap");
    assert_eq!(caps::CLIPBOARD_TEXT, "clipboard.text");
    assert_eq!(caps::FILES, "files");
    assert_eq!((PROTO_MAJOR, PROTO_MINOR), (1, 0));
}

#[test]
fn two_identical_builds_agree_on_everything_sorted_by_name() {
    let negotiated = negotiate(&sample_hello(), &sample_hello()).expect("compatible");
    assert_eq!(
        negotiated,
        Negotiated {
            proto_major: PROTO_MAJOR,
            proto_minor: PROTO_MINOR,
            capabilities: vec![
                Capability::new(caps::CLIPBOARD_TEXT, 1),
                Capability::new(caps::FILES, 1),
                Capability::new(caps::INPUT, 1),
                Capability::new(caps::KEYMAP, 1),
            ],
        }
    );
}

#[test]
fn the_minor_version_settles_on_the_lower_one() {
    let newer = hello(1, 3, &[]);
    let older = hello(1, 1, &[]);
    assert_eq!(
        negotiate(&newer, &older).expect("compatible").proto_minor,
        1
    );
    assert_eq!(
        negotiate(&older, &newer).expect("compatible").proto_minor,
        1
    );
}

#[test]
fn different_majors_are_incompatible_and_the_error_names_both_versions() {
    let local = hello(1, 0, &[]);
    let remote = hello(2, 3, &[]);
    let err = negotiate(&local, &remote).expect_err("incompatible");
    assert_eq!(
        err,
        ProtoError::Incompatible {
            local_major: 1,
            local_minor: 0,
            remote_major: 2,
            remote_minor: 3,
        }
    );
    let text = err.to_string();
    assert!(text.contains("1.0") && text.contains("2.3"), "{text}");
}

#[test]
fn capabilities_intersect_by_name_at_the_lower_version() {
    let local = hello(
        1,
        0,
        &[
            ("input", 2),
            ("keymap", 1),
            ("clipboard.text", 1),
            ("files", 1),
        ],
    );
    let remote = hello(1, 0, &[("video", 1), ("clipboard.text", 3), ("input", 1)]);
    let negotiated = negotiate(&local, &remote).expect("compatible");
    assert_eq!(
        negotiated.capabilities,
        vec![
            Capability::new("clipboard.text", 1),
            Capability::new("input", 1),
        ]
    );
    assert_eq!(negotiated.capability_version("input"), Some(1));
    assert_eq!(negotiated.capability_version("clipboard.text"), Some(1));
    assert_eq!(negotiated.capability_version("files"), None);
    assert_eq!(negotiated.capability_version("video"), None);
}

#[test]
fn the_result_does_not_depend_on_order_or_on_who_runs_it() {
    let a = hello(1, 2, &[("c", 1), ("a", 4), ("b", 2)]);
    let a_shuffled = hello(1, 2, &[("b", 2), ("c", 1), ("a", 4)]);
    let b = hello(1, 1, &[("b", 5), ("a", 1), ("c", 1), ("d", 1)]);
    let expected = negotiate(&a, &b).expect("compatible");
    assert_eq!(negotiate(&a_shuffled, &b).expect("compatible"), expected);
    assert_eq!(negotiate(&b, &a).expect("compatible"), expected);
    let names: Vec<_> = expected
        .capabilities
        .iter()
        .map(|c| c.name.as_str())
        .collect();
    assert_eq!(names, ["a", "b", "c"]);
}

#[test]
fn a_name_listed_twice_counts_at_its_highest_version() {
    let local = hello(1, 0, &[("input", 2)]);
    let remote = hello(1, 0, &[("input", 1), ("input", 3)]);
    let negotiated = negotiate(&local, &remote).expect("compatible");
    assert_eq!(negotiated.capabilities, vec![Capability::new("input", 2)]);
}

#[test]
fn no_shared_capabilities_is_not_an_error() {
    let negotiated = negotiate(&hello(1, 0, &[("a", 1)]), &hello(1, 0, &[])).expect("compatible");
    assert!(negotiated.capabilities.is_empty());
}

/// A `Hello` from a future major version whose shape changed: `displays` is
/// no longer a list.
#[derive(Serialize)]
struct FutureMajorHello {
    proto_major: u16,
    proto_minor: u16,
    displays: String,
}

#[test]
fn an_undecodable_hello_from_another_major_reports_incompatibility() {
    let body = rmp_serde::to_vec_named(&FutureMajorHello {
        proto_major: 2,
        proto_minor: 5,
        displays: "a new format".to_owned(),
    })
    .expect("encodes");
    let expected = ProtoError::Incompatible {
        local_major: PROTO_MAJOR,
        local_minor: PROTO_MINOR,
        remote_major: 2,
        remote_minor: 5,
    };

    let mut decoder = FrameDecoder::new();
    decoder
        .push(&raw_frame(KIND_HELLO, &body))
        .expect("framing is fine");
    assert_eq!(decoder.pop(), Some(Err(expected.clone())));

    let mut datagram = KIND_HELLO.to_be_bytes().to_vec();
    datagram.extend_from_slice(&body);
    assert_eq!(decode_datagram(&datagram), Err(expected));
}

#[test]
fn an_undecodable_hello_from_the_same_major_is_a_decode_error() {
    let body = rmp_serde::to_vec_named(&FutureMajorHello {
        proto_major: PROTO_MAJOR,
        proto_minor: 9,
        displays: "broken".to_owned(),
    })
    .expect("encodes");
    let mut decoder = FrameDecoder::new();
    decoder
        .push(&raw_frame(KIND_HELLO, &body))
        .expect("framing is fine");
    assert!(matches!(
        decoder.pop(),
        Some(Err(ProtoError::Decode {
            kind: KIND_HELLO,
            ..
        }))
    ));
}
