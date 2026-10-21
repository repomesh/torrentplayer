// SPDX-License-Identifier: MIT OR Apache-2.0

//! Mapping between file byte offsets and torrent pieces.

use std::ops::Range;

/// The piece layout of a whole torrent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PieceGeometry {
    piece_len: u64,
    total_len: u64,
}

impl PieceGeometry {
    /// Creates a geometry for a torrent of `total_len` bytes split into `piece_len`-byte pieces.
    ///
    /// Returns `None` if either value is zero, because no valid torrent has that layout.
    #[must_use]
    pub fn new(piece_len: u32, total_len: u64) -> Option<Self> {
        (piece_len > 0 && total_len > 0).then_some(Self {
            piece_len: u64::from(piece_len),
            total_len,
        })
    }

    /// Length of every piece except possibly the last one.
    #[must_use]
    pub const fn piece_len(&self) -> u64 {
        self.piece_len
    }

    /// Total length of the torrent in bytes.
    #[must_use]
    pub const fn total_len(&self) -> u64 {
        self.total_len
    }

    /// Number of pieces in the torrent.
    #[must_use]
    pub const fn piece_count(&self) -> u64 {
        self.total_len.div_ceil(self.piece_len)
    }

    /// Index of the piece that contains the absolute torrent offset `abs`.
    ///
    /// Offsets past the end map to the last piece.
    #[must_use]
    pub fn piece_at(&self, abs: u64) -> u64 {
        (abs.min(self.total_len - 1)) / self.piece_len
    }

    /// Absolute torrent offset where `piece` starts.
    #[must_use]
    pub const fn piece_start(&self, piece: u64) -> u64 {
        piece * self.piece_len
    }

    /// The pieces that overlap the absolute byte range `abs` (empty if the range is empty).
    #[must_use]
    pub fn pieces_overlapping(&self, abs: Range<u64>) -> Range<u64> {
        let end = abs.end.min(self.total_len);
        if abs.start >= end {
            return 0..0;
        }
        self.piece_at(abs.start)..self.piece_at(end - 1) + 1
    }
}

/// Where a file sits inside its torrent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileSpan {
    /// Absolute offset of the file's first byte within the torrent.
    pub offset_in_torrent: u64,
    /// File length in bytes.
    pub len: u64,
}

impl FileSpan {
    /// Converts a file-relative offset to an absolute torrent offset.
    #[must_use]
    pub const fn to_abs(&self, file_offset: u64) -> u64 {
        self.offset_in_torrent + file_offset
    }

    /// Clamps a file-relative range to the file's bounds.
    #[must_use]
    pub fn clamp(&self, range: Range<u64>) -> Range<u64> {
        let start = range.start.min(self.len);
        start..range.end.clamp(start, self.len)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_degenerate_layouts() {
        assert!(PieceGeometry::new(0, 10).is_none());
        assert!(PieceGeometry::new(10, 0).is_none());
    }

    #[test]
    fn counts_partial_last_piece() {
        let g = PieceGeometry::new(16, 40).unwrap();
        assert_eq!(g.piece_count(), 3);
        assert_eq!(g.piece_at(0), 0);
        assert_eq!(g.piece_at(15), 0);
        assert_eq!(g.piece_at(16), 1);
        assert_eq!(g.piece_at(39), 2);
        assert_eq!(g.piece_at(1_000), 2);
    }

    #[test]
    fn overlapping_pieces() {
        let g = PieceGeometry::new(16, 40).unwrap();
        assert_eq!(g.pieces_overlapping(0..0), 0..0);
        assert_eq!(g.pieces_overlapping(0..1), 0..1);
        assert_eq!(g.pieces_overlapping(15..17), 0..2);
        assert_eq!(g.pieces_overlapping(30..1_000), 1..3);
        assert_eq!(g.pieces_overlapping(50..60), 0..0);
    }

    #[test]
    fn file_span_clamps_to_bounds() {
        let f = FileSpan {
            offset_in_torrent: 100,
            len: 50,
        };
        assert_eq!(f.to_abs(10), 110);
        assert_eq!(f.clamp(10..20), 10..20);
        assert_eq!(f.clamp(40..90), 40..50);
        assert_eq!(f.clamp(70..90), 50..50);
    }
}
