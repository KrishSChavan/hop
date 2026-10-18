//! `hop run`: everything around the core loop. It loads the config and the
//! key, starts the network (binding the port first, so a second copy of Hop
//! stops before it touches input), starts capture, runs the core loop on
//! this thread, and on the way out gives input back before it says goodbye
//! to the peer.

use std::io;
use std::panic::{self, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use hop_config::{Config, Side};
use hop_core::{Chord, CoreConfig};
use hop_keymap::ModifierPolicy;
use hop_net::{Fingerprint, NetConfig, NetError, PinnedPeer};
use hop_platform::{Permission, PermissionState, Platform, PlatformError};
use hop_types::{KeyCode, Os};
use tokio::runtime::Runtime;

use super::link::NetLink;
use super::{Agent, AgentError, PeerPlacement, Settings, Signals};
use crate::logging::{Destination, Filter, Log};
use crate::{native, paths, startup, status};

/// How long the log gets to write its last lines before Hop exits.
const LOG_FLUSH: Duration = Duration::from_secs(1);

/// The network runtime's own threads get this long to finish on the way out.
const RUNTIME_SHUTDOWN: Duration = Duration::from_secs(1);

/// Runs the agent on this thread until asked to quit. On macOS this is
/// `run_with_ui`'s worker thread, so AppKit keeps the main thread.
pub(crate) fn main() -> Result<ExitCode> {
    let signals = Arc::new(Signals::default());
    // This thread is the core thread: everything that rings, rings it.
    signals.attach();
    let log = start_log();
    install_panic_hook(signals.clone(), log.clone());
    // A panic here has already given input back by the time it reaches
    // this point (unwinding dropped the agent and the platform); catching
    // it only gives the log a moment to write the hook's report before the
    // panic goes on to end the process.
    let outcome = panic::catch_unwind(AssertUnwindSafe(|| run(&signals)));
    if let Ok(Err(error)) = &outcome {
        tracing::error!(error = %format!("{error:#}"), "Hop stopped");
    }
    if let Some(log) = &log {
        log.flush(LOG_FLUSH);
    }
    let code = match outcome {
        Ok(result) => result?,
        Err(payload) => panic::resume_unwind(payload),
    };
    Ok(if signals.panicked() {
        ExitCode::FAILURE
    } else {
        code
    })
}

fn run(signals: &Arc<Signals>) -> Result<ExitCode> {
    status!("Hop {} is starting.", env!("CARGO_PKG_VERSION"));
    let startup = startup::load()?;
    for note in &startup.notes {
        status!("{note}");
    }
    let config = &startup.config;
    let chosen = choose_peer(config, &startup.config_path)?;
    let core = core_config(config);
    let emergency = core.emergency_chord.clone();
    let settings = Settings {
        me: startup.identity.device_id(),
        os: Os::current(),
        core,
        policy: ModifierPolicy {
            swap_cmd_ctrl: config.keymap.swap_cmd_ctrl,
        },
        peer: PeerPlacement {
            id: chosen.pin.id.clone(),
            name: chosen.pin.name.clone(),
            side: chosen.side.edge(),
        },
    };

    let platform = native::native().context("could not start Hop's platform layer")?;
    let displays = platform
        .displays
        .displays()
        .context("could not read this computer's displays")?;

    let runtime = NetRuntime::new()?;
    let mut net = NetConfig::new(
        config.network.port,
        config.device.name.clone(),
        env!("CARGO_PKG_VERSION"),
    );
    net.displays = displays;
    net.peers = vec![chosen.pin.clone()];
    let ring = signals.clone();
    net.on_event = Some(Arc::new(move || ring.ring()));
    // Bound before capture starts: the port is the single-instance lock.
    let (handle, events) = runtime
        .block_on(hop_net::start(startup.identity.clone(), net))?
        .map_err(|error| match error {
            NetError::Bind { source, .. } => {
                bind_failed(config.network.port, &source, &startup.config_path)
            }
            other => anyhow::Error::new(other).context("could not start the network"),
        })?;
    runtime.spawn(quit_on_signal(signals.clone()));

    check_accessibility(&platform)?;
    let link = NetLink::new(handle.clone(), events);
    let mut agent =
        Agent::start(platform, link, settings, signals.clone()).map_err(|error| match error {
            AgentError::Capture(PlatformError::PermissionDenied(permission)) => anyhow!(
                "Hop needs the {permission} permission to share this computer's keyboard and \
                 mouse. Open System Settings > Privacy & Security > {permission}, switch Hop \
                 on, then start Hop again"
            ),
            other => anyhow::Error::new(other),
        })?;
    status!(
        "Hop is running. Push the cursor past the {} edge of this computer's screen to move to \
         {}. Quit with Ctrl+C{}.",
        chosen.side,
        chosen.pin.name,
        if cfg!(target_os = "macos") {
            " in the Terminal window that started Hop"
        } else {
            ""
        }
    );
    status!("{}", emergency_keys_note(&emergency));
    agent.run();
    status!("Quitting: giving the keyboard and mouse back.");
    agent.quit();
    // Dropping the agent fails open once more (it is idempotent) and drops
    // the platform: capture stops and the injector lets go of everything.
    drop(agent);
    runtime.block_on(handle.shutdown())?;
    drop(runtime);
    status!("Hop stopped.");
    Ok(ExitCode::SUCCESS)
}

/// What to tell the user when Hop's port cannot be bound. A port in use
/// almost always means another copy of Hop; any other refusal (Windows
/// reserves port ranges for Hyper-V and WSL, and answers "access denied")
/// does not, and sending the user to look for one would mislead them.
fn bind_failed(port: u16, source: &io::Error, config_path: &Path) -> anyhow::Error {
    let elsewhere = format!(
        "set network.port in {} to another number (at most 65534) on both computers",
        config_path.display()
    );
    if source.kind() == io::ErrorKind::AddrInUse {
        anyhow!(
            "UDP port {port} is already in use, most likely by another copy of Hop on this \
             computer: quit it first. If no other copy is running, another program uses the \
             port: {elsewhere}"
        )
    } else {
        anyhow!("this computer would not let Hop use UDP port {port} ({source}): {elsewhere}")
    }
}

/// Starts the log and makes it the destination of every `tracing` event.
fn start_log() -> Option<Log> {
    let (log, path) = open_log(paths::log_dir())?;
    if log.install(Filter::AGENT)
        && let Some(path) = &path
    {
        println!("Hop's log: {}", path.display());
    }
    Some(log)
}

/// The agent's log: `hop.log` in `dir`, and the file it writes. Without a
/// file Hop still runs, and its status lines still reach the terminal.
fn open_log(dir: Option<PathBuf>) -> Option<(Log, Option<PathBuf>)> {
    let why = match dir {
        Some(dir) => {
            let path = dir.join(paths::LOG_FILE);
            match Log::start(Destination::File(path.clone())) {
                Ok(log) => return Some((log, Some(path))),
                Err(error) => format!("could not open the log {} ({error})", path.display()),
            }
        }
        None => "could not work out where the log folder is".to_owned(),
    };
    eprintln!("warning: {why}; running without a log");
    Log::start(Destination::StatusOnly)
        .ok()
        .map(|log| (log, None))
}

/// Any panic, on any thread, ends Hop failing open (docs/M2-DESIGN.md,
/// "Panics"). On the core thread, unwinding drops the platform, which gives
/// input back; elsewhere the hook asks the core thread to quit.
///
/// The hook writes the thread and the place, and the message only when it
/// is a fixed string: a formatted message could quote input. It never
/// blocks.
fn install_panic_hook(signals: Arc<Signals>, log: Option<Log>) {
    panic::set_hook(Box::new(move |info| {
        let thread = std::thread::current();
        let name = thread.name().unwrap_or("unnamed");
        let place = info
            .location()
            .map(|at| format!("{}:{}", at.file(), at.line()))
            .unwrap_or_else(|| "an unknown place".to_owned());
        let message = info
            .payload()
            .downcast_ref::<&'static str>()
            .copied()
            .unwrap_or("its message is not logged");
        let text = format!(
            "Hop hit an internal error (thread {name}, {place}: {message}). Giving the keyboard \
             and mouse back and quitting."
        );
        match &log {
            Some(log) => log.write_status(&text),
            None => eprintln!("{text}"),
        }
        signals.mark_panicked();
        signals.request_quit();
    }));
}

/// The paired computer to share with, as `hop-net` pins it.
struct ChosenPeer {
    pin: PinnedPeer,
    side: Side,
}

/// The first paired computer in the config. M2 shares with one computer;
/// any others are left alone, with a status line saying so.
fn choose_peer(config: &Config, path: &Path) -> Result<ChosenPeer> {
    let mut paired = config.peers.iter().filter_map(|peer| {
        let fingerprint = Fingerprint::from_hex(peer.pinned_fingerprint()?).ok()?;
        Some(ChosenPeer {
            pin: PinnedPeer {
                id: peer.id.clone(),
                fingerprint,
                name: peer.name.clone(),
                address: peer.address.clone(),
            },
            side: peer.side,
        })
    });
    let Some(first) = paired.next() else {
        if config.peers.is_empty() {
            bail!(
                "this computer is not paired with another one yet. Run \"hop pair\" on one \
                 computer and \"hop pair <code>\" on the other (from a terminal), then start Hop \
                 again"
            );
        }
        bail!(
            "none of the computers in {} is paired yet (they have no fingerprint). Run \"hop \
             pair\" on both computers, then start Hop again",
            path.display()
        );
    };
    let others: Vec<String> = paired.map(|peer| peer.pin.name).collect();
    if !others.is_empty() {
        status!(
            "Hop shares with one computer at a time for now: using {}, not {}.",
            first.pin.name,
            others.join(", ")
        );
    }
    Ok(first)
}

/// The hand-off settings from the config.
fn core_config(config: &Config) -> CoreConfig {
    let input = &config.input;
    CoreConfig {
        edge_delay_ms: input.edge_delay_ms,
        corner_dead_zone: input.corner_dead_zone,
        pointer_speed: input.pointer_speed,
        takeback_threshold: input.takeback_threshold,
        emergency_chord: chord(&input.emergency_chord),
        ..CoreConfig::default()
    }
}

/// The emergency chord from its key names: the last key completes it, the
/// others must be held. A name Hop does not know falls back to the default
/// chord, because a session must always have a way out.
pub(crate) fn chord(names: &[String]) -> Chord {
    let codes: Option<Vec<KeyCode>> = names
        .iter()
        .map(|name| hop_keymap::from_name(name))
        .collect();
    match codes.as_deref() {
        Some([modifiers @ .., key]) => Chord::new(modifiers.iter().copied(), *key),
        _ => {
            status!(
                "input.emergency_chord names a key Hop does not know, so the emergency keys are \
                 the default ones: Left Control + Left Option/Alt + Left Shift + Escape."
            );
            Chord::default()
        }
    }
}

/// Says how to end a session at once from this computer's keyboard.
fn emergency_keys_note(chord: &Chord) -> String {
    let keys = if *chord != Chord::default() {
        "the keys set in input.emergency_chord"
    } else if cfg!(target_os = "macos") {
        "Control + Option + Shift + Escape (the left-hand keys)"
    } else {
        "Ctrl + Alt + Shift + Esc (the left-hand keys)"
    };
    format!(
        "If the keyboard and mouse ever seem stuck on the other computer, press {keys} on this \
         one to bring them back at once."
    )
}

/// On a Mac, capture needs Accessibility. Asking shows macOS's prompt; the
/// user switches Hop on and starts it again.
fn check_accessibility(platform: &Platform) -> Result<()> {
    match platform.permissions.state(Permission::Accessibility) {
        PermissionState::Granted | PermissionState::NotNeeded => Ok(()),
        PermissionState::Denied | PermissionState::NotDetermined => {
            let _ = platform.permissions.request(Permission::Accessibility);
            bail!(
                "Hop needs the Accessibility permission to share this Mac's keyboard and mouse. \
                 Open System Settings > Privacy & Security > Accessibility, switch Hop on, then \
                 start Hop again"
            )
        }
    }
}

/// Waits for a request to quit and passes it to the core thread. A second
/// request exits at once, even if the core thread is stuck: the OS then
/// removes Hop's input hooks, and the other computer lets go of what it
/// injected within 1.5 s.
async fn quit_on_signal(signals: Arc<Signals>) {
    let why = next_quit_signal().await;
    tracing::info!(signal = why, "asked to quit");
    signals.request_quit();
    let _ = next_quit_signal().await;
    tracing::warn!("asked to quit again; exiting at once");
    std::process::exit(130);
}

#[cfg(unix)]
async fn next_quit_signal() -> &'static str {
    use tokio::signal::unix::{SignalKind, signal};

    async fn wait(kind: SignalKind) {
        match signal(kind) {
            Ok(mut listener) => {
                if listener.recv().await.is_none() {
                    std::future::pending::<()>().await;
                }
            }
            // Without this listener the others still work.
            Err(_) => std::future::pending::<()>().await,
        }
    }

    tokio::select! {
        () = wait(SignalKind::interrupt()) => "Ctrl+C",
        () = wait(SignalKind::terminate()) => "a request to quit (SIGTERM)",
        () = wait(SignalKind::hangup()) => "the terminal closing (SIGHUP)",
    }
}

