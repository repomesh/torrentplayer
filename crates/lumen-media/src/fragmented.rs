// SPDX-License-Identifier: MIT OR Apache-2.0

//! Fragmented MP4, as the audio conversion writes it: where its initialization segment ends,
//! and the type a player passes to Media Source Extensions to play it.
//!
//! The codec configuration comes from the torrent's file (the video is copied), so it is parsed
//! as strictly as the rest of this crate: every size is checked against its parent, the walk
//! follows fixed paths, and nothing panics on any input.

use std::fmt::Write as _;

/// The largest initialization segment accepted. Real ones are a few kilobytes.
pub const MAX_INIT_SEGMENT: usize = 1024 * 1024;

/// Most boxes visited at one level.
const MAX_BOXES: usize = 64;

/// The length of the initialization segment (`ftyp` and `moov`) at the start of `data`, once
/// all of it is there. `None` if more data is needed; `Some(Err)` if `data` does not start
/// with one.
#[must_use]
pub fn init_segment_len(data: &[u8]) -> Option<Result<usize, InitError>> {
    let mut at = 0;
    for _ in 0..MAX_BOXES {
        let header = data.get(at..at + 8)?;
        let size = u32::from_be_bytes([header[0], header[1], header[2], header[3]]) as usize;
        let kind = &header[4..8];
        if size < 8 || at + size > MAX_INIT_SEGMENT {
            return Some(Err(InitError));
        }
        match kind {
            b"ftyp" | b"free" | b"skip" => {}
            b"moov" => {
                return (data.len() >= at + size).then_some(Ok(at + size));
            }
            _ => return Some(Err(InitError)),
        }
        at += size;
    }
    Some(Err(InitError))
}

/// The output did not start with an initialization segment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("the conversion did not start with an MP4 initialization segment")]
pub struct InitError;

/// The MIME type with codecs for Media Source Extensions, such as
/// `video/mp4; codecs="avc1.64001f,mp4a.40.2"`, from an initialization segment. `None` if a
/// track's codec is not one a webview plays (H.264, HEVC, AV1, VP9, or AAC).
#[must_use]
pub fn mse_type(init: &[u8]) -> Option<String> {
    let moov = boxes(init).find(|b| &b.kind == b"moov")?;
    let mut codecs = Vec::new();
    for trak in boxes(moov.body).filter(|b| &b.kind == b"trak") {
        let stsd = path(trak.body, &[*b"mdia", *b"minf", *b"stbl", *b"stsd"])?;
        // FullBox header and entry count, then the first sample entry.
        let entry = boxes(stsd.get(8..)?).next()?;
        codecs.push(codec(entry)?);
    }
    (!codecs.is_empty()).then(|| format!("video/mp4; codecs=\"{}\"", codecs.join(",")))
}

/// A box: its type and body.
#[derive(Clone, Copy)]
struct Bx<'a> {
    kind: [u8; 4],
    body: &'a [u8],
}

/// The boxes in `data`, stopping at the first malformed one.
fn boxes(data: &[u8]) -> impl Iterator<Item = Bx<'_>> {
    let mut rest = data;
    std::iter::from_fn(move || {
        let header = rest.get(..8)?;
        let size = u32::from_be_bytes([header[0], header[1], header[2], header[3]]) as usize;
        let kind = [header[4], header[5], header[6], header[7]];
        let body = rest.get(8..size)?;
        rest = &rest[size..];
        Some(Bx { kind, body })
    })
    .take(MAX_BOXES)
}

/// The body of the box at `path` below `data`.
fn path<'a>(data: &'a [u8], path: &[[u8; 4]]) -> Option<&'a [u8]> {
    let mut body = data;
    for kind in path {
        body = boxes(body).find(|b| &b.kind == kind)?.body;
    }
    Some(body)
}

/// Bytes of a `VisualSampleEntry` before its child boxes.
const VISUAL_ENTRY: usize = 78;
/// Bytes of an `AudioSampleEntry` before its child boxes.
const AUDIO_ENTRY: usize = 28;

