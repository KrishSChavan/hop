//! What can go wrong while loading, checking or saving a config.

use std::io;
use std::path::{Path, PathBuf};

use crate::ValidationError;

/// An error from loading, validating or saving a config.
///
/// The messages name the file and, through [`std::error::Error::source`], the
/// underlying cause, so print the whole chain (for example `{:#}` with
/// `anyhow`) to see why a file could not be read or parsed.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// The config file exists but could not be read.
    #[error("could not read the config file {}", .path.display())]
    Read {
        /// The file.
        path: PathBuf,
        /// Why it could not be read.
        #[source]
        source: io::Error,
    },

    /// The config file is not valid TOML, or a setting has the wrong type
    /// (for example text where a number belongs). The source error shows
    /// the line and column.
    #[error("could not parse the config file {}", .path.display())]
    Parse {
        /// The file.
        path: PathBuf,
        /// What is wrong, with the line and column.
        #[source]
        source: toml::de::Error,
    },

    /// The settings break one or more rules, all listed in `errors`.
    #[error("{}", invalid_message(.path.as_deref(), .errors))]
    Invalid {
        /// The file the settings came from or were going to, if any.
        path: Option<PathBuf>,
        /// Every broken rule, in the order the settings appear in the file.
        errors: Vec<ValidationError>,
    },

    /// The settings could not be written as TOML, for example because a
    /// folder name is not valid Unicode.
    #[error("could not encode the config as TOML")]
    Encode(#[source] toml::ser::Error),

    /// The config file could not be written.
    #[error("could not write the config file {}", .path.display())]
    Write {
        /// The file.
        path: PathBuf,
        /// Why it could not be written.
        #[source]
        source: io::Error,
    },

    /// The system's random number source failed while generating a device id.
    #[error("could not generate a device id")]
    Random(#[source] getrandom::Error),
}

/// One line listing every problem, so the whole story fits in a log entry.
fn invalid_message(path: Option<&Path>, errors: &[ValidationError]) -> String {
    let subject = match path {
        // "For" fits both directions: settings read from the file, or about
        // to be saved to it.
        Some(path) => format!("the config for {}", path.display()),
        None => "the config".to_owned(),
    };
    let count = match errors.len() {
        1 => "1 problem".to_owned(),
        n => format!("{n} problems"),
    };
    let list = errors
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("; ");
    format!("{subject} has {count}: {list}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invalid_lists_every_problem_on_one_line() {
        let error = ConfigError::Invalid {
            path: Some(PathBuf::from("config.toml")),
            errors: vec![
                ValidationError::NoChordKeys,
                ValidationError::Empty {
                    key: "device.name".to_owned(),
                },
            ],
        };
        assert_eq!(
            error.to_string(),
            "the config for config.toml has 2 problems: \
             input.emergency_chord must list at least one key; \
             device.name must not be empty"
        );
    }

    #[test]
    fn invalid_without_a_file() {
        let error = ConfigError::Invalid {
            path: None,
            errors: vec![ValidationError::NoChordKeys],
        };
        assert_eq!(
            error.to_string(),
            "the config has 1 problem: input.emergency_chord must list at least one key"
        );
    }
}
