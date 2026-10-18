//! Puts Hop's icon and version details into the Windows executable, so
//! Explorer, the taskbar and the file's Properties show "Hop" with its
//! icon. Builds for other systems are left alone.
//!
//! On a Windows PC the resource compiler (rc.exe) comes with the Windows
//! SDK, which Rust needs anyway; a build there fails if it cannot embed
//! them, so a release never ships without them. Type-checking for Windows
//! from a Mac (scripts/dev/check-windows.sh) has no resource compiler and
//! links nothing, so there it only warns.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use embed_resource::CompilationResult;

fn main() {
    let manifest_dir =
        PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let icon = manifest_dir.join("../../assets/icon/hop.ico");
    println!("cargo::rerun-if-changed=build.rs");
    println!("cargo::rerun-if-changed={}", icon.display());
    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }

    let out_dir = PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR"));
    let script = out_dir.join("hop-app.rc");
    let version = Version::from_env();
    fs::write(&script, resource_script(&icon, &version))
        .unwrap_or_else(|error| panic!("could not write {}: {error}", script.display()));

    match embed_resource::compile(&script, embed_resource::NONE) {
        CompilationResult::Ok | CompilationResult::NotWindows => {}
        // `cfg!(windows)` is the computer running this build, not the target.
        failed if cfg!(windows) => {
            panic!("could not embed Hop's icon and version details: {failed}")
        }
        skipped => println!(
            "cargo::warning=Hop.exe gets no icon or version details from this build \
             (no Windows resource compiler here: {skipped}); a build on Windows has them"
        ),
    }
}

/// The package's version, as Windows wants it: the text, and four numbers.
struct Version {
    text: String,
    numbers: [u16; 4],
}

impl Version {
    fn from_env() -> Version {
        let number = |name: &str| -> u16 {
            env::var(name)
                .ok()
                .and_then(|value| value.parse().ok())
                .unwrap_or(0)
        };
        Version {
            text: env::var("CARGO_PKG_VERSION").unwrap_or_default(),
            numbers: [
                number("CARGO_PKG_VERSION_MAJOR"),
                number("CARGO_PKG_VERSION_MINOR"),
                number("CARGO_PKG_VERSION_PATCH"),
                0,
            ],
        }
    }
}

/// The resource script: the icon (the first icon is the program's), and a
/// VERSIONINFO block naming Hop. Numbers in place of winver.h's names, so
/// the script needs no headers. It is UTF-8, which the pragma tells the
/// resource compiler, in case the folder's path is not plain ASCII.
fn resource_script(icon: &Path, version: &Version) -> String {
    // Backslashes are escaped in resource script strings.
    let icon = icon.display().to_string().replace('\\', "\\\\");
    let [major, minor, patch, build] = version.numbers;
    let numbers = format!("{major},{minor},{patch},{build}");
    let text = &version.text;
    format!(
        "#pragma code_page(65001)

1 ICON \"{icon}\"

1 VERSIONINFO
FILEVERSION {numbers}
PRODUCTVERSION {numbers}
FILEFLAGSMASK 0x3F
FILEFLAGS 0x0
FILEOS 0x40004
FILETYPE 0x1
FILESUBTYPE 0x0
BEGIN
    BLOCK \"StringFileInfo\"
    BEGIN
        BLOCK \"040904B0\"
        BEGIN
            VALUE \"FileDescription\", \"Hop\"
            VALUE \"FileVersion\", \"{text}\"
            VALUE \"InternalName\", \"hop-app\"
            VALUE \"OriginalFilename\", \"Hop.exe\"
            VALUE \"ProductName\", \"Hop\"
            VALUE \"ProductVersion\", \"{text}\"
        END
    END
    BLOCK \"VarFileInfo\"
    BEGIN
        VALUE \"Translation\", 0x409, 1200
    END
END
"
    )
}
