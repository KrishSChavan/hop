//! Where Hop's files live on each operating system.
//!
//! The lookups take the OS and the environment as arguments so tests can
//! try every OS with made-up values, without touching the real environment.

use std::ffi::OsString;
use std::path::PathBuf;

use hop_types::Os;

use crate::{Config, TransferConfig};

impl Config {
    /// Where the config file lives on this computer:
    ///
    /// - macOS: `~/Library/Application Support/Hop/config.toml`
    /// - Windows: `%APPDATA%\Hop\config.toml`
    /// - other systems: `$XDG_CONFIG_HOME/hop/config.toml`, or
    ///   `~/.config/hop/config.toml` when `XDG_CONFIG_HOME` is not set
    ///
    /// An empty or relative `XDG_CONFIG_HOME` is ignored. `None` when the
    /// variable the location rests on (`APPDATA` on Windows, `HOME`
    /// elsewhere) is missing, empty or not a full path: a relative location
    /// would depend on whichever folder the app was started from.
    pub fn default_path() -> Option<PathBuf> {
        config_path(Os::current(), process_env)
    }
}

impl TransferConfig {
    /// The folder for received files: `received_dir` when it is set,
    /// otherwise "Hop Received" in the Downloads folder (`~/Downloads` on
    /// macOS and other systems, `%USERPROFILE%\Downloads` on Windows).
    ///
    /// `None` when `received_dir` is unset and the home folder is unknown.
    /// A Downloads folder moved elsewhere in Windows settings is not
    /// detected here, since that needs an OS call.
    pub fn received_dir_or_default(&self) -> Option<PathBuf> {
        received_dir(self, Os::current(), process_env)
    }
}

/// Reads a variable from this process's real environment.
fn process_env(name: &str) -> Option<OsString> {
    std::env::var_os(name)
}

/// The config file location for `os`, reading variables through `env`.
pub(crate) fn config_path(os: Os, env: impl Fn(&str) -> Option<OsString>) -> Option<PathBuf> {
    let path = match os {
        Os::MacOs => dir_var(&env, "HOME")?
            .join("Library")
            .join("Application Support")
            .join("Hop"),
        Os::Windows => dir_var(&env, "APPDATA")?.join("Hop"),
        Os::Linux => match dir_var(&env, "XDG_CONFIG_HOME") {
            Some(config_home) => config_home.join("hop"),
            None => dir_var(&env, "HOME")?.join(".config").join("hop"),
        },
    };
    Some(path.join("config.toml"))
}

/// The folder for received files on `os`, reading variables through `env`.
pub(crate) fn received_dir(
    transfer: &TransferConfig,
    os: Os,
    env: impl Fn(&str) -> Option<OsString>,
) -> Option<PathBuf> {
    if let Some(dir) = &transfer.received_dir {
        return Some(dir.clone());
    }
    let home = match os {
        Os::Windows => dir_var(&env, "USERPROFILE")?,
        Os::MacOs | Os::Linux => dir_var(&env, "HOME")?,
    };
    Some(home.join("Downloads").join("Hop Received"))
}

