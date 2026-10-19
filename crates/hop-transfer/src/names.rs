//! File names from the other computer: cleaning them ([`sanitize`]) and
//! finding a name nothing uses yet ([`free_path`]).

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// The longest file name [`sanitize`] and [`free_path`] produce, in UTF-8
/// bytes: 255, the limit of macOS's file systems. Windows counts UTF-16
/// units instead, and a name never has more of those than UTF-8 bytes.
pub const MAX_NAME_BYTES: usize = 255;

/// The longest ending, dot included, that still counts as an extension
/// when a name is shortened. A longer one is cut like the rest of the name.
const MAX_EXTENSION_BYTES: usize = 32;

/// How many names [`free_path`] tries: `name.ext`, then `name (2).ext` up to
/// `name (9999).ext`.
const MAX_CANDIDATES: u32 = 9999;

/// Characters Windows does not allow in a file name. The two slashes never
/// get this far (they separate path components), but are listed anyway.
const FORBIDDEN: [char; 9] = ['\\', '/', ':', '*', '?', '"', '<', '>', '|'];

/// Windows device names, which stay reserved with any extension
/// (`con.txt` is the console too). Microsoft's current list includes `COM0`,
/// `LPT0` and the superscript digits besides the classic ones.
const RESERVED: [&str; 30] = [
    "CON", "PRN", "AUX", "NUL", "COM0", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7",
    "COM8", "COM9", "COM¹", "COM²", "COM³", "LPT0", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6",
    "LPT7", "LPT8", "LPT9", "LPT¹", "LPT²", "LPT³",
];

/// `name`, from the other computer, made safe to use as a file name in the
/// Received folder on either system, or `None` if nothing usable is left.
///
/// 1. Only the last path component is kept, whichever separator (`/` or
///    `\`) the other system uses: `../x` gives `x`.
/// 2. Control characters, the invisible characters that reorder text or
///    hide it (bidirectional marks, embeddings, overrides and isolates,
///    zero-width characters, line and paragraph separators), and
///    `\ / : * ? " < > |` are removed, and so are trailing dots and spaces
///    (Windows drops those itself).
/// 3. Nothing is left, or only `.` or `..`: `None`.
/// 4. A name over [`MAX_NAME_BYTES`] is shortened to fit, on a character
///    boundary, keeping its extension.
/// 5. A Windows device name (`CON`, `PRN`, `AUX`, `NUL`, `COM0` to `COM9`,
///    `LPT0` to `LPT9`, also with any extension) gets an underscore in front:
///    `con.txt` gives `_con.txt`.
///
/// The result is never a path and is the same on both systems.
pub fn sanitize(name: &str) -> Option<String> {
    let last = name.rsplit(['/', '\\']).next().unwrap_or_default();
    let mut cleaned: String = last.chars().filter(|c| is_allowed(*c)).collect();
    trim_end_dots_and_spaces(&mut cleaned);
    if cleaned.is_empty() || cleaned == "." || cleaned == ".." {
        return None;
    }
    let mut safe = shorten(&cleaned, MAX_NAME_BYTES);
    trim_end_dots_and_spaces(&mut safe);
    // Checked last, because shortening can expose a device name: `CON`,
    // then 250 spaces and `x.txt`, ends up as `CON` and spaces before
    // `.txt`, which Windows reads as `CON`.
    if is_reserved(&safe) {
        safe.insert(0, '_');
        if safe.len() > MAX_NAME_BYTES {
            safe = shorten(&safe, MAX_NAME_BYTES);
            trim_end_dots_and_spaces(&mut safe);
        }
    }
    (!safe.is_empty()).then_some(safe)
}

/// The path in `dir` for a file called `name` that nothing uses yet:
/// `name.ext` if it is free, otherwise `name (2).ext`, `name (3).ext` and
/// so on, each at most [`MAX_NAME_BYTES`]. `name` must be a single file
/// name, such as [`sanitize`] gives.
///
/// Any entry counts as taken: a file, a folder, or a link (even a broken
/// one). The check goes through the file system, so on one that ignores
/// case (macOS and Windows by default) `Report.pdf` takes `report.pdf`.
/// Choosing and using the name are two steps, so callers that may race
/// each other must hold a lock across both.
///
/// # Errors
///
/// An error reading `dir` (other than a name being free), or
/// [`io::ErrorKind::AlreadyExists`] if all 9,999 names are taken.
pub fn free_path(dir: &Path, name: &str) -> io::Result<PathBuf> {
    let (stem, extension) = split_extension(name);
    for number in 1..=MAX_CANDIDATES {
        let candidate = if number == 1 {
            name.to_owned()
        } else {
            numbered(stem, extension, number)
        };
        let path = dir.join(candidate);
        match fs::symlink_metadata(&path) {
            Ok(_) => continue,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(path),
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "every numbered name for this file is taken",
    ))
}

