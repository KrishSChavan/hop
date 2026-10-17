//! Hop's TOML configuration: the settings, their defaults, the rules they
//! must follow, and loading and saving the file. See docs/ARCHITECTURE.md
//! ("Configuration"), and `config.example.toml` next to this crate's
//! `Cargo.toml` for a complete, commented file.
//!
//! - [`Config::default_path`] says where the file lives on this computer.
//! - [`Config::load`] reads it. A missing file, section or setting takes its
//!   default; settings this version does not know become [`Warning`]s rather
//!   than errors, so an older Hop can read a newer file.
//! - [`Config::validate`] checks the rules; `load` and `save` run it too.
//! - [`Config::save`] writes the file atomically.
//! - [`Config::first_run`] makes the config for a computer new to Hop,
//!   including its device id.
//!
//! This crate only reads and writes text and files. Key names in the
//! emergency chord stay strings here; `hop-keymap` turns them into key codes.
//!
//! # Example
//!
//! Starting up, with first-run setup when this computer has no id yet:
//!
//! ```no_run
//! use hop_config::Config;
//!
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! let path = Config::default_path().ok_or("no home folder")?;
//! let (mut config, warnings) = Config::load(&path)?;
//! for warning in &warnings {
//!     eprintln!("{warning}");
//! }
//! if !config.device.has_id() {
//!     // The platform layer supplies the computer's real name.
//!     config.device = Config::first_run("Krish's MacBook")?.device;
//!     config.save(&path)?;
//! }
//! # Ok(())
//! # }
//! ```

#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod config;
mod error;
mod file;
mod paths;
mod validate;

pub use config::{
    ClipboardConfig, Config, DeviceConfig, InputConfig, KeymapConfig, NetworkConfig, PeerConfig,
    Side, TransferConfig,
};
pub use error::ConfigError;
pub use file::Warning;
pub use validate::ValidationError;
