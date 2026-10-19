//! Small helpers for the app: the Accessibility permission, opening System
//! Settings and the log folder, keeping macOS from napping Hop while it
//! shares, and giving the app its name in the Mac's app menu. The agent
//! uses one of them too, to show files that just arrived.
//!
//! The permission and the name go through `hop-platform`'s backends, like
//! every other OS call; folders, files and the Settings pane open with the
//! OS's own `open` (macOS) or `explorer` (Windows), on a thread of their
//! own, so neither the app nor the agent ever waits for them.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;

use hop_platform::PermissionState;

pub use crate::native::{Responsive, stay_responsive};
use crate::{paths, status};

/// macOS's link to System Settings > Privacy & Security > Accessibility.
#[cfg(target_os = "macos")]
const ACCESSIBILITY_SETTINGS: &str =
    "x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility";

/// Whether Hop may watch and drive this computer's keyboard and mouse: on a
/// Mac, the Accessibility permission (`Granted` or `Denied`, since macOS
/// does not say whether it ever asked). Every other OS needs no permission,
/// so this is `NotNeeded` there. Cheap enough to call every few seconds,
/// and it never shows anything.
pub fn accessibility() -> PermissionState {
    #[cfg(target_os = "macos")]
    {
        mac::with_permissions(|permissions| {
            permissions.state(hop_platform::Permission::Accessibility)
        })
        .unwrap_or(PermissionState::Denied)
    }
    #[cfg(not(target_os = "macos"))]
    {
        PermissionState::NotNeeded
    }
}

/// Shows macOS's "Hop would like to control this computer" prompt, which
/// offers to open System Settings, if Hop does not have Accessibility yet.
/// The prompt comes up on its own; this returns at once. Nothing happens on
/// other systems.
pub fn request_accessibility() {
    #[cfg(target_os = "macos")]
    {
        let asked = mac::with_permissions(|permissions| {
            permissions.request(hop_platform::Permission::Accessibility)
        });
        if !matches!(asked, Some(Ok(()))) {
            status!("Could not ask macOS for the Accessibility permission.");
        }
    }
}

/// Names this process "Hop" on a Mac, so the app menu reads "About Hop",
/// "Hide Hop" and "Quit Hop" rather than the executable's lower-case name
/// (Hop.app keeps `Contents/MacOS/hop`). Call it on the main thread before
/// the window opens. Nothing happens on other systems.
pub fn use_app_name() {
    #[cfg(target_os = "macos")]
    hop_platform_macos::set_process_name("Hop");
}

/// Opens System Settings > Privacy & Security > Accessibility on a Mac,
/// where the user switches Hop on. Nothing happens on other systems.
pub fn open_accessibility_settings() {
    if let Some(command) = accessibility_settings_command() {
        launch(command, "System Settings");
    }
}

/// Opens Hop's log folder in Finder or Explorer, making it first if it is
/// not there yet. Nothing happens on other systems.
pub fn open_log_folder() {
    let Some(dir) = paths::log_dir() else {
        status!("Could not work out where Hop's log folder is.");
        return;
    };
    if let Err(error) = std::fs::create_dir_all(&dir) {
        status!("Could not make the log folder {} ({error}).", dir.display());
        return;
    }
    if let Some(command) = folder_command(&dir) {
        launch(command, "the log folder");
    }
}

/// Shows files that just arrived from the other computer, in Finder or
/// Explorer: one file selected in its folder, or the folder that holds
/// several. Returns at once: the command runs on a thread of its own, so
/// the agent's core thread never waits for it. Nothing happens on other
/// systems or without files.
pub(crate) fn show_received(files: &[PathBuf]) {
    if let Some(command) = reveal_target(files).and_then(|target| reveal_command(&target)) {
        launch(command, "the folder the files arrived in");
    }
}

/// What to show for files that just arrived.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Reveal {
    /// This file, selected in its folder.
    File(PathBuf),
    /// This folder: it holds several files that arrived together.
    Folder(PathBuf),
}

