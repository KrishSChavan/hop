# Roadmap

Each milestone ends with something you can try. Estimates assume Claude writes the code; the pace is mostly set by test sessions on the two laptops.

## M0: Foundation (done)

- **Scope:** the workspace, shared types, wire protocol, keymap, the hand-off state machine (simulated, no OS access), configuration, CI, and docs.
- **Done when:** `cargo test --workspace` passes on macOS and Windows, and a simulated two-device session crosses, types, takes back and times out correctly.
- **Result:** 267 tests, including property tests over random two-device sessions. Try it with `cargo run -p hop -- simulate`.

## M1: Platform spikes on the real machines

**Status (2026-10-18):** both platform halves are done, and the Rust approach is a go on both. All 7 self-test steps pass on the user's Mac (macOS 26.6.2) and on the PC (Windows 11 Home, build 26200: a 150% laptop screen beside a 100% external one, and a 1000 Hz Razer mouse). On the PC, `cargo test --workspace` also passes (551 tests).

Lessons from the first runs on the Mac:
- Never point Hop's output into a folder macOS protects (Desktop, Documents, Downloads). Hop has no access there, so LaunchServices refused to start it at all, with error -10810. Logs go to `~/Library/Logs/Hop`.
- An accessory (menu-bar-only) app needs a main menu with the Edit commands, or text fields ignore Cmd+A, Cmd+C and Cmd+V.
- Signing every build with the one personal identity keeps the Accessibility grant across rebuilds.
- A self-test the user follows must wait for them before each step, not run on a timer.

Lessons from the first run on the PC:
- Step 5 failed once only because the mouse was still during its 3 seconds: the cursor is hidden then, so nothing shows when to move it. It passed when rerun with the mouse moving (`hop selftest --steps 5`).

- **Windows (on the PC):**
  - low-level mouse and keyboard hooks on a dedicated thread, with a watchdog;
  - SendInput at absolute coordinates with scancodes;
  - cursor hiding;
  - edge detection with mixed DPI scaling.
- **macOS (only after IT approval for the work Mac):**
  - an event tap that is passive while local and suppressing while remote;
  - injection with event numbers;
  - cursor park and hide;
  - the Accessibility prompt flow.
- **Both:** a crude network link that sends motion and keys one way, and Wi-Fi latency measurements.
- **Done when:** one laptop crudely drives the other, latency numbers are recorded, and Rust is go or no-go (ADR-2).

## M2: The link

**Status (2026-10-18):** built and tested on loopback; not yet run between the real laptops. It contains:
- `hop-net`: identity, mutually pinned QUIC with Ed25519 and no resumption, SPAKE2 pairing bound to the TLS session, and mDNS discovery;
- `hop run`, `hop pair` and `scripts/macos/run-agent.sh`;
- 568 tests across the workspace.

The design was security-reviewed before implementation (see [M2-DESIGN.md](M2-DESIGN.md)).

Follow-ups:
- `hop unpair`.
- The agent checks macOS Secure Input, so keys meant for the PC don't go to a focused password field.
- Peers learn about display changes without reconnecting.
- The device key moves to the Keychain and Credential Manager.
- A byte budget for peer messages, before M4. M4 keeps at most one clipboard text per peer waiting, and file data has its own bounded stream; other messages are still bounded by count only.

- **Scope:**
  - QUIC transport with pinned keys;
  - pairing with a one-time code;
  - mDNS discovery and a manual-address fallback;
  - heartbeats;
  - the local control API and `hopctl`.
- **Done when:** the two laptops find and pair with each other, reconnect after Wi-Fi drops, and `hopctl status` shows the session.

## M3: Two-way control

**Status (2026-10-19):** the first test between the two laptops (2026-10-18, Mac driving, PC on its right) worked, and found:
- **A bounce at the edge.** Parking warped the Mac's cursor to the middle of the screen, and macOS added that jump to the next mouse movement, which sent the cursor straight back from the PC. Fixed in 0.1.1: the Mac parks the cursor where it is, and takes any remaining warp back out of the motion.
- **The PC's cursor left visible** after the cursor came back to the Mac. Built on branch m4-m5: the computer the cursor left hides its cursor until its own mouse is used (rule 11 in ARCHITECTURE.md).
- **Ctrl+V should paste on the PC.** Keys now stay as labelled by default (0.1.1); the swap is the app's Shortcuts choice (m4-m5).
- **Wi-Fi power saving** can delay the first crossing after a quiet spell; the core now nudges the other computer as the cursor heads for the edge (m4-m5), and the log records each crossing's answer time.

- **Scope:**
  - the full hand-off in both directions, including take-back;
  - keys as labelled, with the Cmd ↔ Ctrl swap as a choice;
  - release-all on every exit;
  - lock-key sync;
  - Secure Input detection on the Mac;
  - the emergency chord.
- **Done when:** both directions pass a manual checklist (copy, paste, cut, undo, select all, save; typing; scrolling; clicking; dragging windows), with no stuck keys over an hour of use.

