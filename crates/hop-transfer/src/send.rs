//! Reading the files to send: which of the dropped items can go
//! ([`plan`]), and each file's bytes and SHA-256 ([`FileReader`]).

use std::fmt;
use std::fs::{self, File};
use std::io::{self, Read as _};
use std::path::{Path, PathBuf};

use sha2::{Digest as _, Sha256};

use crate::Digest;

/// A file that will be sent.
///
/// Its name is for the other computer and the app; never for the log.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OutgoingFile {
    /// Where the file is on this computer.
    pub path: PathBuf,
    /// Its name, as the offer carries it: the last component of `path`
    /// (with any invalid Unicode replaced). The receiver cleans it.
    pub name: String,
    /// Its length in bytes when it was planned.
    pub size: u64,
}

/// What [`plan`] found among the dropped items.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Plan {
    /// The files to send, in the order they were dropped.
    pub files: Vec<OutgoingFile>,
    /// How many items were left out: folders, missing items and files that
    /// cannot be read.
    pub skipped: u32,
}

/// Sorts the dropped `paths` into the files that can be sent and the
/// items that cannot.
///
/// Only regular files are sent (a link to one sends the file it points
/// to). Folders, items that no longer exist and files that cannot be
/// opened for reading are skipped and counted. Each file is opened once to
/// check that it can be read; nothing is changed.
pub fn plan(paths: &[PathBuf]) -> Plan {
    let mut planned = Plan::default();
    for path in paths {
        match outgoing(path) {
            Some(file) => planned.files.push(file),
            None => planned.skipped = planned.skipped.saturating_add(1),
        }
    }
    planned
}

fn outgoing(path: &Path) -> Option<OutgoingFile> {
    let metadata = fs::metadata(path).ok()?;
    if !metadata.is_file() {
        return None;
    }
    let name = path.file_name()?.to_string_lossy().into_owned();
    File::open(path).ok()?;
    Some(OutgoingFile {
        path: path.to_path_buf(),
        name,
        size: metadata.len(),
    })
}

/// Why a file could not be read to the end. The messages name no file.
#[derive(Debug, thiserror::Error)]
pub enum ReadError {
    /// The file's length is no longer the planned size: it changed after
    /// it was dropped, or while it was being read.
    #[error("the file changed while it was being sent")]
    Changed,
    /// The file could not be opened or read.
    #[error("could not read the file: {0}")]
    Io(#[from] io::Error),
}

/// What [`FileReader::next`] gives.
#[derive(Clone, PartialEq, Eq)]
pub enum Piece {
    /// The next bytes of the file.
    Data(Vec<u8>),
    /// Every byte has been read and the file ends there: its SHA-256.
    Done(Digest),
}

impl fmt::Debug for Piece {
    // Lengths only: never the contents.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Piece::Data(bytes) => write!(f, "Data({} bytes)", bytes.len()),
            Piece::Done(_) => f.write_str("Done"),
        }
    }
}

/// Reads one planned file in pieces, hashing it, and fails if its length
/// is not the planned size.
pub struct FileReader {
    file: File,
    size: u64,
    read: u64,
    hasher: Sha256,
    /// The digest, once the end was confirmed.
    done: Option<Digest>,
}

impl FileReader {
    /// Opens `file` for reading.
    ///
    /// # Errors
    ///
    /// [`ReadError::Changed`] if its length is no longer the planned size,
    /// and [`ReadError::Io`] if it cannot be opened.
    pub fn open(file: &OutgoingFile) -> Result<FileReader, ReadError> {
        let handle = File::open(&file.path)?;
        if handle.metadata()?.len() != file.size {
            return Err(ReadError::Changed);
        }
        Ok(FileReader {
            file: handle,
            size: file.size,
            read: 0,
            hasher: Sha256::new(),
            done: None,
        })
    }

