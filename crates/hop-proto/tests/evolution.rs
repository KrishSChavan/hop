//! Rule 1 of the evolution rules, seen from this build: messages from a
//! newer peer that carry fields this build does not know still decode.
//!
//! The other direction (an older message missing a newer field marked
//! `#[serde(default)]`) is tested next to the body decoder in
//! `src/message.rs`, because today's messages have no such field yet.

mod common;

use std::collections::BTreeMap;

use common::{display, raw_frame, sample_hello};
use hop_proto::{
    Decoded, FrameDecoder, KIND_CORE, KIND_HELLO, MAX_CLIPBOARD_TEXT, Message, ProtoError,
    decode_datagram,
};
use hop_types::{CoreMessage, DisplayInfo, MotionMark, Rect};
use serde::Serialize;

/// `Hello` as a newer build might send it: new fields at the top level
/// (before and after the known ones) and inside nested structs.
#[derive(Serialize)]
struct NewerHello {
    wallpaper: BTreeMap<String, Vec<u64>>,
    proto_major: u16,
    proto_minor: u16,
    app_version: String,
    device_id: String,
    device_name: String,
    os: String,
    displays: Vec<NewerDisplay>,
    capabilities: Vec<NewerCapability>,
    battery_percent: Option<u8>,
}

#[derive(Serialize)]
struct NewerDisplay {
    id: u32,
    frame: Rect,
    scale: f64,
    primary: bool,
    refresh_hz: f64,
}

#[derive(Serialize)]
struct NewerCapability {
    name: String,
    version: u16,
    experimental: bool,
}

fn newer_display(display: &DisplayInfo) -> NewerDisplay {
    NewerDisplay {
        id: display.id,
        frame: display.frame,
        scale: display.scale,
        primary: display.primary,
        refresh_hz: 120.0,
    }
}

#[test]
fn a_newer_hello_with_extra_fields_decodes() {
    let mut expected = sample_hello();
    expected.proto_minor = 7;
    expected.displays = vec![display(1, 0.0, true), display(2, -1920.0, false)];

    let newer = NewerHello {
        wallpaper: BTreeMap::from([("colors".to_owned(), vec![1, 2, 3])]),
        proto_major: expected.proto_major,
        proto_minor: expected.proto_minor,
        app_version: expected.app_version.clone(),
        device_id: expected.device_id.to_string(),
        device_name: expected.device_name.clone(),
        os: "macos".to_owned(),
        displays: expected.displays.iter().map(newer_display).collect(),
        capabilities: expected
            .capabilities
            .iter()
            .map(|capability| NewerCapability {
                name: capability.name.clone(),
                version: capability.version,
                experimental: true,
            })
            .collect(),
        battery_percent: Some(80),
    };
    let body = rmp_serde::to_vec_named(&newer).expect("encodes");

    let mut decoder = FrameDecoder::new();
    decoder
        .push(&raw_frame(KIND_HELLO, &body))
        .expect("framing is fine");
    assert_eq!(
        decoder.pop(),
        Some(Ok(Decoded::Message(Message::Hello(expected))))
    );
}

/// `CoreMessage` as a newer build might send it.
#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
enum NewerCoreMessage {
    Motion {
        seq: u64,
        mark: NewerMark,
        predicted: bool,
    },
    ClipboardText {
        generation: u64,
        text: String,
        source_app: String,
    },
}

#[derive(Serialize)]
struct NewerMark {
    n: u64,
    total_dx: f64,
    velocity: (f64, f64),
    total_dy: f64,
}

#[test]
fn a_newer_core_message_with_extra_fields_decodes() {
    let newer = NewerCoreMessage::Motion {
        seq: 3,
        mark: NewerMark {
            n: 10,
            total_dx: 4.5,
            velocity: (1.0, 2.0),
            total_dy: -1.5,
        },
        predicted: true,
    };
    let mut datagram = KIND_CORE.to_be_bytes().to_vec();
    datagram.extend(rmp_serde::to_vec_named(&newer).expect("encodes"));
    assert_eq!(
        decode_datagram(&datagram),
        Ok(Decoded::Message(Message::Core(CoreMessage::Motion {
            seq: 3,
            mark: MotionMark {
                n: 10,
                total_dx: 4.5,
                total_dy: -1.5,
            },
        })))
    );
}

#[test]
fn limits_still_apply_to_a_newer_message() {
    let newer = NewerCoreMessage::ClipboardText {
        generation: 1,
        text: "a".repeat(MAX_CLIPBOARD_TEXT + 1),
        source_app: "Notes".to_owned(),
    };
    let body = rmp_serde::to_vec_named(&newer).expect("encodes");
    let mut decoder = FrameDecoder::new();
    decoder
        .push(&raw_frame(KIND_CORE, &body))
        .expect("framing is fine");
    assert_eq!(
        decoder.pop(),
        Some(Err(ProtoError::ClipboardTooLarge {
            len: MAX_CLIPBOARD_TEXT + 1,
            max: MAX_CLIPBOARD_TEXT,
        }))
    );
}
