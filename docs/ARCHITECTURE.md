# Hop architecture

Hop shares one keyboard and mouse between a Mac and a Windows laptop on the same home network. Push the cursor past a screen edge and it continues on the other computer, and the keyboard follows. Either laptop can control the other. Text copied on one machine can be pasted on the other with that machine's own shortcut, and files dragged across the edge land in a "Received" folder.

## Principles

1. **One native agent per OS.** A single long-running process (a menu-bar app on macOS, a tray app on Windows) owns all OS access. A browser extension cannot capture or inject OS input, so it can only ever be a companion.
2. **An OS-independent core.** All decision logic lives in `hop-core`, a sans-IO state machine: events go in, actions come out, and time is passed in explicitly. It never touches the OS or the network, so it can be tested fully on any machine.
3. **A thin platform layer.** `hop-platform` is the only crate that calls OS APIs, behind small traits.
4. **An evolvable protocol.** Peers exchange a `Hello` listing their capabilities, and a feature turns on only when both sides support it. Unknown message kinds and fields are ignored.
5. **Fail open.** Any error, timeout or disconnect returns both machines to local control and releases every key and button Hop pressed.

## Crates

| Crate | Role |
|---|---|
| `hop-types` | Shared vocabulary: device ids, OS, geometry, input events, session messages |
| `hop-proto` | Wire format: framing, encoding, `Hello` and capability negotiation, size limits |
| `hop-keymap` | Key tables (USB HID, macOS keycodes, Windows scancodes), the Cmd/Ctrl swap, pressed-key tracking |
| `hop-core` | Hand-off state machine, screen layout and edge mapping |
| `hop-config` | TOML configuration |
| `hop-platform` | OS traits, plus the macOS and Windows implementations (M1) |
| `apps/hop` | The agent binary (a CLI for now; the menu-bar/tray app later) |

Planned: `hop-net` (QUIC transport, pairing, discovery; M2), `hop-control` (local control API; M2), `hop-transfer` (file transfer; M5), `apps/hopctl` (CLI client), and the Tauri settings window (M6).

## Process model

**macOS:** one signed `.app` with `LSUIElement` set (menu bar only, no Dock icon).
- Main thread: AppKit work, such as the status item, windows and pasteboard polling.
- Input thread: a `CGEventTap` on its own `CFRunLoop`, plus event injection and cursor park/hide.
- An async runtime: networking, the control API and file I/O.

**Windows:** one tray executable that never runs elevated.
- Main thread: tray and UI.
- Hook thread: `WH_MOUSE_LL` and `WH_KEYBOARD_LL` with a message loop. Callbacks only enqueue, so they stay far below the 1000 ms hook timeout.
- An injection thread.
- An OLE/clipboard thread (STA).
- An async runtime.

On both systems the core runs on its own task. Platform threads feed it events over channels and carry out its actions. Hop's own injected events are tagged so capture can tell them apart from physical input.

## Coordinates and layout

- Each device reports its displays in its own logical coordinates (macOS points, Windows device-independent pixels). The origin is the top-left of its primary display, and y grows downward.
- The layout places each device's coordinate space at an `origin` in a shared global plane. In v1 the config says which side of this device the peer is on (left, right, above, below). The layout builder places the peer's displays, as a group, flush against that side of the local group, centered along the shared edge.
- **Exit-to-entry mapping.** When the cursor pushes past edge E of local display D, the target is chosen like this:
  1. Candidates are displays on *other* devices whose opposite edge lies within `adjacency_tolerance` of D's edge line and whose span overlaps D's span.
  2. Pick the candidate with the largest overlap, breaking ties by the smallest gap.
  3. Map the position along the edge proportionally: `t = (p - D.start) / D.len`, then `entry = N.start + t * N.len`. Corners map to corners.
  4. Place the entry point 1 unit inside N's edge.
- **Crossing rules (from Idle):** all of these must hold.
  - The motion pushes outward at an edge with no local display beyond it.
  - The edge faces a neighbor.
  - No mouse button is physically held (v1).
  - The position is not within `corner_dead_zone` of either end of D's edge span.
  - The cursor has pushed against that edge for at least `edge_delay_ms` (0 means immediately).
  - The neighbor is connected.

## Control hand-off (two-way)

Each device is in exactly one state.

| State | Meaning | Capture mode | Local cursor |
|---|---|---|---|
| **Idle** | Local control | Passive | Normal |
| **Entering(target, seq)** | Crossed an edge, sent `Enter`, waiting for `EnterAck` | Suppress | Parked; physical input buffered (at most 64 events) |
| **Controlling(target, seq)** | Our physical input is forwarded to `target` | Suppress | Parked and hidden |
| **Controlled(by, seq)** | We inject input received from `by` | Passive (our own physical input is never suppressed) | Driven by injection |

A session is identified by (controller id, `seq`). The controller picks a new, increasing `seq` for every `Enter`. Messages that don't belong to the current session are ignored, except `Enter`.

### Rules