/// `stem (number)extension`, with the stem shortened so the whole name
/// fits in [`MAX_NAME_BYTES`].
fn numbered(stem: &str, extension: &str, number: u32) -> String {
    let suffix = format!(" ({number})");
    let room = MAX_NAME_BYTES.saturating_sub(extension.len() + suffix.len());
    format!("{}{suffix}{extension}", cut(stem, room))
}

/// Whether `c` may stay in a file name.
fn is_allowed(c: char) -> bool {
    !c.is_control() && !is_invisible(c) && !FORBIDDEN.contains(&c)
}

/// The invisible characters that change how the text around them is shown
/// (bidirectional marks, embeddings, overrides and isolates), the
/// zero-width characters that hide text, and the line and paragraph
/// separators. The same set `hop-net` removes from names it shows, plus the
/// Arabic letter mark.
fn is_invisible(c: char) -> bool {
    matches!(
        c,
        '\u{061C}'
            | '\u{200B}'..='\u{200F}'
            | '\u{2028}'..='\u{202E}'
            | '\u{2060}'..='\u{2069}'
            | '\u{FEFF}'
    )
}

fn trim_end_dots_and_spaces(name: &mut String) {
    let kept = name.trim_end_matches(['.', ' ']).len();
    name.truncate(kept);
}

/// Whether Windows would read `name` as a device: the part before the
/// first dot, without trailing spaces, is a reserved name in any case.
fn is_reserved(name: &str) -> bool {
    let base = name
        .split('.')
        .next()
        .unwrap_or_default()
        .trim_end_matches(' ');
    RESERVED
        .iter()
        .any(|reserved| reserved.to_lowercase() == base.to_lowercase())
}

/// `name` split into its stem and its extension (dot included, or empty).
/// A leading dot (`.profile`) starts the stem rather than an extension,
/// and an ending longer than [`MAX_EXTENSION_BYTES`] is not an extension.
pub(crate) fn split_extension(name: &str) -> (&str, &str) {
    match name.rfind('.') {
        Some(dot) if dot > 0 && name.len() - dot <= MAX_EXTENSION_BYTES => name.split_at(dot),
        _ => (name, ""),
    }
}

/// `name` cut to at most `max` bytes, keeping its extension.
pub(crate) fn shorten(name: &str, max: usize) -> String {
    if name.len() <= max {
        return name.to_owned();
    }
    let (stem, extension) = split_extension(name);
    if !extension.is_empty() && extension.len() < max {
        format!("{}{extension}", cut(stem, max - extension.len()))
    } else {
        cut(name, max).to_owned()
    }
}

