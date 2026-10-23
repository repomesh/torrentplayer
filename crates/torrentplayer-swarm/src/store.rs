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
use std::sync::{Arc, PoisonError, RwLock, Weak};

/// Stores verified pieces for one torrent.
#[derive(Debug)]
pub(crate) struct PieceStore {
    file: Arc<CacheFile>,
    /// Where each stored piece starts in the file, and its length.
    index: RwLock<Vec<Option<(u64, u32)>>>,
}

/// The cache file, which instances of the same torrent may share (see [`PieceStore::share`]).
#[derive(Debug)]
pub(crate) struct CacheFile {
    file: File,
    /// Where the next piece is appended.
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
            file: Arc::new(CacheFile {
                file,
                next: AtomicU64::new(0),
            }),
            index: RwLock::new(vec![None; piece_count as usize]),
        })
    }

    /// The cache file, without keeping it open: [`PieceStore::share`] can use it for as long
    /// as any store does.
    pub(crate) fn cache_file(&self) -> Weak<CacheFile> {
        Arc::downgrade(&self.file)
    }

    /// An empty store that appends to `file` (see [`PieceStore::cache_file`]), if a store still
    /// uses it. For a torrent added again while an earlier instance is still in use (a reader
    /// finishing a request): truncating the file under that reader would hand it zeros or
    /// other pieces' bytes. The earlier instances' pieces stay in the file until it is next
    /// created.
    pub(crate) fn share(file: &Weak<CacheFile>, piece_count: u32) -> Option<Self> {
        Some(Self {
            file: file.upgrade()?,
            index: RwLock::new(vec![None; piece_count as usize]),
        })
    }

    /// Appends a verified piece. Blocking: call from a blocking thread.
    pub(crate) fn write_piece(&self, piece: u32, data: &[u8]) -> io::Result<()> {
        let len = u32::try_from(data.len())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "piece too large"))?;
        let at = self.file.next.fetch_add(u64::from(len), Ordering::Relaxed);
        write_all_at(&self.file.file, data, at)?;
        if let Some(slot) = self
            .index
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .get_mut(piece as usize)
        {
            *slot = Some((at, len));
        }
        Ok(())
    }

    /// Reads `buf.len()` bytes starting `offset` bytes into a stored piece. Blocking. Fails
    /// rather than read past the end of the piece, into whichever piece was stored after it.
    pub(crate) fn read(&self, piece: u32, offset: u32, buf: &mut [u8]) -> io::Result<()> {
        let (base, len) = self
            .index
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .get(piece as usize)
            .copied()
            .flatten()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "piece is not stored"))?;
        if u64::from(offset) + buf.len() as u64 > u64::from(len) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "read past the end of the piece",
            ));
        }
        read_exact_at(&self.file.file, buf, base + u64::from(offset))
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

    #[test]
    fn reads_never_cross_into_the_next_stored_piece() {
        let dir = tempfile::tempdir().unwrap();
        let store = PieceStore::create(dir.path(), 4).unwrap();
        store.write_piece(1, b"first").unwrap();
        store.write_piece(2, b"second").unwrap();

        // Piece 2 follows piece 1 in the file: these reads would return "stse" and "rst" + "s".
        let mut buf = [0u8; 4];
        assert_eq!(
            store.read(1, 3, &mut buf).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
        assert_eq!(
            store.read(1, 2, &mut buf).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
        assert!(store.read(1, u32::MAX, &mut buf).is_err());
        store.read(1, 1, &mut buf).unwrap();
        assert_eq!(&buf, b"irst");
        store.read(1, 5, &mut []).unwrap();
    }

    #[test]
    fn a_shared_store_leaves_the_previous_instances_pieces_in_place() {
        let dir = tempfile::tempdir().unwrap();
        let old = PieceStore::create(dir.path(), 4).unwrap();
        old.write_piece(0, b"old piece").unwrap();
        let file = old.cache_file();
        let new = PieceStore::share(&file, 4).unwrap();
        new.write_piece(0, b"new piece").unwrap();

        let mut buf = [0u8; 9];
        old.read(0, 0, &mut buf).unwrap();
        assert_eq!(
            &buf, b"old piece",
            "the old instance still reads its own data"
        );
        new.read(0, 0, &mut buf).unwrap();
        assert_eq!(&buf, b"new piece");
        assert_eq!(
            new.read(1, 0, &mut buf).unwrap_err().kind(),
            io::ErrorKind::NotFound,
            "the new instance starts empty"
        );

        // The file is shared for as long as any store uses it, not only the first.
        drop(old);
        let third = PieceStore::share(&new.cache_file(), 4).unwrap();
        drop(new);
        assert!(PieceStore::share(&file, 4).is_some());
        drop(third);
        assert!(
            PieceStore::share(&file, 4).is_none(),
            "closed with the last store"
        );
    }
}
