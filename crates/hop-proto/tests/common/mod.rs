//! Sample messages and helpers shared by the integration tests.

// Each test file uses a different subset of these helpers.
#![allow(dead_code)]

use hop_proto::{
    Decoded, FileEntry, FileFailure, FileMessage, FileRefusal, FrameDecoder, Hello, Message,
    PROTO_MAJOR, PROTO_MINOR, ProtoError, default_capabilities,
};
use hop_types::{
    CoreMessage, DeviceId, DisplayInfo, Edge, EntryPoint, InputEvent, KeyCode, MotionMark,
    MouseButton, Os, Point, Rect, RejectReason, ScrollUnit, Size,
};

pub fn display(id: u32, x: f64, primary: bool) -> DisplayInfo {
    DisplayInfo {
        id,
        frame: Rect {
            origin: Point { x, y: 0.0 },
            size: Size {
                width: 1512.0,
                height: 982.0,
            },
        },
        scale: 2.0,
        primary,
    }
}

/// A realistic `Hello`: a 64-character key fingerprint as the id and two
/// displays.
pub fn sample_hello() -> Hello {
    Hello {
        proto_major: PROTO_MAJOR,
        proto_minor: PROTO_MINOR,
        app_version: "0.1.0".to_owned(),
        device_id: DeviceId::new(
            "9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08",
        ),
        device_name: "Krish's MacBook Air".to_owned(),
        os: Os::MacOs,
        displays: vec![display(1, 0.0, true), display(2, 1512.0, false)],
        capabilities: default_capabilities(),
    }
}

pub fn entry(edge: Edge) -> EntryPoint {
    EntryPoint {
        display_id: 1,
        edge,
        position: Point { x: 0.0, y: 400.5 },
    }
}

/// At least one sample of every `CoreMessage` variant, and of every enum
/// value inside them. `variant_name` keeps this list honest.
pub fn every_core_message() -> Vec<CoreMessage> {
    let mark = MotionMark {
        n: 42,
        total_dx: 10.5,
        total_dy: -3.25,
    };
    vec![
        CoreMessage::Enter {
            seq: 1,
            entry: entry(Edge::Left),
            held_keys: vec![KeyCode::LEFT_META, KeyCode::keyboard(0x04)],
            source_os: Os::MacOs,
        },
        CoreMessage::Enter {
            seq: 2,
            entry: entry(Edge::Top),
            held_keys: vec![],
            source_os: Os::Windows,
        },
        CoreMessage::Enter {
            seq: 3,
            entry: entry(Edge::Bottom),
            held_keys: vec![KeyCode::consumer(0xE9)],
            source_os: Os::Linux,
        },
        CoreMessage::EnterAck { seq: 3 },
        CoreMessage::EnterReject {
            seq: 4,
            reason: RejectReason::Busy,
        },
        CoreMessage::EnterReject {
            seq: 5,
            reason: RejectReason::Unavailable,
        },
        CoreMessage::Leave {
            seq: 6,
            to: DeviceId::new("windows-laptop"),
            entry: entry(Edge::Right),
        },
        CoreMessage::TakeBack { seq: 7 },
        CoreMessage::Release { seq: 8 },
        CoreMessage::Input {
            seq: 9,
            mark,
            events: vec![
                InputEvent::PointerMotion { dx: 1.5, dy: -2.0 },
                InputEvent::Button {
                    button: MouseButton::Left,
                    pressed: true,
                },
                InputEvent::Button {
                    button: MouseButton::Right,
                    pressed: false,
                },
                InputEvent::Button {
                    button: MouseButton::Middle,
                    pressed: true,
                },
                InputEvent::Button {
                    button: MouseButton::Back,
                    pressed: true,
                },
                InputEvent::Button {
                    button: MouseButton::Forward,
                    pressed: false,
                },
                InputEvent::Scroll {
                    dx: 0.0,
                    dy: -1.0,
                    unit: ScrollUnit::Line,
                },
                InputEvent::Scroll {
                    dx: 12.25,
                    dy: 3.0,
                    unit: ScrollUnit::Pixel,
                },
                InputEvent::Key {
                    code: KeyCode::LEFT_SHIFT,
                    pressed: true,
                    repeat: false,
                },
                InputEvent::Key {
                    code: KeyCode::keyboard(0x04),
                    pressed: true,
                    repeat: true,
                },
            ],
        },
        CoreMessage::Input {
            seq: 10,
            mark: MotionMark::default(),
            events: vec![],
        },
        CoreMessage::Motion { seq: 11, mark },
        CoreMessage::Heartbeat { seq: 12 },
        CoreMessage::ClipboardText {
            generation: 13,
            text: "héllo\r\nwörld 👋".to_owned(),
        },
        CoreMessage::ClipboardText {
            generation: 14,
            text: String::new(),
        },
    ]
}

