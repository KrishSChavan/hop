# M6 (first part): the Hop app

The user asked for a simple app on each computer that does what the terminal did:
- pair the two computers;
- say which side the other computer is on;
- start and stop sharing, and show what is happening.

Both apps download from the GitHub Releases page. This document is the spec for that work. The menu-bar and tray icon, launch at login and the Windows installer stay in M6's later part.

## Shape

- **`crates/hop-agent`** (new library). Everything in `apps/hop/src` moves here:
  - the agent: `agent/` (with `exec`, `link`, `start`, `fake`, `tests`);
  - `logging`, `native`, `paths`, `simulate` and `startup`;
  - the pairing flow;
  - the command line itself (`cli`).

  It adds the APIs the app needs: `service`, `pairing`, `settings` and `system`. It stays `#![forbid(unsafe_code)]`, and its OS access goes through `hop-platform`, as before.
- **`apps/hop`** (the `hop` command). Its `main` becomes a one-line call to `hop_agent::cli::main()`. Every subcommand keeps its behaviour, output and exit codes.
- **`apps/hop-app`** (new). The app, built with eframe/egui 0.37 and the wgpu renderer. The binary is `hop-app`.
  - The Windows release ships it as `Hop.exe` (a windowed program, no console).
  - On the Mac it is the executable inside `Hop.app` (still `Contents/MacOS/hop`, with the bundle id `com.krishschavan.hop` and the same signing identity, so the Accessibility grant carries over).
  - On macOS only, a first argument that names a `hop` subcommand or a help/version flag goes to `hop_agent::cli::main()`. The scripts start `Hop.app` through LaunchServices with `--args selftest` or `--args run`, so this keeps them working. Anything else opens the window; that covers no arguments and the `-psn_…` arguments LaunchServices may add.

## The window

One window titled "Hop", about 480×680 points, resizable, with the icon from `assets/icon`. Sections run top to bottom. The labels in quotes are normative, because `docs/INSTALL.md` quotes them.

1. **Header:** "Hop" and the version, then "This computer: {name} ({OS})".
2. **Sharing**
   - A coloured dot and one status line:
     - "Not sharing."
     - "Starting…"
     - "Looking for {peer}…": running, not connected yet.
     - "Connected to {peer}."
     - "Using {peer}: this keyboard and mouse control it now.": controlling.
     - "{peer} is using this computer.": controlled.
     - "Stopped: {reason}": the reason in full.
   - The button "Start sharing" or "Stop sharing". It is disabled, with the hint "Pair with your other computer first.", while nothing is paired.
   - While running, two lines:
     - "Push the cursor past the {left/right/top/bottom} edge of this screen to move to {peer}."
     - The emergency-keys sentence the agent already writes.
   - On Windows, always: "If Windows asks whether to let Hop use networks, choose Private networks and click Allow."
3. **Your other computer**
   - When nothing is paired: "Not paired yet. Use Pair below."
   - Otherwise, for each paired computer:
     - "{name} ({OS})".
     - "Where is it?" with four toggle buttons: "Left", "Right", "Above", "Below".
     - The tip "On {name}, choose {opposite} for this computer."
     - A small "Address" field and a "Save address" button: optional, "only if it is not found automatically".
     - "Forget…", which asks "Forget {name}? You will need to pair again." with "Forget" and "Cancel".
   - A change saves at once. Sharing restarts when it is running.
4. **Pair**, with two buttons: "Show a pairing code" and "Enter a code".
   - **Show a pairing code:**
     - The code, large: "482 913".
     - "On the other computer, open Hop, click Enter a code and type this code. It works once, for 2 minutes. Never show it in a screen share."
     - The time left, and "Cancel".
     - Wrong tries show "Someone entered a wrong code (1 of 3 tries used)."
     - When the other computer has entered the code, a dialog asks "{name} ({OS}) entered the code. Pair with it?", followed by the replacement sentence when there is one, and "Answer Yes only if {name} says it is waiting for you." The buttons are "Yes, pair" and "No". No answer before the time runs out counts as No.
   - **Enter a code:**
     - A "Code" field (digits; spaces are fine).
     - Under "If the other computer is not found", an "Address" field.
     - "Pair" and "Cancel".
     - Progress lines: "Looking for the computer showing the code…", "Connecting…", "Now click Yes, pair on {host}."
     - When nothing is found, or several computers are found, say so and point to the Address field.
   - **The result:** "Paired with {name}. It sits {to the left of/to the right of/above/below} this screen; change that under Your other computer." Or "Not paired: {reason}." When sharing was running, it restarts with the new pairing.
5. **Permissions** (Mac only)
   - "Accessibility: Allowed" with a green tick.
   - Or "Accessibility: Not allowed yet" with the button "Allow…". The button shows macOS's prompt and opens System Settings > Privacy & Security > Accessibility.
   - The state is checked again every 2 seconds until it is granted.
6. **Activity**
   - The last 100 status lines, newest at the bottom, in a scroll area.
   - Pairing progress lines go here too.
   - "Open log folder".

**Closing the window** stops sharing first (failing open: input back, then goodbye), waits at most 3 seconds, and quits. Cmd+Q on the Mac does the same.

## APIs in hop-agent (sketch; names may change, guarantees may not)

