//! Hop's file transfers on disk (milestone M5; docs/M4-M5-DESIGN.md, "The
//! transfer protocol"): turning a name from the other computer into a safe
//! file name, saving a file that arrives in pieces, and reading the files
//! the user dropped. `hop-net` moves the bytes; this crate only reads and
//! writes the files it is given.
//!
//! # Pieces
//!
//! | Piece | Entry points | Runs on |
//! |---|---|---|
//! | Names | [`sanitize`], [`free_path`] | the receiving computer |
//! | Receiving | [`IncomingFile`], [`sweep`] | the receiving computer |
//! | Sending | [`plan`], [`FileReader`] | the sending computer |
//!
//! Every function here except [`sanitize`] does blocking disk I/O: call it
//! from a thread of its own, never from an async runtime's workers.
//!
//! # Receiving a file
//!
//! ```no_run
//! use std::path::Path;
//! use hop_transfer::{Digest, IncomingFile, sweep};
//!
//! # fn demo(folder: &Path, pieces: &[Vec<u8>], digest: Digest) -> Result<(), hop_transfer::ReceiveError> {
//! // Once per run, before the first file: delete what an earlier run left.
//! let _removed = sweep(folder)?;
//! // The name and size come from the other computer's offer.
//! let mut file = IncomingFile::create(folder, "Quarterly report.pdf", 48_213)?;
//! for piece in pieces {
//!     file.write(piece)?;
//! }
//! // Checks the size and the SHA-256, marks the file as coming from
//! // another computer, then gives it its final name.
//! let saved = file.finish(&digest)?;
//! # let _ = (saved.path, saved.unmarked);
//! # Ok(())
//! # }
//! ```
//!
//! Dropping an [`IncomingFile`] before [`IncomingFile::finish`] succeeds
//! deletes everything it wrote.
//!
//! # Guarantees
//!
//! - A name from the other computer never becomes a path. [`sanitize`]
//!   keeps only its last component and cleans it; [`IncomingFile::create`]
//!   refuses a name that does not survive.
//! - Nothing is overwritten. A file arrives under a temporary name
//!   (`.<name>.hop-part`) and gets its final name only once its size and
//!   SHA-256 check out, from [`free_path`]: `name.ext`, `name (2).ext`, and
//!   so on.
//! - Nothing incomplete keeps a real name: a failure, or dropping the
//!   writer, deletes the temporary file, and [`sweep`] deletes what a crash
//!   left behind (only those temporary files).
//! - A received file is checked before it is opened, as a download is. On
//!   Windows it gets the Mark of the Web while it still has its temporary
//!   name, so it never has its real name without it; on the Mac, macOS
//!   quarantines what Hop.app creates. A drive that cannot keep the mark
//!   still gets the file, and [`Saved::unmarked`] says so.
//! - Errors and `Debug` output of the reader and writer never contain file
//!   names or contents, so they are safe to log.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod names;
mod receive;
mod send;

pub use names::{MAX_NAME_BYTES, free_path, sanitize};
pub use receive::{IncomingFile, ReceiveError, Saved, TEMP_SUFFIX, sweep};
pub use send::{FileReader, OutgoingFile, Piece, Plan, ReadError, plan};

/// A file's SHA-256, as it travels after the file's bytes.
pub type Digest = [u8; DIGEST_LEN];

/// The length of a [`Digest`] in bytes: 32.
pub const DIGEST_LEN: usize = 32;

/// Temporary folders for the tests: unique per test and per run, and
/// deleted when the test ends, even when it fails.
#[cfg(test)]
pub(crate) mod scratch {
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU32, Ordering};

    pub(crate) struct Scratch {
        dir: PathBuf,
    }

    impl Scratch {
        pub(crate) fn new(test: &str) -> Scratch {
            static NEXT: AtomicU32 = AtomicU32::new(0);
            let dir = std::env::temp_dir().join(format!(
                "hop-transfer-{test}-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).unwrap();
            Scratch { dir }
        }

        pub(crate) fn path(&self) -> &Path {
            &self.dir
        }

        /// The names in the folder, sorted.
        pub(crate) fn names(&self) -> Vec<String> {
            let mut names: Vec<String> = fs::read_dir(&self.dir)
                .unwrap()
                .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
                .collect();
            names.sort();
            names
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.dir);
        }
    }
}
