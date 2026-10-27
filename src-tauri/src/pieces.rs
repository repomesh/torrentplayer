// SPDX-License-Identifier: MIT OR Apache-2.0

//! The same downloaded-piece map for queue rows and the player's seek bar.

/// Maximum cells in a file's piece map, each 0 (missing) to 255 (fully downloaded).
pub(crate) const PIECE_MAP_CELLS: usize = 200;

/// Downsamples a file's piece range without moving gaps or dropping an unknown tail.
pub(crate) fn piece_map(have: &[bool], range: std::ops::Range<u64>, cells: usize) -> Vec<u8> {
    let start = usize::try_from(range.start).unwrap_or(usize::MAX);
    let end = usize::try_from(range.end).unwrap_or(usize::MAX);
    if start >= end || start >= have.len() || cells == 0 {
        return vec![0; cells.min(1)];
    }
    let count = end - start;
    let cells = cells.min(count);
    (0..cells)
        .map(|cell| {
            let lo = start + cell * count / cells;
            let hi = start + (cell + 1) * count / cells;
            let done = (lo..hi)
                .filter(|&piece| have.get(piece).copied().unwrap_or(false))
                .count();
            u8::try_from(done * 255 / (hi - lo)).unwrap_or(u8::MAX)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preserves_downloaded_sections_gaps_and_partial_cells() {
        let have = [true, true, false, false, true, false, true, true];
        assert_eq!(piece_map(&have, 0..8, 4), vec![255, 0, 127, 255]);
        assert_eq!(
            piece_map(&have, 0..8, 100),
            vec![255, 255, 0, 0, 255, 0, 255, 255]
        );
        assert_eq!(piece_map(&have, 2..4, 10), vec![0, 0]);
        assert_eq!(piece_map(&have, 20..30, 10), vec![0]);
        assert_eq!(piece_map(&[], 0..0, 10), vec![0]);
        assert!(piece_map(&have, 0..8, 0).is_empty());
    }

    #[test]
    fn maps_each_file_within_its_own_piece_range_and_keeps_unknown_tails_empty() {
        let have = [false, false, true, true, false, true, false, false];
        assert_eq!(
            piece_map(&have, 2..6, PIECE_MAP_CELLS),
            vec![255, 255, 0, 255]
        );
        assert_eq!(piece_map(&[true, true], 0..4, 2), vec![255, 0]);
    }
}
