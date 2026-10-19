# M2 design: pairing, the encrypted link, discovery, and the agent

M2 turns the two working halves into one Hop: the laptops find each other, pair once with a code, keep an encrypted connection, and the `hop run` agent wires the core, the platform layer and the network together. When M2 is done, the Mac's trackpad and keyboard drive the PC across the screen edge, and the PC's drive the Mac.

## Threat model

Hop carries every keystroke typed while one laptop controls the other, which makes a Hop connection as sensitive as a keylogger. Anyone on the same Wi-Fi (a guest, a compromised smart device) must not be able to:
- read what is typed or copied;
- inject input into either laptop;
- impersonate a laptop to take control;
- pair with a laptop without the user saying yes.

They may still see that two devices talk, and can block the traffic (denial of service is out of scope). Every device on a network the laptop joins can also tell that it runs Hop, and learn its name (from mDNS) and its id (from mDNS, and from the certificate that any device can fetch by connecting). It cannot learn anything that is typed.

Both laptops are trusted; their accounts and disks are out of scope. A paired laptop is fully trusted, since it can type into the other. Remove a lost or replaced laptop with `hop unpair` (see Pairing).

## Identity

- On first run each device creates a long-term Ed25519 key pair. From it, rcgen (with its ring backend) builds a self-signed X.509 certificate.
  - The certificate names nothing: its subject is `CN=hop`, with no alternative names, and never the device name or the user's email, because any device that connects receives it.
  - It keeps rcgen's default validity (1975 to 4096). Hop never checks certificate dates, so a wrong clock on either laptop cannot break the link.
  - Only the private key is stored. The certificate is rebuilt from the key at every start, so it never goes stale, and the pin follows the key.
- The device's **fingerprint** is SHA-256 over the certificate's DER SubjectPublicKeyInfo, in lowercase hex (64 characters). This is the standard SPKI pin (RFC 7469).
  - The SubjectPublicKeyInfo is hashed with its outer SEQUENCE, exactly as `rustls::server::ParsedCertificate::subject_public_key_info()` returns it.
  - One function computes the fingerprint from a certificate, for this device and for peers alike, so the two can never disagree about the encoding.
- The device id (`DeviceId`) is the first 32 hex characters of the fingerprint.
  - Hop derives the id from the key at every start and corrects `device.id` in the config when it differs. The first M2 run replaces M0's random id this way.
  - Ids name devices. Security checks always compare the full fingerprint.