```rust
// service: the agent on its own thread, for apps.
pub enum Session { Idle, Controlling, Controlled }   // Entering counts as Controlling
pub enum AgentUpdate {
    Started { peer: String, side: Side, emergency_keys: String },
    Connected { name: String, os: Os },
    Disconnected,
    Session(Session),
    Stopped { error: Option<String> },               // always the last update
}
pub struct AgentHandle { /* signals, thread */ }
impl AgentHandle {
    pub fn start(updates: std::sync::mpsc::Sender<AgentUpdate>) -> std::io::Result<AgentHandle>;
    pub fn stop(&self);                               // asks to quit, failing open; returns at once
    pub fn is_finished(&self) -> bool;
    pub fn wait(self, timeout: Duration) -> bool;     // true when the thread ended in time
}
```

- **The agent thread:**
  - It runs the same sequence as `hop run`, ending with the same quit sequence.
  - Errors become `Stopped { error: Some(text) }`, with the full `{:#}` chain: not paired, Accessibility missing, port in use.
  - A panic becomes a fixed sentence.
- **Updates:**
  - The core thread only ever `send`s on an unbounded channel, so it never waits for the app.
  - Session updates go out only when the session kind changes.
- **The app's side:** a bridge thread drains the channel into the app's state and calls `request_repaint`.
- **Panics while the app runs:** a panic anywhere asks the running agent to quit, as `hop run`'s hook does. The hook logs only fixed text.

```rust
// pairing: the flow of `hop pair` without a terminal.
pub enum PairRole { Show, Join { code: Zeroizing<String>, address: Option<String> } }
pub enum PairEvent {
    Code(Zeroizing<String>),          // to show; never logged
    NotAnnounced { reason: String },  // mDNS failed; the joiner needs this computer's address
    WrongCode { failures: u32 },
    Confirm { name: String, os: Os, replaces: Option<String>, within: Duration },
    Searching, NoHostFound, SeveralHosts(Vec<(String, Vec<IpAddr>)>), Connecting,
    WaitingForConfirmation { host: String },
    Finished(Result<Saved, String>),  // always the last event
}
pub struct PairSession { /* thread, runtime, cancel, answer */ }
impl PairSession {
    pub fn start(role: PairRole, events: std::sync::mpsc::Sender<PairEvent>) -> std::io::Result<PairSession>;
    pub fn answer(&self, yes: bool);  // to Confirm
    pub fn cancel(&self);             // closes the listener or connection at once; Finished follows
    pub fn is_finished(&self) -> bool;
}
```

- **Front ends:**
  - The terminal and the app drive one async flow, and each words the events its own way.
  - The terminal keeps every current sentence, `--address` included.
  - The save rules and their tests (`add_peer`, `save_peer`, `replaced_old_key`) move unchanged.
- **Errors:** pairing errors reach the app as plain sentences. Only the app shows the code, and the code is dropped (zeroized) when the session ends.

```rust
// settings: read and change the config file. Each change re-reads the file, validates, and saves.
pub struct Overview { pub name: String, pub os: Os, pub config_path: PathBuf, pub peers: Vec<PeerView>, pub notes: Vec<String> }
pub struct PeerView { pub id: DeviceId, pub name: String, pub os: Os, pub side: Side, pub address: Option<String>, pub paired: bool }
pub fn overview() -> anyhow::Result<Overview>;  // startup::load(): makes the key and config on a first run
pub fn set_side(id: &DeviceId, side: Side) -> anyhow::Result<()>;  // refuses a side another peer has
pub fn set_address(id: &DeviceId, address: Option<&str>) -> anyhow::Result<()>;  // trimmed; empty = none
pub fn forget(id: &DeviceId) -> anyhow::Result<()>;
pub fn opposite(side: Side) -> Side;
// …plus `_at(path, …)` versions for tests.

// system: small helpers for the app.
pub fn accessibility() -> PermissionState;     // NotNeeded off the Mac
pub fn request_accessibility();                // macOS's prompt
pub fn open_accessibility_settings();          // `open x-apple.systempreferences:…Privacy_Accessibility`
pub fn open_log_folder();                      // Finder or Explorer
pub fn stay_responsive() -> impl Drop or a unit guard;  // App Nap exemption while sharing (hold it on the main thread)
```

## Logging in the app

- **One global log,** started when the app starts.
  - The file is `hop-app.log` in the usual log folder, rotated like `hop.log` (to `hop-app.old.log`).
  - Status lines also go to a tap, a callback that runs on the log's writer thread and feeds the Activity list.
  - Nothing is echoed to stdout.
- **The rules are unchanged:** never a key, clipboard text, pairing code or secret.

## Release

- **Assets:** a GitHub Release `vX.Y.Z` carries `Hop-macOS.zip` (`Hop.app`, arm64, signed) and `Hop-Windows.zip` (`Hop.exe` plus `README.txt`).
- **The Mac asset** is built and uploaded from the Mac by `scripts/macos/release.sh vX.Y.Z`. It is the release's first asset, and publishing the release starts the Windows build.
- **The Windows asset** is built by `.github/workflows/release.yml`, on `release: published` (or by hand, with a tag), and uploaded to the same release.
- **First run** on each computer (Gatekeeper, SmartScreen, the firewall, Accessibility) is in [INSTALL.md](INSTALL.md).

## Not in this part

- A tray or menu-bar icon.
- Launch at login.
- An installer.
- Renaming the computer.
- Changing other settings: they keep their defaults.
- The self-test, which stays a `hop` command.
