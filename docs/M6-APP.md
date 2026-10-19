# M6 (first part): the Hop app

The user asked for a simple app on each computer that does what the terminal did:
- pair the two computers;
- say which side the other computer is on;
- start and stop sharing, and show what is happening.

Both apps download from the GitHub Releases page. This document is the spec for that work, updated to match what was built. The menu-bar and tray icon, launch at login and the Windows installer stay in M6's later part.

Since then (docs/M4-M5-DESIGN.md), the window also offers the Shortcuts choice, and Activity says when copy and paste works and what happened to files sent between the computers.

## Shape

- **`crates/hop-agent`** (new library). Everything in `apps/hop/src` moved here:
  - the agent: `agent/` (with `exec`, `link`, `start`, `clipboard`, `files`, `fake`, `tests`);
  - `logging`, `native`, `paths`, `simulate` and `startup`;
  - the pairing flow (`pairing/`, with the save rules in `pairing/save.rs`);
  - the command line itself (`cli`).

  It adds the APIs the app needs: `service`, `pairing`, `settings` and `system`. It stays `#![forbid(unsafe_code)]`, and its OS access goes through `hop-platform` and its backends, as before.
- **`apps/hop`** (the `hop` command). Its `main` is a one-line call to `hop_agent::cli::main()`. Every subcommand keeps its behaviour, output and exit codes. Lines in `hop.log` now name `hop_agent::…` where they said `hop::…`; status lines are unchanged.
- **`apps/hop-app`** (new). The app, built with eframe/egui 0.37 and the wgpu renderer (eframe features `default_fonts`, `wgpu` and `clipboard`). The binary is `hop-app`.
  - The Windows release ships it as `Hop.exe`: a windowed program with no console (only its test binary keeps one), with Hop's icon and version details ("Hop") built in, and the C runtime inside, so it needs no Visual C++ Redistributable.
  - On the Mac it is the executable inside `Hop.app` (still `Contents/MacOS/hop`, with the bundle id `com.krishschavan.hop` and the same signing identity, so the Accessibility grant carries over).
  - On macOS only, a first argument that names a `hop` subcommand or a help/version flag goes to `hop_agent::cli::main()`. The scripts start `Hop.app` through LaunchServices with `--args selftest` or `--args run`, so this keeps them working. Anything else opens the window; that covers no arguments and the `-psn_…` arguments LaunchServices may add.
  - `Hop.app`'s Info.plist keeps `LSUIElement`, so those commands never show in the Dock, as before. The window asks for a regular app when it opens (a Dock icon, the app menu, a place in the app switcher), and names the process "Hop", so the app menu says "About Hop" and "Quit Hop".

## The window

One window titled "Hop", about 480×680 points (380×420 at the smallest), resizable, with the icon from `assets/icon`. Sections run top to bottom. The labels in quotes are normative, because `docs/INSTALL.md` quotes them; `apps/hop-app/src/model/words.rs` holds every sentence, and its tests pin the quoted ones. The agent's status lines and the drop zone's label are hop-agent's (`agent/clipboard.rs`, `agent/files.rs`), and pinned there.

1. **Header:** "Hop" and "version {X.Y.Z}", then "This computer: {name} ({OS})".
   - If the settings cannot be read, a red box says "Hop could not read its settings: {error}." with a "Try again" button. Until then the sharing button is greyed out with the hint "Hop cannot share until it can read its settings (see above).", and Your other computer says "Hop cannot show your other computer until it can read its settings (see above)."
2. **Sharing**
   - A coloured dot and one status line:
     - "Not sharing."
     - "Starting…": also while sharing restarts after a change.
     - "Looking for {peer}…": running, not connected yet.
     - "Connected to {peer}."
     - "Using {peer}: this keyboard and mouse control it now.": controlling.
     - "{peer} is using this computer.": controlled.
     - "Stopping…": between Stop sharing (or the window closing) and the agent's last word.
     - "Stopped: {reason}": the reason in full.
   - The button "Start sharing" or "Stop sharing". It is disabled, with the hint "Pair with your other computer first.", while nothing is paired.
   - While running, two lines:
     - "Push the cursor past the {left/right/top/bottom} edge of this screen to move to {peer}."
     - The emergency-keys sentence the agent already writes.
   - On Windows, always: "If Windows asks whether to let Hop use networks, choose Private networks and click Allow."
