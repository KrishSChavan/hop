use serde::{Deserialize, Serialize};

use crate::{DeviceId, Edge, InputEvent, KeyCode, Os, Point};

/// Where the cursor should appear on the receiving device, in that device's
/// own logical coordinates.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct EntryPoint {
    pub display_id: u32,
    /// The edge of `display_id` the cursor enters through.
    pub edge: Edge,
    pub position: Point,
}

/// Cumulative pointer motion the controller has sent in the current session.
///
/// `n` increases with every motion update. Because the totals are cumulative,
/// a lost or reordered `Motion` datagram is repaired by the next one, and an
/// `Input` message can carry the mark so the receiver catches up before it
/// applies a click.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct MotionMark {
    pub n: u64,
    pub total_dx: f64,
    pub total_dy: f64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RejectReason {
    /// Already controlling, or being controlled by, another device.
    Busy,
    /// The receiving side is not accepting control right now (screen locked,
    /// paused by the user, missing permissions).
    Unavailable,
}

/// Messages of the control hand-off protocol (see docs/ARCHITECTURE.md).
///
/// `seq` identifies a control session: the controller picks a new, increasing
/// `seq` for every `Enter`, and every later message of that session carries
/// it. Messages whose `seq` does not match the current session are ignored.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CoreMessage {
    /// Controller to target: my cursor crossed into you; you are now
    /// controlled by me. `held_keys` are keys physically held on the
    /// controller at the crossing (the target presses them).
    Enter {
        seq: u64,
        entry: EntryPoint,
        held_keys: Vec<KeyCode>,
        source_os: Os,
    },
    EnterAck {
        seq: u64,
    },
    EnterReject {
        seq: u64,
        reason: RejectReason,
    },
    /// Controlled to controller: the cursor left my screens toward `to`;
    /// `entry` is in `to`'s coordinates.
    Leave {
        seq: u64,
        to: DeviceId,
        entry: EntryPoint,
    },
    /// Controlled to controller: there was physical input on my side, so I am
    /// taking control back.
    TakeBack {
        seq: u64,
    },
    /// Controller to controlled: the session is over (emergency chord,
    /// timeout, user action).
    Release {
        seq: u64,
    },
    /// Controller to controlled, reliable and ordered: buttons, keys and
    /// scroll. Apply motion up to `mark` before applying `events`.
    Input {
        seq: u64,
        mark: MotionMark,
        events: Vec<InputEvent>,
    },
    /// Controller to controlled, unreliable: the latest cumulative motion.
    Motion {
        seq: u64,
        mark: MotionMark,
    },
    /// Liveness while a session is active, both directions.
    Heartbeat {
        seq: u64,
    },
    /// Either direction: the sender's clipboard now holds this text.
    ClipboardText {
        generation: u64,
        text: String,
    },
}

impl CoreMessage {
    /// The session `seq` this message belongs to, if it belongs to one.
    pub fn seq(&self) -> Option<u64> {
        match self {
            CoreMessage::Enter { seq, .. }
            | CoreMessage::EnterAck { seq }
            | CoreMessage::EnterReject { seq, .. }
            | CoreMessage::Leave { seq, .. }
            | CoreMessage::TakeBack { seq }
            | CoreMessage::Release { seq }
            | CoreMessage::Input { seq, .. }
            | CoreMessage::Motion { seq, .. }
            | CoreMessage::Heartbeat { seq } => Some(*seq),
            CoreMessage::ClipboardText { .. } => None,
        }
    }
}
