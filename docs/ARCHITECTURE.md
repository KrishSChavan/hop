# Hop architecture

Hop shares one keyboard and mouse between a Mac and a Windows laptop on the same home network. Push the cursor past a screen edge and it continues on the other computer, and the keyboard follows. Either laptop can control the other. Text copied on one machine can be pasted on the other with that machine's own shortcut. Files dragged to the shared edge drop on a target that appears there, and are copied to the other computer's Downloads › Hop Received folder.

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
| `hop-keymap` | Key tables (USB HID, macOS keycodes, Windows scancodes), the optional Cmd/Ctrl swap, pressed-key tracking |
| `hop-core` | Hand-off state machine, screen layout and edge mapping |
| `hop-config` | TOML configuration |
| `hop-platform` | OS traits, plus the macOS and Windows implementations (M1) |
| `hop-net` | QUIC link with pinned keys, pairing with a one-time code, mDNS discovery (M2); clipboard text, nudges and file transfers (M4, M5) |
| `hop-transfer` | Files on disk (M5): cleaning received names and finding free ones, saving under temporary names with a SHA-256 check, sweeping leftovers, reading files to send |
| `hop-selftest` | The guided platform self-test and the latency probe (M1) |
| `hop-agent` | The agent (core thread, platform and network wiring), clipboard sync, drop zones and transfer status, the pairing flow, settings, the log, and the `hop` command line; shared by both apps (M6) |
| `apps/hop` | The `hop` command line |
| `apps/hop-app` | The app: one window (eframe/egui) to pair, set where the other computer is, choose the shortcuts, and start and stop sharing (M6, see [M6-APP.md](M6-APP.md)) |

Planned: `hop-control` (local control API), `apps/hopctl` (CLI client), and the menu-bar and tray icon (M6).

## Process model

**macOS:** one signed `.app`. Its Info.plist sets `LSUIElement`, so the `hop` commands that scripts start through LaunchServices show no Dock icon. The app's window asks for a regular app (Dock icon, app menu) while it is open.
- Main thread: AppKit work: the app's window (winit and egui) and the drop-zone panels. Other threads dispatch work to it and never wait for it. Later: the status item.
- "hop-capture": a `CGEventTap` on its own `CFRunLoop`, with the suppression-lease watchdog. It also shows a hidden cursor again at the first physical mouse event, and watches the left button for file drags. It never waits on anything.
- "hop-clipboard": runs every `NSPasteboard` call; each one is bounded by `CLIPBOARD_TIMEOUT` (250 ms). A serial queue reads the drag pasteboard.
- The core thread ("hop-agent"): the core, injection and the cursor (parking, hiding).
- An async runtime ("hop-net", two workers): networking. File reads and writes run on blocking threads, never on the workers or the core thread.

**Windows:** one windowed executable (`Hop.exe`, the tray icon later) that never runs elevated.
- Main thread: the window (winit and egui); later the tray.
- "hop-input": `WH_MOUSE_LL` and `WH_KEYBOARD_LL` with a message loop, Raw Input, and the small windows that hide a parked or left-behind cursor. Callbacks only enqueue or post, so they stay far below the 1000 ms hook timeout.
- "hop-clipboard": a message-only window that owns what Hop writes to the clipboard, with its own message loop; every clipboard call is bounded by `CLIPBOARD_TIMEOUT`.
- "hop-drop": an OLE thread (`OleInitialize`, a message loop) that owns the drop-zone windows, registered with `RegisterDragDrop`.
- The core thread ("hop-agent"): the core and injection (`SendInput`).
- An async runtime ("hop-net", two workers), as on the Mac.