3. **Your other computer**
   - When nothing is paired: "Not paired yet. Use Pair below."
   - Otherwise, for each paired computer (a leftover entry without a key is not listed, but it still holds its side, and the error names it):
     - "{name} ({OS})".
     - "Where is it?" with four toggle buttons: "Left", "Right", "Above", "Below".
     - The tip "On {name}, choose {opposite} for this computer."
     - "Address (only if it is not found automatically)", a field (greyed hint "e.g. 192.168.1.23") and a "Save address" button. Return in the field saves too.
     - "Forget…", which asks "Forget {name}? You will need to pair again." with "Forget" and "Cancel".
     - A change that cannot be saved says "Not saved: {error}." under that computer.
   - A change saves at once. Sharing restarts when it is running. Forgetting the last paired computer stops sharing instead.
4. **Shortcuts:** what Cmd and Ctrl do with the other computer's keyboard on this one. The computer the keys arrive on applies the choice, so each computer has its own, and the words are about the other computer's keyboard.
   - "With {peer}'s keyboard on this computer:" ("With the other computer's keyboard on this computer:" before pairing), then two choices, the first one the default:
     - on the PC, for keys from the Mac's keyboard: "Keep keys as they are: Ctrl+C copies, Cmd is the Windows key" and "Swap Cmd and Ctrl: Cmd+C copies, as on a Mac";
     - on the Mac, for keys from the PC's keyboard: "Keep keys as they are: the Windows key is Cmd" and "Swap Ctrl and Cmd: Ctrl+C copies, as on a PC".
   - Under them: "The choice for this computer's keyboard is in Hop on {peer}." ("…on the other computer." before pairing).
   - A choice saves at once (`keymap.cmd_ctrl`, `keep` or `swap`), and sharing restarts when it is running, because the agent maps keys as it starts. A change that cannot be saved says "Not saved: {error}." under the choice.
   - While the settings cannot be read: "Hop cannot show this choice until it can read its settings (see above)."
5. **Pair**, with two buttons: "Show a pairing code" and "Enter a code".
   - **Show a pairing code:**
     - "Getting a code…", then the code, large: "482 913". It cannot be selected, so a stray drag cannot copy it.
     - "On the other computer, open Hop, click Enter a code and type this code. It works once, for 2 minutes. Never show it in a screen share."
     - "Time left: m:ss", and "Cancel" ("Cancelling…" until the session has ended, then "Pairing cancelled.").
     - Wrong tries show "Someone entered a wrong code (1 of 3 tries used)."
     - If this computer could not be announced on the local network: "Could not announce this computer on the local network ({reason}). On the other computer, type this computer's IP address in Address, under "If the other computer is not found"."
     - When the other computer has entered the code, a dialog asks "{name} ({OS}) entered the code. Pair with it?", followed by the replacement sentence when there is one, "Answer Yes only if {name} says it is waiting for you." and "No answer before the time runs out counts as No (m:ss left)." The buttons are "Yes, pair" and "No"; clicking beside the dialog does not answer. Once the time is up, the buttons go and the dialog says "No answer in time." until the session ends; a late click counts for nothing. The question bounces the Dock icon (on Windows, flashes the taskbar button), in case the window is hidden.
     - After Yes: "Pairing with {name}…".
   - **Enter a code:**
     - A "Code" field (greyed hint "6 digits"; spaces are fine).
     - Under "If the other computer is not found", an "Address" field.
     - "Pair" (or Return in the Code field) and "Cancel".
     - Progress lines: "Looking for the computer showing the code…", "Connecting…", "Now click Yes, pair on {host}."
     - When nothing is found, or several computers are found, it says so and points to the Address field ("…type its address in Address, under "If the other computer is not found", and click Pair again.").
     - The typed code stays after a failure, so "click Pair again" works. It is wiped once pairing succeeds, when the form is cancelled, and when this computer shows a code instead.
   - **The result:** "Paired with {name}. It sits {to the left of/to the right of/above/below} this screen; change that under Your other computer." When the pairing replaced one with another key, "It replaces the pairing with {old}, whose old key can no longer connect to this computer." When this computer saved the pairing but cannot be sure the other heard the yes, "However: {reason}." Or "Not paired: {reason}." When sharing was running, it restarts with the new pairing.
   - **Reasons** are the app's own sentences, never the terminal's ("run hop pair again"): for example "the code did not match. Check it against the code the other computer shows, and click Pair again".
