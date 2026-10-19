# Decisions

## Requirements (from the user, 2026-10-16)

| # | Topic | Answer |
|---|---|---|
| 1 | The Mac | An employer-managed work laptop (see Constraints) |
| 2 | Second machine | Windows 11 on Intel (x64) |
| 3 | Control direction | Two-way: either laptop controls the other |
| 4 | Audience | Personal tool; not distributed |
| 5 | Languages the user reads | None. Claude writes and maintains the code |
| 6 | File drag-and-drop in v1 | Files land in a "Received" folder; exact drop-at-location later |
| 7 | Typical files | Images, text files, PDFs |
| 8 | Clipboard in v1 | Text only |
| 9 | Shortcuts | Keys stay as labelled: with the Mac's keyboard on the PC, Ctrl+V pastes (changed 2026-10-18; it was "Swap Cmd ↔ Ctrl only"). The swap stays available as a setting |
| 10 | Future ideas | More machines, use across the internet, a phone companion, a Chrome extension, and an AI assistant that understands and can control all the devices |
| 11 | Network | Home Wi-Fi on both; no Ethernet |
| 12 | Tooling | Claude Code will run on the PC; use a GitHub repository with CI |
| 13 | Windows UAC prompts, lock screen, admin apps | Later (v2) |
| 14 | Mac system shortcuts while on the PC | Sent to the PC where macOS allows it |
| 15 | Volume and media keys | Stay on the Mac |
| 16 | Keyboard layout | US on both |
| 17 | macOS versions | 26 and 27 |
| 18 | Password-manager clipboard items | Never synced |
| 19 | Name | "Hop" (working name) |

## Constraints

- **Work Mac.** The Mac is managed by the user's employer. On 2026-10-17 the user confirmed their IT department has approved running Hop on it. Keep Hop transparent there: no hidden capture, nothing logged.
- **The PC.** The Windows laptop is the user's personal machine (confirmed 2026-10-17).
- **GitHub.** The GitHub CLI on the Mac is signed into the employer's GitHub Enterprise. Hop's repository belongs on the user's personal github.com account, which the user connects.
- **Apple signing.** Mac builds are signed with the user's personal Apple Development identity, so macOS permission grants survive rebuilds. No paid programs are needed for personal use.

## Architecture decisions

**ADR-1: a native agent per OS.** A browser extension cannot capture or inject OS input, see Finder or Explorer drags, or open LAN sockets. One signed menu-bar/tray process per OS owns all OS access.

**ADR-2: a Rust core with a thin, swappable shell.** The core and platform crates never depend on the UI framework. The settings window comes last (Tauri 2), after the native parts work. Rust is gated: if the M1 platform spikes hit a blocker in Rust's OS bindings for more than two days, fall back to Swift (Mac) plus C# (Windows) for the platform layer.

**ADR-3: a sans-IO core.** `hop-core` is a pure state machine with injected time, tested by deterministic two-device simulation and property tests.

**ADR-4: the wire format.** Kind-tagged frames carry MessagePack bodies with named fields. Capabilities are negotiated in `Hello`. Unknown kinds and fields are ignored. Chosen over protobuf for simplicity while every peer runs the same Rust core; revisit if a non-Rust peer appears.

**ADR-5: keys travel as physical positions.** Keys are sent as USB HID usages, and the receiver maps them. In v1 the only translation is the optional Meta ↔ Control swap between Mac and Windows, off by default since 2026-10-18 (requirement 9).

**ADR-6: the controlled device owns its cursor.** The controlled device is authoritative for its cursor position and decides when the cursor leaves. Whichever device the cursor is leaving computes the entry point on the next device.

**ADR-7: motion marks.** Motion is sent as cumulative marks over an unreliable channel. Input events carry the mark, so lost motion never misplaces a click.

**ADR-8: v1 scope.**
- In: cursor and keyboard both ways, the Cmd ↔ Ctrl swap, text clipboard, and files to a Received folder.
- Out: exact drop-at-location, images and files on the clipboard, Windows UAC and lock screen, internet use, and Linux.

**ADR-9: transport (M2).** QUIC (`quinn` + `rustls`) with mutually pinned keys, SPAKE2 pairing on a one-time code, and mDNS discovery with a manual-address fallback.

**ADR-10: clipboard and files (M4, M5).** Text travels when the cursor leaves a computer, on the control stream, so it always arrives before the next key. Files are dropped on a target that appears at the shared edge during a drag (the cursor never crosses with a button held), and travel on their own low-priority QUIC stream, paced, hash-checked and saved under temporary names until complete. The details are in [M4-M5-DESIGN.md](M4-M5-DESIGN.md).
