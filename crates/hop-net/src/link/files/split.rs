//! Cutting a file data stream into each file's bytes and checksum: after
//! the transfer id, for each file in offer order, exactly `size` bytes and
//! then its 32-byte SHA-256.

use bytes::Bytes;
use hop_transfer::{DIGEST_LEN, Digest};

/// One file's share of the data stream.
#[derive(Clone, PartialEq, Eq)]
pub(super) enum Piece {
    /// The next bytes of the current file.
    Data(Bytes),
    /// The current file's SHA-256: its bytes are complete.
    End(Digest),
}

impl std::fmt::Debug for Piece {
    // Lengths only: never the contents.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Piece::Data(bytes) => write!(f, "Data({} bytes)", bytes.len()),
            Piece::End(_) => f.write_str("End"),
        }
    }
}

/// Bytes arrived after the last file's checksum.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct ExtraBytes;

/// Splits the data stream of one transfer, whatever pieces the network
/// delivers it in.
pub(super) struct Splitter {
    sizes: Vec<u64>,
    /// The file arriving now; `sizes.len()` once every file is complete.
    index: usize,
    /// Its bytes still to come. At zero, its checksum is arriving.
    left: u64,
    /// Its checksum, as far as it has arrived.
    digest: Vec<u8>,
}

impl Splitter {
    /// A splitter for files of these sizes, in offer order.
    pub(super) fn new(sizes: Vec<u64>) -> Splitter {
        let left = sizes.first().copied().unwrap_or(0);
        Splitter {
            sizes,
            index: 0,
            left,
            digest: Vec::with_capacity(DIGEST_LEN),
        }
    }

    /// Whether every file and its checksum has arrived.
    pub(super) fn is_done(&self) -> bool {
        self.index >= self.sizes.len()
    }

    /// Adds the next bytes of the stream to `out` as pieces, without
    /// copying them.
    ///
    /// # Errors
    ///
    /// [`ExtraBytes`] if bytes follow the last checksum. The pieces before
    /// them are still in `out`.
    pub(super) fn push(
        &mut self,
        mut bytes: Bytes,
        out: &mut Vec<Piece>,
    ) -> Result<(), ExtraBytes> {
        while !bytes.is_empty() {
            if self.is_done() {
                return Err(ExtraBytes);
            }
            if self.left > 0 {
                let take =
                    usize::try_from(self.left).map_or(bytes.len(), |left| left.min(bytes.len()));
                out.push(Piece::Data(bytes.split_to(take)));
                self.left -= take as u64;
                continue;
            }
            let take = (DIGEST_LEN - self.digest.len()).min(bytes.len());
            self.digest.extend_from_slice(&bytes.split_to(take));
            if let Ok(digest) = Digest::try_from(self.digest.as_slice()) {
                out.push(Piece::End(digest));
                self.digest.clear();
                self.index += 1;
                self.left = self.sizes.get(self.index).copied().unwrap_or(0);
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The stream for files with these contents, and fake checksums
    /// (`[index; 32]`) after each.
    fn stream(contents: &[&[u8]]) -> Vec<u8> {
        let mut stream = Vec::new();
        for (index, content) in contents.iter().enumerate() {
            stream.extend_from_slice(content);
            stream.extend_from_slice(&[index as u8; DIGEST_LEN]);
        }
        stream
    }

    /// Each file's content and checksum, as the splitter gave them.
    type Files = Vec<(Vec<u8>, Digest)>;

    /// Splits `bytes` delivered in pieces of `chunk` bytes, merging the data
    /// pieces back into each file's content.
    fn split(sizes: &[u64], bytes: &[u8], chunk: usize) -> (Files, Result<(), ExtraBytes>) {
        let mut splitter = Splitter::new(sizes.to_vec());
        let mut pieces = Vec::new();
        let mut result = Ok(());
        for part in bytes.chunks(chunk.max(1)) {
            result = splitter.push(Bytes::copy_from_slice(part), &mut pieces);
            if result.is_err() {
                break;
            }
        }
        let mut files = Vec::new();
        let mut content = Vec::new();
        for piece in pieces {
            match piece {
                Piece::Data(bytes) => {
                    assert!(!bytes.is_empty());
                    content.extend_from_slice(&bytes);
                }
                Piece::End(digest) => files.push((std::mem::take(&mut content), digest)),
            }
        }
        assert!(content.is_empty(), "data after the last checksum piece");
        (files, result)
    }

    #[test]
    fn files_and_checksums_come_out_whatever_the_pieces() {
        let contents: [&[u8]; 4] = [b"hello", b"", b"x", &[7u8; 1000]];
        let sizes: Vec<u64> = contents.iter().map(|c| c.len() as u64).collect();
        let bytes = stream(&contents);
        for chunk in [1, 2, 31, 32, 33, 100, 4096] {
            let (files, result) = split(&sizes, &bytes, chunk);
            assert_eq!(result, Ok(()), "chunk {chunk}");
            assert_eq!(files.len(), 4, "chunk {chunk}");
            for (index, (content, digest)) in files.iter().enumerate() {
                assert_eq!(content.as_slice(), contents[index], "chunk {chunk}");
                assert_eq!(*digest, [index as u8; DIGEST_LEN]);
            }
        }
    }

    #[test]
    fn it_is_done_only_after_the_last_checksum() {
        let mut splitter = Splitter::new(vec![2]);
        let mut pieces = Vec::new();
        assert!(!splitter.is_done());
        splitter
            .push(Bytes::from_static(b"ab"), &mut pieces)
            .unwrap();
        assert!(!splitter.is_done(), "the checksum is still missing");
        splitter
            .push(Bytes::from(vec![0u8; 31]), &mut pieces)
            .unwrap();
        assert!(!splitter.is_done());
        splitter
            .push(Bytes::from(vec![0u8; 1]), &mut pieces)
            .unwrap();
        assert!(splitter.is_done());
        assert_eq!(pieces.len(), 2);
        assert!(Splitter::new(Vec::new()).is_done());
    }

    #[test]
    fn bytes_after_the_last_checksum_are_an_error() {
        let contents: [&[u8]; 2] = [b"one", b"two"];
        let mut bytes = stream(&contents);
        bytes.extend_from_slice(b"junk");
        for chunk in [1, 5, 1000] {
            let (files, result) = split(&[3, 3], &bytes, chunk);
            assert_eq!(result, Err(ExtraBytes), "chunk {chunk}");
            assert_eq!(files.len(), 2, "both files came before the junk");
        }
        let mut splitter = Splitter::new(Vec::new());
        assert_eq!(
            splitter.push(Bytes::from_static(b"x"), &mut Vec::new()),
            Err(ExtraBytes)
        );
    }

    #[test]
    fn data_pieces_share_the_received_buffer() {
        let bytes = Bytes::from(stream(&[b"0123456789"]));
        let mut splitter = Splitter::new(vec![10]);
        let mut pieces = Vec::new();
        splitter.push(bytes.clone(), &mut pieces).unwrap();
        let Piece::Data(data) = &pieces[0] else {
            panic!("data first");
        };
        assert_eq!(data.as_ptr(), bytes.as_ptr(), "no copy");
        assert_eq!(format!("{:?}", pieces[0]), "Data(10 bytes)");
    }
}
