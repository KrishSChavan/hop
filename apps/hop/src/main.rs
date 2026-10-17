//! The Hop agent.
//!
//! Until the network link lands (milestone M2), it can play a simulated
//! session, check this computer's platform layer with a guided self-test,
//! measure the network round trip to the other computer, and show the
//! configuration.

#![forbid(unsafe_code)]

mod native;
mod simulate;

use std::net::{SocketAddr, ToSocketAddrs};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use hop_config::Config;
use hop_platform::{Permission, PermissionState};
use hop_selftest::{DEFAULT_LATENCY_PORT, SelfTestOptions, StepId, Verdict};

#[derive(Parser)]
#[command(
    name = "hop",
    version,
    about = "One keyboard and mouse across a Mac and a Windows PC"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Play a simulated session between a Mac and a Windows PC. Nothing
    /// touches this computer: input, cursors and the network are simulated.
    Simulate,
    /// Check this computer's platform layer with a guided self-test. It
    /// moves the cursor, types into its own test window and briefly holds
    /// the mouse, always with instructions and a countdown first.
    Selftest {
        /// Write the report here instead of the usual log folder.
        #[arg(long)]
        report: Option<PathBuf>,
        /// Run only these steps, by number (step 1 always runs), e.g.
        /// --steps 2,5
        #[arg(long, value_delimiter = ',')]
        steps: Vec<usize>,
    },
    /// List this computer's displays the way Hop sees them.
    Displays,
    /// Show the OS permissions Hop needs, and optionally ask for them.
    Permissions {
        /// Ask macOS for the Accessibility permission (shows its prompt).
        #[arg(long)]
        request: bool,
    },
    /// Measure the network round trip to the other computer. Only numbered
    /// test packets are sent, never any input.
    Latency {
        #[command(subcommand)]
        action: LatencyAction,
    },
    /// Show where the configuration file lives and what it contains.
    Config {
        #[command(subcommand)]
        action: ConfigAction,
    },
}

#[derive(Subcommand)]
enum LatencyAction {
    /// Answer probes from the other computer until stopped with Ctrl+C.
    Serve {
        #[arg(long, default_value_t = DEFAULT_LATENCY_PORT)]
        port: u16,
    },
    /// Send probes to the other computer and report the round-trip times.
    Probe {
        /// The other computer's address, such as 192.168.1.20.
        host: String,
        #[arg(long, default_value_t = DEFAULT_LATENCY_PORT)]
        port: u16,
        /// How many probes to send.
        #[arg(long, default_value_t = 300)]
        count: u32,
        /// Milliseconds between probes.
        #[arg(long, default_value_t = 20)]
        interval_ms: u64,
    },
}

#[derive(Subcommand)]
enum ConfigAction {
    /// Print the path of the configuration file.
    Path,
    /// Load and check the configuration (the defaults if there is no file),
    /// then print it.
    Show {
        /// Read this file instead of the usual location.
        #[arg(long)]
        file: Option<PathBuf>,
    },
}

fn main() -> Result<ExitCode> {
    match Cli::parse().command {
        Command::Simulate => {
            simulate::run(true);
            Ok(ExitCode::SUCCESS)
        }
        Command::Selftest { report, steps } => selftest(report, &steps),
        Command::Displays => displays().map(|()| ExitCode::SUCCESS),
        Command::Permissions { request } => permissions(request).map(|()| ExitCode::SUCCESS),
        Command::Latency { action } => latency(action).map(|()| ExitCode::SUCCESS),
        Command::Config { action } => match action {
            ConfigAction::Path => {
                println!("{}", default_path()?.display());
                Ok(ExitCode::SUCCESS)
            }
            ConfigAction::Show { file } => show_config(file).map(|()| ExitCode::SUCCESS),
        },
    }
}