    /// The next piece of at most `max` bytes (at least 1), or, once all
    /// `size` bytes were read and the file is confirmed to end there,
    /// [`Piece::Done`] with its SHA-256 (again on every later call).
    ///
    /// # Errors
    ///
    /// [`ReadError::Changed`] if the file ends early or goes on past its
    /// planned size, and [`ReadError::Io`] if reading fails.
    pub fn next(&mut self, max: usize) -> Result<Piece, ReadError> {
        if let Some(digest) = self.done {
            return Ok(Piece::Done(digest));
        }
        let left = self.size - self.read;
        if left == 0 {
            // The file must end exactly here.
            let mut probe = [0u8; 1];
            if read_some(&mut self.file, &mut probe)? > 0 {
                return Err(ReadError::Changed);
            }
            let digest: Digest = std::mem::take(&mut self.hasher).finalize().into();
            self.done = Some(digest);
            return Ok(Piece::Done(digest));
        }
        let want = usize::try_from(left).unwrap_or(usize::MAX).min(max.max(1));
        let mut piece = vec![0u8; want];
        let count = read_some(&mut self.file, &mut piece)?;
        if count == 0 {
            return Err(ReadError::Changed);
        }
        piece.truncate(count);
        self.hasher.update(&piece);
        self.read += count as u64;
        Ok(Piece::Data(piece))
    }
}

impl fmt::Debug for FileReader {
    // No name or path: this may end up in a log.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FileReader")
            .field("size", &self.size)
            .field("read", &self.read)
            .finish_non_exhaustive()
    }
}

