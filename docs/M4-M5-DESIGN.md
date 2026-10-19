# Fixes from the first test, M4 (clipboard) and M5 (files): design

On 2026-10-18 the user ran the app between the two laptops for the first time (Mac driving, PC on the Mac's right) and asked for five things:

1. Crossing to the PC takes a while; coming back is fast.
2. When the cursor is back on the Mac, the PC's cursor stays visible at the PC's left edge. Hide it.
3. Copy and paste between the computers (milestone M4, not built yet).
4. With the Mac's keyboard on the PC, Ctrl+V should paste (today Cmd+V does, and Ctrl+V opens Windows' clipboard history).
5. Dragging a file from the Mac to the PC does nothing (milestone M5, not built yet).

This document was the contract for that work, and now records it as built (2026-10-19): the fixes in release 0.1.1, then M4 and M5 on branch m4-m5, after an independent review. docs/ARCHITECTURE.md keeps the normative behaviour.

## 1. Crossing is slow: the bounce loop

**What the log showed.** The Mac's app log had 2,361 sessions, and 2,331 of them ended with `CursorLeft` 5 to 11 ms after they started. Bursts of up to 114 crossings came one after the other before the cursor stayed on the PC; 87 times the user gave up with the cursor still on the Mac.

**Cause.** Parking warped the Mac's cursor from the right edge to the centre of the main display (`CGWarpMouseCursorPosition`), then detached it. macOS adds a warp's displacement to the delta fields of the next mouse event. Apple's IOHIDSystem source shows the mechanism; GLFW and SDL subtract it; Deskflow fixed the same symptom in July 2026 (PR #9963) by freezing the cursor in place instead. So the first motion after a crossing carried about −755 units. It was buffered while entering and flushed as one step, the PC's cursor (entered 1 unit inside its left edge) left at once through that edge, the PC sent `Leave`, and the Mac, still pushing right, crossed again.

**Fix (macOS backend).**
- **Park in place.** `park()` no longer warps: it remembers the position, detaches the mouse (`CGAssociateMouseAndMouseCursorPosition(false)`) and hides the cursor where it is, at the edge it crossed, where the user's own motion took it. One visible side effect: an auto-hidden Dock or menu bar on the shared edge stays shown while the other computer is used (Deskflow behaves the same).
- **Compensate the warps that remain** (restoring the cursor, and failing open). The displacement (`to − from`) is recorded *before* each `CGWarpMouseCursorPosition`, and the converter subtracts it from the first motion event after the warp **only if that makes the delta smaller** (|raw − w| < |raw|). That rule is harmless on a macOS that does not fold the warp, and on a stale event from before the warp; the record is dropped after 3 physical motion events or once used, ignored for displacements under 2 units, and never touched by Hop's own injected events. A warp that fails is cancelled by noting the reverse move. Every warp that remains runs along the shared edge, so even a missed compensation cannot push the cursor back across.

**Wi-Fi power saving.** With 802.11 power save, an access point holds traffic for a dozing laptop until its next beacon wake-up, often 100 to 300 ms, while traffic a laptop sends goes out at once. That makes the first crossing after a quiet spell slower in exactly one direction (towards the dozing PC). Two changes:
- **Nudges.** While idle, when this computer's own pointer moves towards an edge that leads to a connected peer, stays on screen, and ends level with that edge's part leading there and within `nudge_distance` (300 units) of it, the core emits `Action::Nudge { to }`: at most one per event per peer, and at most every `nudge_interval_ms` (200 ms). A distance of 0 turns nudges off; the distance is capped at 5000 and the interval kept between 50 ms and 10 s. The network sends a tiny datagram of a new kind (4), which every build ignores; receiving it wakes the peer's radio before the `Enter` arrives. Older builds drop unknown datagram kinds, so no capability is needed.
- **Measure.** The agent logs how long each `Enter` waited for its `EnterAck` (`answered_in_ms`), so the next test session shows whether crossing is still slow.

## 2. Hiding the cursor that was left behind

When the cursor leaves a controlled computer through an edge (the session ends with `CursorLeft`), that computer's cursor stays where the last injected move put it. Now it is hidden until that computer's own mouse is used.

