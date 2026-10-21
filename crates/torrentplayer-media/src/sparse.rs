// SPDX-License-Identifier: MIT OR Apache-2.0

//! The parts of a file read so far, and how a parser asks for more.
//!
//! The parsers are pure functions over a [`Sparse`] view. When they need bytes that have not
//! been read, they stop with [`Stop::Need`]; the driver reads them and runs the parser again
//! from the start. Parsing a header is cheap, so re-running beats a resumable state machine
//! and keeps the parsers simple to read and test.

use std::ops::Range;

/// Why a parser stopped without an answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Stop {
    /// These bytes are needed and have not been read yet.
    Need(Range<u64>),
    /// The container is malformed or exceeds a limit.
    Invalid(&'static str),
    /// Not a container this crate parses.
    Unsupported,
}

/// Chunks of a file at known offsets.
#[derive(Debug)]
pub(crate) struct Sparse {
    len: u64,
    chunks: Vec<(u64, Vec<u8>)>,
}

impl Sparse {
    /// An empty view of a file of `len` bytes.
    pub(crate) fn new(len: u64) -> Self {
        Self {
            len,
            chunks: Vec::new(),
        }
    }

    /// A view with the whole file in memory (tests).
    #[cfg(test)]
    pub(crate) fn whole(bytes: Vec<u8>) -> Self {
        let mut s = Self::new(bytes.len() as u64);
        s.insert(0, bytes);
        s
    }

    /// The file's length.
    pub(crate) fn len(&self) -> u64 {
        self.len
    }

    /// Adds bytes read at `offset`.
    pub(crate) fn insert(&mut self, offset: u64, bytes: Vec<u8>) {
        self.chunks.push((offset, bytes));
    }

    /// Exactly `len` bytes at `offset`. Fails with [`Stop::Invalid`] if they lie past the end of
    /// the file, and with [`Stop::Need`] if they have not been read.
    pub(crate) fn get(&self, offset: u64, len: usize) -> Result<&[u8], Stop> {
        let end = u64::try_from(len)
            .ok()
            .and_then(|n| offset.checked_add(n))
            .filter(|end| *end <= self.len)
            .ok_or(Stop::Invalid("a structure runs past the end of the file"))?;
        if len == 0 {
            return Ok(&[]);
        }
        for (start, bytes) in &self.chunks {
            let Some(at) = offset
                .checked_sub(*start)
                .and_then(|d| usize::try_from(d).ok())
            else {
                continue;
            };
            if let Some(slice) = at.checked_add(len).and_then(|e| bytes.get(at..e)) {
                return Ok(slice);
            }
        }
        Err(Stop::Need(offset..end))
    }

    /// One byte at `offset`.
    pub(crate) fn u8_at(&self, offset: u64) -> Result<u8, Stop> {
        Ok(self.array::<1>(offset)?[0])
    }

    /// A big-endian `u32` at `offset`.
    pub(crate) fn u32_at(&self, offset: u64) -> Result<u32, Stop> {
        Ok(u32::from_be_bytes(self.array(offset)?))
    }

    /// A big-endian `u64` at `offset`.
    pub(crate) fn u64_at(&self, offset: u64) -> Result<u64, Stop> {
        Ok(u64::from_be_bytes(self.array(offset)?))
    }

    /// `N` bytes at `offset`.
    pub(crate) fn array<const N: usize>(&self, offset: u64) -> Result<[u8; N], Stop> {
        let mut out = [0u8; N];
        out.copy_from_slice(self.get(offset, N)?);
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_inside_chunks_and_asks_for_the_rest() {
        let mut s = Sparse::new(100);
        s.insert(10, vec![1, 2, 3, 4]);
        s.insert(50, vec![9; 8]);
        assert_eq!(s.get(11, 2), Ok(&[2u8, 3][..]));
        assert_eq!(s.u32_at(10), Ok(0x0102_0304));
        assert_eq!(s.u64_at(50), Ok(0x0909_0909_0909_0909));
        assert_eq!(
            s.get(12, 4),
            Err(Stop::Need(12..16)),
            "straddles a chunk end"
        );
        assert_eq!(s.get(0, 1), Err(Stop::Need(0..1)));
        assert!(
            matches!(s.get(99, 2), Err(Stop::Invalid(_))),
            "past the end"
        );
        assert!(
            matches!(s.get(u64::MAX, 2), Err(Stop::Invalid(_))),
            "overflow"
        );
        assert_eq!(s.get(100, 0), Ok(&[][..]), "empty read at the end");
    }
}