/// Every `CoreMessage` variant name. The match has no catch-all arm, so a
/// new variant breaks the build here until it gets a sample above.
pub fn variant_name(message: &CoreMessage) -> &'static str {
    match message {
        CoreMessage::Enter { .. } => "enter",
        CoreMessage::EnterAck { .. } => "enter_ack",
        CoreMessage::EnterReject { .. } => "enter_reject",
        CoreMessage::Leave { .. } => "leave",
        CoreMessage::TakeBack { .. } => "take_back",
        CoreMessage::Release { .. } => "release",
        CoreMessage::Input { .. } => "input",
        CoreMessage::Motion { .. } => "motion",
        CoreMessage::Heartbeat { .. } => "heartbeat",
        CoreMessage::ClipboardText { .. } => "clipboard_text",
    }
}

pub const VARIANT_COUNT: usize = 10;

/// Every value of `FileRefusal`; `refusal_name` keeps it honest.
pub const EVERY_REFUSAL: [FileRefusal; 5] = [
    FileRefusal::Off,
    FileRefusal::TooMany,
    FileRefusal::TooLarge,
    FileRefusal::Busy,
    FileRefusal::BadNames,
];

/// Every value of `FileFailure`; `failure_name` keeps it honest.
pub const EVERY_FAILURE: [FileFailure; 5] = [
    FileFailure::Incomplete,
    FileFailure::Corrupt,
    FileFailure::Disk,
    FileFailure::Cancelled,
    FileFailure::TimedOut,
];

/// At least one sample of every `FileMessage` variant, and an answer and an
/// outcome with every refusal and failure. `file_variant_name` keeps this
/// list honest. Each fits in a datagram, like every other sample.
pub fn every_file_message() -> Vec<FileMessage> {
    let mut messages = vec![
        FileMessage::Offer {
            id: 1,
            files: vec![
                FileEntry {
                    name: "Quarterly report.pdf".to_owned(),
                    size: 48_213,
                },
                FileEntry {
                    name: "héllo wörld 👋.txt".to_owned(),
                    size: 0,
                },
                FileEntry {
                    name: "../not a path\\at all".to_owned(),
                    size: u64::MAX,
                },
            ],
        },
        FileMessage::Offer {
            id: u64::MAX,
            files: vec![],
        },
        FileMessage::Answer {
            id: 1,
            accepted: true,
            refusal: None,
        },
        FileMessage::Outcome {
            id: 1,
            saved: 3,
            failure: None,
        },
    ];
    messages.extend(EVERY_REFUSAL.map(|refusal| FileMessage::Answer {
        id: 2,
        accepted: false,
        refusal: Some(refusal),
    }));
    messages.extend(EVERY_FAILURE.map(|failure| FileMessage::Outcome {
        id: 3,
        saved: 1,
        failure: Some(failure),
    }));
    messages
}

/// Every `FileMessage` variant name. No catch-all arm: a new variant breaks
/// the build here until it gets a sample above.
pub fn file_variant_name(message: &FileMessage) -> &'static str {
    match message {
        FileMessage::Offer { .. } => "offer",
        FileMessage::Answer { .. } => "answer",
        FileMessage::Outcome { .. } => "outcome",
    }
}

pub const FILE_VARIANT_COUNT: usize = 3;

/// Every refusal's wire name. No catch-all arm, as above.
pub fn refusal_name(refusal: FileRefusal) -> &'static str {
    match refusal {
        FileRefusal::Off => "off",
        FileRefusal::TooMany => "too_many",
        FileRefusal::TooLarge => "too_large",
        FileRefusal::Busy => "busy",
        FileRefusal::BadNames => "bad_names",
    }
}

/// Every failure's wire name. No catch-all arm, as above.
pub fn failure_name(failure: FileFailure) -> &'static str {
    match failure {
        FileFailure::Incomplete => "incomplete",
        FileFailure::Corrupt => "corrupt",
        FileFailure::Disk => "disk",
        FileFailure::Cancelled => "cancelled",
        FileFailure::TimedOut => "timed_out",
    }
}

/// Every sample message: `Hello` first, then the core and file messages and
/// a nudge.
pub fn every_message() -> Vec<Message> {
    let mut messages = vec![Message::Hello(sample_hello())];
    messages.extend(every_core_message().into_iter().map(Message::Core));
    messages.extend(every_file_message().into_iter().map(Message::File));
    messages.push(Message::Nudge);
    messages
}

/// A file offer of `count` files whose names are each `name_len` bytes of
/// ASCII.
pub fn offer(count: usize, name_len: usize) -> FileMessage {
    FileMessage::Offer {
        id: 7,
        files: (0..count)
            .map(|index| FileEntry {
                name: format!("{index:0>width$}", width = name_len)
                    .chars()
                    .take(name_len)
                    .collect(),
                size: u64::MAX - index as u64,
            })
            .collect(),
    }
}

/// A frame built by hand, for bodies `encode_frame` would never produce.
pub fn raw_frame(kind: u16, body: &[u8]) -> Vec<u8> {
    let len = u32::try_from(body.len()).expect("test bodies are small");
    let mut frame = kind.to_be_bytes().to_vec();
    frame.extend_from_slice(&len.to_be_bytes());
    frame.extend_from_slice(body);
    frame
}

/// Pops every complete frame.
pub fn pop_all(decoder: &mut FrameDecoder) -> Vec<Result<Decoded, ProtoError>> {
    std::iter::from_fn(|| decoder.pop()).collect()
}