## M4: Text clipboard

**Status (2026-10-19):** built on branch m4-m5 and reviewed; not yet tried between the laptops. Text goes along when the cursor leaves a computer, on the control stream, so it arrives before the next key. Password-manager items, empty text and text over 1 MiB are never sent. See [M4-M5-DESIGN.md](M4-M5-DESIGN.md).

- **Scope:** text sync both ways on screen switch, an echo guard, line-ending normalization, and skipping password-manager items.
- **Done when:** copying on one machine and pasting with the native shortcut on the other works in Notes, TextEdit, Word and a browser.

## M5: Files to the Received folder

**Status (2026-10-19):** built on branch m4-m5 and reviewed; not yet tried between the laptops. See [M4-M5-DESIGN.md](M4-M5-DESIGN.md).

- **Scope:**
  - a transfer engine (hash-verified, temporary-then-rename, paced, on its own stream): crates/hop-transfer and hop-net;
  - a drop target that appears at the shared edge while files are dragged near it, a thin band that takes the drop, with a label that drags pass through;
  - delivery to `Downloads/Hop Received`, which opens on the receiving computer, with the names in the app's Activity, and the files marked as downloaded.
- **Done when:** dropping several images or PDFs at the edge lands them intact in both directions, and window dragging, snapping and other drag and drop at the shared edge still work.

## M6: App shell

**Status (2026-10-19):** the first part, the app, is built and reviewed (see [M6-APP.md](M6-APP.md)), and ran on the two laptops on 2026-10-18 (0.1.0, then 0.1.1). On branch m4-m5 it also has the Shortcuts choice and Activity lines for copy and paste and for files. One window on each computer pairs the two, says where the other computer sits, starts and stops sharing, and shows what is happening. A GitHub Release carries `Hop-macOS.zip` and `Hop-Windows.zip`: `scripts/macos/release.sh` builds the Mac zip and publishes the release, and the Release workflow adds the Windows zip. [INSTALL.md](INSTALL.md) walks the user through the first run on each computer.

Left for M6:
- the menu-bar and tray icon;
- launch at login;
- a Windows installer that creates the firewall rule;
- signing Hop.exe and notarizing Hop.app (Smart App Control, when it is on, blocks the unsigned Hop.exe);
- the quit sequence when Windows signs out or shuts down: the app needs to catch the window's end-session messages (today the Mac takes back its input after its 1.5 s timeout);
- a diagnostics bundle;
- renaming the computer, and the other settings, in the app;
- closing the last pairing gap: a joiner cancelled in the fraction of a second after the yes arrives forgets a pairing the other computer saved (pair_join should save before its goodbye; see [M6-APP.md](M6-APP.md));
- the "done when" check below.

- **Scope:**
  - menu-bar and tray UI;
  - a settings window for pairing, permission status and screen layout (built: the app);
  - launch at login;
  - a Windows installer that creates the firewall rule;
  - a diagnostics bundle (never containing clipboard contents).
- **Done when:** Hop installs and works from a fresh user account on each OS, and survives sleep, wake and reboot.

## Open questions from the M0 reviews

Decide these when the milestone in brackets is built.

- **[M3] A click resent after a loss.** If a click's packet is lost and resent, motion sent after it can arrive first, so the click lands late. Options: send motion reliably while a button is held, or have the receiver step back to the click's motion mark.
- **[M3] Edge delay.** `edge_delay_ms` applies only to crossings from Idle; leaving the controlled device is immediate. Decide whether both directions should feel the same.
- **[M3] Modifiers after other endings.** Held modifiers come back locally only when the cursor returns through the edge. After a take-back or timeout, a still-held Shift stops working until it is pressed again (lost, never stuck).
- **[M3] Take-back sensitivity.** The take-back threshold adds up over the whole session, so small bumps of the controlled laptop's trackpad can add up. A short time window would be more forgiving.
- **[M3] Corner dead zones between monitors.** Where two displays continue one edge, the dead zones leave a 20-unit band in the middle. That is what the spec says, but it may feel odd.
- **[M2] Display changes.** Peers are not told when a device's displays change until they reconnect. Add a message for it.
- **[M2] Peer addresses.** Decide whether `peers.address` is a host or `host:port`.
- **[M6] Saving the config.** Saving drops comments and unknown keys. Keep them (`toml_edit`) now that the app writes the file (Where is it?, Address, Shortcuts, Forget… and pairing).
- **[M2] Unknown values.** An unknown value (for example a future `os = "android"`) fails the whole config load, unlike unknown keys, which only warn.

## Later

- Images and files on the clipboard.
- Exact drop-at-location: onto Windows first, then the Mac (Finder and the Desktop).
- More than two machines (the layout already models N devices).
- Use across the internet (a relay or hole-punching transport behind the same trait).
- A phone companion.
- A Chrome extension (via Native Messaging to the control API).
- The AI assistant: an MCP adapter over the control API, so an assistant can see and act on all devices with your approval.
- Windows UAC prompts and lock screen (a separate privileged service).
- Linux.