/// The RFC 6381 codec string of a sample entry.
fn codec(entry: Bx<'_>) -> Option<String> {
    let kind = std::str::from_utf8(&entry.kind).ok()?;
    let config = |name: &[u8; 4], skip: usize| {
        boxes(entry.body.get(skip..)?)
            .find(|b| &b.kind == name)
            .map(|b| b.body)
    };
    match &entry.kind {
        b"avc1" | b"avc3" => {
            let c = config(b"avcC", VISUAL_ENTRY)?;
            // configurationVersion, then profile, compatibility flags, and level.
            let [profile, compat, level] = [*c.get(1)?, *c.get(2)?, *c.get(3)?];
            Some(format!("{kind}.{profile:02x}{compat:02x}{level:02x}"))
        }
        b"hvc1" | b"hev1" => hevc(kind, config(b"hvcC", VISUAL_ENTRY)?),
        b"av01" => av1(config(b"av1C", VISUAL_ENTRY)?),
        b"vp09" => {
            // FullBox header, then profile, level, and bit depth in the top four bits.
            let c = config(b"vpcC", VISUAL_ENTRY)?;
            let [profile, level, depth] = [*c.get(4)?, *c.get(5)?, *c.get(6)? >> 4];
            Some(format!("vp09.{profile:02}.{level:02}.{depth:02}"))
        }
        // The conversion always encodes AAC-LC.
        b"mp4a" if entry.body.len() >= AUDIO_ENTRY => Some("mp4a.40.2".to_owned()),
        _ => None,
    }
}

/// `hvc1.<profile>.<compatibility>.<tier><level>.<constraints>` (ISO/IEC 14496-15, annex E).
fn hevc(kind: &str, c: &[u8]) -> Option<String> {
    let first = *c.get(1)?;
    let space = ["", "A", "B", "C"][usize::from(first >> 6)];
    let tier = if first & 0x20 == 0 { 'L' } else { 'H' };
    let profile = first & 0x1F;
    let compat = u32::from_be_bytes(c.get(2..6)?.try_into().ok()?).reverse_bits();
    let constraints = c.get(6..12)?;
    let level = *c.get(12)?;
    let kept = constraints
        .iter()
        .rposition(|&b| b != 0)
        .map_or(0, |last| last + 1);
    let mut out = format!("{kind}.{space}{profile}.{compat:X}.{tier}{level}");
    for byte in &constraints[..kept] {
        let _ = write!(out, ".{byte:X}");
    }
    Some(out)
}

