// SPDX-License-Identifier: MIT OR Apache-2.0

//! A fixed-size bitset for per-piece flags.
//!
//! Every connected peer has one of these for the pieces it has, so a torrent with 100,000 pieces
//! and 50 peers holds 50 of them: one bit per piece is 12.5 KB each instead of 100 KB as a
//! `Vec<bool>`.

/// One bit per piece.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Bits {
    words: Box<[u64]>,
    len: usize,
}

impl Bits {
    /// `len` bits, all clear.
    pub(crate) fn new(len: usize) -> Self {
        Self {
            words: vec![0; len.div_ceil(64)].into_boxed_slice(),
            len,
        }
    }

    pub(crate) fn len(&self) -> usize {
        self.len
    }

    /// Bit `i`; `false` beyond the end.
    pub(crate) fn get(&self, i: usize) -> bool {
        i < self.len && self.words[i / 64] & (1 << (i % 64)) != 0
    }

    /// Sets bit `i` and returns whether it was clear. Does nothing beyond the end.
    pub(crate) fn set(&mut self, i: usize) -> bool {
        if i >= self.len {
            return false;
        }
        let word = &mut self.words[i / 64];
        let mask = 1 << (i % 64);
        let was_clear = *word & mask == 0;
        *word |= mask;
        was_clear
    }

    /// The indexes of the set bits, in order.
    pub(crate) fn ones(&self) -> impl Iterator<Item = usize> + '_ {
        self.words.iter().enumerate().flat_map(|(w, &word)| {
            let mut rest = word;
            std::iter::from_fn(move || {
                if rest == 0 {
                    return None;
                }
                let bit = rest.trailing_zeros() as usize;
                rest &= rest - 1;
                Some(w * 64 + bit)
            })
        })
    }

    /// Heap bytes held.
    #[cfg(test)]
    pub(crate) fn heap_bytes(&self) -> usize {
        size_of_val(&*self.words)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bits_set_get_and_iterate() {
        let mut bits = Bits::new(130);
        assert_eq!(bits.len(), 130);
        assert!(bits.set(0));
        assert!(bits.set(64));
        assert!(bits.set(129));
        assert!(!bits.set(64), "already set");
        assert!(!bits.set(130), "beyond the end");
        assert!(bits.get(0) && bits.get(64) && bits.get(129));
        assert!(!bits.get(1) && !bits.get(130) && !bits.get(usize::MAX));
        assert_eq!(bits.ones().collect::<Vec<_>>(), [0, 64, 129]);
        assert_eq!(Bits::new(0).ones().count(), 0);
    }

    #[test]
    fn a_peer_bitfield_for_a_huge_torrent_takes_an_eighth_of_a_vec_of_bools() {
        // 100,000 pieces and 50 peers: 5 MB of `Vec<bool>` per torrent becomes 625 KB.
        let pieces = 100_000;
        let peers = 50;
        let compact: usize = (0..peers).map(|_| Bits::new(pieces).heap_bytes()).sum();
        let bools = peers * pieces * size_of::<bool>();
        assert_eq!(compact, peers * 12_504);
        assert!(compact * 7 < bools, "{compact} vs {bools}");
    }
}