1. **Idle → Entering.** The crossing rules hold. Actions:
   - `SetCaptureMode(Suppress)` and `ParkCursor`.
   - Inject local key-ups for every physically held key, so local apps never see a stuck key.
   - Send `Enter { seq, entry, held_keys, source_os }`. `held_keys` are the held *modifier* keys and `entry` is in the target's coordinates.
   - Remember the exit point, so the cursor can be restored there.
2. **Entering → Controlling** on `EnterAck(seq)`: flush the buffered input in order. On `EnterReject`, or no ack within `enter_timeout_ms` (500): go to Idle with `RestoreCursor(exit point)` and `SetCaptureMode(Passive)`, and notify.
3. **A device receives `Enter` from X:**
   - **While Idle:** go to Controlled(X, seq). Then `MoveCursorTo(entry)`, inject key-downs for `held_keys` (the runtime maps them from `source_os`), and send `EnterAck`.
   - **While Controlling or Controlled with another device:** `EnterReject(Busy)`.
   - **While Entering(X)**, meaning both crossed at once: the device with the lower `DeviceId` keeps the controller role. The winner rejects with `Busy`. The loser aborts its own Entering (restoring its cursor and capture mode), then accepts as above. A later reject of the loser's own stale `Enter` is ignored.
4. **Controlling.** Every physical event is forwarded.
   - Pointer motion updates the session's `MotionMark` (n += 1, totals += delta) and is sent as `Motion { seq, mark }`. This message is unreliable and the latest mark wins.
   - Buttons, keys and scroll are sent as `Input { seq, mark, events }`. This message is reliable and ordered, and it carries the current mark.
   - Key codes are sent unmapped. The receiver maps them.
5. **Controlled: applying motion.**
   - On `Motion` or `Input` with `mark.n` greater than the last applied n, apply `delta = mark.total - applied_total` and remember the new totals and n. For `Input`, catch up to the mark this way *before* applying its events, so a click lands where it should even if a motion datagram was lost.
   - Each motion step computes `new = pos + delta * pointer_speed`.
   - If `new` is inside a local display, emit `MoveCursorTo(new)`.
   - Otherwise, find the edge crossed of the display that contains `pos`. If that edge faces a neighbor (same mapping rules), no injected button is held, and the point is outside the corner dead zones, then:
     - send `Leave { seq, to, entry }`, with `entry` in `to`'s coordinates;
     - emit `ReleaseAllInjected`;
     - go to Idle.
   - Otherwise clamp to that display and emit `MoveCursorTo(clamped)`.
6. **Controlled: other events.** Buttons, keys and scroll from `Input` become `Inject { event, source_os }`. Injected buttons are tracked, so exits can be blocked while one is held.
7. **The controller receives `Leave(seq, to)`.**
   - If `to` is this device: go to Idle with `RestoreCursor(entry)` and `SetCaptureMode(Passive)`, and inject local key-downs for physically held modifiers.
   - If `to` is a third device: go to Entering(that device) using `entry`. This is for setups with more than two devices.
8. **Take back.** While Controlled, physical local input is a key or button press, or physical motion totalling at least `takeback_threshold` (4 units) since the session began. On it:
   - send `TakeBack(seq)`;
   - emit `ReleaseAllInjected`;
   - go to Idle, letting the local input through normally.

   When the controller receives `TakeBack`, it goes to Idle with `RestoreCursor(exit point)` and `SetCaptureMode(Passive)`.
9. **Emergency chord.** The default is Left Control + Left Alt/Option + Left Shift + Escape. Seen on the controller while Entering or Controlling, the core:
   - does not forward the Escape;
   - sends `Release(seq)`;
   - goes to Idle and restores the cursor.

   When the controlled side receives `Release`, it emits `ReleaseAllInjected` and goes to Idle.
10. **Liveness.** In any session state, send `Heartbeat(seq)` every `heartbeat_interval_ms` (250) if nothing else was sent in that interval. If nothing arrives from the session peer for `peer_timeout_ms` (1500), or the peer disconnects:
    - the controller goes to Idle and restores its cursor;
    - the controlled side emits `ReleaseAllInjected` and goes to Idle;
    - both notify the UI.

### Invariants

The core's tests check these:
- Capture is Suppress exactly in Entering and Controlling.
- Two devices never control each other at the same time.
- Every exit from Controlled emits `ReleaseAllInjected`.
- No session state survives `peer_timeout_ms` of silence.
- Messages from stale sessions change nothing.

### Core API (shape)

```rust
pub type Millis = u64;

pub enum Event {
    Tick,
    PeerConnected { peer: DeviceId, os: Os, displays: Vec<DisplayInfo> },
    PeerDisconnected { peer: DeviceId },
    PeerMessage { from: DeviceId, msg: CoreMessage },
    LocalInput { event: InputEvent, cursor: Point, physical: bool },
    LayoutChanged(Layout),
}

pub enum Action {
    Send { to: DeviceId, msg: CoreMessage },
    SetCaptureMode(CaptureMode),             // Passive | Suppress
    ParkCursor,
    RestoreCursor { at: Point },
    MoveCursorTo { to: Point },              // while Controlled; drag if buttons held
    Inject { event: InputEvent, source_os: Os },
    ReleaseAllInjected,
    Notify(Notice),
}

impl Core {
    pub fn new(me: DeviceId, os: Os, config: CoreConfig, layout: Layout) -> Self;
    pub fn handle(&mut self, now: Millis, event: Event) -> Vec<Action>;
}
```

