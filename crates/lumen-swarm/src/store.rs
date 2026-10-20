// SPDX-License-Identifier: MIT OR Apache-2.0

//! Append-only piece storage.
//!
//! Verified pieces are appended to one cache file in arrival order, with an index from piece to
//! offset. Streaming downloads pieces far out of order (the end of a file for its container
//! index, a seek target), and writing far past the end of an ordinary file makes NTFS zero-fill
//! the gap first: about 0.9 s for a 6 GB offset on a fast SSD, and much longer on a hard drive.
//! Appending avoids that on every file system without sparse-file APIs. See
//! `docs/adr/0006-streaming-engine.md`.

use std::fs::{File, OpenOptions};
use std::io;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{PoisonError, RwLock};

/// Stores verified pieces for one torrent.
#[derive(Debug)]
pub(crate) struct PieceStore {
    file: File,
    index: RwLock<Vec<Option<u64>>>,
    next: AtomicU64,
}

impl PieceStore {
    /// Creates (or truncates) the cache file `dir/pieces.dat`.
    pub(crate) fn create(dir: &Path, piece_count: u32) -> io::Result<Self> {
        std::fs::create_dir_all(dir)?;
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(dir.join("pieces.dat"))?;
        Ok(Self {
            file,
            index: RwLock::new(vec![None; piece_count as usize]),
            next: AtomicU64::new(0),
        })
    }

    /// Appends a verified piece. Blocking: call from a blocking thread.
    pub(crate) fn write_piece(&self, piece: u32, data: &[u8]) -> io::Result<()> {
        let at = self.next.fetch_add(data.len() as u64, Ordering::Relaxed);
        write_all_at(&self.file, data, at)?;
        if let Some(slot) = self
            .index
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .get_mut(piece as usize)
        {
            *slot = Some(at);
        }
        Ok(())
    }

    /// Reads `buf.len()` bytes starting `offset` bytes into a stored piece. Blocking.
    pub(crate) fn read(&self, piece: u32, offset: u32, buf: &mut [u8]) -> io::Result<()> {
        let base = self
            .index
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .get(piece as usize)
            .copied()
            .flatten()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "piece is not stored"))?;
        read_exact_at(&self.file, buf, base + u64::from(offset))
    }
}

#[cfg(unix)]
fn write_all_at(file: &File, buf: &[u8], offset: u64) -> io::Result<()> {
    use std::os::unix::fs::FileExt as _;
    file.write_all_at(buf, offset)
}

#[cfg(unix)]
fn read_exact_at(file: &File, buf: &mut [u8], offset: u64) -> io::Result<()> {
    use std::os::unix::fs::FileExt as _;
    file.read_exact_at(buf, offset)
}

#[cfg(windows)]
fn write_all_at(file: &File, mut buf: &[u8], mut offset: u64) -> io::Result<()> {
    use std::os::windows::fs::FileExt as _;
    while !buf.is_empty() {
        let n = file.seek_write(buf, offset)?;
        if n == 0 {
            return Err(io::ErrorKind::WriteZero.into());
        }
        buf = &buf[n..];
        offset += n as u64;
    }
    Ok(())
}

#[cfg(windows)]
fn read_exact_at(file: &File, mut buf: &mut [u8], mut offset: u64) -> io::Result<()> {
    use std::os::windows::fs::FileExt as _;
    while !buf.is_empty() {
        let n = file.seek_read(buf, offset)?;
        if n == 0 {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        buf = &mut buf[n..];
        offset += n as u64;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stores_out_of_order_pieces() {
        let dir = tempfile::tempdir().unwrap();
        let store = PieceStore::create(dir.path(), 4).unwrap();
        store.write_piece(3, b"third piece").unwrap();
        store.write_piece(0, b"zeroth").unwrap();

        let mut buf = [0u8; 5];
        store.read(3, 6, &mut buf).unwrap();
        assert_eq!(&buf, b"piece");
        store.read(0, 0, &mut buf).unwrap();
        assert_eq!(&buf, b"zerot");
        assert_eq!(
            store.read(1, 0, &mut buf).unwrap_err().kind(),
            io::ErrorKind::NotFound
        );
        assert!(
            store.read(0, 4, &mut buf).is_err(),
            "reading past the piece fails"
        );
        assert!(store.read(99, 0, &mut buf).is_err());
    }
}