6. **Permissions** (Mac only)
   - "Accessibility: Allowed" with a green tick.
   - Or "Accessibility: Not allowed yet" with the button "Allow…", and under it "Hop needs Accessibility to share this Mac's keyboard and mouse." The button shows macOS's prompt and opens System Settings > Privacy & Security > Accessibility.
   - The state is checked again every 2 seconds until it is granted.
7. **Activity**
   - The last 100 status lines, newest at the bottom, in a scroll area.
   - Pairing progress lines go here too, but never to the log: a reason can quote the address the user typed. The log gets one line per pairing, with the name and the side.
   - **Copy and paste:** the agent's status line "Copy and paste now work between this computer and {peer}." once for each connection that carries clipboard text. Nothing is said per copy, and copied text is never shown or logged. If this computer's clipboard cannot be read as the cursor leaves, the agent says why once a run: "Copied text did not go to {peer}: {reason}." On a Mac where macOS does not let Hop read the clipboard, the reason names System Settings > Privacy & Security > Paste from Other Apps.
   - **Files:** the agent's status lines count files and give reasons, never names.
     - The computer that sends says "Sending 3 items to {peer}…" when files are dropped on the edge, then "Sent 3 files to {peer}." (with how many folders or unreadable files were left out), or why not: "The files you dropped were not sent: {peer} is not connected.", "{peer} did not take the files: …", "{peer} could not save the files in its Hop Received folder, so nothing was sent. …" (with where to switch on Downloads Folder on a Mac), "{peer} did not answer, so nothing was sent. …", or "Not all the files reached {peer}: …". It waits 2 minutes for the answer, since the first files to a Mac wait there until the user allows Hop to use the Downloads folder.
     - The computer that receives them says "Received 2 files from {peer}. Their folder opens now." (or "Its folder" for one file), and opens the folder: in Finder or Explorer, one file shows selected (`open -R`, `explorer /select,"…"`), several show in their folder. Or why not: "Not all the files from {peer} arrived: …", "Files from {peer} did not come: …" (the other computer stopped waiting for the answer), or "Could not take the files from {peer}: …" when it cannot use its Hop Received folder, since that is where it can be fixed.
     - A transfer that breaks off is blamed on the right computer on each side: one that could not read or save a file says so of itself, and the other one says the other computer could not save a file or stopped sending.
   - Files that arrived are also named, by the app's own line "Files from {peer}: notes.txt, photo.jpg" ("File from" for one): the first 10 names, then "and {n} more". The names come in the agent's `FilesReceived` update; they are shown here and never written to the log.
   - "Open log folder".