## Keyboard

- Keys travel as USB HID usages, which name physical key positions. The receiving OS applies its own keyboard layout, so both laptops should use the same layout (US).
- **Shortcut translation (v1).** When the source and target OS differ (Mac ↔ Windows), the *receiver* swaps Meta (Command or the Windows key) with Control, left with left and right with right.
  - Cmd+C typed on the Mac keyboard arrives on Windows as Ctrl+C.
  - Ctrl+C typed on the PC keyboard arrives on the Mac as Cmd+C.
  - The Mac's Control key acts as the Windows key.
  - Option and Alt are the same HID keys and need no mapping.
- The receiver records the mapped code at key-down, so the key-up always matches even if settings change mid-press. It releases every injected key and button when a session ends.
- Auto-repeat comes from the source OS (key-downs with `repeat: true`).

## Clipboard (v1: text only)

- **Change detection.** The Mac polls `NSPasteboard.changeCount` (there is no change notification). Windows listens for `WM_CLIPBOARDUPDATE`.
- **When text is sent.** The text is read and sent when the cursor *leaves* the machine (and at most 1 MiB is sent), not on every copy. That keeps programmatic pasteboard reads rare. Every cross-machine paste involves a switch anyway.
- **Skipped items.** Items marked as concealed or transient (password managers: `org.nspasteboard.ConcealedType` on the Mac, the clipboard-history exclusion formats on Windows) are never synced.
- **Line endings.** LF and CRLF are normalized on the way in and out.
- **Echo guard.** Hop records the change count of its own writes and ignores them, and every message carries a `generation`.

## Files (M5)

Dragging a file across the edge sends it, and it lands in a "Hop Received" folder in Downloads with a notification and a reveal action. Transfers use their own low-priority stream, stream to disk, are hash-verified, and use temporary names that are renamed only when complete. Exact drop-at-location comes after v1.

## Wire protocol

- **Frames.** A stream frame is `kind (u16, big-endian) | length (u32, big-endian) | body`. A datagram is `kind (u16, big-endian) | body`.
- **Body.** MessagePack with named fields (`rmp-serde`). Unknown fields are ignored and missing ones take defaults, so messages can grow.
- **Kinds.** 1 is `Hello`, 2 is `Core` (`CoreMessage`). Unknown kinds are skipped.
- **`Hello`.** It carries:
  - protocol major and minor versions;
  - app version;
  - device id and name;
  - OS and displays;
  - capabilities, as a list of `{ name, version }`.

  The major version must match. The minor version and the capabilities negotiate down to what both sides support.
- **Limits.** A stream frame is at most 1 MiB, a datagram at most 1200 bytes, and clipboard text at most 1 MiB. Oversized input is rejected, never truncated.

## Transport and security (M2)

- **Transport.** QUIC (`quinn` + `rustls`). Motion goes as datagrams; keys, buttons and session messages on a high-priority stream; clipboard and files on low-priority streams.
- **Pairing.** Each device has a long-term key. Devices pair once with a one-time code (SPAKE2), then pin each other's keys. There is no plaintext mode.
- **Discovery.** mDNS, with a manual-address fallback.
- **Wi-Fi.** On Wi-Fi (the only network here), the receiver smooths motion and the UI shows latency.

## Control API and extensions (M2 and later)

The agent exposes a local JSON-RPC API on a socket only the user can open (Unix socket on macOS, a named pipe checked against the user's SID on Windows), never on an open localhost port. Every client uses it:
- the settings UI;
- the `hopctl` CLI and tests;
- a future browser-extension bridge (Native Messaging);
- a phone-companion bridge;
- the planned AI assistant, through an MCP adapter on the same API, with explicit user approval for actions.

## Configuration

TOML, at `~/Library/Application Support/Hop/config.toml` on macOS and `%APPDATA%\Hop\config.toml` on Windows. It holds:
- the device name and id;
- peers: name, id, OS, side, and an optional address;
- input settings: edge delay, corner dead zone, pointer speed, take-back threshold, emergency chord;
- keymap: `swap_cmd_ctrl`;
- clipboard: enabled, size limit, never sync concealed items;
- the folder for received files;
- the network port. The default 47800 is chosen to avoid Synergy/Deskflow's 24800.

## Testing

- `hop-core`: a deterministic simulation of two devices over a simulated network (latency, loss), plus property tests of the invariants.
- `hop-proto`, `hop-keymap`, `hop-config`: unit and property tests.
- Platform code: manual test checklists on the real machines (M1 and later), then scripted injection tests where possible.