/// One file is shown selected; for several, the folder of the first opens
/// (they all arrive in the same one).
fn reveal_target(files: &[PathBuf]) -> Option<Reveal> {
    match files {
        [] => None,
        [file] => Some(Reveal::File(file.clone())),
        [first, ..] => first
            .parent()
            .filter(|folder| !folder.as_os_str().is_empty())
            .map(|folder| Reveal::Folder(folder.to_owned())),
    }
}

/// The command that shows `target`: `open -R` (a file) or `open` (a
/// folder) on a Mac, `explorer /select,"…"` or `explorer` on Windows, none
/// elsewhere.
fn reveal_command(target: &Reveal) -> Option<Command> {
    match target {
        Reveal::Folder(folder) => folder_command(folder),
        Reveal::File(file) => select_command(file),
    }
}

/// The command that shows `file` selected in its folder.
fn select_command(file: &Path) -> Option<Command> {
    #[cfg(target_os = "macos")]
    {
        let mut command = Command::new("open");
        command.arg("-R").arg(file);
        Some(command)
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt as _;

        // Passed as it is: Rust's own quoting would put the quotes around
        // "/select,…" as a whole, which Explorer does not understand.
        let mut command = Command::new("explorer");
        command.raw_arg(select_argument(file));
        Some(command)
    }
    #[cfg(not(any(target_os = "macos", windows)))]
    {
        let _ = file;
        None
    }
}

/// Explorer's argument for showing `file` selected: `/select,"<file>"`,
/// with the quotes around the path only. A Windows path cannot contain a
/// quote, so none can end the argument early.
#[cfg_attr(
    not(any(windows, test)),
    expect(dead_code, reason = "only Windows selects with Explorer")
)]
fn select_argument(file: &Path) -> OsString {
    let mut argument = OsString::from("/select,\"");
    argument.push(file.as_os_str());
    argument.push("\"");
    argument
}

/// The command that opens the Accessibility pane: `open` with its link on
/// a Mac, none elsewhere.
fn accessibility_settings_command() -> Option<Command> {
    #[cfg(target_os = "macos")]
    {
        let mut command = Command::new("open");
        command.arg(ACCESSIBILITY_SETTINGS);
        Some(command)
    }
    #[cfg(not(target_os = "macos"))]
    {
        None
    }
}

/// The command that shows `dir`: `open` on a Mac, `explorer` on Windows,
/// none elsewhere.
fn folder_command(dir: &Path) -> Option<Command> {
    let program = if cfg!(target_os = "macos") {
        "open"
    } else if cfg!(windows) {
        "explorer"
    } else {
        return None;
    };
    let mut command = Command::new(program);
    command.arg(dir);
    Some(command)
}

/// Runs `command` on a thread of its own and waits for it there, so the app
/// never waits and no finished process is left behind. A failure becomes a
/// status line naming `what`.
fn launch(mut command: Command, what: &'static str) {
    let started = std::thread::Builder::new()
        .name("hop-open".into())
        .spawn(move || {
            // Explorer answers 1 even when it opened the folder, so only a
            // command that did not start counts as a failure.
            if let Err(error) = command.status() {
                status!("Could not open {what} ({error}).");
            }
        });
    if let Err(error) = started {
        status!("Could not open {what} ({error}).");
    }
}

#[cfg(target_os = "macos")]
mod mac {
    use std::sync::{Mutex, OnceLock, PoisonError};

    use hop_platform::Permissions;

    use crate::native;

    /// The platform's permission checker, kept for the life of the app so
    /// a check every few seconds costs only the check. The rest of the
    /// platform it came with is dropped at once: nothing was started, so
    /// dropping it touches nothing.
    static PERMISSIONS: OnceLock<Option<Mutex<Box<dyn Permissions>>>> = OnceLock::new();

    /// Runs `f` with the permission checker; `None` if the platform layer
    /// could not start.
    pub(super) fn with_permissions<R>(f: impl FnOnce(&dyn Permissions) -> R) -> Option<R> {
        let permissions = PERMISSIONS
            .get_or_init(|| {
                native::native()
                    .ok()
                    .map(|platform| Mutex::new(platform.permissions))
            })
            .as_ref()?;
        let permissions = permissions.lock().unwrap_or_else(PoisonError::into_inner);
        Some(f(permissions.as_ref()))
    }
}

#[cfg(test)]
mod tests {
    use std::ffi::OsStr;
    use std::path::PathBuf;

