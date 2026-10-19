//! Saving a file that arrives in pieces: under a temporary name first,
//! hashed while written, and renamed only once it checks out.

use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};

use sha2::{Digest as _, Sha256};

use crate::Digest;
use crate::names::{MAX_NAME_BYTES, cut, free_path, sanitize};

/// The ending of every temporary file: a file that is arriving is written
/// to `.<name>.hop-part` in the Received folder, where `<name>` is its
/// cleaned name, shortened if needed.
pub const TEMP_SUFFIX: &str = ".hop-part";

/// How many temporary names [`IncomingFile::create`] tries when one is
/// taken: `.<name>.hop-part`, then `.<name>.2.hop-part` and so on.
const TEMP_ATTEMPTS: u32 = 100;

/// Held while a finished file is given its final name, so two files
/// finishing at once in this process never pick the same free name.
static PLACING: Mutex<()> = Mutex::new(());

/// Why a file that was arriving was not saved. The messages name no file.
#[derive(Debug, thiserror::Error)]
pub enum ReceiveError {
    /// The name from the other computer does not survive
    /// [`sanitize`](crate::sanitize).
    #[error("the file name cannot be used")]
    BadName,
    /// More bytes arrived than the file's declared size.
    #[error("more bytes arrived than the file's size")]
    TooLong,
    /// Fewer bytes arrived than the file's declared size.
    #[error("the file ended after {received} of its {expected} bytes")]
    TooShort {
        /// The declared size.
        expected: u64,
        /// The bytes that arrived.
        received: u64,
    },
    /// The bytes do not match the SHA-256 the other computer sent.
    #[error("the file does not match its checksum")]
    Corrupt,
    /// Reading the folder or writing the file failed (for example, the
    /// disk is full).
    #[error("could not write the file: {0}")]
    Io(#[from] io::Error),
}

/// One file arriving from the other computer, from its first byte until
/// it is saved.
///
/// [`create`](Self::create) opens a new temporary file in the folder,
/// [`write`](Self::write) appends the pieces in order while hashing them,
/// and [`finish`](Self::finish) checks the size and the SHA-256 and gives
/// the file its final name. Dropping it before `finish` succeeds deletes
/// the temporary file.
pub struct IncomingFile {
    dir: PathBuf,
    /// The cleaned name the file gets once it checks out.
    name: String,
    temp: PathBuf,
    /// `None` once the file was closed for its rename.
    file: Option<File>,
    size: u64,
    written: u64,
    hasher: Sha256,
    /// Set once the file has its final name; the temporary one is gone.
    placed: bool,
}

impl IncomingFile {
    /// Starts receiving a file called `name` (as the other computer sent
    /// it) of `size` bytes in `dir`, which must exist.
    ///
    /// The file is created under a temporary name that nothing uses yet
    /// (never opening an existing file), so nothing in the folder changes
    /// until [`finish`](Self::finish).
    ///
    /// # Errors
    ///
    /// [`ReceiveError::BadName`] if the name does not survive
    /// [`sanitize`], and [`ReceiveError::Io`] if the file cannot be created.
    pub fn create(dir: &Path, name: &str, size: u64) -> Result<IncomingFile, ReceiveError> {
        let name = sanitize(name).ok_or(ReceiveError::BadName)?;
        let (temp, file) = create_temp(dir, &name)?;
        Ok(IncomingFile {
            dir: dir.to_path_buf(),
            name,
            temp,
            file: Some(file),
            size,
            written: 0,
            hasher: Sha256::new(),
            placed: false,
        })
    }

    /// Appends the next piece of the file.
    ///
    /// # Errors
    ///
    /// [`ReceiveError::TooLong`] if the file would grow past its declared
    /// size (nothing of the piece is written), and [`ReceiveError::Io`] if
    /// writing fails. Either way the file cannot be saved any more: drop
    /// it.
    pub fn write(&mut self, piece: &[u8]) -> Result<(), ReceiveError> {
        let length = u64::try_from(piece.len()).map_err(|_| ReceiveError::TooLong)?;
        let total = self
            .written
            .checked_add(length)
            .filter(|total| *total <= self.size)
            .ok_or(ReceiveError::TooLong)?;
        let file = self.file.as_mut().ok_or(ReceiveError::TooLong)?;
        file.write_all(piece)?;
        self.hasher.update(piece);
        self.written = total;
        Ok(())
    }