**Core.** Two new actions:
- `Action::ConcealCursor`: hide the cursor where it is. Nothing is frozen, moved or suppressed. Emitted only when the controlled side ends a session with `CursorLeft` (in `motion_step`), after `ReleaseAllInjected`.
- `Action::RevealCursor`: show it again. Emitted, while concealed, before anything else in the same action list:
  - on physical pointer input here (motion, a button press, scrolling; keys do not reveal);
  - when a peer enters again (`accept`), before `MoveCursorTo`;
  - on `LocalStall` (fail open).

The core keeps a `cursor_concealed` flag, true only while idle. Not on `Released`, `TakenBack`, timeouts, disconnects or stalls: those are not the cursor visibly leaving, and failing open favours a visible cursor.

**Platform.** `CursorControl` gains `conceal()` and `reveal()` (defaults: `Unsupported` and no-op). Every backend must also reveal **by itself** on the first physical mouse event (motion, button or wheel) and when dropped, so a stalled core thread never leaves an invisible cursor.
- **macOS:** `CGDisplayHideCursor` through the existing `hide()` (with the private `SetsCursorInBackground` switch), sharing the single `hidden` flag with parking so hides and shows stay balanced. A `concealed` flag (with a lock-free mirror the tap reads) lets the tap reveal on a physical mouse event without ever blocking (`try_cursor`, else owe it to the watchdog tick). Parking and restoring clear `concealed`.
- **Windows:** the existing 8×8, alpha-1, topmost, no-activate overlay with a transparent class cursor, placed around the cursor's current position, with no warp, no clip and no swallowing (a "veil", separate from parking). The mouse hook removes it on the first *physical* mouse event (Hop's own injected events, which can reach the hook after the veil went up, never remove it; a peer entering again reveals it explicitly); park, restore, lapse, stop, drop and a watchdog check (cursor no longer inside the veil) remove it too. For 500 ms after it goes up the veil follows Hop's own late injected moves, so it stays over the cursor; with no mouse hook installed it is not put up (an error) or is lifted. It is fail-open by construction: the window dies with the process. `SetSystemCursor` is never used.

## 3. Shortcuts: keys stay as labelled, with a setting

Requirement 9 in docs/DECISIONS.md changes (user, 2026-10-18): with the Mac's keyboard on the PC, Ctrl+V pastes, as on any PC. So the default is now **no swap**: every key arrives as itself. The Mac's Cmd key arrives on the PC as the Windows key, and the PC's Windows key arrives on the Mac as Cmd.

- The swap stays available as a setting, applied, as before, by the **receiving** computer to keys from the other computer's keyboard.
- Config: `[keymap] cmd_ctrl = "keep"` (default) or `"swap"`. The old `swap_cmd_ctrl` key is **retired**: read without a warning, ignored, and dropped at the next save. Every saved file has `swap_cmd_ctrl = true` (the app never offered the choice), so renaming the key is what moves existing installs to the new default once.
- `ModifierPolicy::default()` becomes no swap.
- The app gets a "Shortcuts" choice (see docs/M6-APP.md). Changing it restarts sharing, like the other settings.

## 4. Text clipboard (M4)

**When.** Text travels when the cursor *leaves* a computer, not on every copy:
- the controller sends its clipboard after its `Enter`, at the end of that action batch (or just before the next message), so it is still the next message on the stream;
- the controlled side reads and sends its clipboard *before* its `Leave`, so the controller writes it while still controlling, before it gets its own keyboard back and a local Cmd+V could paste the old text. The cost: the Leave waits for the read (normally a few ms, at most `CLIPBOARD_TIMEOUT`), and keys typed on the controller in that moment go to a session that already ended, as keys typed while a Leave travels always did.

On the ordered control stream the text therefore always arrives before any later key (so Ctrl+V after crossing pastes the new text). The receiving agent writes it to the OS clipboard synchronously, on the core thread, before it handles the next event.

