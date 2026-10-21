// SPDX-License-Identifier: MIT OR Apache-2.0

//! Strict, defensive parsing of HTTP `Range` headers ([RFC 9110 §14]).
//!
//! Only single byte ranges are honored, which is all that media elements and libmpv send.
//! Anything malformed is ignored and the full content is served, as the RFC allows.
//!
//! [RFC 9110 §14]: https://www.rfc-editor.org/rfc/rfc9110#section-14

use std::ops::RangeInclusive;

/// Longest `Range` header value we are willing to look at.
pub const MAX_RANGE_HEADER_LEN: usize = 128;

/// How a request's `Range` header should be answered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RangeRequest {
    /// No usable range: respond `200 OK` with the whole content.
    Full,
    /// Respond `206 Partial Content` with these bytes.
    Partial(RangeInclusive<u64>),
    /// The range starts past the end: respond `416 Range Not Satisfiable`.
    Unsatisfiable,
}

/// Parses an optional `Range` header value for content of `len` bytes.
#[must_use]
pub fn parse_range(header: Option<&[u8]>, len: u64) -> RangeRequest {
    let Some(header) = header else {
        return RangeRequest::Full;
    };
    if header.len() > MAX_RANGE_HEADER_LEN {
        return RangeRequest::Full;
    }
    let Ok(header) = std::str::from_utf8(header) else {
        return RangeRequest::Full;
    };
    let header = header.trim();
    let Some((unit, spec)) = header.split_once('=') else {
        return RangeRequest::Full;
    };
    if !unit.trim().eq_ignore_ascii_case("bytes") || spec.contains(',') {
        // Other units and multi-range requests are ignored.
        return RangeRequest::Full;
    }
    parse_spec(spec.trim(), len).unwrap_or(RangeRequest::Full)
}

/// Parses a single range-spec (`a-b`, `a-`, or `-n`). `None` means malformed.
fn parse_spec(spec: &str, len: u64) -> Option<RangeRequest> {
    let (first, last) = spec.split_once('-')?;
    if first.is_empty() {
        // "-n": the last n bytes.
        let suffix = parse_u64(last)?;
        if suffix == 0 || len == 0 {
            return Some(RangeRequest::Unsatisfiable);
        }
        return Some(RangeRequest::Partial(len.saturating_sub(suffix)..=len - 1));
    }

    // "a-b" or "a-"
    let start = parse_u64(first)?;
    let last = if last.is_empty() {
        None
    } else {
        Some(parse_u64(last)?)
    };
    if last.is_some_and(|l| l < start) {
        return None;
    }
    if start >= len {
        return Some(RangeRequest::Unsatisfiable);
    }
    let end = last.map_or(len - 1, |l| l.min(len - 1));
    Some(RangeRequest::Partial(start..=end))
}

/// Parses a non-empty run of ASCII digits. Signs, spaces, and overflow are rejected.
fn parse_u64(s: &str) -> Option<u64> {
    if s.is_empty() || s.len() > 20 || !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    s.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn p(h: &str, len: u64) -> RangeRequest {
        parse_range(Some(h.as_bytes()), len)
    }

    #[test]
    fn absent_header_is_full() {
        assert_eq!(parse_range(None, 10), RangeRequest::Full);
    }

    #[test]
    fn common_forms() {
        assert_eq!(p("bytes=0-", 100), RangeRequest::Partial(0..=99));
        assert_eq!(p("bytes=10-19", 100), RangeRequest::Partial(10..=19));
        assert_eq!(p("bytes=90-200", 100), RangeRequest::Partial(90..=99));
        assert_eq!(p("bytes=-10", 100), RangeRequest::Partial(90..=99));
        assert_eq!(p("bytes=-1000", 100), RangeRequest::Partial(0..=99));
        assert_eq!(p("Bytes = 5-5", 100), RangeRequest::Partial(5..=5));
    }

    #[test]
    fn unsatisfiable() {
        assert_eq!(p("bytes=100-", 100), RangeRequest::Unsatisfiable);
        assert_eq!(p("bytes=100-200", 100), RangeRequest::Unsatisfiable);
        assert_eq!(p("bytes=-0", 100), RangeRequest::Unsatisfiable);
        assert_eq!(p("bytes=0-", 0), RangeRequest::Unsatisfiable);
        assert_eq!(p("bytes=-5", 0), RangeRequest::Unsatisfiable);
    }

    #[test]
    fn malformed_is_ignored() {
        for h in [
            "",
            "bytes",
            "bytes=",
            "bytes=-",
            "bytes=a-b",
            "bytes=10-5",
            "bytes=+1-2",
            "bytes= 1-2,3-4",
            "bytes=0-1,5-6",
            "items=0-1",
            "bytes=99999999999999999999999-",
            "bytes=1--2",
            "bytes=1-2-3",
            "bytes=0x10-",
            "bytes=5-x",
            "bytes=-x",
            "bytes=5- 6",
        ] {
            assert_eq!(p(h, 100), RangeRequest::Full, "{h:?}");
        }
        let long = format!("bytes=0-{}", "1".repeat(MAX_RANGE_HEADER_LEN));
        assert_eq!(p(&long, 100), RangeRequest::Full);
        assert_eq!(parse_range(Some(&[0xff, 0xfe]), 100), RangeRequest::Full);
    }

    proptest! {
        #[test]
        fn never_panics_and_stays_in_bounds(header in proptest::collection::vec(any::<u8>(), 0..64), len in 0u64..u64::MAX) {
            if let RangeRequest::Partial(r) = parse_range(Some(&header), len) {
                prop_assert!(r.start() <= r.end());
                prop_assert!(*r.end() < len);
            }
        }

        #[test]
        fn well_formed_ranges_round_trip(len in 1u64..1 << 40, a in any::<u64>(), b in any::<u64>()) {
            let (start, end) = (a.min(b) % len, a.max(b));
            let got = p(&format!("bytes={start}-{end}"), len);
            prop_assert_eq!(got, RangeRequest::Partial(start..=end.min(len - 1)));
        }
    }
}