**Closing the window** stops sharing first (failing open: input back, then goodbye), waits at most 3 seconds, and quits:
- With the close button, the window stays up, saying "Stopping…", until sharing and pairing have stopped or the 3 seconds are up.
- With Cmd+Q, macOS quits as soon as eframe's exit hook returns, so the hook does the same work and blocks for up to 3 seconds.
- On a Mac, a request to quit from the OS (SIGTERM from `kill`, Ctrl+C or the closing of a terminal that started Hop) closes the window the same way. A second request exits at once, as `hop run` does.
- On Windows, signing out or shutting down ends Hop without the quit sequence (a windowed program gets no console events; catching the window's end-session messages is left for later). The OS removes Hop's hooks, and the Mac gets its keyboard and mouse back within 1.5 s, when its link times out.

## APIs in hop-agent (sketch; names may change, guarantees may not)

```rust
// service: the agent on its own thread, for apps.
pub const PANICKED: &str;                            // the fixed Stopped reason after a panic
pub enum Session { Idle, Controlling, Controlled }   // Entering counts as Controlling
pub enum AgentUpdate {
    Started { peer: String, side: Side, emergency_keys: String },
    Connected { name: String, os: Os },
    Disconnected,
    Session(Session),
    FilesReceived { from: String, names: Vec<String> }, // file names: show them, never log them
    Stopped { error: Option<String> },               // always the last update
}
pub struct AgentHandle { /* signals, thread */ }     // dropping it stops the agent
impl AgentHandle {
    pub fn start(updates: std::sync::mpsc::Sender<AgentUpdate>) -> std::io::Result<AgentHandle>;
    pub fn stop(&self);                               // asks to quit, failing open; returns at once
    pub fn is_finished(&self) -> bool;
    pub fn wait(self, timeout: Duration) -> bool;     // true when the thread ended in time
}
pub fn install_panic_hook(log: Option<logging::Log>); // once, on the main thread
pub fn on_quit_signal(on_quit: impl FnOnce() + Send + 'static) -> std::io::Result<()>; // Mac: SIGTERM, SIGINT, SIGHUP
```

- **The agent thread:**
  - It runs the same sequence as `hop run`, ending with the same quit sequence.
  - Errors become `Stopped { error: Some(text) }`, with the full `{:#}` chain: not paired, Accessibility missing, port in use.
  - A stop that arrives while the agent is still starting ends it before capture starts (or before the network, if it is that early): `Stopped` comes without `Started`. `hop run` does the same for a very early Ctrl+C.
  - A panic becomes the fixed sentence `PANICKED`.
  - To restart: `stop()`, wait for the thread to end, then `start()` again. A new agent cannot bind the port before the old one has let go.
- **Updates:**
  - The core thread only ever `send`s on an unbounded channel, so it never waits for the app.
  - Session updates go out only when the session kind changes.
  - `FilesReceived` goes out once files from the other computer have arrived complete in the Received folder, with the sender's name and the name each file was saved under. It is apart from the session: it never changes the session kind. It is the one update with file names in it, so no update is ever logged, its `Debug` included.
  - Files sent, and transfers that stopped, are status lines (counts and reasons only), so they reach Activity as every status line does, and the terminal too under `hop run`.
- **The app's side:** a bridge thread for each agent run and each pairing session drains its channel into the app's state and calls `request_repaint`. A panic while handing one item over does not end the bridge, so the last word always arrives; the log's tap for the Activity list is guarded the same way.
- **Panics while the app runs:** a panic anywhere asks the running agent to quit, as `hop run`'s hook does, and the hook logs only fixed text. On a Mac, AppKit calls the window's code through functions a panic cannot unwind out of, so a panic on the main thread would abort the process before anything was dropped. Therefore:
  - on the main thread, the hook also waits up to 3 seconds for the agent to have stopped (an agent never waits for the main thread, so this cannot deadlock), then up to 1 second for the log;
  - each frame of the window runs guarded: a panic in it stops sharing and pairing (`shutdown`) before it goes on.

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
    Unconfirmed { reason: String },   // saved here, but the other computer may not know; Finished(Ok) follows
    Finished(Result<Saved, String>),  // always the last event
}
pub struct PairSession { /* thread, runtime, cancel, answer */ }  // dropping it cancels
impl PairSession {
    pub fn start(role: PairRole, events: std::sync::mpsc::Sender<PairEvent>) -> std::io::Result<PairSession>;
    pub fn answer(&self, yes: bool);  // to Confirm
    pub fn cancel(&self);             // closes the listener or connection at once; Finished follows
    pub fn is_finished(&self) -> bool;
}
```

- **Front ends:**
  - The terminal and the app drive one async flow, and each words the events its own way.
  - The terminal keeps every current sentence, `--address` included: hop-net's errors print as they always did.
  - The app gets its own sentences, which never mention `hop pair` or "laptops" (the one exception: a pairing port in use asks whether `hop pair` holds it).
  - The save rules and their tests (`add_peer`, `save_peer`, `replaced_old_key`) moved unchanged.
  - The session trims the code and the address; an empty address means "look for it on the network".
- **Errors:** pairing errors reach the app as plain sentences. Fixed ones: "cancelled", "nothing was saved on either computer", "no answer in time, so nothing was saved on either computer". The reason can quote the address the user typed, so it is shown but never logged. Only the app shows the code, and the code is dropped (zeroized) when the session ends.
- **A joiner that stops early** (the app's Cancel, or its window closing, while it waits for the answer) closes its connection with `EXPIRED`, never with the normal close the other computer takes as "the yes arrived". A cancelled session keeps its network running for another 100 ms, so that close (or the `DECLINED` of a question nobody answered) actually leaves the computer. So the computer showing the code does not report a pairing the joiner did not save.
- **One narrow gap is left:** a Cancel that lands in the fraction of a second after the yes arrived, while the joiner says goodbye but before it saves. The computer showing the code then reports a pairing that the joiner forgot. Pairing again fixes it. Closing the gap means saving before the goodbye, inside hop-net's `pair_join`; that is on the roadmap.

```rust
// settings: read and change the config file. Each change re-reads the file, validates, and saves atomically.
pub struct Overview { pub name: String, pub os: Os, pub config_path: PathBuf, pub peers: Vec<PeerView>, pub cmd_ctrl: CmdCtrl, pub notes: Vec<String> }
pub struct PeerView { pub id: DeviceId, pub name: String, pub os: Os, pub side: Side, pub address: Option<String>, pub paired: bool }
pub fn overview() -> anyhow::Result<Overview>;  // startup::load(): makes the key and config on a first run
pub fn set_side(id: &DeviceId, side: Side) -> anyhow::Result<()>;  // refuses a side another peer has
pub fn set_address(id: &DeviceId, address: Option<&str>) -> anyhow::Result<()>;  // trimmed; empty = none
pub fn forget(id: &DeviceId) -> anyhow::Result<()>;  // an id that is already gone is fine
pub fn set_cmd_ctrl(choice: CmdCtrl) -> anyhow::Result<()>;  // Shortcuts: Keep (the default) or Swap
pub fn opposite(side: Side) -> Side;
// …plus `_at(path, …)` versions for tests.