**Only when it changed.** The agent remembers `synced`, the clipboard's change count when both computers last had the same content (after sending, or right after writing the peer's text). It starts at the current change count when sharing starts, so an old clipboard is never pushed on the first crossing; if that count cannot be read, the first crossing only records it (failing closed), and the agent re-reads it every 3 s between sessions until it knows it. A crossing sends only if the change count differs; it then records the new count even if nothing could be sent (no text, a password-manager item, or too large), so it does not retry until the clipboard changes again. A failed read (a timeout) records nothing, so the next crossing tries again. Writing the peer's text records the count `write_text` returns, which is the echo guard. A write that fails is kept and retried after 100, 300, 700 and 1,500 ms, then given up at 2 s; it is dropped when newer text arrives or when something is copied here, and the older local clipboard is never sent back over it. A send that fails because the connection was going away is offered again on the next connection. A failed read is said once per run in Activity ("Copied text did not go to …: reason"), never with any text.

**Never sent:** items marked `org.nspasteboard.ConcealedType`, `TransientType` or `AutoGeneratedType` (Mac); items with `ExcludeClipboardContentFromMonitorProcessing`, `Clipboard Viewer Ignore`, `CanIncludeInClipboardHistory` = 0 or `CanUploadToCloudClipboard` = 0 (Windows); empty text, on both computers, so neither ever empties the other's clipboard; text over `clipboard.max_text_bytes` (never cut short). Text is never logged. On the Mac, a "Paste from Other Apps" setting of always deny makes reads fail with an error that names the setting.

**Bounded platform calls.** A pasteboard read can wait on the app that copied (lazy providers on the Mac, delayed rendering on Windows, up to 30 s). Every `TextClipboard` method returns within `CLIPBOARD_TIMEOUT` (250 ms) or fails, and the work runs on the backend's own thread; a call whose caller already gave up before it started never starts:
- **Windows:** a "hop-clipboard" thread with a message-only owner window and a message loop; `OpenClipboard(owner)` with 5 tries 5 ms apart; `CF_UNICODETEXT` written directly (no delayed rendering); `GetClipboardSequenceNumber` for the change count; CRLF ↔ LF.
- **macOS:** `NSPasteboard.generalPasteboard` on a helper thread (it is not main-thread-only), with a bounded wait. `pasteboardItems`' first item is checked for the concealed markers before `stringForType`.

**Network.** hop-net advertises `clipboard.text` when `NetConfig::clipboard` is on (from `clipboard.enabled`), delivers `ClipboardText` only when negotiated, and keeps at most one pending `ClipboardText` per peer in the event queue (a newer one replaces it at the back), which bounds memory.

**Wire.** Unchanged: `CoreMessage::ClipboardText { generation, text }`, LF line endings, at most 1 MiB.

## 5. Files to the Received folder (M5)

**What the user does.** Drag files from Finder or Explorer to the edge of the screen that leads to the other computer. A drop zone appears along that edge ("Drop to send to Windows PC"). Drop the files on it. They are copied (never moved) to the other computer's **Downloads › Hop Received** folder, which then opens there (one file selected; several, the folder). The cursor does not cross during the drag: the core still never crosses with a button held.

### 5.1 The drop zone (platform)

`hop-platform` gets:

```rust
pub struct DropZone { pub display: Rect, pub edge: Edge, pub start: f64, pub end: f64 }
pub trait DropCatcher: Send {
    fn set_zones(&mut self, zones: Vec<DropZone>, label: &str) -> Result<()>;
    fn take_drops(&mut self) -> Vec<Vec<PathBuf>>;
}
```

`Platform` gets `drops: Box<dyn DropCatcher>`. `NoDrops` is the stub for builds and fakes without one. A zone is the whole edge of one display whose crossing target belongs to the peer, minus the corner dead zones and any part where another display of this computer continues past it. Where the peer's screens are shorter than the edge, the zone still covers the whole edge, because the cursor crosses there too. `Core::edges_toward(peer)` computes the zones; it is empty until the core has handled the peer's `PeerConnected`, so the agent asks after that event and again after each layout change. The agent sets the zones when a peer that negotiated `files` connects (and on layout changes) and clears them when it goes.

