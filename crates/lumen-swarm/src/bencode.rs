// SPDX-License-Identifier: MIT OR Apache-2.0

//! A small, strict bencode decoder for the untrusted dictionaries TorrentStream reads itself: the
//! `url-list` of a `.torrent` file and peers' extension messages (BEP 10, BEP 11).
//!
//! Values borrow from the input. Nesting depth and the number of values are bounded, integers
//! must be canonical, and dictionary keys must be byte strings. Large inputs such as a whole
//! `.torrent` file are walked with [`dict_entries`], which locates each top-level value without
//! decoding it, so only the small values the caller asks for are ever materialized.

/// Deepest nesting accepted.
const MAX_DEPTH: usize = 32;
/// Most values a single [`decode`] may produce.
const MAX_VALUES: usize = 10_000;

/// A decoded value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Value<'a> {
    Int(i64),
    Bytes(&'a [u8]),
    List(Vec<Value<'a>>),
    /// Entries in input order.
    Dict(Vec<(&'a [u8], Value<'a>)>),
}

impl<'a> Value<'a> {
    /// The value under `key`, if this is a dictionary that has it.
    pub(crate) fn get(&self, key: &[u8]) -> Option<&Value<'a>> {
        match self {
            Self::Dict(entries) => entries.iter().find(|(k, _)| *k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    pub(crate) fn as_bytes(&self) -> Option<&'a [u8]> {
        match self {
            Self::Bytes(b) => Some(b),
            _ => None,
        }
    }

    #[cfg(test)]
    pub(crate) fn as_int(&self) -> Option<i64> {
        match self {
            Self::Int(i) => Some(*i),
            _ => None,
        }
    }

    #[cfg(test)]
    pub(crate) fn as_list(&self) -> Option<&[Value<'a>]> {
        match self {
            Self::List(items) => Some(items),
            _ => None,
        }
    }
}

/// Malformed or oversized bencode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub(crate) enum BencodeError {
    #[error("malformed bencode at byte {0}")]
    Malformed(usize),
    #[error("bencode nested too deeply")]
    TooDeep,
    #[error("bencode has too many values")]
    TooLarge,
}

/// Decodes one value from the start of `input`; returns it and the bytes it used. Data after
/// it is left to the caller (BEP 9 metadata messages, for example, carry a payload there).
pub(crate) fn decode_prefix(input: &[u8]) -> Result<(Value<'_>, usize), BencodeError> {
    let mut d = Decoder {
        input,
        pos: 0,
        budget: MAX_VALUES,
    };
    let value = d.value(0)?;
    Ok((value, d.pos))
}

/// Decodes `input`, which must be exactly one value.
pub(crate) fn decode(input: &[u8]) -> Result<Value<'_>, BencodeError> {
    let (value, used) = decode_prefix(input)?;
    if used == input.len() {
        Ok(value)
    } else {
        Err(BencodeError::Malformed(used))
    }
}

/// Keys of a dictionary, each with the raw bytes of its value.
pub(crate) type RawEntries<'a> = Vec<(&'a [u8], &'a [u8])>;

/// Walks a top-level dictionary without decoding its values: returns each key with the raw
/// bytes of its value, for [`decode`] to decode the ones that matter.
pub(crate) fn dict_entries(input: &[u8]) -> Result<RawEntries<'_>, BencodeError> {
    let mut d = Decoder {
        input,
        pos: 0,
        budget: usize::MAX,
    };
    if d.peek() != Some(b'd') {
        return Err(BencodeError::Malformed(0));
    }
    d.pos += 1;
    let mut entries = Vec::new();
    while d.peek() != Some(b'e') {
        let key = d.bytes()?;
        let start = d.pos;
        d.skip(1)?;
        entries.push((key, &input[start..d.pos]));
    }
    d.pos += 1;
    if d.pos == input.len() {
        Ok(entries)
    } else {
        Err(BencodeError::Malformed(d.pos))
    }
}

struct Decoder<'a> {
    input: &'a [u8],
    pos: usize,
    /// Values still allowed.
    budget: usize,
}

impl<'a> Decoder<'a> {
    fn peek(&self) -> Option<u8> {
        self.input.get(self.pos).copied()
    }

    fn take_budget(&mut self) -> Result<(), BencodeError> {
        self.budget = self.budget.checked_sub(1).ok_or(BencodeError::TooLarge)?;
        Ok(())
    }

