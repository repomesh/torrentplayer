// SPDX-License-Identifier: MIT OR Apache-2.0

//! What a torrent's metadata says about it, for a search result's details: its files, piece
//! size, trackers, and the optional `private` flag, `comment`, `created by`, and `creation
//! date` of the `.torrent` file. The metadata is untrusted: the extra fields are read by a
//! small bounded bencode scanner and kept as plain text of at most [`MAX_TEXT_CHARS`]
//! characters without control characters; nothing here is ever HTML.

use crate::engine::FileEntry;

/// Longest comment or creator kept, in characters.
pub const MAX_TEXT_CHARS: usize = 500;
/// Deepest nesting the scanner follows.
const MAX_DEPTH: usize = 32;

/// A torrent's metadata, summed up.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TorrentDetails {
    /// Lowercase hex info hash.
    pub info_hash: String,
    /// Display name.
    pub name: String,
    /// Every file, in torrent order.
    pub files: Vec<FileEntry>,
    /// Piece size in bytes.
    pub piece_len: u32,
    /// Total size in bytes.
    pub total_len: u64,
    /// Trackers announced over UDP, and over HTTP(S).
    pub udp_trackers: usize,
    /// See `udp_trackers`.
    pub http_trackers: usize,
    /// Web seeds (HTTP sources of the same data).
    pub web_seeds: usize,
    /// The extra fields of the `.torrent` file, when it is known.
    pub extras: Extras,
}

/// Optional fields of a `.torrent` file.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Extras {
    /// The `private` flag (peers only from its trackers: no DHT or peer exchange); `None` if
    /// unknown.
    pub private: Option<bool>,
    /// The creator's comment.
    pub comment: Option<String>,
    /// The program that made it.
    pub created_by: Option<String>,
    /// When it was made, in seconds since 1970.
    pub created_at: Option<i64>,
}

/// Reads the optional fields of a `.torrent` file. Anything malformed is left out.
#[must_use]
pub fn extras(torrent: &[u8]) -> Extras {
    let mut out = Extras::default();
    let mut scan = Scan {
        data: torrent,
        at: 0,
    };
    // Only a file that parses to the end counts.
    let complete = scan.dict(0, &mut |key, scan, depth| {
        match key {
            b"comment" | b"comment.utf-8" => {
                out.comment = Some(scan.text()?)
                    .filter(|t| !t.is_empty())
                    .or(out.comment.take());
            }
            b"created by" => out.created_by = Some(scan.text()?).filter(|t| !t.is_empty()),
            b"creation date" => out.created_at = Some(scan.int()?),
            b"info" => {
                return scan.dict(depth + 1, &mut |key, scan, _| {
                    if key == b"private" {
                        out.private = Some(scan.int()? == 1);
                    } else {
                        scan.skip(depth + 2)?;
                    }
                    Some(())
                });
            }
            _ => scan.skip(depth + 1)?,
        }
        Some(())
    });
    if complete.is_some() {
        out
    } else {
        Extras::default()
    }
}

/// Called with each key of a dictionary; it must read the value.
type Entry<'f, 'a> = dyn FnMut(&[u8], &mut Scan<'a>, usize) -> Option<()> + 'f;

/// A cursor over bencoded data. Every method returns `None` on malformed input.
struct Scan<'a> {
    data: &'a [u8],
    at: usize,
}

impl<'a> Scan<'a> {
    fn peek(&self) -> Option<u8> {
        self.data.get(self.at).copied()
    }

    /// Reads a dictionary, calling `entry` with each key; `entry` must consume the value.
    fn dict(&mut self, depth: usize, entry: &mut Entry<'_, 'a>) -> Option<()> {
        if depth > MAX_DEPTH || self.peek()? != b'd' {
            return None;
        }
        self.at += 1;
        while self.peek()? != b'e' {
            let key = self.bytes()?.to_vec();
            entry(&key, self, depth)?;
        }
        self.at += 1;
        Some(())
    }

    fn bytes(&mut self) -> Option<&[u8]> {
        let colon = self.data.get(self.at..)?.iter().position(|&b| b == b':')?;
        let len: usize = std::str::from_utf8(&self.data[self.at..self.at + colon])
            .ok()?
            .parse()
            .ok()?;
        let start = self.at + colon + 1;
        let end = start.checked_add(len)?;
        let value = self.data.get(start..end)?;
        self.at = end;
        Some(value)
    }

    fn int(&mut self) -> Option<i64> {
        if self.peek()? != b'i' {
            return None;
        }
        let end = self.data.get(self.at..)?.iter().position(|&b| b == b'e')?;
        let value = std::str::from_utf8(&self.data[self.at + 1..self.at + end])
            .ok()?
            .parse()
            .ok()?;
        self.at += end + 1;
        Some(value)
    }

    /// A string value as plain, bounded text (empty if it has none).
    fn text(&mut self) -> Option<String> {
        let raw = self.bytes()?;
        let text: String = String::from_utf8_lossy(raw)
            .chars()
            .map(|c| if c.is_control() { ' ' } else { c })
            .collect();
        let text: String = text.trim().chars().take(MAX_TEXT_CHARS).collect();
        Some(text)
    }

    fn skip(&mut self, depth: usize) -> Option<()> {
        if depth > MAX_DEPTH {
            return None;
        }
        match self.peek()? {
            b'i' => self.int().map(|_| ()),
            b'l' => {
                self.at += 1;
                while self.peek()? != b'e' {
                    self.skip(depth + 1)?;
                }
                self.at += 1;
                Some(())
            }
            b'd' => self.dict(depth, &mut |_, scan, depth| scan.skip(depth + 1)),
            b'0'..=b'9' => self.bytes().map(|_| ()),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_optional_fields() {
        let torrent = b"d8:announce15:udp://t.ex:80807:comment24:Lecture 1 \x07of the course10:created by13:mktorrent 1.113:creation datei1700000000e4:infod6:lengthi4e4:name1:a7:privatei1e12:piece lengthi16384e6:pieces20:aaaaaaaaaaaaaaaaaaaaee";
        let e = extras(torrent);
        assert_eq!(
            e.comment.as_deref(),
            Some("Lecture 1  of the course"),
            "control characters go"
        );
        assert_eq!(e.created_by.as_deref(), Some("mktorrent 1.1"));
        assert_eq!(e.created_at, Some(1_700_000_000));
        assert_eq!(e.private, Some(true));
    }

    #[test]
    fn leaves_out_what_is_missing_or_malformed() {
        let plain =
            b"d4:infod6:lengthi4e4:name1:a12:piece lengthi16384e6:pieces20:aaaaaaaaaaaaaaaaaaaaee";
        assert_eq!(extras(plain), Extras::default());
        for bad in [
            &b""[..],
            b"x",
            b"d7:comment99:short",
            b"d13:creation dateixe",
            b"l",
        ] {
            let _ = extras(bad);
        }
        assert_eq!(
            extras(b"d7:comment5:hello").comment,
            None,
            "unterminated: nothing trusted"
        );
        // Deep nesting stops the scan instead of overflowing the stack.
        let mut deep = b"d1:x".to_vec();
        deep.extend(std::iter::repeat_n(b'l', 10_000));
        assert_eq!(extras(&deep), Extras::default());
    }

    #[test]
    fn long_text_is_cut() {
        let comment = "c".repeat(2_000);
        let torrent = format!("d7:comment{}:{comment}e", comment.len());
        assert_eq!(
            extras(torrent.as_bytes())
                .comment
                .map(|c| c.chars().count()),
            Some(MAX_TEXT_CHARS)
        );
    }
}
