//! The Hop agent.
//!
//! Until the platform layer lands (milestone M1), it can play a simulated
//! session between a Mac and a Windows PC, and show the configuration.

#![forbid(unsafe_code)]

mod simulate;

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use hop_config::Config;

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
    /// Show where the configuration file lives and what it contains.
    Config {
        #[command(subcommand)]
        action: ConfigAction,
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

fn main() -> Result<()> {
    match Cli::parse().command {
        Command::Simulate => {
            simulate::run(true);
            Ok(())
        }
        Command::Config { action } => match action {
            ConfigAction::Path => {
                println!("{}", default_path()?.display());
                Ok(())
            }
            ConfigAction::Show { file } => show_config(file),
        },
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
