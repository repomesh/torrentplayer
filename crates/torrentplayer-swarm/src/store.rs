// SPDX-License-Identifier: MIT OR Apache-2.0

//! Append-only piece storage that survives restarts.
//!
//! Verified pieces are appended to one cache file in arrival order, with an index from piece to
//! offset. Streaming downloads pieces far out of order (the end of a file for its container
//! index, a seek target), and writing far past the end of an ordinary file makes NTFS zero-fill
//! the gap first: about 0.9 s for a 6 GB offset on a fast SSD, and much longer on a hard drive.
//! Appending avoids that on every file system without sparse-file APIs. See
//! `docs/adr/0006-streaming-engine.md`.
//!
//! The index is also appended to `pieces.idx` next to `pieces.dat`, one fixed-size record per
//! stored piece after a header naming the format and the torrent, so a later session can use
//! the pieces again. Nothing is flushed on purpose: after a crash or power loss either file may
//! end early or hold garbage, so the index read back only says where a piece may be. Records
//! that are torn, fail their checksum, or point past the end of `pieces.dat` are skipped, and
//! every piece found is still unverified: it cannot be read until [`PieceStore::verify`] has
//! checked it against the torrent's SHA-1.

use std::fs::{File, OpenOptions};
use std::io;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, PoisonError, RwLock, RwLockReadGuard, RwLockWriteGuard, Weak};

use sha1::Digest as _;

use crate::metainfo::Metainfo;

/// The cache file.
const DATA_FILE: &str = "pieces.dat";
/// The cache file's index.
const INDEX_FILE: &str = "pieces.idx";
/// Starts `pieces.idx`.
const INDEX_MAGIC: &[u8; 8] = b"TPPIECES";
/// The format of `pieces.idx`. An index in another format is not read: the cache starts empty.
const INDEX_VERSION: u32 = 1;
/// Magic, version, piece count, info hash.
const HEADER_LEN: usize = 8 + 4 + 4 + 20;
/// Piece, length, offset, checksum.
const RECORD_LEN: usize = 4 + 4 + 8 + 8;
/// An index holding more records than this per piece is not read (the cache starts empty).
/// Each record stands for bytes appended to `pieces.dat`, and the cap on unused bytes there
/// (see [`PieceStore::open`]) keeps an honest index at about two per piece.
const MAX_RECORDS_PER_PIECE: u64 = 4;

/// Stores verified pieces for one torrent.
#[derive(Debug)]
pub(crate) struct PieceStore {
    file: Arc<CacheFile>,
}

/// The cache file and its index, which instances of the same torrent share (see
/// [`PieceStore::share`]).
#[derive(Debug)]
pub(crate) struct CacheFile {
    file: File,
    /// Where the next piece is appended.
    next: AtomicU64,
    /// `pieces.idx`.
    index_file: File,
    /// Where the next index record is written.
    next_record: AtomicU64,
    info_hash: [u8; 20],
    /// Where each stored piece is.
    index: RwLock<Vec<Option<Slot>>>,
}

/// Where a stored piece is in the cache file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Slot {
    at: u64,
    len: u32,
    /// Checked against the torrent's hash in this session. Only verified pieces are read.
    verified: bool,
}