// system: small helpers for the app.
pub fn accessibility() -> PermissionState;     // NotNeeded off the Mac; never prompts
pub fn request_accessibility();                // macOS's prompt
pub fn open_accessibility_settings();          // `open x-apple.systempreferences:…Privacy_Accessibility`
pub fn open_log_folder();                      // makes it, then Finder or Explorer
pub fn use_app_name();                         // Mac: names the process "Hop" for the app menu
pub fn stay_responsive() -> Responsive;        // App Nap exemption while sharing (hold it on the main thread)
```

The window reads the settings again after every change and pairing. A change of the computers or of the Shortcuts choice restarts a running agent. What loading has to say (a key made just now, a problem with the config) goes to the log and Activity once, as at startup.

## Logging in the app

- **One global log,** started when the app starts (before anything else, since a process has only one).
  - The file is `hop-app.log` in the usual log folder, rotated like `hop.log` (to `hop-app.old.log`).
  - Status lines also go to a tap, a callback that runs on the log's writer thread and feeds the Activity list.
  - Nothing is echoed to stdout.
- **The rules are unchanged:** never a key, clipboard text, pairing code or secret, and never a file name or a file's contents. File names appear only in Activity, from `FilesReceived`; the log counts files.

## Development

- A development build with the `screenshot` feature saves the window, as it is first drawn, to the file `EFRAME_SCREENSHOT_TO` names, then exits: `EFRAME_SCREENSHOT_TO=shot.png cargo run -p hop-app --features screenshot`. eframe takes screenshots only with its OpenGL renderer, so the feature adds that renderer and the app uses it then. It opens the window for a moment, so tell the user first.
- The window's tests draw it headless with egui, for each state above, and click its buttons with made-up pointer events: nothing reaches the real mouse or screen.

## Release

- **Assets:** a GitHub Release `vX.Y.Z` carries `Hop-macOS.zip` (`Hop.app`, arm64, signed) and `Hop-Windows.zip` (`Hop.exe` plus `README.txt`).
- **The Mac asset** is built and uploaded from the Mac by `scripts/macos/release.sh vX.Y.Z`. It is the release's first asset, and publishing the release starts the Windows build. The script refuses unless the tag matches the version, the tree is clean, `Cargo.lock` matches `Cargo.toml`, and HEAD is on origin.
- **The Windows asset** is built by `.github/workflows/release.yml`, on `release: published` (or by hand, with a tag), and uploaded to the same release. It builds with `--locked` and the C runtime linked in (`crt-static`), and keeps the job's token out of the checkout while cargo runs.
- **First run** on each computer (the browser's warning, Gatekeeper, SmartScreen and Smart App Control, the firewall, Accessibility) is in [INSTALL.md](INSTALL.md).

## Not in this part

- A tray or menu-bar icon.
- Launch at login.
- An installer.
- Signing Hop.exe and notarizing Hop.app. Until then each computer warns the first time, and Smart App Control, when it is on, blocks Hop.exe.
- Renaming the computer.
- Changing other settings (besides the side, the address and Shortcuts): they keep their defaults.
- The self-test, which stays a `hop` command.