On both systems the core runs on its own thread. Platform threads feed it events over channels and carry out its actions. Hop's own injected events are tagged so capture can tell them apart from physical input.

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
| **Idle** | Local control | Passive | Normal, or hidden where it stopped after a session that left through an edge, until this computer's own mouse is used |
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
   - **While Idle:** go to Controlled(X, seq). Then show a hidden cursor again (`RevealCursor`, first), `MoveCursorTo(entry)`, inject key-downs for `held_keys` (the runtime maps them from `source_os`), and send `EnterAck`.
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
     - emit `ConcealCursor`: this device's cursor is hidden where it is, since the cursor is now on the other device (see rule 11);
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
11. **The cursor left behind.** When a session ends because the cursor left the controlled device through an edge (rule 5), that device hides its cursor where it is (`ConcealCursor`): nothing is moved, frozen or suppressed. It shows it again (`RevealCursor`, always the first action of its list) on physical pointer input there (motion, a button press, scrolling; keys do not), when a peer enters (rule 3), and on a stall. The platform also shows it by itself at the first physical mouse event and when Hop stops, so a stalled core never leaves an invisible cursor. Other endings (take-back, release, timeouts, disconnects) never hide it.
12. **Nudges.** While idle, physical motion that stays on screen and heads toward the part of an edge that leads to a connected peer, ending within `nudge_distance` (300 units; 0 turns this off) of it, makes the core send that peer a nudge (`Nudge { to }`), at most every `nudge_interval_ms` (200). A nudge is a tiny datagram every build ignores; receiving it wakes a dozing Wi-Fi radio before the `Enter` arrives.

### Details settled while building M0

The rules above leave some cases open. `hop-core` decides them as follows, and its rustdoc on `Core` is the authoritative list.

1. **Session numbers by parity.** Each device pair splits session numbers by parity: the device with the lower `DeviceId` uses odd numbers and the other uses even ones. Heartbeats carry only a `seq`, so without this two devices whose counters both started at 1 could keep each other's dead sessions alive ("ghost sessions").
2. **Best-effort goodbyes.** When a session ends by enter timeout, peer timeout, buffer overflow or a lost tie-break, the controller still sends `Release` and the controlled side still sends `TakeBack`. The tie-break loser also sends `Release` for its own abandoned `Enter`.
3. **Repeated `Enter`.** An `Enter` from the current session peer replaces the session (ended as "superseded"). A duplicate `Enter` only refreshes liveness. Any other device gets `Busy`.
4. **Entering buffer.** More than 64 buffered events abandons the crossing, because dropping only some of them could lose a key-up. Consecutive motion and scroll events are merged, so normal mouse movement doesn't fill the buffer.
5. **Edge disarm.** After a crossing or session that went out through an edge ends without the cursor coming back, that edge is disarmed until the cursor stops pushing against it. This prevents repeated crossings toward a busy or silent peer.
6. **Heartbeat timing.** Motion datagrams don't count as "something sent" when deciding whether a heartbeat is due. The heartbeat interval is capped at a third of the peer timeout.
7. **Layout first.** Within one `handle` call, layout changes are applied before timers fire, so a timeout never restores the cursor onto a display that was just unplugged.
8. **No displays.** A device with no displays answers `Enter` with `EnterReject(Unavailable)`. A peer whose displays fail validation stays connected but can't be entered, and the UI gets a notice.
9. **Entry inset.** The 1-unit inset is measured from the edge line: an entry through a right or bottom edge lands on the last column or row (x = 1511 on a 1512-wide screen), and one through a left or top edge lands 1 unit in.
10. **Take-back threshold.** Measured as path length: the total distance the controlled device's own pointer moved since the session began.
11. **Monitors on the controlled side.** On the controlled device, motion follows the cursor across touching displays of that device, and the exit is judged at the last display it passes. Without an exit, the cursor stops where an OS would put it.
12. **`LocalInput.cursor`.** This is the position *before* the motion, and the core treats `cursor + delta` as where the motion tries to go. A macOS event tap reports the position after the motion, so the platform layer must pass the previous position.

### Invariants

The core's tests check these:
- Capture is Suppress exactly in Entering and Controlling.
- The cursor is hidden by `ConcealCursor` only while Idle, and is never parked or moved while hidden; `RevealCursor` comes first in its action list.
- `Nudge` goes out only while Idle, to a connected peer.
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
    LocalStall,                               // the runtime can no longer vouch for local input
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
    ConcealCursor,                            // rule 11
    RevealCursor,
    Nudge { to: DeviceId },                   // rule 12
    Notify(Notice),
}