/// The folder an environment variable names, if it is set to a full path.
/// The XDG spec says to ignore relative values; Hop does so for every
/// variable, since a relative one is never what the user meant.
fn dir_var(env: &impl Fn(&str) -> Option<OsString>, name: &str) -> Option<PathBuf> {
    let dir = PathBuf::from(env(name)?);
    dir.is_absolute().then_some(dir)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::path::Path;

    use super::*;

    /// A full path on the machine running the tests, so the same test works
    /// on macOS and Windows hosts.
    fn full(relative: &str) -> PathBuf {
        let root = if cfg!(windows) { r"C:\" } else { "/" };
        relative
            .split('/')
            .fold(PathBuf::from(root), |path, part| path.join(part))
    }

    /// An environment holding only `vars`.
    fn env<'a>(vars: &'a [(&'a str, &'a Path)]) -> impl Fn(&str) -> Option<OsString> + 'a {
        let vars: BTreeMap<&str, &Path> = vars.iter().copied().collect();
        move |name| vars.get(name).map(|value| value.as_os_str().to_owned())
    }

    #[test]
    fn macos_uses_application_support() {
        let home = full("Users/krish");
        assert_eq!(
            config_path(Os::MacOs, env(&[("HOME", &home)])),
            Some(
                home.join("Library")
                    .join("Application Support")
                    .join("Hop")
                    .join("config.toml")
            )
        );
    }

    #[test]
    fn windows_uses_appdata() {
        let appdata = full("Users/krish/AppData/Roaming");
        let home = full("Users/krish");
        assert_eq!(
            config_path(Os::Windows, env(&[("APPDATA", &appdata), ("HOME", &home)])),
            Some(appdata.join("Hop").join("config.toml"))
        );
    }

    #[test]
    fn linux_prefers_xdg_config_home() {
        let home = full("home/krish");
        let xdg = full("home/krish/.cfg");
        assert_eq!(
            config_path(
                Os::Linux,
                env(&[("HOME", &home), ("XDG_CONFIG_HOME", &xdg)])
            ),
            Some(xdg.join("hop").join("config.toml"))
        );
    }

    #[test]
    fn linux_falls_back_to_dot_config() {
        let home = full("home/krish");
        let expected = Some(home.join(".config").join("hop").join("config.toml"));
        assert_eq!(config_path(Os::Linux, env(&[("HOME", &home)])), expected);
        // An empty or relative XDG_CONFIG_HOME is ignored, as the XDG spec says.
        for xdg in ["", "relative/cfg"] {
            let vars = [
                ("HOME", home.as_path()),
                ("XDG_CONFIG_HOME", Path::new(xdg)),
            ];
            assert_eq!(config_path(Os::Linux, env(&vars)), expected, "{xdg:?}");
        }
    }

    #[test]
    fn missing_empty_or_relative_variables_give_no_path() {
        for os in [Os::MacOs, Os::Windows, Os::Linux] {
            assert_eq!(config_path(os, env(&[])), None, "{os}");
            for value in ["", "relative/home"] {
                let vars = [
                    ("HOME", Path::new(value)),
                    ("APPDATA", Path::new(value)),
                    ("XDG_CONFIG_HOME", Path::new(value)),
                ];
                assert_eq!(config_path(os, env(&vars)), None, "{os} {value:?}");
            }
        }
    }

    #[test]
    fn windows_ignores_home() {
        let home = full("Users/krish");
        assert_eq!(config_path(Os::Windows, env(&[("HOME", &home)])), None);
    }

    #[test]
    fn received_files_default_to_downloads() {
        let home = full("Users/krish");
        let profile = full("Users/krish-pc");
        let vars = [("HOME", home.as_path()), ("USERPROFILE", profile.as_path())];
        let unset = TransferConfig::default();
        let in_downloads = |base: &Path| Some(base.join("Downloads").join("Hop Received"));

        assert_eq!(
            received_dir(&unset, Os::MacOs, env(&vars)),
            in_downloads(&home)
        );
        assert_eq!(
            received_dir(&unset, Os::Linux, env(&vars)),
            in_downloads(&home)
        );
        assert_eq!(
            received_dir(&unset, Os::Windows, env(&vars)),
            in_downloads(&profile)
        );
        assert_eq!(received_dir(&unset, Os::MacOs, env(&[])), None);
        assert_eq!(received_dir(&unset, Os::Windows, env(&[])), None);
    }

    #[test]
    fn a_configured_received_dir_wins() {
        let chosen = full("Volumes/Shared/Inbox");
        let transfer = TransferConfig {
            received_dir: Some(chosen.clone()),
        };
        for os in [Os::MacOs, Os::Windows, Os::Linux] {
            assert_eq!(received_dir(&transfer, os, env(&[])), Some(chosen.clone()));
        }
    }

    #[test]
    fn the_real_lookups_do_not_panic() {
        // Their results depend on the machine, so only check they run.
        let _ = Config::default_path();
        let _ = TransferConfig::default().received_dir_or_default();
    }
}
