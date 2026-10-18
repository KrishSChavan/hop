//! What a start of `hop-app` asks for: the window, or (on a Mac only) the
//! `hop` command line.
//!
//! On the Mac, `hop-app` is the executable inside Hop.app. The scripts start
//! Hop.app through LaunchServices with `--args run` or `--args selftest`, so
//! a first argument that names a `hop` subcommand, or a help or version
//! flag, goes to the command line (docs/M6-APP.md, "Shape"). Anything else
//! opens the window: no arguments at all, and the `-psn_...` argument
//! LaunchServices may add when Hop is opened from Finder.

use std::ffi::OsStr;

/// The `hop` subcommands (see `hop_agent::cli`), plus clap's own `help`.
pub const SUBCOMMANDS: [&str; 9] = [
    "run",
    "pair",
    "simulate",
    "selftest",
    "displays",
    "permissions",
    "latency",
    "config",
    "help",
];

/// The help and version flags clap answers by itself.
pub const FLAGS: [&str; 4] = ["-h", "--help", "-V", "--version"];

/// Where a start of `hop-app` goes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Launch {
    /// The `hop` command line, with this process's arguments.
    CommandLine,
    /// The Hop window.
    Window,
}

/// Decides from the first argument after the program's name. Only exact
/// names count, so a typo or a LaunchServices argument opens the window
/// rather than failing on the command line, where nobody would see it.
pub fn launch(first: Option<&OsStr>) -> Launch {
    match first.and_then(OsStr::to_str) {
        Some(arg) if SUBCOMMANDS.contains(&arg) || FLAGS.contains(&arg) => Launch::CommandLine,
        _ => Launch::Window,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decide(first: Option<&str>) -> Launch {
        launch(first.map(OsStr::new))
    }

    #[test]
    fn every_hop_subcommand_and_flag_goes_to_the_command_line() {
        for name in SUBCOMMANDS.iter().chain(FLAGS.iter()) {
            assert_eq!(decide(Some(name)), Launch::CommandLine, "{name}");
        }
    }

    #[test]
    fn no_arguments_and_launch_services_arguments_open_the_window() {
        assert_eq!(decide(None), Launch::Window);
        // Finder may add the process serial number as the first argument.
        assert_eq!(decide(Some("-psn_0_1234567")), Launch::Window);
        // Xcode and `open` can add AppKit's own settings.
        assert_eq!(
            decide(Some("-NSDocumentRevisionsDebugMode")),
            Launch::Window
        );
        assert_eq!(decide(Some("")), Launch::Window);
    }

    #[test]
    fn only_exact_names_count() {
        for near in [
            "Run",
            "RUN",
            "run ",
            "selftests",
            "--run",
            "-help",
            "-v",
            "version",
        ] {
            assert_eq!(decide(Some(near)), Launch::Window, "{near:?}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn an_argument_that_is_not_unicode_opens_the_window() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt as _;

        let odd = OsString::from_vec(vec![b'r', 0xff, b'n']);
        assert_eq!(launch(Some(&odd)), Launch::Window);
    }
}