/// One `read`, retried when a signal interrupts it.
fn read_some(file: &mut File, buf: &mut [u8]) -> io::Result<usize> {
    loop {
        match file.read(buf) {
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            other => return other,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write as _;

    use super::*;
    use crate::scratch::Scratch;

    /// Reads `file` to the end in pieces of `max` bytes.
    fn read_all(file: &OutgoingFile, max: usize) -> Result<(Vec<u8>, Digest), ReadError> {
        let mut reader = FileReader::open(file)?;
        let mut content = Vec::new();
        loop {
            match reader.next(max)? {
                Piece::Data(piece) => {
                    assert!(!piece.is_empty() && piece.len() <= max);
                    content.extend_from_slice(&piece);
                }
                Piece::Done(digest) => return Ok((content, digest)),
            }
        }
    }

    #[test]
    fn only_readable_regular_files_are_planned() {
        let scratch = Scratch::new("plan");
        let dir = scratch.path();
        fs::write(dir.join("a.txt"), b"alpha").unwrap();
        fs::write(dir.join("empty"), b"").unwrap();
        fs::create_dir(dir.join("folder")).unwrap();
        let paths = vec![
            dir.join("a.txt"),
            dir.join("folder"),
            dir.join("missing.txt"),
            dir.join("empty"),
            dir.join("a.txt"),
        ];
        let planned = plan(&paths);
        let names: Vec<&str> = planned.files.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(names, ["a.txt", "empty", "a.txt"]);
        assert_eq!(planned.skipped, 2);
        assert_eq!(planned.files[0].size, 5);
        assert_eq!(planned.files[0].path, dir.join("a.txt"));
        assert_eq!(plan(&[]), Plan::default());
    }

    #[cfg(unix)]
    #[test]
    fn unreadable_files_are_skipped_and_links_send_their_target() {
        use std::os::unix::fs::PermissionsExt as _;
        let scratch = Scratch::new("plan-unix");
        let dir = scratch.path();
        let locked = dir.join("locked.txt");
        fs::write(&locked, b"no").unwrap();
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();
        fs::write(dir.join("target.txt"), b"linked").unwrap();
        std::os::unix::fs::symlink(dir.join("target.txt"), dir.join("link.txt")).unwrap();
        let planned = plan(&[locked.clone(), dir.join("link.txt")]);
        // Running as root can read anything; the link is the point then.
        let readable = File::open(&locked).is_ok();
        assert_eq!(planned.skipped, u32::from(!readable));
        let link = planned.files.last().unwrap();
        assert_eq!((link.name.as_str(), link.size), ("link.txt", 6));
        assert_eq!(read_all(link, 4).unwrap().0, b"linked");
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o600)).unwrap();
    }

    #[test]
    fn a_file_is_read_in_pieces_with_its_checksum() {
        let scratch = Scratch::new("read");
        let path = scratch.path().join("data.bin");
        let content: Vec<u8> = (0..100_000u32).map(|i| (i % 251) as u8).collect();
        fs::write(&path, &content).unwrap();
        let file = plan(&[path]).files.remove(0);
        for max in [1, 7, 4096, 1 << 20] {
            let (read, digest) = read_all(&file, max).unwrap();
            assert_eq!(read, content);
            assert_eq!(digest, <[u8; 32]>::from(Sha256::digest(&content)));
        }
        let empty = scratch.path().join("empty");
        fs::write(&empty, b"").unwrap();
        let file = plan(&[empty]).files.remove(0);
        let (read, digest) = read_all(&file, 10).unwrap();
        assert!(read.is_empty());
        assert_eq!(digest, <[u8; 32]>::from(Sha256::digest(b"")));
    }

    #[test]
    fn done_repeats_once_reached() {
        let scratch = Scratch::new("read-done");
        let path = scratch.path().join("x");
        fs::write(&path, b"xy").unwrap();
        let file = plan(&[path]).files.remove(0);
        let mut reader = FileReader::open(&file).unwrap();
        assert_eq!(reader.next(10).unwrap(), Piece::Data(b"xy".to_vec()));
        let done = reader.next(10).unwrap();
        assert!(matches!(done, Piece::Done(_)));
        assert_eq!(reader.next(10).unwrap(), done);
    }

    #[test]
    fn a_file_that_changed_since_it_was_planned_fails() {
        let scratch = Scratch::new("read-changed");
        let path = scratch.path().join("growing.log");
        fs::write(&path, b"12345678").unwrap();
        let file = plan(std::slice::from_ref(&path)).files.remove(0);

        // Changed before it was opened.
        fs::write(&path, b"123456789").unwrap();
        assert!(matches!(FileReader::open(&file), Err(ReadError::Changed)));

        // Grew while it was read.
        fs::write(&path, b"12345678").unwrap();
        let mut reader = FileReader::open(&file).unwrap();
        assert!(matches!(reader.next(5).unwrap(), Piece::Data(_)));
        fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(b"more")
            .unwrap();
        assert!(matches!(reader.next(5).unwrap(), Piece::Data(_)));
        assert!(matches!(reader.next(5), Err(ReadError::Changed)));

        // Shrank while it was read.
        fs::write(&path, b"12345678").unwrap();
        let mut reader = FileReader::open(&file).unwrap();
        assert!(matches!(reader.next(4).unwrap(), Piece::Data(_)));
        fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .unwrap()
            .set_len(5)
            .unwrap();
        assert!(matches!(reader.next(4).unwrap(), Piece::Data(_)));
        assert!(matches!(reader.next(4), Err(ReadError::Changed)));

        // Gone before it was opened.
        fs::remove_file(&path).unwrap();
        assert!(matches!(FileReader::open(&file), Err(ReadError::Io(_))));
    }

    #[test]
    fn debug_output_shows_no_contents_or_names() {
        let scratch = Scratch::new("read-debug");
        let path = scratch.path().join("secret.txt");
        fs::write(&path, b"secret contents").unwrap();
        let file = plan(&[path]).files.remove(0);
        let mut reader = FileReader::open(&file).unwrap();
        assert!(!format!("{reader:?}").contains("secret"));
        let piece = reader.next(100).unwrap();
        assert_eq!(format!("{piece:?}"), "Data(15 bytes)");
    }
}