    /// How many bytes have been written so far.
    pub fn written(&self) -> u64 {
        self.written
    }

    /// Checks that every byte arrived and that they match `expected`, then
    /// renames the file to a free name in the folder ([`free_path`]) and
    /// returns where it is.
    ///
    /// # Errors
    ///
    /// [`ReceiveError::TooShort`], [`ReceiveError::Corrupt`], or
    /// [`ReceiveError::Io`] if the rename fails. The temporary file is
    /// deleted in every case.
    pub fn finish(mut self, expected: &Digest) -> Result<PathBuf, ReceiveError> {
        if self.written != self.size {
            return Err(ReceiveError::TooShort {
                expected: self.size,
                received: self.written,
            });
        }
        let digest: Digest = std::mem::take(&mut self.hasher).finalize().into();
        if digest != *expected {
            return Err(ReceiveError::Corrupt);
        }
        // Closed before the rename, so nothing holds the finished file open.
        drop(self.file.take());
        let _placing = PLACING.lock().unwrap_or_else(PoisonError::into_inner);
        // A received file never gets a name the sweep would take for a
        // leftover temporary file.
        let name = if is_temp_name(&self.name) {
            format!("_{}", self.name)
        } else {
            self.name.clone()
        };
        let path = free_path(&self.dir, &name)?;
        fs::rename(&self.temp, &path)?;
        self.placed = true;
        Ok(path)
    }
}

impl Drop for IncomingFile {
    fn drop(&mut self) {
        if !self.placed {
            drop(self.file.take());
            let _ = fs::remove_file(&self.temp);
        }
    }
}

impl fmt::Debug for IncomingFile {
    // No name or path: this may end up in a log.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("IncomingFile")
            .field("size", &self.size)
            .field("written", &self.written)
            .finish_non_exhaustive()
    }
}

/// Creates a new temporary file for `name` in `dir`, never opening one that
/// exists.
fn create_temp(dir: &Path, name: &str) -> Result<(PathBuf, File), ReceiveError> {
    for attempt in 1..=TEMP_ATTEMPTS {
        let temp = dir.join(temp_name(name, attempt));
        match OpenOptions::new().write(true).create_new(true).open(&temp) {
            Ok(file) => return Ok((temp, file)),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error.into()),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "every temporary name for this file is taken",
    )
    .into())
}

/// `.<name>.hop-part`, or `.<name>.<attempt>.hop-part` after the first
/// attempt, with `name` shortened so the whole fits in [`MAX_NAME_BYTES`].
fn temp_name(name: &str, attempt: u32) -> String {
    let number = if attempt > 1 {
        format!(".{attempt}")
    } else {
        String::new()
    };
    let room = MAX_NAME_BYTES.saturating_sub(1 + number.len() + TEMP_SUFFIX.len());
    format!(".{}{number}{TEMP_SUFFIX}", cut(name, room))
}

/// Whether `name` has the shape of a temporary file: a leading dot and the
/// [`TEMP_SUFFIX`] ending.
fn is_temp_name(name: &str) -> bool {
    name.starts_with('.') && name.ends_with(TEMP_SUFFIX) && name.len() > TEMP_SUFFIX.len()
}