The target is shown **only during a drag near a zone**, so it never blocks ordinary clicks or window snapping at the edge, and it always accepts as a copy. After the review it is **two windows per zone**: a *band* 8 units deep at the very edge (it holds the display's outermost column, where a cursor pushed against the edge stops), the only one that takes drops; and a *label* 120 units deep that drags and clicks pass through, shown only while files are over the band. Icons, a Dock, sidebars and other drop targets near the edge keep working.
- **macOS:** the tap (which must never block) hands mouse-down and dragged events to a serial helper queue. That queue snapshots `NSPasteboard(name: .drag).changeCount` at mouse-down and, on dragged events (at most every 50 ms), checks whether it changed and the drag pasteboard holds `public.file-url`. During such a drag, within 150 points of a zone, the band appears: a borderless non-activating `NSPanel` (`hidesOnDeactivate = false`, `ignoresMouseEvents = false`, level at least `statusBar`, one level above the label, collection behaviour `canJoinAllSpaces | fullScreenAuxiliary | stationary | ignoresCycle`, shown with `orderFrontRegardless`), glowing faintly to show where to push. Its view is registered for `NSPasteboardTypeFileURL`: `draggingEntered` shows the label panel (`ignoresMouseEvents = true`, no dragged types) and returns `.copy`; `performDragOperation` reads file URLs (`NSURL`, file URLs only) into the drops queue and answers YES only if it took at least one; the panels hide after the drop, beyond 300 points, or about a second after mouse-up. A press while a target is up, or the tap's watchdog seeing no button held, hides a target whose button-up was missed. AppKit work is dispatched to the main queue without waiting (the app and `hop run` both run a main run loop). File promises (Photos, Mail) are out of scope.
- **Windows:** a "hop-drop" STA thread (`OleInitialize`, a message loop) owns, per zone, the band (a topmost, no-activate, layered tool window at alpha 1, invisible but hit-testable, registered with `RegisterDragDrop` and an `IDropTarget` made with `windows-core`'s `implement`) and the label (`WS_EX_LAYERED | WS_EX_TRANSPARENT`, never registered for drops). The mouse hook posts, never waits: "left button held and moved past the drag threshold within 150 logical units of a zone" and "left button up here". `DragEnter` with `CF_HDROP` shows the label and answers `DROPEFFECT_COPY` if the source allows a copy (otherwise nothing); `DragEnter` with anything else makes the band step aside (hidden for the rest of that drag), so the drag reaches the app underneath; `Drop` copies the paths (`DragQueryFileW`), answers copy at once and hides. After a drag that ends elsewhere the band hides at once; one that ends over the band waits 500 ms (5 s more while files are over it) for a drop that arrives late. `RevokeDragDrop` runs before the band is destroyed.

### 5.2 The transfer protocol

- **Capability** `files`, version 1. Without it, no drop zone is shown.
- **Control messages:** a new message kind 3, `FileMessage`, on the control stream (older builds skip unknown kinds):
  - `Offer { id, files: [{ name, size }] }`, from the sender;
  - `Answer { id, accepted, refusal }`, from the receiver;
  - `Outcome { id, saved, failure }`, from the receiver after the data.
  Limits are checked on encode and decode: at most 1,000 files per offer, names at most 1,024 bytes (`ProtoError::FileOfferTooLarge`). In hop-proto the wire enums are `FileRefusal { Off, TooMany, TooLarge, Busy, BadNames }` and `FileFailure { Incomplete, Corrupt, Disk, Cancelled, TimedOut }`.
- **Data:** after an accepted `Answer`, the sender opens one **unidirectional** QUIC stream for the transfer, at priority −1. It writes the transfer id (8 bytes, big-endian), then for each file in offer order exactly `size` bytes followed by their SHA-256 (32 bytes), then finishes the stream.
- **Stream credit.** Unidirectional streams stay at 0 in the transport settings. After `files` is negotiated on the current connection, the receiver allows 1 (`Connection::set_max_concurrent_uni_streams`), so only a pinned peer that finished `Hello` can open one, and there is at most one incoming file stream per direction. With 2 MiB stream and 4 MiB connection windows, that keeps flow-control room for the control stream.
- **Pace.** File data is written at most at `Limits::file_rate` (12 MiB/s by default), so a large transfer does not fill the Wi-Fi queue in front of the input.
- **Receiver checks** before answering: receiving is on and the folder exists or can be created (otherwise `Off`); at most `max_transfer_files` (1,000) files of at most `max_file_bytes` (4 GiB) each and `max_transfer_bytes` (16 GiB) in total; every name must survive the sanitizer (an empty offer is `BadNames`). Offers are answered one at a time per connection, at most 4 wait, and each connection sends its transfers in drop order.
- **Saving.** Each file is written to a temporary name in the folder (`.<name>.hop-part`), hashed while written, checked against the digest, then marked as coming from another computer and renamed to a free name (`name.ext`, `name (2).ext`, …; never overwriting). The mark: on Windows the `Zone.Identifier` stream with `ZoneId=3` (best effort; a volume without streams leaves the file unmarked, and the log counts it), on the Mac the quarantine attribute, which macOS sets on every file Hop.app creates because its Info.plist has `LSFileQuarantineEnabled`. So Gatekeeper, SmartScreen and Office's Protected View treat received files like downloads. Any failure, a disconnect or a replaced connection deletes the temporary files of that transfer. The folder is not touched at all until the first offer arrives (on the Mac, any access inside Downloads asks for permission once, which must not happen just because sharing started); it is created and swept of leftover temporary files then. Disk I/O never runs on the network's two tokio workers (a blocking thread fed by a bounded channel) and never on the core thread.
- **Names.** The sanitizer keeps only the last path component; removes control, bidi and zero-width characters, line and paragraph separators, `\ / : * ? " < > |` and trailing dots and spaces; refuses `.`, `..` and an empty result; prefixes Windows reserved names (`CON`, `PRN`, `AUX`, `NUL`, `COM0`–`COM9`, `LPT0`–`LPT9` and their superscript-digit forms, with any extension) with an underscore, checked after shortening; and trims to 255 bytes, keeping an extension of up to 32 bytes. A received name that looks like a temporary one gets an underscore, so a sweep can never delete it.
- **Sender** reads files on a blocking thread, sends only regular files (folders and unreadable files are skipped and counted), and fails a file whose length changes while it is read.
- **Timeouts:** up to 2 minutes for the answer to an offer (`Limits::answer_timeout`: the Mac's Downloads prompt can hold it), then 30 s without progress while sending or receiving (`transfer_timeout`). No answer is its own failure (`NoAnswer`): nothing was sent.
- **Logs** never contain file names or contents, only counts and reasons. Names appear only in the app.

### 5.3 Events and the app

- hop-net reports `NetEvent::Transfer(TransferEvent)`: `Received { from, id, files }` (final paths), `Sent { to, id, files, skipped }`, and `Failed { peer, id, outgoing, reason }`. Every transfer ends with exactly one event, on both computers (a refusal is reported on the refusing computer too); unfinished ones become `Failed(Disconnected)` just before `PeerDown` or a replacing `PeerUp`. `TransferFailure` tells this computer's own failure (`Io`) from the other computer's (`PeerFailed`), and has `NoAnswer` for an offer that got no answer.
- On `Received`, the agent shows the files (one file selected in its folder with `open -R` or Explorer `/select,` built with a raw argument; several, the folder), on a short-lived thread whose output goes nowhere, so no path can reach a log. It tells the app with the one new update, `AgentUpdate::FilesReceived { from, names }`, which lists the names in Activity (not in the log). Sent and failed transfers are content-free status lines.
- On the Mac, writing into Downloads needs the Downloads-folder permission once; Hop.app's Info.plist gets `NSDownloadsFolderUsageDescription`, plus `NSDesktopFolderUsageDescription` and `NSDocumentsFolderUsageDescription` in case macOS asks while reading files dropped from those folders, and `LSFileQuarantineEnabled` (above).
- Config: `[transfer] enabled = true` (new; off means no `files` capability, so no drop zone and no receiving) and the existing `received_dir`.

## 6. Interfaces, by crate

These are the shared seams; each work package below owns the code behind them.

- **hop-platform:** `CLIPBOARD_TIMEOUT`; `CursorControl::{conceal, reveal}` (defaulted); `TextClipboard { change_count(&self) -> Result<u64>; read_text(&self, max_bytes: usize) -> Result<Option<String>>; write_text(&mut self, text: &str) -> Result<u64> }`; `to_lf` and `to_crlf` helpers; `DropZone`, `DropCatcher`, `NoDrops`; `Platform::drops`.
- **hop-core:** `Action::{ConcealCursor, RevealCursor, Nudge { to }}`; `CoreConfig::{nudge_distance, nudge_interval_ms}`; `Core::edges_toward(&self, peer) -> Vec<SharedEdge>` with `SharedEdge { display, frame, edge, start, end }`.
- **hop-proto:** `caps::FILES`; message kinds 3 (`FileMessage`) and 4 (`Nudge`, datagram only, no body); `FileMessage`, `FileEntry`, `FileRefusal`, `FileFailure`; the file limits.
- **hop-config:** `KeymapConfig::cmd_ctrl: CmdCtrl { Keep, Swap }` (default `Keep`), the retired `swap_cmd_ctrl`; `TransferConfig::enabled` (default true).
- **hop-keymap:** `ModifierPolicy::default()` is no swap.
- **hop-net:** `NetConfig::{clipboard: bool, files: Option<FilesConfig { received_dir: PathBuf }>}`; `NetEvent::Transfer(TransferEvent)` with `TransferEvent` and `TransferFailure`; `NetHandle::send_files(&self, to, paths: Vec<PathBuf>) -> Result<u64, SendError>`; `NetHandle::nudge(&self, to)`; `PeerUp::{supports_clipboard, supports_files}`; file bounds in `Limits`.
- **hop-transfer** (new crate, pure Rust): file-name sanitizing and free names; the receiving writer (temporary file, hashing, verify, rename, cleanup on drop, sweep); the sending reader (regular files, sizes, hashing).
- **hop-agent:** `Link::{send_files, nudge}`; clipboard sync in the executor; drop zones and transfer events; the shortcuts setting in `settings`; `AgentUpdate::FilesReceived`.

## 7. Work packages

| Package | Owns | Depends on |
|---|---|---|
| T0 Foundation | the interfaces above, compiling, with stubs; hop-config and hop-keymap changes; `Core::edges_toward` | — |
| T1 Core | hop-core: conceal, reveal and nudge logic, scenarios, sim invariants, properties | T0 |
| T2 macOS | hop-platform-macos: park in place, warp compensation, conceal, clipboard, drop zone; Info.plist | T0 |
| T3 Windows | hop-platform-windows: veil, clipboard thread, drop zone; logic tests that run on the Mac | T0 |
| T4 Network | hop-proto, hop-transfer, hop-net: clipboard delivery and bound, files protocol, nudges | T0 |
| T5 Agent and app | hop-agent, apps/hop-app, docs/M6-APP.md, docs/INSTALL.md | T0 |
| T6 Docs | ARCHITECTURE, ROADMAP, DECISIONS, README, CLAUDE.md, M2-DESIGN, the Windows README | all |

All seven were built on 2026-10-19, merged on branch m4-m5, then reviewed from six angles (integration, failing open, security, macOS, Windows, wording), each finding challenged by a second reviewer. The review's fixes are described in the sections above: the two-window drop target, stepping aside for other drags, the marks on received files, the clipboard order on a Leave, retries and failing closed, the answer timeout, and the wording of failures.

## 8. Rules for every package

- Tests never move the cursor, type, read or write the real clipboard, or open windows. Use fakes, in-memory events, private pasteboards, loopback and temporary folders.
- Never log keystrokes, clipboard text, file names or file contents. `CoreMessage`, `Action`, `NetEvent` and `AgentUpdate` derive `Debug`: never log them with `?`/`{:?}`.
- Fail open: any error, timeout or disconnect gives both machines back their own input and shows the cursor.
- OS calls only in the platform backends; every other crate keeps `#![forbid(unsafe_code)]`.
- Done means `cargo fmt --all --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace` and `scripts/dev/check-windows.sh` all pass.
