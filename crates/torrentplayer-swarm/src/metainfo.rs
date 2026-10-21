// SPDX-License-Identifier: MIT OR Apache-2.0

//! Parsed `.torrent` metadata (BitTorrent v1).

use std::borrow::Cow;

use librqbit_core::torrent_metainfo::torrent_from_bytes;
use url::Url;

use crate::bencode::{self, Value};

/// Size of a request block. Every client uses 16 KiB.
pub const BLOCK_LEN: u32 = 16 * 1024;

/// Errors parsing a `.torrent` file.
#[derive(Debug, thiserror::Error)]
pub enum MetainfoError {
    /// The bencode is malformed or required fields are missing.
    #[error("the .torrent file is malformed: {0}")]
    Malformed(String),
    /// The metadata is internally inconsistent (for example, the piece hashes do not match the
    /// total length).
    #[error("the .torrent file is invalid: {0}")]
    Invalid(String),
}

/// One file inside a torrent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MetaFile {
    /// Path components inside the torrent. Callers must validate them before using them as a
    /// file system path; `torrentplayer-swarm` never does, because it stores pieces, not files.
    pub path: Vec<String>,
    /// Length in bytes.
    pub len: u64,
    /// Absolute offset of the file's first byte in the torrent.
    pub offset: u64,
    /// A BEP 47 padding file, which never needs downloading.
    pub padding: bool,
}

/// The parts of a `.torrent` file the engine needs.
#[derive(Debug, Clone)]
pub struct Metainfo {
    /// SHA-1 of the bencoded info dictionary.
    pub info_hash: [u8; 20],
    /// Display name.
    pub name: String,
    /// Length of every piece except possibly the last.
    pub piece_len: u32,
    /// Total length of all files.
    pub total_len: u64,
    /// Files in torrent order.
    pub files: Vec<MetaFile>,
    /// Announce URLs (http, https, udp), deduplicated, in tier order.
    pub trackers: Vec<Url>,
    /// Web seeds (BEP 19 `url-list`): http and https URLs without credentials, deduplicated,
    /// at most [`MAX_WEB_SEEDS`].
    pub web_seeds: Vec<Url>,
    /// Whether the torrent has a file list (and so a top-level folder named [`Metainfo::name`])
    /// rather than a single file.
    pub multi_file: bool,
    piece_hashes: Vec<[u8; 20]>,
}

/// Most web seeds kept from a `.torrent` file.
pub const MAX_WEB_SEEDS: usize = 8;