/// `av01.<profile>.<level><tier>.<bit depth>` (AV1 codec ISO media file format, section 5).
fn av1(c: &[u8]) -> Option<String> {
    let [first, second] = [*c.get(1)?, *c.get(2)?];
    let profile = first >> 5;
    let level = first & 0x1F;
    let tier = if second & 0x80 == 0 { 'M' } else { 'H' };
    let depth = match (second & 0x40 != 0, second & 0x20 != 0) {
        (false, _) => 8,
        (true, false) => 10,
        (true, true) => 12,
    };
    Some(format!("av01.{profile}.{level:02}{tier}.{depth:02}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    proptest! {
        #[test]
        fn arbitrary_init_bytes_never_panic(bytes in proptest::collection::vec(any::<u8>(), 0..8192)) {
            let _ = init_segment_len(&bytes);
            let _ = mse_type(&bytes);
        }
    }

    fn bx(kind: [u8; 4], body: &[u8]) -> Vec<u8> {
        let size = u32::try_from(body.len() + 8).unwrap();
        [&size.to_be_bytes()[..], &kind, body].concat()
    }

    fn trak(entry: &[u8]) -> Vec<u8> {
        let stsd = bx(*b"stsd", &[&[0, 0, 0, 0, 0, 0, 0, 1][..], entry].concat());
        let stbl = bx(*b"stbl", &stsd);
        bx(*b"trak", &bx(*b"mdia", &bx(*b"minf", &stbl)))
    }

    fn visual(kind: [u8; 4], config: &[u8]) -> Vec<u8> {
        bx(kind, &[&[0; VISUAL_ENTRY][..], config].concat())
    }

    fn init(traks: &[Vec<u8>]) -> Vec<u8> {
        [
            bx(*b"ftyp", b"isom\0\0\x02\0"),
            bx(*b"moov", &traks.concat()),
        ]
        .concat()
    }

    fn mp4a() -> Vec<u8> {
        bx(*b"mp4a", &[0; AUDIO_ENTRY])
    }

    #[test]
    fn h264_with_aac() {
        let avc = visual(*b"avc1", &bx(*b"avcC", &[1, 0x64, 0x00, 0x1F, 0xFF]));
        let segment = init(&[trak(&avc), trak(&mp4a())]);
        assert_eq!(
            mse_type(&segment).as_deref(),
            Some("video/mp4; codecs=\"avc1.64001f,mp4a.40.2\"")
        );
    }

    #[test]
    fn hevc_main10() {
        // Main 10, main tier, level 5.1 (153), progressive-source constraint flag.
        let mut hvcc = vec![1, 0x02, 0x20, 0x00, 0x00, 0x00, 0x90, 0, 0, 0, 0, 0, 153];
        hvcc.extend([0; 10]);
        let segment = init(&[trak(&visual(*b"hvc1", &bx(*b"hvcC", &hvcc)))]);
        assert_eq!(
            mse_type(&segment).as_deref(),
            Some("video/mp4; codecs=\"hvc1.2.4.L153.90\"")
        );
    }

    #[test]
    fn av1_and_vp9() {
        // Main profile, level 8 (4.0), main tier, 10-bit.
        let av1c = bx(*b"av1C", &[0x81, 0x08, 0x40, 0x00]);
        let segment = init(&[trak(&visual(*b"av01", &av1c))]);
        assert_eq!(
            mse_type(&segment).as_deref(),
            Some("video/mp4; codecs=\"av01.0.08M.10\"")
        );
        let vpcc = bx(*b"vpcC", &[1, 0, 0, 0, 0, 31, 0x82, 0]);
        let segment = init(&[trak(&visual(*b"vp09", &vpcc))]);
        assert_eq!(
            mse_type(&segment).as_deref(),
            Some("video/mp4; codecs=\"vp09.00.31.08\"")
        );
    }

    #[test]
    fn unsupported_codec_or_malformed_entry() {
        let mp4v = visual(*b"mp4v", &bx(*b"esds", &[0; 8]));
        assert_eq!(mse_type(&init(&[trak(&mp4v), trak(&mp4a())])), None);
        // avcC cut short.
        let avc = visual(*b"avc1", &bx(*b"avcC", &[1, 0x64]));
        assert_eq!(mse_type(&init(&[trak(&avc)])), None);
        assert_eq!(mse_type(b"garbage"), None);
        assert_eq!(mse_type(&init(&[])), None);
    }

    #[test]
    fn finds_the_end_of_the_init_segment() {
        let segment = init(&[trak(&mp4a())]);
        let moof = bx(*b"moof", &[0; 16]);
        let stream = [segment.clone(), moof].concat();
        for cut in 0..segment.len() {
            assert_eq!(init_segment_len(&stream[..cut]), None, "cut at {cut}");
        }
        assert_eq!(
            init_segment_len(&stream[..segment.len()]),
            Some(Ok(segment.len()))
        );
        assert_eq!(init_segment_len(&stream), Some(Ok(segment.len())));
    }

    #[test]
    fn rejects_output_that_is_not_an_init_segment() {
        assert_eq!(
            init_segment_len(&bx(*b"moof", &[0; 8])),
            Some(Err(InitError))
        );
        assert_eq!(
            init_segment_len(&[0, 0, 0, 4, b'f', b't', b'y', b'p']),
            Some(Err(InitError))
        );
        let huge = [&u32::MAX.to_be_bytes()[..], b"moov"].concat();
        assert_eq!(init_segment_len(&huge), Some(Err(InitError)));
    }
}