impl PieceStore {
    /// Opens the cache in `dir`, keeping the pieces an earlier session stored there for `meta`
    /// (unverified: see [`PieceStore::verify`]). Starts empty, truncating `pieces.dat`, without
    /// a usable index: none (a cache from before the index existed), one in another format or
    /// for another torrent, or a cache file with more unused bytes than the whole torrent
    /// (pieces stored again and again over many sessions: rare enough not to be worth
    /// compacting). Blocking; reads only the index.
    pub(crate) fn open(dir: &Path, meta: &Metainfo) -> io::Result<Self> {
        std::fs::create_dir_all(dir)?;
        let header = header(meta);
        let index_path = dir.join(INDEX_FILE);
        let records = std::fs::read(&index_path)
            .ok()
            .and_then(|bytes| parse_index(&bytes, &header, meta));
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(dir.join(DATA_FILE))?;
        let data_len = file.metadata()?.len();
        let kept = records.and_then(|(slots, count)| {
            let slots: Vec<Option<Slot>> = slots
                .into_iter()
                .map(|slot| slot.filter(|s| s.at + u64::from(s.len) <= data_len))
                .collect();
            let used: u64 = slots.iter().flatten().map(|s| u64::from(s.len)).sum();
            (data_len.saturating_sub(used) <= meta.total_len).then_some((slots, count))
        });
        let (slots, next, next_record, index_file) = if let Some((slots, count)) = kept {
            let index_file = OpenOptions::new()
                .read(true)
                .write(true)
                .open(&index_path)?;
            // After the last whole record: a torn one is overwritten.
            let next_record = (HEADER_LEN + count * RECORD_LEN) as u64;
            (slots, data_len, next_record, index_file)
        } else {
            // The index first: a crash before the cache file is truncated leaves an empty
            // index, never records pointing into a new cache file.
            let index_file = OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(true)
                .open(&index_path)?;
            write_all_at(&index_file, &header, 0)?;
            file.set_len(0)?;
            let slots = vec![None; meta.piece_count() as usize];
            (slots, 0, HEADER_LEN as u64, index_file)
        };
        Ok(Self {
            file: Arc::new(CacheFile {
                file,
                next: AtomicU64::new(next),
                index_file,
                next_record: AtomicU64::new(next_record),
                info_hash: meta.info_hash,
                index: RwLock::new(slots),
            }),
        })
    }

    /// The cache file, without keeping it open: [`PieceStore::share`] can use it for as long
    /// as any store does.
    pub(crate) fn cache_file(&self) -> Weak<CacheFile> {
        Arc::downgrade(&self.file)
    }

    /// A store on `file` (see [`PieceStore::cache_file`]), if a store still uses it. For a
    /// torrent added again while an earlier instance is still in use (a reader finishing a
    /// request): opening the cache again could truncate it under that reader. The stores share
    /// the index too, so pieces either verifies are the other's as well.
    pub(crate) fn share(file: &Weak<CacheFile>) -> Option<Self> {
        Some(Self {
            file: file.upgrade()?,
        })
    }

    /// The stored pieces in their order in the cache file, and whether each is verified.
    pub(crate) fn stored(&self) -> Vec<(u32, bool)> {
        let index = self.file.index();
        let mut stored: Vec<(u64, u32, bool)> = index
            .iter()
            .enumerate()
            .filter_map(|(piece, slot)| {
                let slot = slot.as_ref()?;
                Some((slot.at, u32::try_from(piece).ok()?, slot.verified))
            })
            .collect();
        drop(index);
        stored.sort_unstable();
        stored.into_iter().map(|(_, p, v)| (p, v)).collect()
    }

    /// Checks a stored piece against `expected` (its SHA-1), so that it can be read. Returns
    /// whether it is stored and verified; a piece that does not match (or cannot be read) is
    /// forgotten. Blocking: call from a blocking thread.
    pub(crate) fn verify(&self, piece: u32, expected: &[u8; 20]) -> bool {
        let Some(slot) = self.file.slot(piece) else {
            return false;
        };
        if slot.verified {
            return true;
        }
        let mut data = vec![0; slot.len as usize];
        let ok = match read_exact_at(&self.file.file, &mut data, slot.at) {
            Ok(()) => sha1::Sha1::digest(&data).as_slice() == expected,
            Err(error) => {
                tracing::debug!(%error, piece, "could not read a stored piece");
                false
            }
        };
        let mut index = self.file.index_mut();
        // Unless the piece was stored again meanwhile.
        if let Some(current) = index.get_mut(piece as usize)
            && *current == Some(slot)
        {
            *current = ok.then_some(Slot {
                verified: true,
                ..slot
            });
        }
        ok
    }