#[cfg(windows)]
async fn next_quit_signal() -> &'static str {
    use tokio::signal::windows;

    // tokio keeps the console's close, logoff and shutdown notifications
    // waiting until the process exits, which gives Hop the few seconds
    // Windows allows to give input back.
    macro_rules! wait {
        ($listener:expr) => {
            async {
                match $listener {
                    Ok(mut listener) => {
                        if listener.recv().await.is_none() {
                            std::future::pending::<()>().await;
                        }
                    }
                    Err(_) => std::future::pending::<()>().await,
                }
            }
        };
    }

    tokio::select! {
        () = wait!(windows::ctrl_c()) => "Ctrl+C",
        () = wait!(windows::ctrl_break()) => "Ctrl+Break",
        () = wait!(windows::ctrl_close()) => "the console window closing",
        () = wait!(windows::ctrl_logoff()) => "signing out",
        () = wait!(windows::ctrl_shutdown()) => "the computer shutting down",
    }
}

#[cfg(not(any(unix, windows)))]
async fn next_quit_signal() -> &'static str {
    let _ = tokio::signal::ctrl_c().await;
    "Ctrl+C"
}

/// The network's tokio runtime, with two workers. Dropping it gives its
/// threads a bounded time to finish, so a stuck name lookup cannot keep Hop
/// from exiting.
struct NetRuntime {
    runtime: Option<Runtime>,
}