/// Deletes the temporary files an earlier run left in `dir` (a crash, or a
/// computer switched off mid-transfer) and returns how many it deleted.
///
/// Only plain files whose names have the temporary shape
/// (`.<name>.hop-part`) are deleted; never a folder, a link or any other
/// file. A missing folder has nothing to sweep. Call it once per run,
/// before the first file arrives: it would also delete the temporary file
/// of a transfer in progress.
///
/// # Errors
///
/// Only if the folder exists but cannot be read; a file that cannot be
/// deleted is skipped.
pub fn sweep(dir: &Path) -> io::Result<usize> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(0),
        Err(error) => return Err(error),
    };
    let mut removed = 0;
    for entry in entries.flatten() {
        let name = entry.file_name();
        if !name.to_str().is_some_and(is_temp_name) {
            continue;
        }
        // The entry's own type: a link is never followed.
        if !entry.file_type().is_ok_and(|kind| kind.is_file()) {
            continue;
        }
        if fs::remove_file(entry.path()).is_ok() {
            removed += 1;
        }
    }
    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scratch::Scratch;

    fn sha256(bytes: &[u8]) -> Digest {
        Sha256::digest(bytes).into()
    }

    /// Receives `content` as `name` in one piece per `piece` bytes.
    fn receive(dir: &Path, name: &str, content: &[u8], piece: usize) -> PathBuf {
        let mut file = IncomingFile::create(dir, name, content.len() as u64).unwrap();
        for chunk in content.chunks(piece.max(1)) {
            file.write(chunk).unwrap();
        }
        file.finish(&sha256(content)).unwrap()
    }

    #[test]
    fn a_file_arrives_under_a_temporary_name_and_is_renamed_once_it_checks_out() {
        let scratch = Scratch::new("receive");
        let content = b"hello from the other computer".repeat(1000);
        let mut file =
            IncomingFile::create(scratch.path(), "greeting.txt", content.len() as u64).unwrap();
        assert_eq!(scratch.names(), [".greeting.txt.hop-part"]);
        for chunk in content.chunks(4096) {
            file.write(chunk).unwrap();
        }
        assert_eq!(file.written(), content.len() as u64);
        assert_eq!(
            scratch.names(),
            [".greeting.txt.hop-part"],
            "not before finish"
        );
        let saved = file.finish(&sha256(&content)).unwrap();
        assert_eq!(saved, scratch.path().join("greeting.txt"));
        assert_eq!(scratch.names(), ["greeting.txt"]);
        assert_eq!(fs::read(&saved).unwrap(), content);
    }

    #[test]
    fn names_are_cleaned_and_never_clobber() {
        let scratch = Scratch::new("receive-names");
        let dir = scratch.path();
        fs::write(dir.join("report.pdf"), b"mine").unwrap();
        assert_eq!(
            receive(dir, "../../report.pdf", b"one", 2),
            dir.join("report (2).pdf")
        );
        assert_eq!(
            receive(dir, "report.pdf", b"two", 2),
            dir.join("report (3).pdf")
        );
        assert_eq!(receive(dir, "CON.txt", b"three", 2), dir.join("_CON.txt"));
        assert_eq!(fs::read(dir.join("report.pdf")).unwrap(), b"mine");
        assert_eq!(fs::read(dir.join("report (3).pdf")).unwrap(), b"two");
        assert!(matches!(
            IncomingFile::create(dir, "..", 1),
            Err(ReceiveError::BadName)
        ));
    }

    #[test]
    fn an_empty_file_is_saved() {
        let scratch = Scratch::new("receive-empty");
        let file = IncomingFile::create(scratch.path(), "empty", 0).unwrap();
        let saved = file.finish(&sha256(b"")).unwrap();
        assert_eq!(fs::read(saved).unwrap(), b"");
    }

    #[test]
    fn a_wrong_checksum_or_size_saves_nothing() {
        let scratch = Scratch::new("receive-bad");
        let dir = scratch.path();

        let mut file = IncomingFile::create(dir, "a.bin", 4).unwrap();
        file.write(b"abcd").unwrap();
        assert!(matches!(
            file.finish(&sha256(b"abce")),
            Err(ReceiveError::Corrupt)
        ));
        assert!(scratch.names().is_empty(), "{:?}", scratch.names());

        let mut file = IncomingFile::create(dir, "b.bin", 4).unwrap();
        file.write(b"abc").unwrap();
        assert!(matches!(
            file.finish(&sha256(b"abc")),
            Err(ReceiveError::TooShort {
                expected: 4,
                received: 3
            })
        ));
        assert!(scratch.names().is_empty());

        let mut file = IncomingFile::create(dir, "c.bin", 4).unwrap();
        file.write(b"abc").unwrap();
        assert!(matches!(file.write(b"de"), Err(ReceiveError::TooLong)));
        assert_eq!(file.written(), 3, "nothing of the piece was written");
        drop(file);
        assert!(scratch.names().is_empty());
    }

    #[test]
    fn dropping_an_unfinished_file_deletes_it() {
        let scratch = Scratch::new("receive-drop");
        let mut file = IncomingFile::create(scratch.path(), "half.mov", 10).unwrap();
        file.write(b"12345").unwrap();
        assert_eq!(scratch.names(), [".half.mov.hop-part"]);
        drop(file);
        assert!(scratch.names().is_empty());
    }

    #[test]
    fn a_taken_temporary_name_gets_a_number() {
        let scratch = Scratch::new("receive-temp");
        let dir = scratch.path();
        fs::write(dir.join(".same.hop-part"), b"someone else's").unwrap();
        let first = IncomingFile::create(dir, "same", 1).unwrap();
        let second = IncomingFile::create(dir, "same", 1).unwrap();
        assert_eq!(
            scratch.names(),
            [".same.2.hop-part", ".same.3.hop-part", ".same.hop-part"]
        );
        drop((first, second));
        assert_eq!(scratch.names(), [".same.hop-part"], "only their own files");
    }

    #[test]
    fn temporary_names_fit_even_for_the_longest_names() {
        let longest = "x".repeat(MAX_NAME_BYTES);
        for attempt in [1, 2, 100] {
            let temp = temp_name(&longest, attempt);
            assert!(temp.len() <= MAX_NAME_BYTES, "{}", temp.len());
            assert!(is_temp_name(&temp));
        }
        let scratch = Scratch::new("receive-long");
        let content = b"long name";
        let saved = receive(scratch.path(), &longest, content, 3);
        assert_eq!(saved, scratch.path().join(&longest));
    }

    #[test]
    fn a_file_named_like_a_temporary_one_is_saved_under_another_name() {
        let scratch = Scratch::new("receive-lookalike");
        let saved = receive(scratch.path(), ".notes.hop-part", b"real", 4);
        assert_eq!(saved, scratch.path().join("_.notes.hop-part"));
        assert_eq!(
            sweep(scratch.path()).unwrap(),
            0,
            "the sweep leaves it alone"
        );
        // An ordinary name that merely ends the same way is not temporary.
        let saved = receive(scratch.path(), "draft.hop-part", b"real", 4);
        assert_eq!(saved, scratch.path().join("draft.hop-part"));
        assert_eq!(sweep(scratch.path()).unwrap(), 0);
    }

    #[test]
    fn the_sweep_deletes_only_leftover_temporary_files() {
        let scratch = Scratch::new("sweep");
        let dir = scratch.path();
        for name in [
            ".a.pdf.hop-part",
            ".b.2.hop-part",
            "keep.pdf",
            "keep.hop-part",
            ".keep",
        ] {
            fs::write(dir.join(name), b"x").unwrap();
        }
        fs::create_dir(dir.join(".folder.hop-part")).unwrap();
        assert_eq!(sweep(dir).unwrap(), 2);
        assert_eq!(
            scratch.names(),
            [".folder.hop-part", ".keep", "keep.hop-part", "keep.pdf"]
        );
        assert_eq!(sweep(&dir.join("missing")).unwrap(), 0);
    }

    #[cfg(unix)]
    #[test]
    fn the_sweep_never_follows_a_link() {
        let scratch = Scratch::new("sweep-link");
        let dir = scratch.path();
        fs::write(dir.join("target.txt"), b"keep me").unwrap();
        std::os::unix::fs::symlink(dir.join("target.txt"), dir.join(".link.hop-part")).unwrap();
        assert_eq!(sweep(dir).unwrap(), 0);
        assert_eq!(fs::read(dir.join("target.txt")).unwrap(), b"keep me");
    }

    #[test]
    fn debug_output_names_no_file() {
        let scratch = Scratch::new("receive-debug");
        let file = IncomingFile::create(scratch.path(), "secret plans.txt", 3).unwrap();
        let shown = format!("{file:?}");
        assert!(!shown.contains("secret"), "{shown}");
        assert!(!format!("{}", ReceiveError::Corrupt).contains("secret"));
    }
}