    /// Appends a verified piece. Blocking: call from a blocking thread.
    pub(crate) fn write_piece(&self, piece: u32, data: &[u8]) -> io::Result<()> {
        let len = u32::try_from(data.len())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "piece too large"))?;
        let at = self.file.next.fetch_add(u64::from(len), Ordering::Relaxed);
        write_all_at(&self.file.file, data, at)?;
        if let Some(slot) = self.file.index_mut().get_mut(piece as usize) {
            *slot = Some(Slot {
                at,
                len,
                verified: true,
            });
        }
        let record = record(&self.file.info_hash, piece, len, at);
        let record_at = self
            .file
            .next_record
            .fetch_add(RECORD_LEN as u64, Ordering::Relaxed);
        if let Err(error) = write_all_at(&self.file.index_file, &record, record_at) {
            // Stored for this session all the same; the next one downloads it again.
            tracing::warn!(%error, piece, "could not index a stored piece");
        }
        Ok(())
    }

    /// Reads `buf.len()` bytes starting `offset` bytes into a stored piece. Blocking. Fails
    /// rather than read past the end of the piece, into whichever piece was stored after it,
    /// and for a piece kept from an earlier session that is not verified yet.
    pub(crate) fn read(&self, piece: u32, offset: u32, buf: &mut [u8]) -> io::Result<()> {
        let Slot { at, len, .. } = self
            .file
            .slot(piece)
            .filter(|s| s.verified)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "piece is not stored"))?;
        if u64::from(offset) + buf.len() as u64 > u64::from(len) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "read past the end of the piece",
            ));
        }
        read_exact_at(&self.file.file, buf, at + u64::from(offset))
    }
}

impl CacheFile {
    fn index(&self) -> RwLockReadGuard<'_, Vec<Option<Slot>>> {
        self.index.read().unwrap_or_else(PoisonError::into_inner)
    }

    fn index_mut(&self) -> RwLockWriteGuard<'_, Vec<Option<Slot>>> {
        self.index.write().unwrap_or_else(PoisonError::into_inner)
    }

    fn slot(&self, piece: u32) -> Option<Slot> {
        self.index().get(piece as usize).copied().flatten()
    }
}

/// The header `pieces.idx` starts with for `meta`.
fn header(meta: &Metainfo) -> [u8; HEADER_LEN] {
    let mut header = [0; HEADER_LEN];
    header[..8].copy_from_slice(INDEX_MAGIC);
    header[8..12].copy_from_slice(&INDEX_VERSION.to_le_bytes());
    header[12..16].copy_from_slice(&meta.piece_count().to_le_bytes());
    header[16..].copy_from_slice(&meta.info_hash);
    header
}

/// An index record: piece, length, and offset (little-endian), then the first 8 bytes of the
/// SHA-1 of the info hash and those fields, which catches torn and stray records.
fn record(info_hash: &[u8; 20], piece: u32, len: u32, at: u64) -> [u8; RECORD_LEN] {
    let mut record = [0; RECORD_LEN];
    record[..4].copy_from_slice(&piece.to_le_bytes());
    record[4..8].copy_from_slice(&len.to_le_bytes());
    record[8..16].copy_from_slice(&at.to_le_bytes());
    let check = record_check(info_hash, &record);
    record[16..].copy_from_slice(&check);
    record
}

fn record_check(info_hash: &[u8; 20], record: &[u8; RECORD_LEN]) -> [u8; 8] {
    let digest = sha1::Sha1::new()
        .chain_update(info_hash)
        .chain_update(&record[..16])
        .finalize();
    let mut check = [0; 8];
    check.copy_from_slice(&digest[..8]);
    check
}