- The key file (PKCS#8 PEM) is readable only by the user:
  - macOS: `~/Library/Application Support/Hop/identity.pem`. The mode is 0600, set as the file is created (not afterwards), in a 0700 folder. A file readable by others is tightened back to 0600, with a warning.
  - Windows: `%LOCALAPPDATA%\Hop\identity.pem`. It is local, not roaming, because a device key must never follow the user to another PC. It inherits the profile's permissions (the user, SYSTEM and Administrators). An explicit access list needs OS calls and comes with the move to Credential Manager.
  - Creating the file is atomic and never replaces an existing one. For example, write a temporary file and hard-link it into place, which fails if another Hop process (say `hop pair` and `hop run` on first start) got there first; then load whichever file is there.
  - If a key file exists but cannot be read or parsed, Hop stops with a clear error. It never quietly makes a new identity, since that would silently unpair both laptops. Deleting the file on purpose makes a new identity, and then both laptops pair again.

  Moving the key to the macOS Keychain and Windows Credential Manager comes later (noted in the roadmap).

## Transport

- **QUIC** with `quinn` 0.11 and `rustls` 0.23, using the **ring** crypto provider. The aws-lc-rs default would need CMake and NASM to build on the PC.
  - `quinn` uses `default-features = false` with features `runtime-tokio` and `rustls-ring`, which also drops `rustls-platform-verifier`.
  - `rustls` uses `default-features = false` with features `ring` and `std`.
  - `cargo tree -i aws-lc-rs` must come back empty for both targets.
- **Mutual TLS 1.3 only, Ed25519 only.** Both sides present their certificate. TLS 1.2 is off, and the verifiers accept only the Ed25519 signature scheme. There is no certificate authority and no plaintext mode.
- **Pinning verifiers** (custom rustls `ServerCertVerifier` and `ClientCertVerifier`):
  - The dialler checks the server's certificate against the pin of the one peer it is dialling, not against any pinned peer.
  - The listener requires a client certificate (`client_auth_mandatory`) and sends no CA hints. It accepts the certificate only if its fingerprint is pinned for some peer.
  - The peer must send exactly one certificate. Host names, intermediates and dates are ignored.
  - Both verifiers still check the TLS 1.3 handshake signature against that same pinned certificate, with `rustls::crypto::verify_tls13_signature` and the provider's algorithms. **This check is what proves the peer holds the pinned key.** Certificates are public, since any device can fetch one by connecting, so a verifier that returns `HandshakeSignatureValid::assertion()` without checking lets anyone impersonate a paired laptop. `verify_tls12_signature` always fails.
  - After the handshake, the connection's certificate (`peer_identity()`) and ALPN are checked again before anything is read or sent, as a second line of defence.
- **No resumption, no early data.**
  - Settings: clients use `Resumption::disabled()` and `enable_early_data = false`. Servers use `NoServerSessionStorage`, `send_tls13_tickets = 0` and `max_early_data_size = 0`. Nothing calls `into_0rtt`.
  - Why: rustls resumes sessions by default, and a resumed handshake skips both certificate verifiers. A pin removed while Hop runs would then not stop a laptop that had connected before. Early data can also be replayed.
  - As a result, every connection is a full handshake through the pinning verifiers, and nothing is sent to a peer before it has proven its key.
- **Secrets stay in memory.**
  - `key_log` is never set. rustls's `KeyLogFile` would honour `SSLKEYLOGFILE` and write the session secrets to disk.
  - SNI is off (`enable_sni = false`), so no host name travels in the clear.
- **ALPN.** `hop/1` is offered on the session listener only, and `hop-pair/1` on the pairing listener only (see Pairing). The ALPN names the kind of connection, not the protocol version. It stays `hop/1` when `PROTO_MAJOR` changes, so mismatched builds still reach `Hello` and can tell the user to update.
- **Ports.**
  - Sessions use UDP 47800 (`network.port`) on both devices. Pairing uses `network.port + 1` (47801), and only while `hop pair` runs.
  - M2 is IPv4 only: bind `0.0.0.0` and dial IPv4 addresses. On Windows a `[::]` socket is IPv6-only by default.
  - The agent binds its port before it starts capture, so a second copy of Hop fails there, before it touches input.
  - Tests bind `127.0.0.1:0`.
- **One connection per pair.**
  - Both devices listen. Each dials a paired peer only while it has no connection to it: the device with the lower id at once, the other after waiting 1 s. Both dial so that a firewall blocking incoming traffic on one laptop cannot keep them apart. Examples are a Wi-Fi network that Windows marks Public, or a managed Mac's firewall.
  - If two connections still form, both sides keep the one dialled by the lower id and close the other with `DUPLICATE`. Both decide from the pinned identities, so they always pick the same one.
  - Reconnecting backs off from 250 ms to 5 s, with ±25% jitter so the two sides don't fall into step. The backoff resets only after a connection has stayed up for 10 s, so a peer that accepts and then drops at once cannot cause a storm.
  - After `INCOMPATIBLE`, Hop retries once a minute and logs it once.
- **Limits.** Everything a peer or a stranger sends is bounded:
  - Handshakes: at most 4 at a time (more are refused), and each must finish within 5 s. quinn's `max_incoming` is set to 16, and its incoming buffers are kept small.
  - `Hello`: the control stream must open, and the peer's `Hello` arrive, within 3 s of the handshake. A `Hello` is at most 64 KiB.
  - Streams: one bidirectional stream (the control stream) and no unidirectional ones in M2; M4 and M5 raise this. The stream window is 2 MiB, the connection window 4 MiB, and the datagram receive buffer 64 KiB.
  - Sending: each connection has a bounded send queue. Reliable messages are never dropped, because a dropped key-up would leave a key held down. When the queue is full, the connection is closed with `OVERLOADED`, and both sides fail open.
- **Liveness.** QUIC keep-alive every 1 s and an idle timeout of 5 s; the core's own heartbeats cover the sessions.
- **Close codes.** The codes are:
  - `SHUTDOWN` (0);
  - `DUPLICATE` (1);
  - `INCOMPATIBLE` (2);
  - `PROTOCOL` (3): a missing, late, oversized or wrong `Hello`, or a frame that does not decode;
  - `OVERLOADED` (4).

  Close reasons are short text for the log, and a peer's reasons are sanitized before they are logged.
- **Time.** Every timeout here and in pairing runs on the monotonic clock. Nothing depends on the wall clock or compares the two laptops' clocks.
- **Channels on a connection:**
  - one bidirectional **control stream**, opened by the dialler. The dialler writes its `Hello` at once, because quinn announces a stream to the peer only when data is written. `Hello` comes first in each direction (hop-proto frames), then every `CoreMessage` except `Motion`, reliable and in order;
  - QUIC **datagrams** for `Motion` (hop-proto datagram encoding), unreliable and latest-wins. A `Motion` larger than `max_datagram_size()`, or one to a peer without datagrams, goes on the control stream instead;
  - clipboard text came in M4 on the control stream, and file data in M5 on unidirectional streams at priority −1 (see M4-M5-DESIGN.md).
- **`Hello` checks.**
  - The connection is closed with `PROTOCOL` unless the peer's `Hello` arrives exactly once and names the device id that the peer's certificate gives.
  - Negotiation must agree on the major version and share `input`, otherwise the connection closes with `INCOMPATIBLE`.
  - Each side lists only the capabilities its build implements. In M2 that is `input` and `keymap`, not yet `clipboard.text`.
  - The peer's name and version are display text: control characters are removed, and each is cut to 64 characters.
- **Bad input.**
  - A control-stream frame that does not decode, or an oversized frame header, closes the connection, because skipping a reliable message could lose a key-up.
  - Unknown kinds are skipped, following hop-proto's rule.
  - A bad datagram is dropped.
  - `ClipboardText` is ignored in M2 and never logged.
- **What the core sees.**
  - A connection becomes the peer's current one when its `Hello` exchange passes. The core then gets `PeerConnected { os, displays }`, with the id that the certificate gives.
  - The core gets `PeerDisconnected` only when the current connection closes.
  - Nothing from a replaced connection reaches the core: the network stops reading the old connection before it announces the new one, and it does not report the old one's close.
  - Datagrams that arrive before `PeerConnected` are dropped.
  - On a flapping link, the core therefore sees clean connect and disconnect pairs, and each drop ends any session, failing open.

## Pairing (once per pair of laptops)

Pairing runs in its own short-lived process, with its own listener on the pairing port. The session listener therefore never accepts an unknown certificate, not even for a moment.

Run pairing in a terminal on both laptops. On the Mac, run `hop pair` straight from Terminal, not through `open`:
- pairing needs no Accessibility;
- Local Network privacy does not apply to command-line tools run from Terminal (Apple TN3179);
- `open` would send the output, pairing code included, to a log file.

1. **Show the code.** On one laptop, run `hop pair`. It shows **"Pairing code: 482 913"** and waits up to 2 minutes, advertising `pair=1` over mDNS.
   - The 6 digits come uniformly from the OS random number generator, with rejection sampling so there is no modulo bias.
   - The code appears on that screen only, and never in a log.
   - It handles one pairing run at a time. A second connection during a run is refused with `BUSY`.
2. **Type the code.** On the other laptop, run `hop pair 482913` (spaces and dashes are allowed).
   - It finds the advertising laptop by mDNS, ignoring its own advertisement, or uses `--address host[:port]` (the default port is 47801). If more than one other laptop advertises `pair=1`, it lists them and asks for `--address` instead of picking one.
   - It connects with ALPN `hop-pair/1`. During pairing only, the verifiers accept any single Ed25519 certificate and record its fingerprint. They still check the handshake signature exactly as above, so the recorded fingerprint belongs to whoever holds that key.
   - If it cannot connect, the laptop showing the code may be blocking incoming connections, as a managed Mac's firewall can. Swap roles and show the code on the other laptop.
3. **SPAKE2.** Both sides run SPAKE2 (`spake2` 0.4, `Ed25519Group`, asymmetric mode).
   - The joining laptop (the TLS client) is side A (`start_a`). The laptop showing the code is side B (`start_b`).
   - The password is the six ASCII digits.
   - Both pass the same two identities in the same order: `id_a` is A's fingerprint and `id_b` is B's, each as 32 raw bytes, as seen in this TLS handshake.
   - A sends its message (33 bytes), B answers with its own, and each side calls `finish` to get the 32-byte key K. K covers the password, both identities and both messages.
4. **Key confirmation**, in the order RFC 9382 gives:
   - Both sides take E, 32 bytes of this connection's TLS exporter (`export_keying_material`, which quinn offers on both sides). E uses the label `EXPORTER-Channel-Binding` and an empty context: the TLS 1.3 channel binding of RFC 9266. A private label would have to start with `EXPERIMENTAL` (RFC 5705).
   - A sends `cA = HMAC-SHA256(K, "hop pair confirm A" || E)` with its name and OS.
   - B checks cA in constant time, then sends `cB = HMAC-SHA256(K, "hop pair confirm B" || E)` with its name and OS.
   - A checks cB.
   - Neither side trusts anything the other sent until the other's confirmation checks out. K, E and both MACs are never logged or stored.
5. **The user confirms on B.**
   - B shows "Krish's PC (Windows) entered the code. Pair with it? Answer y only if Krish's PC says it is waiting for you. [y/N]".
   - Meanwhile A shows "Waiting for you to confirm on Krish's MacBook."
   - On yes, B saves and tells A, and A saves. On no, or no answer before the 2 minutes are up, neither side saves anything.
6. **Saving.** Each side stores the other's name, id, OS and fingerprint in `peers[]`.
   - It replaces an entry with the same id, or else an entry without a fingerprint whose `os` matches, keeping that entry's side and address. Otherwise it adds an entry on the first free side, right then left. The side can be changed later.
   - It re-reads the config just before saving, so it never overwrites a newer file. The agent itself writes the config only at startup, to correct `device.id`.
   - A running agent picks up the new peer when it restarts, because M2 has no control API. `hop pair` says so.

Pairing connections close with `BUSY` (16), `WRONG_CODE` (17), `EXPIRED` (18) or `DECLINED` (19). Each pairing message is at most 1 KiB.

**Attempts.**
- On B, every run in which B has sent its SPAKE2 message counts as an attempt unless it ends in success. That includes a joiner that hangs up before confirming. Steps 3 and 4 must finish within 10 s.
- The code dies after 3 attempts, after 2 minutes, or with the first success, and `hop pair` then exits. After 3 failures it says that someone may be trying to pair, and to start again.
- On A, the joiner runs steps 3 and 4 at most once per code. It may try several addresses before that, but after a failed confirmation it stops, and the user starts again on both laptops. Otherwise a fake "pairing" advertisement could test one guess per retry.

**Guarantees.**
- An eavesdropper learns nothing about the code: the code never crosses the network, and SPAKE2 runs inside TLS.
- An active attacker gets one online guess per run: at most 3 against the laptop showing the code and 1 against the one typing it. The chance of a successful guess is therefore at most 4 in a million. Against the laptop showing the code, a correct guess must also get past the user's "y".
- A relayed connection fails. The relay holds two TLS sessions, which have different certificates (so different SPAKE2 identities) and different exporter values.
- An old pairing cannot be replayed. Every run uses fresh SPAKE2 secrets on a fresh TLS session, the code is single-use, and nothing accepts `hop-pair/1` outside a running `hop pair`.
- `hop pair 482913` stays in the shell history. That is harmless, because the code is dead after this pairing.
- Not covered: someone who sees the code (over a shoulder, in a screen share) and is on the same network within those 2 minutes can race the user. The confirmation on B is the guard. Never show the code in a screen share.
- `spake2` 0.4 implements the python-spake2 (magic-wormhole) variant, not RFC 9382: it has its own M and N, no cofactor clearing and no built-in confirmation. Its README says it has never been independently audited. With the confirmation above and single-use random codes, that is acceptable here.

**Unpairing.** `hop unpair <name>` removes a peer and its pin from this laptop's config. To revoke a lost laptop, run it on the laptop that is left, then restart that laptop's agent.

## Discovery

- `mdns-sd`: each device advertises `_hop._udp.local.` while `hop run` runs.
  - The instance name is the device name, sanitized and at most 63 bytes.
  - The host name is `hop-<first 8 characters of the id>.local.`, never the computer's own host name. A second responder claiming that name can make macOS see a name conflict and rename the Mac.
  - The TXT record carries `v=1` and `id`. It carries no `os`, since `Hello` already does.
  - `hop pair` advertises a separate instance ("<name> (pairing)") with `pair=1` and the pairing port in its SRV record.
- Each device browses continuously and dials a paired peer when its id shows up.
  - A dedicated task drains the browse events at once: mdns-sd's event channel holds 10 events, and its daemon thread blocks while that channel is full.
  - At most 32 instances, with 8 addresses each, are kept.
  - Of the addresses mDNS reports, only local ones (private or link-local IPv4) are dialled.
- **Trust.** Everything learned from mDNS is unauthenticated, and that is safe only because of the pinning above.
  - A spoofed answer can at worst waste a dial: the pin rejects the stranger.
  - The dialler checks the server's certificate before it sends its own, so a fake server learns nothing about this laptop.
  - A failed dial never removes the other candidates (other answers, the configured address).
- **Fallback:** a peer's `address` in the config (an IP or host name, with an optional `:port`) is always tried too. This works on networks that block multicast.
- **Interfaces:** loopback and VPN tunnels (on macOS `utun*`, `ipsec*`, `ppp*`; mdns-sd's `IfKind::Predicate` can select them) are skipped, so Hop is not announced into a work VPN.
- **Leaving:** on quit Hop unregisters, sending mDNS goodbye packets, and waits at most 1 s.
- **First run on each OS:**
  - macOS asks Hop.app for Local Network access (the Info.plist keys are already in Hop.app). If access is denied, sends fail with "no route to host". Hop then says it needs Local Network access instead of retrying silently.
  - Windows Firewall asks whether `hop.exe` may accept connections. The user allows it on private networks, and Windows must mark the home Wi-Fi as Private. On a network marked Public, the PC can still dial the Mac, because both sides dial.

## The agent (`hop run`)

Threads:
- **Capture:** the platform's own thread. Its sink only hands `LocalInput` to the core and never blocks.
- **Core:** owns `Core` and the `Platform`. It carries out the core's actions:
  - platform calls: capture mode, park and restore, move, inject;
  - key mapping: a `hop_keymap::Translator` per incoming session, using `swap_cmd_ctrl` from the config;
  - sends: handed to the network task without blocking. A full queue closes that connection, as described in Transport.

  Every platform call happens on this thread. It does no file I/O (logging is asynchronous), so a slow disk cannot stall it.
- **Network:** a tokio runtime with 2 workers runs the QUIC endpoint, mDNS and connection management. Pairing runs in `hop pair`, not here.

How events reach the core:
- Local input and network events arrive on separate bounded queues, so traffic from a peer can never crowd out local input.
- When the input queue is full, capture drops the event and raises an "input lost" flag (see `LocalStall`).
- When the network queue is full, that connection is closed with `OVERLOADED`, since a reliable message must never be dropped. A `Motion` datagram that does not fit is simply dropped.
- The core waits on both queues with a timeout, so a `Tick` runs every 10 ms even when nothing arrives, and ticks never queue behind events.

On macOS, `hop run` runs inside Hop.app (launched with `open`), so Accessibility and Local Network belong to Hop. Its main thread runs AppKit's event loop (`run_with_ui`), and the core thread is `run_with_ui`'s worker. In the app (docs/M6-APP.md), the window's event loop has the main thread and the core thread is the `hop-agent` thread.
- `hop run` has no window, and the app's window may be hidden, so macOS App Nap would throttle Hop's timers (the 10 ms tick, the lease, QUIC) and could slow its event tap. While sharing, `hop run` and the app hold an `NSProcessInfo` activity (user-initiated, latency-critical, allowing idle sleep) through `hop-platform-macos`.
- On the PC, check that a background `hop run` keeps its tick on time. If Windows power throttling slows it, opt out with `SetProcessInformation` through `hop-platform-windows`.

Fail-open duties of the agent:
- **Lease renewal.**
  - While the core's state needs suppression (Entering or Controlling), renew the lease on every tick, on the core thread only. A separate timer thread would keep renewing for a stalled core and defeat the lease.
  - Before each renewal, read `capture.mode()`. If it reads Passive, the lease has lapsed, so feed `LocalStall` instead of renewing.
  - With a 1 s lease and a 10 ms tick, the core must stall for about a second before input comes back by itself.
- **`LocalStall`** is a new core event, with a new `EndReason`. It means this laptop can no longer vouch for the session. The agent feeds it when:
  - the lease lapsed;
  - captured input was dropped;
  - a platform call the session depends on failed (suppressing, parking, injecting).

  The core then ends any session, failing open:
  - as the controller, it sends `Release`, restores the cursor at the exit point, sets capture to Passive and disarms the edge;
  - as the controlled side, it sends `TakeBack` and emits `ReleaseAllInjected`.

  Either way it returns to Idle and forgets which keys and buttons it believed were held, since it may have missed their key-ups.
- **Quitting.** Quitting can be caused by Ctrl+C, SIGTERM or SIGHUP, the Windows console closing, logoff or shutdown, or a fatal error. Signal handlers only set a flag that the core thread watches. The core thread then, in this order:
  1. gives input back: capture Passive, every injected key and button released, the cursor restored;
  2. sends `Release` or `TakeBack` if in a session, and closes its connections with `SHUTDOWN` so the peer fails open at once, waiting at most 500 ms;
  3. unregisters mDNS and exits.

  Input comes back first, so a hung network cannot delay it.
- **Panics.** Any panic on any thread is fatal, including one the platform layer catches. The build keeps `panic = "unwind"` (the default), because failing open relies on unwinding.
  - The core thread owns the `Platform`, so unwinding drops it: capture stops and gives the cursor back, and the injector releases what it holds. `run_with_ui` then re-raises the panic on the main thread and the process exits.
  - After a panic elsewhere (a network task, the mDNS daemon), the panic hook asks the core thread to quit as above.
  - The hook logs the thread and the location, plus the message only when it is a fixed string, since a formatted one could quote input. It never blocks, with one exception: in the app, a panic on the main thread waits up to 3 s for sharing to stop, because a panic there may abort without unwinding (AppKit calls the window's code through functions a panic cannot unwind out of). The agent never waits for the main thread, so this cannot deadlock.
- **What is left.** If Hop is killed or aborts, the OS removes its event tap or hooks. The peer's core times out within 1.5 s and releases what it injected. Keys that this laptop had injected for the peer, however, stay down until they are pressed again. A watchdog process for that comes later.
- **Logs.** Write to `~/Library/Logs/Hop/hop.log` and `%LOCALAPPDATA%\Hop\Logs\hop.log`.
  - What goes in: connections, sessions and errors, and fingerprints, which are public.
  - What never goes in: keys or key codes, clipboard text, file names and contents, pairing codes, SPAKE2 messages and keys, MACs, exporter values, private keys and TLS secrets.
  - Text from a peer or the network (names, versions, close reasons, mDNS names) has its control characters removed and is cut short before it is logged or printed.
  - Any device on the network can cause failed handshakes. Repeated warnings are therefore rate-limited, and the file is capped at 10 MiB, plus one old file. quinn, rustls and mdns-sd log at `warn` at most.
  - In Hop.app, stdout and stderr also go to files, so they follow the same rules, and the agent keeps them quiet.

## Config changes

- `peers[].fingerprint`: an optional string of 64 lowercase hex characters, written by pairing. A peer without one can't connect (the user must pair first).
- A peer's `id` must equal the first 32 characters of its fingerprint. A peer where they differ is refused, with a warning.
- `device.id` is derived from the key at every start. The first M2 run replaces M0's random id.
- `network.port` may be at most 65534, since pairing uses the next port.

## Out of scope for M2

These come later:
- the local control API and `hopctl` (status shows in the log and the terminal for now);
- clipboard and files;
- the settings window;
- the Keychain;
- a switch that keeps Hop quiet on networks other than home;
- a watchdog process that releases injected keys if Hop dies;
- use across networks.

## Tests

- **Loopback QUIC:**
  - a pinned pair connects;
  - an unpinned or wrong certificate is refused in both directions;
  - a server that presents the pinned certificate but signs with another key is refused, which proves the handshake signature is checked;
  - the verifiers run on every connection (count their calls over two connections between the same configs), so no session is resumed;
  - a `Hello` with an id other than its certificate's is refused, and so are a late `Hello`, an oversized `Hello` and an undecodable frame: each closes the connection;
  - `Hello` and negotiation run, and a major-version mismatch closes with `INCOMPATIBLE`;
  - `Motion` arrives as datagrams;
  - when both sides dial at once, one connection is left, the same one on both sides, and the core never hears about the closed one;
  - a full send queue closes the connection instead of dropping a message;
  - reconnecting after a drop works.
- **Pairing:**
  - the right code pins both sides;
  - a wrong code fails, and the third failure kills the code;
  - a joiner that hangs up after B's SPAKE2 message counts as a failed attempt;
  - the joiner never runs a second exchange with the same code;
  - a second joiner during a run gets `BUSY`;
  - expiry;
  - a relay without the right exporter fails;
  - a declined confirmation saves nothing on either side;
  - the code appears in no log output.
- **End to end:** two agents in one process over loopback, with fake platforms, cross and type with the Cmd/Ctrl swap, and the log of that run contains no key names or codes. The agent is tested without any OS input.
- **Fail open, with fake platforms:**
  - a lapsed lease, a full input queue and a failed injection each lead to `LocalStall`;
  - quitting while controlling sends `Release` and gives the cursor back;
  - quitting while controlled releases the injected keys;
  - a panic on the core thread releases injected keys and restores the cursor.
- **Discovery:** an ignored-by-default test, since multicast is unreliable in CI and would trigger macOS's Local Network prompt.

## Review notes

A security and robustness review of this design, done on 2026-10-18 before implementation. It used RFC 5705, 7469, 8446, 9266 and 9382, Apple TN3179 and its App Nap guide, and the source of spake2 0.4.0, rustls 0.23.46, quinn 0.11.12, quinn-proto 0.11.19, rustls-webpki 0.103.15, rcgen 0.14.10 and mdns-sd 0.21.5.

**Changed, and why.**

Pinning and TLS:
1. **Handshake signatures.** Pinning now requires checking the handshake signature with the pinned key. A pin check on the certificate bytes alone is worthless, because certificates are public. The TLS 1.2 path always fails, and only Ed25519 is accepted.
2. **Resumption and early data.** Both are off. By default rustls resumes sessions, and a resumed handshake skips both verifiers. Early data can be replayed.
3. **Separate pairing listener.** Pairing got its own process, port and listener. rustls calls the client-certificate verifier without telling it the negotiated ALPN, so one listener cannot be pinned for `hop/1` and open for `hop-pair/1`. `hop pair` also cannot share the agent's port without a control API.

SPAKE2 and pairing:
4. **SPAKE2 roles and identities.** The old text said "each side's identity is its own fingerprint". Read literally, that gives the two sides different inputs, and the keys never match: spake2 0.4's symmetric mode takes one identity that both sides share, and its asymmetric mode needs both sides to pass `(id_a, id_b)` in the same order. The roles now follow the TLS roles, the password encoding is fixed, and the text says what `finish` already covers.
5. **Key confirmation.** It now follows RFC 9382's order (A first; neither side done until it has checked the other). It uses the registered RFC 9266 exporter label: the old label `hop pairing` was neither registered nor `EXPERIMENTAL`. MACs are compared in constant time, and nothing a side sent is trusted before its confirmation checks out.
6. **Counting attempts.** Attempts are now counted per SPAKE2 run on B, including runs where the joiner hangs up. Before, the order of the confirmations was open: a joiner could read B's confirmation, check one guess offline and hang up without being counted, which gave it one uncounted guess per run for the whole 2 minutes. The joiner runs once per code, so a fake advertiser cannot collect one guess per retry. Several advertisers mean asking, not picking. B handles one run at a time (`BUSY`). The odds became 4 in a million.
7. **User confirmation.** A y/N confirmation on B, with both names shown, guards against a code that leaked to someone racing the user.
8. **Where the code is shown.** On the Mac, `hop pair` runs from Terminal, which TN3179 exempts from Local Network privacy. Through `open`, the code would land in the stdout log file.
9. **Unpairing.** `hop unpair` gives a way to revoke a lost or replaced laptop.

Connection management:
10. **Both sides dial.** With only the lower id dialling, an inbound block on the other laptop (Windows' Public profile, a managed Mac firewall) would keep the link down forever. The higher id waits 1 s, the backoff has jitter and resets only after 10 s of uptime, and `INCOMPATIBLE` slows retries to once a minute.
11. **Connection events.** The core hears only about the current connection. Before, a replaced duplicate's later close would have reported a live peer as disconnected.
12. **`Hello` checks.** `Hello` must name the certificate's id (before, any pinned peer could claim another's id), arrive once, within 3 s and within 64 KiB. Only implemented capabilities are advertised, so the M2 build no longer invites clipboard text it would drop.
13. **Decode errors.** A control-stream frame that does not decode closes the connection, which narrows hop-proto's "skip a bad frame" advice for this stream, because a skipped `Input` can lose a key-up.
14. **Explicit limits.** quinn's defaults allow 100 bidirectional and 100 unidirectional streams, an unlimited connection window and 65,536 queued incoming connections. A reliable message is never dropped: a full queue closes the connection.
15. **ALPN, ports and close codes.** The ALPN stays fixed across protocol majors, so a mismatch still gets the "update Hop" message. M2 is IPv4 only, since a `[::]` socket is IPv6-only on Windows. Binding before capture makes the port a single-instance lock. Close codes are now defined.

Identity:
16. **The certificate.** It names nothing, its dates are ignored, and it is rebuilt from the key. One fingerprint function serves both sides, which rules out SPKI encoding mismatches. Full fingerprints are compared, and the id is re-derived at every start.
17. **The key file.** It is 0600 at creation, created atomically, and never replaced or silently regenerated. On Windows it moved to `%LOCALAPPDATA%`, because a device key must not roam.

Discovery:
18. **Host name and events.** Hop advertises its own host name, so macOS cannot be pushed into renaming the computer. Browse events are drained at once (mdns-sd's daemon thread blocks on a full 10-event channel), and the caps are explicit.
19. **What gets dialled and announced.** Only local addresses are dialled. VPN and loopback interfaces are skipped, and `os` was dropped from TXT. Hop unregisters on quit, a denied Local Network permission is reported, and the threat model now states what other devices on the network can see.

The agent:
20. **Queues and ticks.** Input and network events have separate non-blocking queues; with one shared queue, peer traffic could have crowded out local input. Ticks come from a wait timeout, and the core thread does no file I/O.
21. **`LocalStall`.** It is defined for both roles and also covers lost input and failed platform calls. It forgets held keys, so a missed key-up cannot become a phantom modifier in the next `Enter`. It disarms the edge, and the lease is renewed only on the core thread.
22. **Quitting and panics.** Quitting has an order, with input first and the network last. Panics are fatal, `unwind` is kept, the core thread owns the `Platform`, and the hook never logs formatted messages.
23. **App Nap.** Hop.app takes an App Nap exemption. A windowless accessory app qualifies for App Nap, which throttles timers and would lapse leases during normal use. The PC needs the same check for power throttling.
24. **Logs.** The rules are wider: pairing and SPAKE2 values, exporter values and TLS secrets are never logged, `key_log` is never set, peer text is sanitized, repeats are rate-limited, the file size is capped, dependencies log at `warn`, and stdout follows the same rules.

Config and tests:
25. **Config.** The fingerprint has a fixed format, the id must match it, and the port is capped at 65534.
26. **Tests.** New tests cover each of the above.

**Not changed, and why.**
- **6 digits, 3 attempts, 2 minutes.** With the counting fixes, the odds are at most 4 in a million per pairing, comparable to Bluetooth passkey entry. More digits would cost usability for little gain.
- **SHA-256 over the SPKI as the pin.** It is the right pin (RFC 7469). It covers the algorithm identifier, and it survives re-issuing the certificate.
- **`spake2` 0.4.** It is the only stable SPAKE2 crate on the allowed list. It is not RFC 9382 (no cofactor clearing, its own M and N), it is unaudited, and its README calls it probably not constant-time. A low-order component in a peer's message cannot help anyone guess the code, and timing matters little for a single-use random code. The password goes through HKDF, not a memory-hard function, which only matters for offline attacks and there are none here. Revisit if a maintained RFC 9382 crate appears.
- **No short authentication string to compare by eye.** The PAKE already defeats a man in the middle, and the y/N prompt covers the leaked-code race.
- **Denial of service.** Burning a code's attempts, mDNS floods (mdns-sd's own cache has no size limit) and spoofed answers stay out of scope. The limits here only bound memory and logs.
- **Exposure on other networks.** Hop's presence, name and id stay visible on any network the Mac joins. This is documented, not fixed; it needs a "home networks only" switch.
- **Pre-authentication code.** The QUIC and TLS code that runs before a peer is authenticated stays exposed to any device on the network. rustls and quinn are memory-safe, and the limits bound the work.
- **The key file.** It stays a file for now, with no explicit Windows access list (the profile's permissions apply) until the Keychain and Credential Manager move.
- **The 1 s lease and 10 ms tick.** They give a 100-fold margin.
- **Killed processes.** Keys that a killed or aborted Hop had injected stay down. That needs the later watchdog process.
- **Sleep, wake and network changes.** Beyond the timeouts above, these remain M6 work.

**Follow-ups outside this file**, for their owners:
- `hop-core`: `Event::LocalStall` and its `EndReason`, as defined above.
- `hop-config`:
  - the `fingerprint` field and the `id` check;
  - the id derived from the key, replacing `first_run`'s random id;
  - `network.port` at most 65534;
  - an identity path under `%LOCALAPPDATA%`;
  - `hop unpair`.
- `hop-platform-macos`: the App Nap activity.
- `hop-platform-windows`: a power-throttling opt-out, if the PC needs it.
- `docs/WINDOWS-SETUP.md`: mark the home Wi-Fi Private and allow `hop.exe`.
- The M6 installer: its firewall rule should cover the program, or both ports 47800 and 47801.
- `docs/ARCHITECTURE.md`: mention the pairing port and the separate pairing process.