impl NetRuntime {
    fn new() -> Result<NetRuntime> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("hop-net")
            .enable_all()
            .build()
            .context("could not start the network threads")?;
        Ok(NetRuntime {
            runtime: Some(runtime),
        })
    }

    fn block_on<F: std::future::Future>(&self, future: F) -> Result<F::Output> {
        let runtime = self
            .runtime
            .as_ref()
            .ok_or_else(|| anyhow!("the network runtime is gone"))?;
        Ok(runtime.block_on(future))
    }

    fn spawn<F>(&self, future: F)
    where
        F: std::future::Future + Send + 'static,
        F::Output: Send + 'static,
    {
        if let Some(runtime) = &self.runtime {
            runtime.spawn(future);
        }
    }
}

impl Drop for NetRuntime {
    fn drop(&mut self) {
        if let Some(runtime) = self.runtime.take() {
            runtime.shutdown_timeout(RUNTIME_SHUTDOWN);
        }
    }
}

#[cfg(test)]
mod tests {
    use hop_config::PeerConfig;
    use hop_types::DeviceId;

    use super::*;

    /// Set for the child process of the signal test, and only for it.
    #[cfg(unix)]
    const SIGNAL_CHILD: &str = "HOP_TEST_SIGNAL_CHILD";

    /// The signal test sends real signals, so it sends them to a child
    /// process (this test binary again, running only `signal_child`), never
    /// to the process running the other tests.
    #[cfg(unix)]
    #[test]
    fn ctrl_c_asks_hop_to_quit_and_a_second_request_exits_at_once() {
        use std::io::{BufRead, BufReader};
        use std::process::{Child, Command, Stdio};
        use std::sync::mpsc;
        use std::time::Instant;

        /// Ends the child however the test ends.
        struct Reap(Child);
        impl Drop for Reap {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }

        let child = Command::new(std::env::current_exe().expect("test binary"))
            .args([
                "--exact",
                "agent::start::tests::signal_child",
                "--ignored",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(SIGNAL_CHILD, "1")
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("child");
        let mut child = Reap(child);
        let stdout = child.0.stdout.take().expect("stdout");
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                let _ = tx.send(line);
            }
        });
        let wait_for = |wanted: &str| {
            let deadline = Instant::now() + Duration::from_secs(20);
            loop {
                let left = deadline.saturating_duration_since(Instant::now());
                match rx.recv_timeout(left) {
                    // libtest prints the test's name on the same line.
                    Ok(line) if line.trim_end().ends_with(wanted) => return,
                    Ok(_) => {}
                    Err(_) => panic!("the child never said {wanted:?}"),
                }
            }
        };
        let send = |signal: &str| {
            let sent = Command::new("kill")
                .args([signal, &child.0.id().to_string()])
                .status()
                .expect("kill");
            assert!(sent.success());
        };
        wait_for("ready");
        send("-INT");
        wait_for("quit requested");
        send("-TERM");
        let deadline = Instant::now() + Duration::from_secs(20);
        let status = loop {
            if let Some(status) = child.0.try_wait().expect("wait") {
                break status;
            }
            assert!(
                Instant::now() < deadline,
                "a second request did not end the child"
            );
            std::thread::sleep(Duration::from_millis(20));
        };
        assert_eq!(status.code(), Some(130), "{status:?}");
    }

    /// The child side of the signal test; does nothing unless started by it.
    #[cfg(unix)]
    #[test]
    #[ignore = "run in a child process by ctrl_c_asks_hop_to_quit_and_a_second_request_exits_at_once"]
    fn signal_child() {
        use tokio::signal::unix::{SignalKind, signal};

        if std::env::var_os(SIGNAL_CHILD).is_none() {
            return;
        }
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .expect("runtime");
        // Listening first means a signal that came before `quit_on_signal`
        // listens is caught here instead of ending the process.
        let _early = runtime.block_on(async {
            (
                signal(SignalKind::interrupt()).expect("SIGINT"),
                signal(SignalKind::terminate()).expect("SIGTERM"),
            )
        });
        let signals = Arc::new(Signals::default());
        runtime.spawn(quit_on_signal(signals.clone()));
        std::thread::sleep(Duration::from_millis(300));
        println!("ready");
        let deadline = std::time::Instant::now() + Duration::from_secs(15);
        while !signals.quit_requested() {
            assert!(std::time::Instant::now() < deadline, "no quit request");
            std::thread::sleep(Duration::from_millis(10));
        }
        println!("quit requested");
        // The second request ends this process with code 130.
        std::thread::sleep(Duration::from_secs(15));
    }

    fn peer(name: &str, hex: char, side: Side, paired: bool) -> PeerConfig {
        let fingerprint: String = std::iter::repeat_n(hex, 64).collect();
        PeerConfig {
            name: name.to_owned(),
            id: DeviceId::new(&fingerprint[..32]),
            os: Os::Windows,
            side,
            address: Some("192.168.1.20".to_owned()),
            fingerprint: paired.then_some(fingerprint),
        }
    }

    #[test]
    fn hop_run_needs_a_paired_computer() {
        let path = Path::new("config.toml");
        let mut config = Config::default();
        let error = choose_peer(&config, path).err().expect("no peers");
        assert!(error.to_string().contains("hop pair"), "{error}");
        config.peers.push(peer("PC", 'a', Side::Right, false));
        let error = choose_peer(&config, path).err().expect("not paired");
        assert!(error.to_string().contains("hop pair"), "{error}");
    }

    #[test]
    fn the_first_paired_computer_is_pinned_with_its_address_and_side() {
        let mut config = Config::default();
        config.peers.push(peer("Old", 'a', Side::Left, false));
        config.peers.push(peer("PC", 'b', Side::Right, true));
        config.peers.push(peer("Laptop", 'c', Side::Above, true));
        let chosen = choose_peer(&config, Path::new("config.toml")).expect("paired");
        assert_eq!(chosen.pin.name, "PC");
        assert_eq!(chosen.pin.id, DeviceId::new("b".repeat(32)));
        assert_eq!(chosen.pin.fingerprint.to_hex(), "b".repeat(64));
        assert_eq!(chosen.pin.address.as_deref(), Some("192.168.1.20"));
        assert_eq!(chosen.side, Side::Right);
    }

    #[test]
    fn a_panic_on_any_thread_asks_the_core_thread_to_quit_and_logs_no_formatted_text() {
        let lines = Arc::new(std::sync::Mutex::new(Vec::new()));
        let log = Log::start(Destination::Memory(lines.clone())).expect("log");
        let signals = Arc::new(Signals::default());
        // The hook is process-wide: install it only around the two panics.
        // A panic in another test meanwhile only lands in this log too.
        let previous = panic::take_hook();
        install_panic_hook(signals.clone(), Some(log.clone()));
        let fixed = std::thread::Builder::new()
            .name("hop-net".into())
            .spawn(|| panic!("a fixed test message"))
            .expect("thread")
            .join();
        let secret = String::from("hunter2");
        let formatted = std::thread::spawn(move || panic!("typed {secret}")).join();
        panic::set_hook(previous);
        assert!(fixed.is_err() && formatted.is_err());
        assert!(signals.quit_requested(), "the core thread is asked to quit");
        assert!(signals.panicked(), "and Hop exits with a failure");

        log.flush(Duration::from_secs(5));
        let text = lines
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .join("\n");
        assert!(
            text.contains("thread hop-net") && text.contains("a fixed test message"),
            "{text}"
        );
        assert!(text.contains("its message is not logged"), "{text}");
        assert!(!text.contains("hunter2"), "{text}");
    }

    #[test]
    fn without_a_log_file_status_lines_still_reach_the_terminal() {
        let dir = std::env::temp_dir().join(format!("hop-no-log-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("dir");
        // A log folder inside a file can never be made.
        let blocker = dir.join("not-a-folder");
        std::fs::write(&blocker, "").expect("file");
        for unusable in [Some(blocker.join("Logs")), None] {
            let (_, file) = open_log(unusable.clone())
                .unwrap_or_else(|| panic!("no log at all for {unusable:?}"));
            assert_eq!(file, None, "{unusable:?}");
        }
        let (_, file) = open_log(Some(dir.join("Logs"))).expect("a log");
        assert_eq!(file, Some(dir.join("Logs").join(paths::LOG_FILE)));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_port_in_use_and_a_refused_port_get_different_advice() {
        let path = Path::new("config.toml");
        let taken = bind_failed(47800, &io::Error::from(io::ErrorKind::AddrInUse), path);
        let taken = format!("{taken:#}");
        assert!(taken.contains("another copy of Hop"), "{taken}");
        assert!(taken.contains("network.port"), "{taken}");
        // Windows refuses ports it reserves (for Hyper-V, say) with "access
        // denied": no other copy of Hop to look for then.
        let refused = bind_failed(
            47800,
            &io::Error::from(io::ErrorKind::PermissionDenied),
            path,
        );
        let refused = format!("{refused:#}");
        assert!(!refused.contains("already running"), "{refused}");
        assert!(refused.contains("network.port in config.toml"), "{refused}");
    }

    #[test]
    fn the_input_settings_reach_the_core() {
        let mut config = Config::default();
        config.input.edge_delay_ms = 150;
        config.input.corner_dead_zone = 20.0;
        config.input.pointer_speed = 1.5;
        config.input.takeback_threshold = 8.0;
        let core = core_config(&config);
        assert_eq!(core.edge_delay_ms, 150);
        assert_eq!(core.corner_dead_zone, 20.0);
        assert_eq!(core.pointer_speed, 1.5);
        assert_eq!(core.takeback_threshold, 8.0);
        assert_eq!(core.emergency_chord, Chord::default());
        assert_eq!(core.peer_timeout_ms, CoreConfig::default().peer_timeout_ms);
    }
}