/// Reads `pieces.idx`: where each piece was stored (the latest record wins), and how many whole
/// records there are. `None` if it does not start with `header` or is too large. Records that
/// fail their checksum, name a piece the torrent does not have, or give a length other than
/// the piece's are skipped; a torn last record is ignored.
fn parse_index(
    bytes: &[u8],
    header: &[u8; HEADER_LEN],
    meta: &Metainfo,
) -> Option<(Vec<Option<Slot>>, usize)> {
    let records = bytes.strip_prefix(header.as_slice())?;
    let max = u64::from(meta.piece_count()) * MAX_RECORDS_PER_PIECE + 64;
    if (records.len() / RECORD_LEN) as u64 > max {
        return None;
    }
    let mut slots = vec![None; meta.piece_count() as usize];
    let chunks = records.chunks_exact(RECORD_LEN);
    let count = chunks.len();
    for chunk in chunks {
        let Ok(r) = <[u8; RECORD_LEN]>::try_from(chunk) else {
            continue;
        };
        let piece = u32::from_le_bytes([r[0], r[1], r[2], r[3]]);
        let len = u32::from_le_bytes([r[4], r[5], r[6], r[7]]);
        let at = u64::from_le_bytes([r[8], r[9], r[10], r[11], r[12], r[13], r[14], r[15]]);
        if r[16..] != record_check(&meta.info_hash, &r)
            || len == 0
            || piece >= meta.piece_count()
            || len != meta.piece_size(piece)
            || at.checked_add(u64::from(len)).is_none()
        {
            continue;
        }
        if let Some(slot) = slots.get_mut(piece as usize) {
            *slot = Some(Slot {
                at,
                len,
                verified: false,
            });
        }
    }
    Some((slots, count))
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
pub(crate) mod tests {
    use super::*;

    /// A single-file torrent of `pieces`, each `piece_len` long but the last.
    pub(crate) fn meta(pieces: &[&[u8]], piece_len: u32) -> Metainfo {
        let total: usize = pieces.iter().map(|p| p.len()).sum();
        let mut bytes = format!(
            "d4:infod6:lengthi{total}e4:name5:a.mp412:piece lengthi{piece_len}e6:pieces{}:",
            pieces.len() * 20
        )
        .into_bytes();
        for piece in pieces {
            bytes.extend_from_slice(&sha1::Sha1::digest(piece));
        }
        bytes.extend_from_slice(b"ee");
        Metainfo::from_bytes(&bytes).unwrap()
    }

    const PIECES: [&[u8]; 4] = [b"zeroth..", b"first...", b"second..", b"thd"];

    fn verify_all(store: &PieceStore, meta: &Metainfo) -> Vec<u32> {
        (0..meta.piece_count())
            .filter(|&p| store.verify(p, meta.piece_hash(p).unwrap()))
            .collect()
    }

    #[test]
    fn stores_out_of_order_pieces() {
        let dir = tempfile::tempdir().unwrap();
        let meta = meta(&PIECES, 8);
        let store = PieceStore::open(dir.path(), &meta).unwrap();
        store.write_piece(3, b"thd").unwrap();
        store.write_piece(0, b"zeroth..").unwrap();

        let mut buf = [0u8; 2];
        store.read(3, 1, &mut buf).unwrap();
        assert_eq!(&buf, b"hd");
        let mut buf = [0u8; 5];
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
        assert_eq!(store.stored(), vec![(3, true), (0, true)]);
    }

    #[test]
    fn reads_never_cross_into_the_next_stored_piece() {
        let dir = tempfile::tempdir().unwrap();
        let meta = meta(&PIECES, 8);
        let store = PieceStore::open(dir.path(), &meta).unwrap();
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
    fn a_shared_store_shares_the_pieces_of_every_instance() {
        let dir = tempfile::tempdir().unwrap();
        let meta = meta(&PIECES, 8);
        let old = PieceStore::open(dir.path(), &meta).unwrap();
        old.write_piece(0, PIECES[0]).unwrap();
        let file = old.cache_file();
        let new = PieceStore::share(&file).unwrap();
        assert_eq!(new.stored(), vec![(0, true)], "verified in this session");
        new.write_piece(1, PIECES[1]).unwrap();

        let mut buf = [0u8; 8];
        old.read(1, 0, &mut buf).unwrap();
        assert_eq!(&buf, PIECES[1]);
        new.read(0, 0, &mut buf).unwrap();
        assert_eq!(&buf, PIECES[0]);

        // The file is shared for as long as any store uses it, not only the first.
        drop(old);
        let third = PieceStore::share(&new.cache_file()).unwrap();
        drop(new);
        assert!(PieceStore::share(&file).is_some());
        drop(third);
        assert!(
            PieceStore::share(&file).is_none(),
            "closed with the last store"
        );
    }

    #[test]
    fn pieces_are_kept_across_sessions_but_read_only_once_verified() {
        let dir = tempfile::tempdir().unwrap();
        let meta = meta(&PIECES, 8);
        let store = PieceStore::open(dir.path(), &meta).unwrap();
        store.write_piece(2, PIECES[2]).unwrap();
        store.write_piece(3, PIECES[3]).unwrap();
        store.write_piece(0, PIECES[0]).unwrap();
        drop(store);

        let store = PieceStore::open(dir.path(), &meta).unwrap();
        assert_eq!(store.stored(), vec![(2, false), (3, false), (0, false)]);
        let mut buf = [0u8; 3];
        assert_eq!(
            store.read(2, 0, &mut buf).unwrap_err().kind(),
            io::ErrorKind::NotFound,
            "not served before it is verified"
        );
        assert_eq!(verify_all(&store, &meta), vec![0, 2, 3]);
        store.read(3, 0, &mut buf).unwrap();
        assert_eq!(&buf, b"thd");

        // New pieces are appended after the kept ones, and kept too.
        store.write_piece(1, PIECES[1]).unwrap();
        drop(store);
        let store = PieceStore::open(dir.path(), &meta).unwrap();
        assert_eq!(verify_all(&store, &meta), vec![0, 1, 2, 3]);
        let mut buf = [0u8; 8];
        store.read(1, 0, &mut buf).unwrap();
        assert_eq!(&buf, PIECES[1]);
        store.read(2, 0, &mut buf).unwrap();
        assert_eq!(&buf, PIECES[2]);
    }

    #[test]
    fn a_torn_or_corrupt_index_tail_is_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let meta = meta(&PIECES, 8);
        let index = dir.path().join(INDEX_FILE);
        let store = PieceStore::open(dir.path(), &meta).unwrap();
        store.write_piece(0, PIECES[0]).unwrap();
        store.write_piece(1, PIECES[1]).unwrap();
        drop(store);

        // A torn third record.
        let mut bytes = std::fs::read(&index).unwrap();
        bytes.extend_from_slice(&record(&meta.info_hash, 2, 8, 16)[..10]);
        std::fs::write(&index, &bytes).unwrap();
        let store = PieceStore::open(dir.path(), &meta).unwrap();
        assert_eq!(store.stored(), vec![(0, false), (1, false)]);
        // The next record replaces it.
        store.write_piece(2, PIECES[2]).unwrap();
        drop(store);
        assert_eq!(
            std::fs::metadata(&index).unwrap().len(),
            (HEADER_LEN + 3 * RECORD_LEN) as u64
        );
        let store = PieceStore::open(dir.path(), &meta).unwrap();
        assert_eq!(verify_all(&store, &meta), vec![0, 1, 2]);
        drop(store);

        // A record whose bytes changed fails its checksum; one past the end of the cache file
        // (its piece never written) is skipped too.
        let mut bytes = std::fs::read(&index).unwrap();
        bytes[HEADER_LEN + RECORD_LEN + 8] ^= 1;
        bytes.extend_from_slice(&record(&meta.info_hash, 3, 3, 24));
        std::fs::write(&index, &bytes).unwrap();
        let store = PieceStore::open(dir.path(), &meta).unwrap();
        assert_eq!(store.stored(), vec![(0, false), (2, false)]);
        drop(store);

        // So is a record for another torrent, a piece out of range, or the wrong length.
        let mut bytes = std::fs::read(&index).unwrap();
        bytes.extend_from_slice(&record(&[7; 20], 1, 8, 8));
        bytes.extend_from_slice(&record(&meta.info_hash, 9, 8, 8));
        bytes.extend_from_slice(&record(&meta.info_hash, 1, 7, 8));
        std::fs::write(&index, &bytes).unwrap();
        let store = PieceStore::open(dir.path(), &meta).unwrap();
        assert_eq!(store.stored(), vec![(0, false), (2, false)]);
    }

    #[test]
    fn a_corrupted_piece_is_not_trusted() {
        let dir = tempfile::tempdir().unwrap();
        let meta = meta(&PIECES, 8);
        let store = PieceStore::open(dir.path(), &meta).unwrap();
        store.write_piece(0, PIECES[0]).unwrap();
        store.write_piece(1, PIECES[1]).unwrap();
        drop(store);
        let data = dir.path().join(DATA_FILE);
        let mut bytes = std::fs::read(&data).unwrap();
        bytes[9] ^= 0xff;
        std::fs::write(&data, &bytes).unwrap();

        let store = PieceStore::open(dir.path(), &meta).unwrap();
        assert_eq!(verify_all(&store, &meta), vec![0]);
        assert_eq!(store.stored(), vec![(0, true)], "the bad copy is forgotten");
        let mut buf = [0u8; 8];
        assert!(store.read(1, 0, &mut buf).is_err());
        store.write_piece(1, PIECES[1]).unwrap();
        store.read(1, 0, &mut buf).unwrap();
        assert_eq!(&buf, PIECES[1]);
    }

    #[test]
    fn a_cache_without_a_usable_index_starts_empty() {
        let dir = tempfile::tempdir().unwrap();
        let meta = meta(&PIECES, 8);
        let data = dir.path().join(DATA_FILE);
        let index = dir.path().join(INDEX_FILE);

        // A cache file from before the index existed.
        std::fs::write(&data, b"zeroth..first...").unwrap();
        let store = PieceStore::open(dir.path(), &meta).unwrap();
        assert!(store.stored().is_empty());
        assert_eq!(std::fs::metadata(&data).unwrap().len(), 0, "truncated");
        store.write_piece(0, PIECES[0]).unwrap();
        drop(store);

        // Another format version, or another torrent's index.
        for (at, flip) in [(8, 3), (20, 0xff)] {
            let mut bytes = std::fs::read(&index).unwrap();
            bytes[at] ^= flip;
            std::fs::write(&index, &bytes).unwrap();
            let store = PieceStore::open(dir.path(), &meta).unwrap();
            assert!(store.stored().is_empty());
            assert_eq!(std::fs::metadata(&data).unwrap().len(), 0);
            store.write_piece(0, PIECES[0]).unwrap();
        }

        // A torn header.
        std::fs::write(&index, &header(&meta)[..10]).unwrap();
        let store = PieceStore::open(dir.path(), &meta).unwrap();
        assert!(store.stored().is_empty());
    }

    #[test]
    fn a_cache_file_mostly_unused_starts_empty() {
        let dir = tempfile::tempdir().unwrap();
        let meta = meta(&PIECES, 8);
        let data = dir.path().join(DATA_FILE);
        let store = PieceStore::open(dir.path(), &meta).unwrap();
        // Stored again and again: 8 more unused bytes each time.
        for _ in 0..4 {
            store.write_piece(0, PIECES[0]).unwrap();
        }
        drop(store);
        let store = PieceStore::open(dir.path(), &meta).unwrap();
        assert_eq!(
            store.stored(),
            vec![(0, false)],
            "24 unused bytes, 27 allowed"
        );
        store.write_piece(0, PIECES[0]).unwrap();
        drop(store);
        let store = PieceStore::open(dir.path(), &meta).unwrap();
        assert!(store.stored().is_empty(), "32 unused bytes");
        assert_eq!(std::fs::metadata(&data).unwrap().len(), 0);
    }

    #[test]
    fn removing_the_folder_removes_the_cache_and_its_index() {
        let dir = tempfile::tempdir().unwrap();
        let cache = dir.path().join("hash");
        let meta = meta(&PIECES, 8);
        let store = PieceStore::open(&cache, &meta).unwrap();
        store.write_piece(0, PIECES[0]).unwrap();
        drop(store);
        let mut names: Vec<_> = std::fs::read_dir(&cache)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        names.sort();
        assert_eq!(names, [DATA_FILE, INDEX_FILE]);
        std::fs::remove_dir_all(&cache).unwrap();
        let store = PieceStore::open(&cache, &meta).unwrap();
        assert!(store.stored().is_empty());
    }
}
