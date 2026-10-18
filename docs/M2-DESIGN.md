# M2 design: pairing, the encrypted link, discovery, and the agent

M2 turns the two working halves into one Hop: the laptops find each other, pair once with a code, keep an encrypted connection, and the `hop run` agent wires the core, the platform layer and the network together. When M2 is done, the Mac's trackpad and keyboard drive the PC across the screen edge, and the PC's drive the Mac.

## Threat model

Hop carries every keystroke typed while one laptop controls the other, which makes a Hop connection as sensitive as a keylogger. Anyone on the same Wi-Fi (a guest, a compromised smart device) must not be able to:
- read what is typed or copied;
- inject input into either laptop;
- impersonate a laptop to take control.

They may still see that two devices talk, and can block the traffic (denial of service is out of scope). Both laptops are trusted; their accounts and disks are out of scope.

## Identity

- On first run each device creates a long-term key pair (Ed25519) and a self-signed X.509 certificate for it (rcgen).
- The device's **fingerprint** is SHA-256 over the certificate's SubjectPublicKeyInfo, in lowercase hex.
- The device id (`DeviceId`) becomes the first 32 hex characters of the fingerprint. On the first M2 run this replaces the random id that M0's `first_run` wrote.
- The key is stored next to the config, in a file only the user can read:
  - macOS: `~/Library/Application Support/Hop/identity.pem` (mode 0600)
  - Windows: `%APPDATA%\Hop\identity.pem` (current user only)

  Moving it to the macOS Keychain and Windows Credential Manager comes later (noted in the roadmap).

## Transport

- **QUIC** with `quinn` 0.11 and `rustls` 0.23, using the **ring** crypto provider. The aws-lc-rs default would need CMake and NASM to build on the PC.
- **Mutual TLS 1.3.** Both sides present their certificate, and custom verifiers accept only a certificate whose fingerprint is pinned in the config for that peer. Host names are ignored; there is no certificate authority and no plaintext mode.
- **ALPN.** `hop/1` for sessions, `hop-pair/1` for pairing.
- **Port.** UDP 47800 on both devices (`network.port` in the config).
- **One connection per pair.** Both devices listen. The device with the lower id dials; if two connections still form (a race), the one dialled by the lower id survives. Reconnection uses backoff starting at 250 ms and capped at 5 s.
- **Liveness.** QUIC keep-alive every 1 s and an idle timeout of 5 s; the core's own heartbeats cover the sessions.
- **Channels on a connection:**
  - one bidirectional **control stream**, opened by the dialler: `Hello` first in each direction (hop-proto frames), then every `CoreMessage` except `Motion`, reliable and in order;
  - QUIC **datagrams** for `Motion` (hop-proto datagram encoding), unreliable and latest-wins;
  - clipboard and file streams come in M4 and M5, at a lower priority than the control stream.
- The core sees `PeerConnected { os, displays }` once the `Hello` exchange succeeds and negotiation passes, and `PeerDisconnected` when the connection closes.

## Pairing (once per pair of laptops)

1. On one laptop: `hop pair`. It shows **"Pairing code: 482 913"** and waits up to 2 minutes, advertising `pair=1` over mDNS.
2. On the other: `hop pair 482913`. It finds the advertising laptop (or uses `--address`), and connects with ALPN `hop-pair/1`. During pairing only, the verifiers accept any certificate and record its fingerprint.
3. Both sides run **SPAKE2** (`spake2` crate) with the code as the password. Each side's identity is its own fingerprint, and the transcript also includes a TLS exporter value (RFC 5705, label `hop pairing`) from this very connection, binding the pairing to it.
4. Each side sends `HMAC(K, "hop-pair-confirm" || role || transcript)`, where K is the SPAKE2 key. If both confirmations verify, each side stores the other's name, id, OS and fingerprint in its config (`peers[]`). The side is set to the left or right default, which can be changed later.

**Guarantees.**
- An eavesdropper learns nothing about the code.
- An active attacker gets one online guess per attempt. The code dies after 3 failed attempts or 2 minutes, so the chance of a successful guess is at most 3 in a million.
- A relayed connection fails the exporter binding.
- The code comes from the OS random number generator and is never logged.

## Discovery

- `mdns-sd`: each device advertises `_hop._udp.local.` with its name as the instance name. The TXT record carries `id`, `os`, `v=1` and, while pairing, `pair=1`.
- Each device browses continuously and dials a paired peer when its id shows up.
- **Fallback:** a peer's `address` in the config (an IP or host name, with an optional `:port`) is always tried too. This works on networks that block multicast.
- **First run on each OS:**
  - macOS asks for Local Network access (the Info.plist keys are already in Hop.app);
  - Windows Firewall asks to allow UDP 47800, which the user must allow on private networks.

## The agent (`hop run`)

Threads:
- **Capture:** the platform's own thread. Its sink only sends `LocalInput` into the core channel.
- **Core:** owns `Core`. It receives events from one channel (local input, peer messages, connect/disconnect, ticks every 10 ms) and carries out the actions:
  - platform calls: capture mode, park and restore, move, inject;
  - key mapping: a `hop_keymap::Translator` per incoming session, using `swap_cmd_ctrl` from the config;
  - sends: handed to the network task.
- **Network:** a tokio runtime with 2 workers runs the QUIC endpoint, mDNS, pairing and connection management.

On macOS, `hop run` runs inside Hop.app (launched with `open`), so Accessibility and Local Network belong to Hop. The app's main thread runs AppKit's event loop (`run_with_ui`) for the UI that comes later.

Fail-open duties of the agent:
- **Lease renewal.** While the core's state needs suppression, renew the lease on every tick.
- **A lapsed lease.** If capture reads Passive while the core still believes it is controlling, the agent stalled. Feed the core `Event::LocalStall` (a new core event), which ends the session with `Release` and returns it to Idle.
- **Quitting.** On quit (Ctrl+C, a signal, a panic), set capture to Passive, release everything injected, and restore the cursor.
- **Logs.** Write to `~/Library/Logs/Hop/hop.log` and `%LOCALAPPDATA%\Hop\Logs\hop.log`, with connections, sessions and errors, never keys, clipboard text or file contents.

## Config changes

- `peers[].fingerprint`: an optional string, written by pairing. A peer without one can't connect (the user must pair first).
- `device.id` is set from the fingerprint on the first M2 run.

## Out of scope for M2

The local control API and `hopctl` (status shows in the log and the terminal for now), clipboard, files, the settings window, the Keychain, and use across networks.

## Tests

- **Loopback QUIC:**
  - a pinned pair connects;
  - an unpinned or wrong certificate is refused in both directions;
  - `Hello` and negotiation run;
  - `Motion` arrives as datagrams;
  - reconnecting after a drop works.
- **Pairing:**
  - the right code pins both sides;
  - a wrong code fails, and the third failure kills the code;
  - expiry;
  - a relay without the right exporter fails.
- **End to end:** two agents in one process over loopback, with fake platforms, cross and type with the Cmd/Ctrl swap. The agent is tested without any OS input.
- **Discovery:** an ignored-by-default test, since multicast is unreliable in CI.