    use super::*;

    // Nothing here runs a command or asks for a permission: the tests only
    // look at the commands that would run.

    #[test]
    fn the_accessibility_pane_opens_only_on_a_mac() {
        let command = accessibility_settings_command();
        if cfg!(target_os = "macos") {
            let command = command.expect("a command on a Mac");
            assert_eq!(command.get_program(), OsStr::new("open"));
            let args: Vec<&OsStr> = command.get_args().collect();
            assert_eq!(
                args,
                [OsStr::new(
                    "x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility"
                )]
            );
        } else {
            assert!(command.is_none());
        }
    }

    #[test]
    fn the_log_folder_opens_in_finder_or_explorer() {
        let dir = PathBuf::from("Logs").join("Hop");
        let command = folder_command(&dir);
        let expected = if cfg!(target_os = "macos") {
            Some("open")
        } else if cfg!(windows) {
            Some("explorer")
        } else {
            None
        };
        match (command, expected) {
            (Some(command), Some(program)) => {
                assert_eq!(command.get_program(), OsStr::new(program));
                let args: Vec<&OsStr> = command.get_args().collect();
                assert_eq!(args, [dir.as_os_str()]);
            }
            (None, None) => {}
            (command, expected) => panic!("{command:?} for {expected:?}"),
        }
    }

    #[test]
    fn one_received_file_is_selected_and_several_open_their_folder() {
        let folder = PathBuf::from("Downloads").join("Hop Received");
        let (notes, photo) = (folder.join("notes.txt"), folder.join("photo.jpg"));
        assert_eq!(reveal_target(&[]), None);
        assert_eq!(
            reveal_target(std::slice::from_ref(&notes)),
            Some(Reveal::File(notes.clone()))
        );
        assert_eq!(reveal_target(&[notes, photo]), Some(Reveal::Folder(folder)));
        // Bare names have no folder to open.
        assert_eq!(
            reveal_target(&[PathBuf::from("a.txt"), PathBuf::from("b.txt")]),
            None
        );
    }

    #[test]
    fn explorer_gets_the_path_in_quotes_after_select() {
        let file = Path::new(r"C:\Users\Krish\Downloads\Hop Received\my notes.txt");
        assert_eq!(
            select_argument(file),
            OsString::from(r#"/select,"C:\Users\Krish\Downloads\Hop Received\my notes.txt""#)
        );
    }

    #[test]
    fn received_files_are_shown_in_finder_or_explorer() {
        let folder = PathBuf::from("Downloads").join("Hop Received");
        let file = folder.join("my notes.txt");
        let shown = |target: Reveal| {
            reveal_command(&target).map(|command| {
                let program = command.get_program().to_owned();
                let args: Vec<OsString> = command.get_args().map(OsStr::to_owned).collect();
                (program, args)
            })
        };
        let selected = shown(Reveal::File(file.clone()));
        let opened = shown(Reveal::Folder(folder.clone()));
        if cfg!(target_os = "macos") {
            assert_eq!(
                selected,
                Some(("open".into(), vec!["-R".into(), file.clone().into()]))
            );
            assert_eq!(opened, Some(("open".into(), vec![folder.into()])));
        } else if cfg!(windows) {
            // The argument goes to Explorer as it is, quotes and all.
            assert_eq!(
                selected,
                Some(("explorer".into(), vec![select_argument(&file)]))
            );
            assert_eq!(opened, Some(("explorer".into(), vec![folder.into()])));
        } else {
            assert_eq!((selected, opened), (None, None));
        }
    }

    /// Off the Mac nothing needs a permission. (On the Mac the check would
    /// ask macOS about this test binary, so it is left to the app.)
    #[cfg(not(target_os = "macos"))]
    #[test]
    fn no_permission_is_needed_off_the_mac() {
        assert_eq!(accessibility(), PermissionState::NotNeeded);
        request_accessibility();
    }

    #[test]
    fn the_app_nap_guard_can_be_held_and_dropped() {
        // On a Mac this begins and ends an NSProcessInfo activity, which
        // touches nothing on screen; elsewhere it holds nothing.
        {
            let _awake: Responsive = stay_responsive();
        }
    }
}
