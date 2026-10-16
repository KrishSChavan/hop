# Roadmap

Each milestone ends with something you can try. Estimates assume Claude writes the code; the pace is mostly set by test sessions on the two laptops.

## M0: Foundation (now)

- **Scope:** the workspace, shared types, wire protocol, keymap, the hand-off state machine (simulated, no OS access), configuration, CI, and docs.
- **Done when:** `cargo test --workspace` passes on macOS and Windows, and a simulated two-device session crosses, types, takes back and times out correctly.

## M1: Platform spikes on the real machines

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

- **Scope:**
  - QUIC transport with pinned keys;
  - pairing with a one-time code;
  - mDNS discovery and a manual-address fallback;
  - heartbeats;
  - the local control API and `hopctl`.
- **Done when:** the two laptops find and pair with each other, reconnect after Wi-Fi drops, and `hopctl status` shows the session.

## M3: Two-way control

- **Scope:**
  - the full hand-off in both directions, including take-back;
  - the Cmd ↔ Ctrl swap;
  - release-all on every exit;
  - lock-key sync;
  - Secure Input detection on the Mac;
  - the emergency chord.
- **Done when:** both directions pass a manual checklist (copy, paste, cut, undo, select all, save; typing; scrolling; clicking; dragging windows), with no stuck keys over an hour of use.

## M4: Text clipboard

- **Scope:** text sync both ways on screen switch, an echo guard, line-ending normalization, and skipping password-manager items.
- **Done when:** copying on one machine and pasting with the native shortcut on the other works in Notes, TextEdit, Word and a browser.

## M5: Files to the Received folder

- **Scope:**
  - a transfer engine (chunked, hash-verified, temporary-then-rename);
  - an edge "catcher" that takes over a Finder or Explorer drag at the shared edge;
  - delivery to `Downloads/Hop Received`, with a notification and a reveal action.
- **Done when:** dragging several images or PDFs across lands them intact in both directions, and window dragging and snapping at the shared edge still work.

## M6: App shell

- **Scope:**
  - menu-bar and tray UI;
  - a settings window for pairing, permission status and screen layout;
  - launch at login;
  - a Windows installer that creates the firewall rule;
  - a diagnostics bundle (never containing clipboard contents).
- **Done when:** Hop installs and works from a fresh user account on each OS, and survives sleep, wake and reboot.

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
