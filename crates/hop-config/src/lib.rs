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
//! - [`Config::first_run`] makes the config for a computer new to Hop.
//! - [`Config::set_device_id`] puts in place the device id derived from
//!   this computer's key ([`device_id_from_fingerprint`]), and
//!   [`PeerConfig::pinned_fingerprint`] gives the key a peer must prove it
//!   holds (docs/M2-DESIGN.md, "Identity").
//!
//! This crate only reads and writes text and files. Key names in the
//! emergency chord stay strings here; `hop-keymap` turns them into key codes.
//! Fingerprints are computed by `hop-net`; this crate only checks their
//! format.
//!
//! # Example
//!
//! Starting up: first-run setup when this computer has no id yet, then the
//! id that belongs to this computer's key.
//!
//! ```no_run
//! use hop_config::{Config, device_id_from_fingerprint};
//!
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! # let fingerprint = "c41d8e2b9a7f4e06b3d5a1f8e9c2b7d05a3f19c7e2b8d4a60f1e9c3b7d2a8e45";
//! let path = Config::default_path().ok_or("no home folder")?;
//! let (mut config, warnings) = Config::load(&path)?;
//! for warning in &warnings {
//!     eprintln!("{warning}");
//! }
//! let mut changed = false;
//! if !config.device.has_id() {
//!     // The platform layer supplies the computer's real name.
//!     config.device = Config::first_run("Krish's MacBook")?.device;
//!     changed = true;
//! }
//! // `fingerprint` is this computer's own, from its key (`hop-net`).
//! let id = device_id_from_fingerprint(fingerprint).ok_or("not a fingerprint")?;
//! changed |= config.set_device_id(id);
//! if changed {
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
mod fingerprint;
mod paths;
mod validate;

pub use config::{
    ClipboardConfig, Config, DeviceConfig, InputConfig, KeymapConfig, NetworkConfig, PeerConfig,
    Side, TransferConfig,
};
pub use error::ConfigError;
pub use file::Warning;
pub use fingerprint::{device_id_from_fingerprint, is_fingerprint};
pub use validate::ValidationError;
