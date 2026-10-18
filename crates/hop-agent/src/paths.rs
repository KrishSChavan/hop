//! Where the agent keeps its key and its logs on each operating system.
//!
//! The config file's location comes from `hop-config`
//! (`Config::default_path`). The lookups here take the OS and the
//! environment as arguments, so tests can try every OS with made-up values
//! without touching the real environment.

use std::ffi::OsString;
use std::path::PathBuf;

use hop_types::Os;

/// The name of the agent's log file inside [`log_dir`].
pub const LOG_FILE: &str = "hop.log";

/// Where this computer's private key lives (docs/M2-DESIGN.md, "Identity"):
///
/// - macOS: `~/Library/Application Support/Hop/identity.pem`, next to the
///   config file;
/// - Windows: `%LOCALAPPDATA%\Hop\identity.pem`. Local, not roaming
///   (`%APPDATA%`, where the config lives), because a device key must never
///   follow the user to another PC;
/// - other systems: `$XDG_CONFIG_HOME/hop/identity.pem`, or
///   `~/.config/hop/identity.pem`.
///
/// `None` when the variable the location rests on is missing or not a full
/// path: a relative location would depend on the folder Hop started in.
pub fn identity_path() -> Option<PathBuf> {
    identity_path_for(Os::current(), process_env)
}

/// The folder for the agent's log: `~/Library/Logs/Hop` on macOS,
/// `%LOCALAPPDATA%\Hop\Logs` on Windows, and `$XDG_STATE_HOME/hop` (or
/// `~/.local/state/hop`) elsewhere. The self-test writes its report to the
/// same folder.
pub fn log_dir() -> Option<PathBuf> {
    log_dir_for(Os::current(), process_env)
}

fn process_env(name: &str) -> Option<OsString> {
    std::env::var_os(name)
}

pub(crate) fn identity_path_for(os: Os, env: impl Fn(&str) -> Option<OsString>) -> Option<PathBuf> {
    let dir = match os {
        Os::MacOs => dir_var(&env, "HOME")?
            .join("Library")
            .join("Application Support")
            .join("Hop"),
        Os::Windows => dir_var(&env, "LOCALAPPDATA")?.join("Hop"),
        Os::Linux => match dir_var(&env, "XDG_CONFIG_HOME") {
            Some(config_home) => config_home.join("hop"),
            None => dir_var(&env, "HOME")?.join(".config").join("hop"),
        },
    };
    Some(dir.join("identity.pem"))
}

pub(crate) fn log_dir_for(os: Os, env: impl Fn(&str) -> Option<OsString>) -> Option<PathBuf> {
    Some(match os {
        Os::MacOs => dir_var(&env, "HOME")?
            .join("Library")
            .join("Logs")
            .join("Hop"),
        Os::Windows => dir_var(&env, "LOCALAPPDATA")?.join("Hop").join("Logs"),
        Os::Linux => match dir_var(&env, "XDG_STATE_HOME") {
            Some(state_home) => state_home.join("hop"),
            None => dir_var(&env, "HOME")?
                .join(".local")
                .join("state")
                .join("hop"),
        },
    })
}

/// The folder a variable names, if it is set to a full path.
fn dir_var(env: &impl Fn(&str) -> Option<OsString>, name: &str) -> Option<PathBuf> {
    let dir = PathBuf::from(env(name)?);
    dir.is_absolute().then_some(dir)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::path::Path;

    use super::*;

    /// A full path on the machine running the tests.
    fn full(relative: &str) -> PathBuf {
        let root = if cfg!(windows) { r"C:\" } else { "/" };
        relative
            .split('/')
            .fold(PathBuf::from(root), |path, part| path.join(part))
    }

    fn env<'a>(vars: &'a [(&'a str, &'a Path)]) -> impl Fn(&str) -> Option<OsString> + 'a {
        let vars: BTreeMap<&str, &Path> = vars.iter().copied().collect();
        move |name| vars.get(name).map(|value| value.as_os_str().to_owned())
    }

    #[test]
    fn the_key_lives_next_to_the_config_on_a_mac() {
        let home = full("Users/krish");
        let path = identity_path_for(Os::MacOs, env(&[("HOME", &home)]));
        assert_eq!(
            path,
            Some(
                home.join("Library")
                    .join("Application Support")
                    .join("Hop")
                    .join("identity.pem")
            )
        );
    }

    #[test]
    fn the_key_never_roams_on_windows() {
        let local = full("Users/krish/AppData/Local");
        let roaming = full("Users/krish/AppData/Roaming");
        let vars = [("LOCALAPPDATA", local.as_path()), ("APPDATA", &roaming)];
        assert_eq!(
            identity_path_for(Os::Windows, env(&vars)),
            Some(local.join("Hop").join("identity.pem"))
        );
        // Without LOCALAPPDATA there is no safe place; APPDATA would roam.
        assert_eq!(
            identity_path_for(Os::Windows, env(&[("APPDATA", &roaming)])),
            None
        );
    }

    #[test]
    fn logs_go_where_the_self_test_report_goes() {
        let home = full("Users/krish");
        assert_eq!(
            log_dir_for(Os::MacOs, env(&[("HOME", &home)])),
            Some(home.join("Library").join("Logs").join("Hop"))
        );
        let local = full("Users/krish/AppData/Local");
        assert_eq!(
            log_dir_for(Os::Windows, env(&[("LOCALAPPDATA", &local)])),
            Some(local.join("Hop").join("Logs"))
        );
        assert_eq!(
            log_dir_for(Os::Linux, env(&[("HOME", &home)])),
            Some(home.join(".local").join("state").join("hop"))
        );
    }

    #[test]
    fn missing_or_relative_variables_give_no_path() {
        for os in [Os::MacOs, Os::Windows, Os::Linux] {
            assert_eq!(identity_path_for(os, env(&[])), None, "{os}");
            assert_eq!(log_dir_for(os, env(&[])), None, "{os}");
            let relative = Path::new("relative/dir");
            let vars = [
                ("HOME", relative),
                ("LOCALAPPDATA", relative),
                ("XDG_CONFIG_HOME", relative),
                ("XDG_STATE_HOME", relative),
            ];
            assert_eq!(identity_path_for(os, env(&vars)), None, "{os}");
            assert_eq!(log_dir_for(os, env(&vars)), None, "{os}");
        }
    }
}
