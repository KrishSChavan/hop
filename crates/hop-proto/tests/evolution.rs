//! Rule 1 of the evolution rules, seen from this build: messages from a
//! newer peer that carry fields this build does not know still decode.
//! And rule 4: an older build skips the kinds added after it (file messages
//! and nudges).
//!
//! The other direction (an older message missing a newer field marked
//! `#[serde(default)]`) is tested next to the body decoder in
//! `src/message.rs`, because today's messages have no such field yet.

mod common;

use std::collections::BTreeMap;

use common::{display, pop_all, raw_frame, sample_hello};
use hop_proto::{
    Decoded, FileEntry, FileFailure, FileMessage, FrameDecoder, KIND_CORE, KIND_FILE, KIND_HELLO,
    KIND_NUDGE, MAX_CLIPBOARD_TEXT, MAX_OFFER_FILES, Message, ProtoError, decode_datagram,
    encode_datagram, encode_frame,
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

/// `FileMessage` as a newer build might send it: new fields in the offer,
/// in its entries and in the outcome.
#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
enum NewerFileMessage {
    Offer {
        id: u64,
        note: String,
        files: Vec<NewerEntry>,
    },
    Outcome {
        id: u64,
        saved: u32,
        failure: Option<String>,
        folder_opened: bool,
    },
}

#[derive(Serialize)]
struct NewerEntry {
    name: String,
    modified: u64,
    size: u64,
    kind: String,
}

fn newer_entry(name: &str, size: u64) -> NewerEntry {
    NewerEntry {
        name: name.to_owned(),
        modified: 1_760_000_000,
        size,
        kind: "public.jpeg".to_owned(),
    }
}

#[test]
fn a_newer_file_message_with_extra_fields_decodes() {
    let newer = NewerFileMessage::Offer {
        id: 12,
        note: "from the drop zone".to_owned(),
        files: vec![newer_entry("photo.jpg", 3_000_000), newer_entry("b", 0)],
    };
    let body = rmp_serde::to_vec_named(&newer).expect("encodes");
    let mut decoder = FrameDecoder::new();
    decoder
        .push(&raw_frame(KIND_FILE, &body))
        .expect("framing is fine");
    assert_eq!(
        decoder.pop(),
        Some(Ok(Decoded::Message(Message::File(FileMessage::Offer {
            id: 12,
            files: vec![
                FileEntry {
                    name: "photo.jpg".to_owned(),
                    size: 3_000_000,
                },
                FileEntry {
                    name: "b".to_owned(),
                    size: 0,
                },
            ],
        }))))
    );

    let newer = NewerFileMessage::Outcome {
        id: 12,
        saved: 1,
        failure: Some("corrupt".to_owned()),
        folder_opened: true,
    };
    let mut datagram = KIND_FILE.to_be_bytes().to_vec();
    datagram.extend(rmp_serde::to_vec_named(&newer).expect("encodes"));
    assert_eq!(
        decode_datagram(&datagram),
        Ok(Decoded::Message(Message::File(FileMessage::Outcome {
            id: 12,
            saved: 1,
            failure: Some(FileFailure::Corrupt),
        })))
    );
}

#[test]
fn limits_still_apply_to_a_newer_file_offer() {
    let newer = NewerFileMessage::Offer {
        id: 1,
        note: String::new(),
        files: (0..=MAX_OFFER_FILES).map(|_| newer_entry("x", 1)).collect(),
    };
    let body = rmp_serde::to_vec_named(&newer).expect("encodes");
    let mut decoder = FrameDecoder::new();
    decoder
        .push(&raw_frame(KIND_FILE, &body))
        .expect("framing is fine");
    assert_eq!(
        decoder.pop(),
        Some(Err(ProtoError::FileOfferTooLarge {
            files: MAX_OFFER_FILES + 1,
            longest_name: 1,
        }))
    );
}

/// An older build knows only kinds 1 and 2, and its decoder returns
/// `Unknown` for every other kind (`framing.rs` checks that for arbitrary
/// kinds). So all it needs to skip a file frame or a nudge is the frame's
/// length: renumbering this build's frames of kinds 3 and 4 to a kind no
/// build knows shows that the frames after them still decode.
#[test]
fn an_older_reader_skips_file_frames_and_nudges() {
    assert!(![KIND_HELLO, KIND_CORE].contains(&KIND_FILE));
    assert!(![KIND_HELLO, KIND_CORE].contains(&KIND_NUDGE));
    let unknown_to_everyone = 0xfffe;
    let renumbered = |message: &Message| {
        let mut frame = encode_frame(message).expect("encodes");
        frame[..2].copy_from_slice(&u16::to_be_bytes(unknown_to_everyone));
        frame
    };
    let heartbeat = Message::Core(CoreMessage::Heartbeat { seq: 1 });
    let offer = Message::File(common::offer(3, 40));
    let mut stream = renumbered(&offer);
    stream.extend(encode_frame(&heartbeat).expect("encodes"));
    stream.extend(renumbered(&Message::Nudge));
    stream.extend(encode_frame(&heartbeat).expect("encodes"));
    let mut decoder = FrameDecoder::new();
    decoder.push(&stream).expect("framing is fine");
    let unknown = Ok(Decoded::Unknown {
        kind: unknown_to_everyone,
    });
    let known = Ok(Decoded::Message(heartbeat));
    assert_eq!(
        pop_all(&mut decoder),
        vec![unknown.clone(), known.clone(), unknown, known]
    );

    // The same for datagrams: an older build drops a nudge as an unknown
    // kind, which is what its datagram reader does with every kind but
    // `Core`.
    let mut nudge = encode_datagram(&Message::Nudge).expect("fits");
    assert_eq!(nudge, KIND_NUDGE.to_be_bytes());
    nudge[..2].copy_from_slice(&u16::to_be_bytes(unknown_to_everyone));
    assert_eq!(
        decode_datagram(&nudge),
        Ok(Decoded::Unknown {
            kind: unknown_to_everyone
        })
    );
}