fn selftest(report: Option<PathBuf>, steps: &[usize]) -> Result<ExitCode> {
    let selected: Vec<StepId> = if steps.is_empty() {
        StepId::ALL.to_vec()
    } else {
        steps
            .iter()
            .map(|&n| {
                StepId::ALL
                    .into_iter()
                    .find(|id| id.number() == n)
                    .with_context(|| {
                        format!(
                            "there is no step {n}; the steps are 1 to {}",
                            StepId::ALL.len()
                        )
                    })
            })
            .collect::<Result<_>>()?
    };
    let mut options = SelfTestOptions::default();
    if report.is_some() {
        options.report_path = report;
    }
    // The test window needs the main thread's event loop on macOS, so the
    // self-test runs on another thread while that loop runs here.
    let result = native::run_with_ui(move || -> Result<hop_selftest::Report> {
        let mut platform = native::native().context("could not start Hop's platform layer")?;
        let mut window = native::open_test_window().context("could not open the test window")?;
        Ok(hop_selftest::run_selected(
            &mut platform,
            window.as_mut(),
            &options,
            &selected,
        ))
    })?;

    let verdict = result.verdict();
    println!("\nSelf-test result: {verdict}");
    if let Some(path) = &result.saved_to {
        println!("Report saved to {}", path.display());
    }
    if let Some(error) = &result.save_error {
        eprintln!("warning: could not save the report: {error}");
    }
    Ok(if verdict == Verdict::Pass {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

fn displays() -> Result<()> {
    let platform = native::native().context("could not start Hop's platform layer")?;
    let displays = platform.displays.displays()?;
    if displays.is_empty() {
        bail!("no displays found");
    }
    for d in displays {
        println!(
            "display {}: {:.0} x {:.0} at ({:.0}, {:.0}), scale {}{}",
            d.id,
            d.frame.size.width,
            d.frame.size.height,
            d.frame.origin.x,
            d.frame.origin.y,
            d.scale,
            if d.primary { ", primary" } else { "" }
        );
    }
    Ok(())
}

fn permissions(request: bool) -> Result<()> {
    let platform = native::native().context("could not start Hop's platform layer")?;
    for permission in [Permission::Accessibility, Permission::LocalNetwork] {
        let state = match platform.permissions.state(permission) {
            PermissionState::Granted => "granted",
            PermissionState::Denied => "not granted",
            PermissionState::NotDetermined => "not decided yet",
            PermissionState::NotNeeded => "not needed on this OS",
        };
        println!("{permission}: {state}");
    }
    if request {
        platform.permissions.request(Permission::Accessibility)?;
        println!(
            "If macOS showed a prompt, open System Settings > Privacy & Security > Accessibility \
             and switch Hop on, then start Hop again."
        );
    }
    Ok(())
}

fn latency(action: LatencyAction) -> Result<()> {
    match action {
        LatencyAction::Serve { port } => {
            let bind = SocketAddr::from(([0, 0, 0, 0], port));
            println!("Answering latency probes on UDP port {port}. Stop with Ctrl+C.");
            hop_selftest::latency_serve(bind, Arc::new(AtomicBool::new(false)))?;
            Ok(())
        }
        LatencyAction::Probe {
            host,
            port,
            count,
            interval_ms,
        } => {
            let peer = (host.as_str(), port)
                .to_socket_addrs()
                .with_context(|| format!("could not find {host}"))?
                .next()
                .with_context(|| format!("{host} has no address"))?;
            println!("Probing {peer} with {count} packets...");
            let stats =
                hop_selftest::latency_probe(peer, count, Duration::from_millis(interval_ms))?;
            println!("{stats}");
            Ok(())
        }
    }
}

fn default_path() -> Result<PathBuf> {
    Config::default_path().context("could not work out where the configuration lives")
}

fn show_config(file: Option<PathBuf>) -> Result<()> {
    let path = match file {
        Some(path) => path,
        None => default_path()?,
    };
    let (config, warnings) =
        Config::load(&path).with_context(|| format!("could not load {}", path.display()))?;
    if path.exists() {
        println!("# {}", path.display());
    } else {
        println!(
            "# {} does not exist yet, so these are the defaults",
            path.display()
        );
    }
    for warning in &warnings {
        eprintln!("warning: {warning}");
    }
    print!("{}", toml::to_string_pretty(&config)?);
    Ok(())
}