    fn value(&mut self, depth: usize) -> Result<Value<'a>, BencodeError> {
        if depth > MAX_DEPTH {
            return Err(BencodeError::TooDeep);
        }
        self.take_budget()?;
        match self.peek() {
            Some(b'i') => self.int().map(Value::Int),
            Some(b'l') => {
                self.pos += 1;
                let mut items = Vec::new();
                while self.peek() != Some(b'e') {
                    items.push(self.value(depth + 1)?);
                }
                self.pos += 1;
                Ok(Value::List(items))
            }
            Some(b'd') => {
                self.pos += 1;
                let mut entries = Vec::new();
                while self.peek() != Some(b'e') {
                    let key = self.bytes()?;
                    entries.push((key, self.value(depth + 1)?));
                }
                self.pos += 1;
                Ok(Value::Dict(entries))
            }
            Some(b'0'..=b'9') => self.bytes().map(Value::Bytes),
            _ => Err(BencodeError::Malformed(self.pos)),
        }
    }

    /// Skips one value, checking its syntax but allocating nothing.
    fn skip(&mut self, depth: usize) -> Result<(), BencodeError> {
        if depth > MAX_DEPTH {
            return Err(BencodeError::TooDeep);
        }
        match self.peek() {
            Some(b'i') => self.int().map(drop),
            Some(b'l') => {
                self.pos += 1;
                while self.peek() != Some(b'e') {
                    self.skip(depth + 1)?;
                }
                self.pos += 1;
                Ok(())
            }
            Some(b'd') => {
                self.pos += 1;
                while self.peek() != Some(b'e') {
                    self.bytes()?;
                    self.skip(depth + 1)?;
                }
                self.pos += 1;
                Ok(())
            }
            Some(b'0'..=b'9') => self.bytes().map(drop),
            _ => Err(BencodeError::Malformed(self.pos)),
        }
    }

    /// `i<digits>e`, canonical: no leading zeros, no `-0`.
    fn int(&mut self) -> Result<i64, BencodeError> {
        let start = self.pos;
        let bad = BencodeError::Malformed(start);
        self.pos += 1;
        let rest = self.input.get(self.pos..).ok_or(bad)?;
        let end = rest.iter().position(|&b| b == b'e').ok_or(bad)?;
        let digits = &rest[..end];
        let unsigned = digits.strip_prefix(b"-").unwrap_or(digits);
        let canonical = !unsigned.is_empty()
            && unsigned.iter().all(u8::is_ascii_digit)
            && (unsigned == b"0" || unsigned[0] != b'0')
            && digits != b"-0";
        if !canonical {
            return Err(bad);
        }
        let value = std::str::from_utf8(digits)
            .ok()
            .and_then(|s| s.parse::<i64>().ok())
            .ok_or(bad)?;
        self.pos += end + 1;
        Ok(value)
    }

    /// `<length>:<bytes>`.
    fn bytes(&mut self) -> Result<&'a [u8], BencodeError> {
        let start = self.pos;
        let bad = BencodeError::Malformed(start);
        let rest = self.input.get(self.pos..).ok_or(bad)?;
        let colon = rest.iter().take(21).position(|&b| b == b':').ok_or(bad)?;
        let digits = &rest[..colon];
        if digits.is_empty()
            || !digits.iter().all(u8::is_ascii_digit)
            || (digits.len() > 1 && digits[0] == b'0')
        {
            return Err(bad);
        }
        let len: usize = std::str::from_utf8(digits)
            .ok()
            .and_then(|s| s.parse().ok())
            .ok_or(bad)?;
        let body_start = self.pos + colon + 1;
        let body_end = body_start.checked_add(len).ok_or(bad)?;
        let body = self.input.get(body_start..body_end).ok_or(bad)?;
        self.pos = body_end;
        Ok(body)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_values() {
        let v = decode(b"d1:ai-12e1:bl3:xyzi0eee").unwrap();
        assert_eq!(v.get(b"a").and_then(Value::as_int), Some(-12));
        let list = v.get(b"b").and_then(Value::as_list).unwrap();
        assert_eq!(list[0].as_bytes(), Some(&b"xyz"[..]));
        assert_eq!(list[1].as_int(), Some(0));
        assert!(v.get(b"c").is_none());
    }

    #[test]
    fn prefix_leaves_trailing_data() {
        let (v, used) = decode_prefix(b"d1:xi1eeTRAILER").unwrap();
        assert_eq!(v.get(b"x").and_then(Value::as_int), Some(1));
        assert_eq!(used, 8);
        assert!(decode(b"d1:xi1eeTRAILER").is_err());
    }

    #[test]
    fn rejects_malformed_and_non_canonical_input() {
        for bad in [
            &b""[..],
            b"i",
            b"ie",
            b"i-0e",
            b"i03e",
            b"i1",
            b"i99999999999999999999e",
            b"5:abc",
            b"05:abcde",
            b"l",
            b"d1:a",
            b"di1ei2ee",
            b"x",
            b"99999999999999999999999:x",
        ] {
            assert!(decode(bad).is_err(), "{:?}", String::from_utf8_lossy(bad));
        }
    }

    #[test]
    fn bounds_depth_and_size() {
        let deep = [vec![b'l'; 100], vec![b'e'; 100]].concat();
        assert_eq!(decode(&deep), Err(BencodeError::TooDeep));
        let wide = [b"l".as_slice(), &b"i1e".repeat(MAX_VALUES + 1), b"e"].concat();
        assert_eq!(decode(&wide), Err(BencodeError::TooLarge));
    }

    #[test]
    fn walks_top_level_entries() {
        let entries = dict_entries(b"d4:infod1:xi1ee8:url-list3:abce").unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0], (&b"info"[..], &b"d1:xi1ee"[..]));
        assert_eq!(entries[1], (&b"url-list"[..], &b"3:abc"[..]));
        assert!(dict_entries(b"l1:ae").is_err());
        assert!(dict_entries(b"d1:ai1eextra").is_err());
    }

    #[test]
    fn arbitrary_bytes_never_panic() {
        let mut x: u32 = 7;
        for _ in 0..20_000 {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            let len = (x % 24) as usize;
            let bytes: Vec<u8> = (0..len)
                .map(|i| b"dlie0123456789:-x"[((x >> (i % 24)) % 17) as usize])
                .collect();
            let _ = decode(&bytes);
            let _ = dict_entries(&bytes);
        }
    }
}