/// The longest start of `text` that is at most `max` bytes and ends on a
/// character boundary.
pub(crate) fn cut(text: &str, max: usize) -> &str {
    if text.len() <= max {
        return text;
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text.get(..end).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scratch::Scratch;

    fn clean(name: &str) -> Option<String> {
        sanitize(name)
    }

    #[test]
    fn only_the_last_path_component_is_kept() {
        assert_eq!(clean("../x").as_deref(), Some("x"));
        assert_eq!(clean("a/b\\c").as_deref(), Some("c"));
        assert_eq!(
            clean("..\\..\\Windows\\win.ini").as_deref(),
            Some("win.ini")
        );
        assert_eq!(clean("/etc/passwd").as_deref(), Some("passwd"));
        assert_eq!(
            clean("C:\\Users\\x\\report.pdf").as_deref(),
            Some("report.pdf")
        );
        assert_eq!(clean("folder/"), None, "nothing after the last slash");
    }

    #[test]
    fn dots_and_empty_names_are_refused() {
        for name in [
            "", ".", "..", "...", " ", " . ", "a/..", "a\\.", "\u{202E}", "\n\t",
        ] {
            assert_eq!(clean(name), None, "{name:?}");
        }
    }

    #[test]
    fn forbidden_control_and_invisible_characters_are_removed() {
        assert_eq!(clean("a:b*c?d\"e<f>g|h").as_deref(), Some("abcdefgh"));
        assert_eq!(
            clean("two\nlines\r\u{7}\u{1b}.txt").as_deref(),
            Some("twolines.txt")
        );
        assert_eq!(clean("del\u{7f}\u{85}.txt").as_deref(), Some("del.txt"));
        // "invoice\u{202E}fdp.exe" shows as "invoiceexe.pdf" where the
        // override is honoured: the cleaned name shows what it is.
        assert_eq!(
            clean("invoice\u{202E}fdp.exe").as_deref(),
            Some("invoicefdp.exe")
        );
        assert_eq!(
            clean("\u{200B}z\u{200D}w\u{2066}j\u{2069}\u{FEFF}\u{061C}\u{2028}.txt").as_deref(),
            Some("zwj.txt")
        );
    }

    #[test]
    fn trailing_dots_and_spaces_are_removed_but_inner_ones_kept() {
        assert_eq!(clean("notes.txt. . ").as_deref(), Some("notes.txt"));
        assert_eq!(clean("name...").as_deref(), Some("name"));
        assert_eq!(
            clean(" leading and inner  spaces .txt").as_deref(),
            Some(" leading and inner  spaces .txt")
        );
        assert_eq!(clean(".profile").as_deref(), Some(".profile"));
        assert_eq!(clean("..hidden").as_deref(), Some("..hidden"));
    }

    #[test]
    fn windows_device_names_get_an_underscore() {
        for (name, expected) in [
            ("CON", "_CON"),
            ("con.txt", "_con.txt"),
            ("LPT9.log", "_LPT9.log"),
            ("nul.tar.gz", "_nul.tar.gz"),
            ("Aux", "_Aux"),
            ("COM1", "_COM1"),
            ("com0.bin", "_com0.bin"),
            ("LPT¹.txt", "_LPT¹.txt"),
            ("con .txt", "_con .txt"),
            ("PRN.", "_PRN"),
        ] {
            assert_eq!(clean(name).as_deref(), Some(expected), "{name:?}");
        }
        for name in [
            "CONSOLE",
            "con1",
            "COM10",
            "LPT",
            "xCON",
            "CON_.txt",
            "my nul.txt",
        ] {
            assert_eq!(
                clean(name).as_deref(),
                Some(name),
                "{name:?} is an ordinary name"
            );
        }
    }

    #[test]
    fn long_names_are_cut_to_255_bytes_keeping_the_extension() {
        let long = format!("{}.pdf", "a".repeat(400));
        let cleaned = clean(&long).unwrap();
        assert_eq!(cleaned.len(), MAX_NAME_BYTES);
        assert!(cleaned.ends_with("a.pdf"), "{cleaned}");

        // Without an extension the name is simply cut.
        let cleaned = clean(&"b".repeat(300)).unwrap();
        assert_eq!(cleaned, "b".repeat(MAX_NAME_BYTES));

        // An ending too long to be an extension is cut like the rest.
        let odd = format!("x.{}", "y".repeat(300));
        assert_eq!(clean(&odd).unwrap().len(), MAX_NAME_BYTES);

        // A name of exactly 255 bytes is left alone.
        let exact = format!("{}.txt", "c".repeat(251));
        assert_eq!(clean(&exact).as_deref(), Some(exact.as_str()));
    }

    #[test]
    fn multi_byte_names_are_cut_on_a_character_boundary() {
        // "é" is 2 bytes and "👋" 4, so 255 bytes fall inside a character.
        for unit in ["é", "👋", "日本"] {
            let long = format!("{}.txt", unit.repeat(200));
            let cleaned = clean(&long).unwrap();
            assert!(cleaned.len() <= MAX_NAME_BYTES, "{unit}: {}", cleaned.len());
            assert!(
                cleaned.len() > MAX_NAME_BYTES - 8,
                "{unit}: {}",
                cleaned.len()
            );
            assert!(cleaned.ends_with(".txt"));
            let stem = cleaned.trim_end_matches(".txt");
            assert!(stem.chars().all(|c| unit.contains(c)), "{cleaned}");
        }
    }

    #[test]
    fn shortening_never_hides_a_device_name_or_leaves_a_trailing_space() {
        let sneaky = format!("CON{}x.txt", " ".repeat(300));
        let cleaned = clean(&sneaky).unwrap();
        assert!(cleaned.starts_with("_CON"), "{cleaned:?}");
        assert!(cleaned.len() <= MAX_NAME_BYTES);
        assert!(cleaned.ends_with(".txt"));

        // Cut inside a run of spaces with no extension: the spaces go too.
        let spaced = format!("{}{}z", "d".repeat(250), " ".repeat(50));
        let cleaned = clean(&spaced).unwrap();
        assert_eq!(cleaned, "d".repeat(250));
    }

    #[test]
    fn every_result_is_a_short_single_component() {
        let hostile = [
            "../../etc/passwd",
            "a/b\\c",
            "CON",
            "con.txt",
            "LPT9.log",
            "trailing. ",
            "ctl\u{0}\u{1}",
            "bidi\u{202E}\u{2066}",
            &"é".repeat(300),
            &format!("{}.{}", "x".repeat(300), "y".repeat(40)),
        ];
        for name in hostile {
            if let Some(cleaned) = clean(name) {
                assert!(!cleaned.is_empty());
                assert!(cleaned.len() <= MAX_NAME_BYTES, "{name:?}");
                assert!(!cleaned.contains(['/', '\\', ':']), "{cleaned:?}");
                assert!(!cleaned.ends_with(['.', ' ']), "{cleaned:?}");
                assert!(!is_reserved(&cleaned), "{cleaned:?}");
                assert_eq!(clean(&cleaned).as_deref(), Some(cleaned.as_str()), "stable");
            }
        }
    }

    #[test]
    fn extensions_are_split_at_the_last_dot_after_the_first_character() {
        assert_eq!(split_extension("a.tar.gz"), ("a.tar", ".gz"));
        assert_eq!(split_extension(".profile"), (".profile", ""));
        assert_eq!(split_extension("README"), ("README", ""));
        let long_ending = format!("a.{}", "b".repeat(MAX_EXTENSION_BYTES));
        assert_eq!(split_extension(&long_ending), (long_ending.as_str(), ""));
    }

    #[test]
    fn free_names_count_up_and_never_reuse_a_taken_one() {
        let scratch = Scratch::new("free-names");
        let dir = scratch.path();
        assert_eq!(
            free_path(dir, "report.pdf").unwrap(),
            dir.join("report.pdf")
        );
        fs::write(dir.join("report.pdf"), b"first").unwrap();
        assert_eq!(
            free_path(dir, "report.pdf").unwrap(),
            dir.join("report (2).pdf")
        );
        fs::write(dir.join("report (2).pdf"), b"second").unwrap();
        // A folder takes a name too.
        fs::create_dir(dir.join("report (3).pdf")).unwrap();
        assert_eq!(
            free_path(dir, "report.pdf").unwrap(),
            dir.join("report (4).pdf")
        );
        // Names without an extension, and dot files.
        fs::write(dir.join("README"), b"").unwrap();
        assert_eq!(free_path(dir, "README").unwrap(), dir.join("README (2)"));
        fs::write(dir.join(".profile"), b"").unwrap();
        assert_eq!(
            free_path(dir, ".profile").unwrap(),
            dir.join(".profile (2)")
        );
        // Nothing was changed on the way.
        assert_eq!(fs::read(dir.join("report.pdf")).unwrap(), b"first");
    }

    #[cfg(unix)]
    #[test]
    fn a_broken_link_takes_its_name() {
        let scratch = Scratch::new("free-link");
        let dir = scratch.path();
        std::os::unix::fs::symlink(dir.join("missing"), dir.join("link.txt")).unwrap();
        assert_eq!(
            free_path(dir, "link.txt").unwrap(),
            dir.join("link (2).txt")
        );
    }

    #[test]
    fn numbered_names_still_fit() {
        let scratch = Scratch::new("free-long");
        let dir = scratch.path();
        let name = format!("{}.txt", "n".repeat(MAX_NAME_BYTES - 4));
        assert_eq!(name.len(), MAX_NAME_BYTES);
        fs::write(dir.join(&name), b"").unwrap();
        let second = free_path(dir, &name).unwrap();
        let second = second.file_name().unwrap().to_str().unwrap();
        assert_eq!(second.len(), MAX_NAME_BYTES);
        assert!(second.ends_with("n (2).txt"), "{second}");
        // Multi-byte stems are cut on a character boundary.
        let wide = format!("{}.txt", "é".repeat(125));
        fs::write(dir.join(&wide), b"").unwrap();
        let second = free_path(dir, &wide).unwrap();
        let second = second.file_name().unwrap().to_str().unwrap();
        assert!(second.len() <= MAX_NAME_BYTES && second.ends_with("é (2).txt"));
    }

    #[test]
    fn a_missing_folder_has_every_name_free() {
        let scratch = Scratch::new("free-missing");
        let missing = scratch.path().join("not yet");
        assert_eq!(free_path(&missing, "a.txt").unwrap(), missing.join("a.txt"));
    }
}