impl Core {
    pub fn new(me: DeviceId, os: Os, config: CoreConfig, layout: Layout) -> Self;
    pub fn handle(&mut self, now: Millis, event: Event) -> Vec<Action>;
    // The parts of this device's edges that lead to `peer` (whole edges
    // minus the corner dead zones), for the drop zones.
    pub fn edges_toward(&self, peer: &DeviceId) -> Vec<SharedEdge>;
}
```

## Keyboard

- Keys travel as USB HID usages, which name physical key positions. The receiving OS applies its own keyboard layout, so both laptops should use the same layout (US).
- **Keys as labelled (the default since 2026-10-18).** Every key arrives as itself. Ctrl stays Ctrl, and the Mac's Command key and the Windows key are the same HID key (Meta): with the Mac's keyboard on the PC, Ctrl+C copies and Cmd acts as the Windows key; with the PC's keyboard on the Mac, the Windows key acts as Command.
- **The optional swap** (`keymap.cmd_ctrl = "swap"`, the app's Shortcuts choice). The *receiving* computer applies it to keys from the other computer's keyboard, when exactly one of the two is a Mac: Meta and Control trade places, left with left and right with right. Cmd+C typed on the Mac then arrives on Windows as Ctrl+C. Each computer's choice covers the other computer's keyboard only.
- Option and Alt are the same HID keys and are never mapped.
- The receiver records the mapped code at key-down, so the key-up always matches even if settings change mid-press. It releases every injected key and button when a session ends.
- Auto-repeat comes from the source OS (key-downs with `repeat: true`).

## Clipboard (M4: text only)

- **When.** Text travels when the cursor *leaves* a computer, never on a copy. The controller sends its clipboard right after its `Enter`; the controlled side sends its clipboard just before its `Leave`, so the controller has written it before it gets its own keyboard back. On the ordered control stream the text therefore arrives before the next key, and the receiving agent writes it to the OS clipboard before it handles the next event.
- **Only when it changed.** Nothing polls or listens. As the cursor leaves, the agent reads the clipboard's change count (`NSPasteboard.changeCount`, `GetClipboardSequenceNumber`) and reads the text only if the count differs from the one both computers last shared. That count starts at its value when sharing starts, so an older clipboard is never pushed; if it cannot be read then, the first crossing only records it. Writing the other computer's text records the count the write returns, which is the echo guard.
- **Never sent.** Items marked `org.nspasteboard.ConcealedType`, `TransientType` or `AutoGeneratedType` (Mac); items with `ExcludeClipboardContentFromMonitorProcessing`, `Clipboard Viewer Ignore`, `CanIncludeInClipboardHistory` = 0 or `CanUploadToCloudClipboard` = 0 (Windows); empty text; text over `clipboard.max_text_bytes` (1 MiB), which is never cut short. Clipboard text is never logged.
- **Bounded.** Every platform clipboard call runs on the backend's own thread and returns within `CLIPBOARD_TIMEOUT` (250 ms), or fails, because reading can wait on the app that copied. A write that fails is retried for about 2 seconds.
- **Line endings.** LF on the wire; Windows gets CRLF.
- **Network.** The `clipboard.text` capability; at most one `ClipboardText` per peer waits for the agent (a newer one replaces it).

## Files (M5)

- **Dropping.** While a file drag (from Finder or Explorer) comes near the edge that leads to the other computer, a drop target appears along it: a thin band at the very edge takes the drop, and a label ("Drop to send to Windows PC") that drags and clicks pass through shows while files are over the band. It appears only during a drag, never blocks other drag and drop or clicks, and always takes a copy, never a move. The cursor never crosses with a button held.
- **Arriving.** The files are copied to the other computer's Downloads › Hop Received folder, which then opens there (one file selected; several, the folder), and the app's Activity names them. They are marked as downloaded (the Mac's quarantine, Windows' Mark of the Web), so the usual system checks apply when they are opened. File names are never logged.
- **Protocol.** Capability `files`, version 1. `FileMessage` (kind 3) on the control stream: `Offer { id, files }`, `Answer { id, accepted, refusal }`, `Outcome { id, saved, failure }`. After an accepted answer, one unidirectional QUIC stream per transfer at priority −1 carries the id (8 bytes), then each file's bytes followed by its SHA-256. One such stream per direction at a time, allowed only after `files` is negotiated; data is paced to 12 MiB/s.
- **Bounds.** At most 1,000 files, 4 GiB each and 16 GiB per transfer; offers are answered one at a time, with at most 4 waiting. The answer may wait up to 2 minutes (the Mac asks once for the Downloads folder); after that, 30 seconds without progress ends a transfer.
- **Saving.** Each file is written as `.<name>.hop-part`, checked, then renamed to a free name, never overwriting. Any failure deletes that transfer's temporary files. The folder is not touched until the first offer arrives, then created and swept of leftovers.
- **Reports.** Every transfer ends with exactly one event, on both computers. Details: [M4-M5-DESIGN.md](M4-M5-DESIGN.md), section 5.
- Exact drop-at-location comes after v1.

## Wire protocol

- **Frames.** A stream frame is `kind (u16, big-endian) | length (u32, big-endian) | body`. A datagram is `kind (u16, big-endian) | body`.
- **Body.** MessagePack with named fields (`rmp-serde`). Unknown fields are ignored and missing ones take defaults, so messages can grow.
- **Kinds.** 1 is `Hello`, 2 is `Core` (`CoreMessage`), 3 is `File` (`FileMessage`, control stream, only after `files` is negotiated), 4 is `Nudge` (datagram only, no body; every build ignores it). Unknown kinds are skipped.
- **`Hello`.** It carries:
  - protocol major and minor versions;
  - app version;
  - device id and name;
  - OS and displays;
  - capabilities, as a list of `{ name, version }`.

  The major version must match. The minor version and the capabilities negotiate down to what both sides support.
- **Limits.** A stream frame body is at most 1 MiB + 64 KiB (room for 1 MiB of clipboard text plus its envelope). A whole datagram, kind included, is at most 1200 bytes. Clipboard text is at most 1 MiB of UTF-8. A file offer lists at most 1,000 files, each name at most 1,024 bytes. Oversized input is rejected, never truncated, and frames are rejected from their header before the body is buffered.
- **Evolution rules.** The crate docs of `hop-proto` set the rules for changing messages: add fields only with defaults; never rename, remove or retype a field; send new enum variants only to peers that negotiated the capability that introduced them.

## Transport and security (M2)

- **Transport.** QUIC (`quinn` + `rustls`). Motion and nudges go as datagrams; keys, buttons, session messages, clipboard text and file messages on the control stream; file data on a unidirectional stream per transfer at priority −1.
- **Pairing.** Each device has a long-term key. Devices pair once with a one-time code (SPAKE2), then pin each other's keys. There is no plaintext mode.
- **Discovery.** mDNS, with a manual-address fallback.
- **Wi-Fi.** A dozing Wi-Fi radio holds incoming packets until it next wakes, often 100 to 300 ms, so the core nudges the other computer as the cursor heads for the shared edge (rule 12), and the log records how long each `Enter` waited for its answer (`answered_in_ms`).
- **Ports.** UDP 47800 carries sessions. Pairing (`hop pair`, or the app's pairing section) runs its own listener, UDP 47801, and only while pairing. `hop latency` uses UDP 47802.
- **Full design.** The details, including the security review that shaped them, are in [M2-DESIGN.md](M2-DESIGN.md).

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
- keymap: `cmd_ctrl`, `"keep"` (the default) or `"swap"`. The older `swap_cmd_ctrl` is retired: read without a warning, ignored, and dropped at the next save;
- clipboard: enabled and size limit (password-manager items are never synced, and there is no setting for that);
- transfer: `enabled` (default true; off means no drop target and no receiving) and `received_dir` (default Downloads/Hop Received);
- the network port. The default 47800 is chosen to avoid Synergy/Deskflow's 24800.

## Testing

- `hop-core`: a deterministic simulation of two devices over a simulated network (latency, loss), plus property tests of the invariants.
- `hop-proto`, `hop-keymap`, `hop-config`: unit and property tests.
- Platform code: manual test checklists on the real machines (M1 and later), then scripted injection tests where possible.