impl Metainfo {
    /// Parses and validates a `.torrent` file.
    ///
    /// # Errors
    ///
    /// Returns an error if the data is not a valid v1 torrent.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, MetainfoError> {
        let meta =
            torrent_from_bytes(bytes).map_err(|e| MetainfoError::Malformed(format!("{e:#}")))?;
        let mut trackers = Vec::new();
        for raw in meta.iter_announce() {
            if let Some(url) = std::str::from_utf8(raw.as_ref())
                .ok()
                .and_then(|s| Url::parse(s).ok())
                .filter(|u| matches!(u.scheme(), "http" | "https" | "udp"))
                && !trackers.contains(&url)
            {
                trackers.push(url);
            }
        }
        let info_hash = meta.info_hash.0;
        let info = meta
            .info
            .data
            .validate()
            .map_err(|e| MetainfoError::Invalid(format!("{e:#}")))?;
        let lengths = info.lengths();
        let piece_len = lengths.default_piece_length();
        let total_len = lengths.total_length();
        let hashes = info.info().pieces.as_ref();
        if hashes.len() % 20 != 0 {
            return Err(MetainfoError::Invalid("piece hashes are truncated".into()));
        }
        let mut piece_hashes: Vec<[u8; 20]> = hashes
            .chunks_exact(20)
            .map(|c| {
                let mut h = [0u8; 20];
                h.copy_from_slice(c);
                h
            })
            .collect();
        let expected = total_len.div_ceil(u64::from(piece_len));
        // librqbit 9 create_torrent adds one extra hash when the length is an exact multiple of
        // the piece length; extra trailing hashes are harmless and ignored. Too few is an error.
        if piece_hashes.len() as u64 > expected {
            piece_hashes.truncate(usize::try_from(expected).unwrap_or(usize::MAX));
        }
        if piece_hashes.len() as u64 != expected {
            return Err(MetainfoError::Invalid(format!(
                "{} piece hashes for {total_len} bytes in {piece_len}-byte pieces (expected {expected})",
                piece_hashes.len()
            )));
        }
        let multi_file = info.info().files.is_some();
        let files = info
            .iter_file_details_ext()
            .map(|f| MetaFile {
                path: f
                    .details
                    .filename
                    .iter_components()
                    .map(Cow::into_owned)
                    .collect(),
                len: f.details.len,
                offset: f.offset,
                padding: f.details.attrs().padding,
            })
            .collect();
        let name = info.name().map_or_else(|| hex(&info_hash), Cow::into_owned);
        Ok(Self {
            info_hash,
            name,
            piece_len,
            total_len,
            files,
            trackers,
            web_seeds: web_seeds(bytes),
            multi_file,
            piece_hashes,
        })
    }

    /// Number of pieces.
    #[must_use]
    pub fn piece_count(&self) -> u32 {
        u32::try_from(self.piece_hashes.len()).unwrap_or(u32::MAX)
    }

    /// Absolute offset of `piece`.
    #[must_use]
    pub fn piece_offset(&self, piece: u32) -> u64 {
        u64::from(piece) * u64::from(self.piece_len)
    }

    /// Length of `piece` (the last piece may be shorter).
    #[must_use]
    pub fn piece_size(&self, piece: u32) -> u32 {
        let start = self.piece_offset(piece);
        let len = self
            .total_len
            .saturating_sub(start)
            .min(u64::from(self.piece_len));
        u32::try_from(len).unwrap_or(0)
    }

    /// Number of 16 KiB blocks in `piece`.
    #[must_use]
    pub fn blocks_in(&self, piece: u32) -> u32 {
        self.piece_size(piece).div_ceil(BLOCK_LEN)
    }

    /// Expected SHA-1 of `piece`.
    #[must_use]
    pub fn piece_hash(&self, piece: u32) -> Option<&[u8; 20]> {
        self.piece_hashes.get(piece as usize)
    }

    /// The piece containing absolute offset `abs` (clamped to the last piece).
    #[must_use]
    pub fn piece_at(&self, abs: u64) -> u32 {
        let last = self.piece_count().saturating_sub(1);
        u32::try_from(abs / u64::from(self.piece_len)).map_or(last, |p| p.min(last))
    }

    /// Pieces overlapping the absolute byte range `start..end`.
    #[must_use]
    pub fn pieces_in(&self, start: u64, end: u64) -> std::ops::Range<u32> {
        let end = end.min(self.total_len);
        if start >= end {
            return 0..0;
        }
        self.piece_at(start)..self.piece_at(end - 1) + 1
    }

    /// The hex info hash.
    #[must_use]
    pub fn info_hash_hex(&self) -> String {
        hex(&self.info_hash)
    }
}

/// The torrent's `url-list` (a string or a list of strings). The torrent itself was already
/// validated, so anything unexpected here just means no web seeds.
fn web_seeds(torrent: &[u8]) -> Vec<Url> {
    let Ok(entries) = bencode::dict_entries(torrent) else {
        return Vec::new();
    };
    let Some(raw) = entries
        .iter()
        .find(|(k, _)| *k == b"url-list")
        .map(|(_, v)| *v)
    else {
        return Vec::new();
    };
    let Ok(value) = bencode::decode(raw) else {
        return Vec::new();
    };
    let candidates: Vec<&[u8]> = match &value {
        Value::Bytes(b) => vec![b],
        Value::List(items) => items.iter().filter_map(Value::as_bytes).collect(),
        _ => Vec::new(),
    };
    let mut urls: Vec<Url> = Vec::new();
    for raw in candidates {
        if let Some(url) = std::str::from_utf8(raw)
            .ok()
            .and_then(|s| Url::parse(s.trim()).ok())
            .filter(|u| matches!(u.scheme(), "http" | "https"))
            .filter(|u| u.host().is_some() && u.username().is_empty() && u.password().is_none())
            && !urls.contains(&url)
            && urls.len() < MAX_WEB_SEEDS
        {
            urls.push(url);
        }
    }
    urls
}

fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push(char::from(DIGITS[usize::from(b >> 4)]));
        s.push(char::from(DIGITS[usize::from(b & 0x0f)]));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A single-file torrent of `len` bytes with zeroed hashes.
    fn torrent(len: u64, piece_len: u32, extra: &str) -> Vec<u8> {
        let pieces = len.div_ceil(u64::from(piece_len));
        let mut t = format!(
            "d{extra}4:infod6:lengthi{len}e4:name6:a.webm12:piece lengthi{piece_len}e6:pieces{}:",
            pieces * 20
        )
        .into_bytes();
        t.extend(std::iter::repeat_n(
            0u8,
            usize::try_from(pieces * 20).unwrap(),
        ));
        t.extend_from_slice(b"ee");
        t
    }

    #[test]
    fn parses_geometry() {
        let m = Metainfo::from_bytes(&torrent(40_000, 16_384, "")).unwrap();
        assert_eq!(m.name, "a.webm");
        assert_eq!(m.piece_count(), 3);
        assert_eq!(m.piece_size(0), 16_384);
        assert_eq!(m.piece_size(2), 40_000 - 2 * 16_384);
        assert_eq!(m.blocks_in(2), 1);
        assert_eq!(m.piece_at(16_384), 1);
        assert_eq!(m.piece_at(1 << 40), 2);
        assert_eq!(m.pieces_in(16_000, 17_000), 0..2);
        assert_eq!(m.pieces_in(50_000, 60_000), 0..0);
        assert_eq!(m.files.len(), 1);
        assert_eq!(m.files[0].offset, 0);
        assert_eq!(m.info_hash_hex().len(), 40);
    }

    #[test]
    fn keeps_only_supported_tracker_schemes() {
        let extra = "8:announce30:udp://tracker.example:1337/ann13:announce-listll25:http://a.example/announceel19:wss://b.example/annee";
        let m = Metainfo::from_bytes(&torrent(10, 16_384, extra)).unwrap();
        let urls: Vec<String> = m.trackers.iter().map(ToString::to_string).collect();
        assert_eq!(urls, ["http://a.example/announce"]);
    }

    #[test]
    fn parses_web_seeds() {
        // A list: credentials, other schemes, duplicates, and junk are dropped.
        let t = torrent(10, 16_384, "");
        let list = b"8:url-listl23:https://a.example/file/23:https://a.example/file/20:ftp://b.example/file25:http://u:p@c.example/filei7e7:garbageee";
        let mut with_list = t[..t.len() - 1].to_vec();
        with_list.extend_from_slice(&list[..]);
        let m = Metainfo::from_bytes(&with_list).unwrap();
        let urls: Vec<String> = m.web_seeds.iter().map(ToString::to_string).collect();
        assert_eq!(urls, ["https://a.example/file/"]);

        // A single string.
        let mut with_one = t[..t.len() - 1].to_vec();
        with_one.extend_from_slice(b"8:url-list23:http://d.example/a.webme");
        let m = Metainfo::from_bytes(&with_one).unwrap();
        assert_eq!(m.web_seeds.len(), 1);
        assert_eq!(m.web_seeds[0].host_str(), Some("d.example"));

        // None at all.
        assert!(Metainfo::from_bytes(&t).unwrap().web_seeds.is_empty());
    }

    #[test]
    fn ignores_extra_trailing_hashes() {
        let mut t = torrent(32_768, 16_384, "");
        // Two pieces of data, three hashes.
        let at = t.windows(9).position(|w| w == b"pieces40:").unwrap();
        t.splice(at..at + 9, b"pieces60:".iter().copied());
        t.splice(at + 9..at + 9, [0u8; 20]);
        assert_eq!(Metainfo::from_bytes(&t).unwrap().piece_count(), 2);
    }

    #[test]
    fn rejects_garbage_and_inconsistent_lengths() {
        assert!(Metainfo::from_bytes(b"not bencode").is_err());
        let mut short = torrent(40_000, 16_384, "");
        // Claim a longer file than the hashes cover.
        let pos = short.windows(5).position(|w| w == b"40000").unwrap();
        short[pos..pos + 5].copy_from_slice(b"90000");
        assert!(Metainfo::from_bytes(&short).is_err());
    }
}
