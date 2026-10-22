// SPDX-License-Identifier: MIT OR Apache-2.0

//! ISO base media (MP4) and `QuickTime` (MOV): the audio and subtitle tracks in `moov`.
//!
//! The walk follows `moov/trak/mdia/hdlr` to find sound and subtitle tracks, then
//! `mdia/minf/stbl/stsd` for the first sample entry's format (`mp4a`, `ac-3`, `ec-3`, ...).
//! For `mp4a`, the object type in `esds` tells AAC from MP3, AC-3, or DTS. Child boxes are
//! found by reading headers only, in order, stopping at the one wanted, so video tracks and
//! sample tables are skipped without being read.

use crate::audio::{AudioCodec, AudioTrack, MAX_AUDIO_TRACKS};
use crate::layout::{MAX_SAMPLES, MAX_SEEK_POINTS, MediaLayout, SeekPoint, push_range, thin};
use crate::sparse::{Sparse, Stop};
use crate::tracks::{
    Container, MAX_SUBTITLE_TRACKS, SubtitleCodec, SubtitleTrack, TrackReport, duration_ms,
    language_tag,
};

/// Top-level boxes visited before giving up on finding `moov`.
const MAX_TOP_LEVEL: usize = 64;
/// Child boxes visited per box.
const MAX_CHILDREN: usize = 256;

/// Top-level box types an MP4 or MOV file can start with.
const FIRST_BOXES: &[&[u8; 4]] = &[
    b"ftyp", b"moov", b"mdat", b"free", b"skip", b"wide", b"pnot", b"uuid", b"styp", b"sidx",
];

/// Whether `head` (the first 8 bytes or more) looks like an MP4 or MOV file.
pub(crate) fn sniff(head: &[u8]) -> bool {
    head.get(4..8)
        .is_some_and(|kind| FIRST_BOXES.iter().any(|k| &k[..] == kind))
}

/// A box: its type and where its payload starts and the box ends.
#[derive(Debug, Clone, Copy)]
struct Bx {
    kind: [u8; 4],
    body: u64,
    end: u64,
}

impl Bx {
    fn body_len(self) -> u64 {
        self.end - self.body
    }
}

/// Reads the header of the box at `at`, inside a parent that ends at `end`.
fn header(data: &Sparse, at: u64, end: u64) -> Result<Bx, Stop> {
    let room = end
        .checked_sub(at)
        .filter(|r| *r >= 8)
        .ok_or(Stop::Invalid("truncated box header"))?;
    let [s0, s1, s2, s3, k0, k1, k2, k3] = data.array::<8>(at)?;
    let kind = [k0, k1, k2, k3];
    let (size, header_len) = match u32::from_be_bytes([s0, s1, s2, s3]) {
        // Extends to the end of its parent (or of the file).
        0 => (room, 8),
        1 if room >= 16 => (data.u64_at(at + 8)?, 16),
        1 => return Err(Stop::Invalid("truncated box header")),
        n => (u64::from(n), 8),
    };
    if size < header_len || size > room {
        return Err(Stop::Invalid("box size out of range"));
    }
    Ok(Bx {
        kind,
        body: at + header_len,
        end: at + size,
    })
}

/// Calls `visit` with each child box of the payload `body..end` until it returns `Some`.
/// Fewer than 8 trailing bytes (`QuickTime` list terminators, padding) are ignored.
fn walk<T>(
    data: &Sparse,
    body: u64,
    end: u64,
    mut visit: impl FnMut(Bx) -> Result<Option<T>, Stop>,
) -> Result<Option<T>, Stop> {
    let mut at = body;
    for _ in 0..MAX_CHILDREN {
        if end.saturating_sub(at) < 8 {
            return Ok(None);
        }
        let child = header(data, at, end)?;
        if let Some(found) = visit(child)? {
            return Ok(Some(found));
        }
        at = child.end;
    }
    Err(Stop::Invalid("too many child boxes"))
}

/// The first child of `parent` of type `kind`.
fn child(data: &Sparse, parent: Bx, kind: [u8; 4]) -> Result<Option<Bx>, Stop> {
    walk(data, parent.body, parent.end, |b| {
        Ok((b.kind == kind).then_some(b))
    })
}

/// Lists the audio and subtitle tracks of an MP4 or MOV file.
pub(crate) fn parse(data: &Sparse) -> Result<TrackReport, Stop> {
    let len = data.len();
    let mut at = 0;
    for _ in 0..MAX_TOP_LEVEL {
        if len - at < 8 {
            break;
        }
        let b = header(data, at, len)?;
        if &b.kind == b"moov" {
            return moov(data, b);
        }
        at = b.end;
    }
    Err(Stop::Invalid("no moov box"))
}

fn moov(data: &Sparse, moov: Bx) -> Result<TrackReport, Stop> {
    let (mut audio, mut subtitles) = (Vec::new(), Vec::new());
    let mut duration = None;
    walk(data, moov.body, moov.end, |b| {
        if &b.kind == b"mvhd" {
            duration = movie_duration(data, b)?;
        } else if &b.kind == b"trak" {
            match track(data, b)? {
                Some(Track::Audio(mut t)) if audio.len() < MAX_AUDIO_TRACKS => {
                    t.position = u32::try_from(audio.len()).unwrap_or(u32::MAX);
                    audio.push(t);
                }
                Some(Track::Subtitle(t)) if subtitles.len() < MAX_SUBTITLE_TRACKS => {
                    subtitles.push(t);
                }
                _ => {}
            }
        }
        Ok(None::<()>)
    })?;
    Ok(TrackReport {
        container: Container::Mp4,
        audio,
        subtitles,
        duration_ms: duration,
    })
}

/// The movie's duration in `mvhd`, in milliseconds. A short box, a zero time scale, or the
/// all-ones "unknown" duration (as in fragmented files) gives `None`.
fn movie_duration(data: &Sparse, mvhd: Bx) -> Result<Option<u64>, Stop> {
    // FullBox, creation and modification times, time scale, then the duration: 32-bit times
    // and duration in version 0, 64-bit ones in version 1.
    let version = if mvhd.body_len() >= 1 {
        data.u8_at(mvhd.body)?
    } else {
        return Ok(None);
    };
    let (scale_at, wide) = match version {
        0 => (12, false),
        1 => (20, true),
        _ => return Ok(None),
    };
    if mvhd.body_len() < scale_at + if wide { 12 } else { 8 } {
        return Ok(None);
    }
    let scale = data.u32_at(mvhd.body + scale_at)?;
    let duration = if wide {
        data.u64_at(mvhd.body + scale_at + 4)?
    } else {
        match data.u32_at(mvhd.body + scale_at + 4)? {
            u32::MAX => u64::MAX,
            d => u64::from(d),
        }
    };
    if duration == u64::MAX {
        return Ok(None);
    }
    // Precision loss is irrelevant at millisecond resolution.
    #[allow(clippy::cast_precision_loss)]
    let ms = duration_ms(duration as f64, f64::from(scale));
    Ok(ms)
}

/// A track this probe reports.
enum Track {
    Audio(AudioTrack),
    Subtitle(SubtitleTrack),
}

/// The track, if `trak` is a sound or subtitle track.
fn track(data: &Sparse, trak: Bx) -> Result<Option<Track>, Stop> {
    let Some(mdia) = child(data, trak, *b"mdia")? else {
        return Ok(None);
    };
    let Some(hdlr) = child(data, mdia, *b"hdlr")? else {
        return Ok(None);
    };
    // FullBox version and flags, `pre_defined` (the QuickTime component type), handler type.
    if hdlr.body_len() < 12 {
        return Err(Stop::Invalid("hdlr box too short"));
    }
    let handler = data.array::<4>(hdlr.body + 8)?;
    if !matches!(&handler, b"soun" | b"sbtl" | b"text" | b"subt" | b"clcp") {
        return Ok(None);
    }
    // FullBox flags: bit 0 marks the track enabled. `track_ID` follows the creation and
    // modification times, which are 32-bit in version 0 and 64-bit in version 1.
    let (default, id) = match child(data, trak, *b"tkhd")? {
        Some(tkhd) if tkhd.body_len() >= 4 => {
            let enabled = data.u8_at(tkhd.body + 3)? & 1 != 0;
            let id_at = if data.u8_at(tkhd.body)? == 1 { 20 } else { 12 };
            let id = if tkhd.body_len() >= id_at + 4 {
                Some(u64::from(data.u32_at(tkhd.body + id_at)?))
            } else {
                None
            };
            (enabled, id)
        }
        Some(_) => return Err(Stop::Invalid("tkhd box too short")),
        None => (true, None),
    };
    let entry = sample_entry(data, mdia)?;
    if &handler == b"soun" {
        let codec = match entry {
            Some(entry) => audio_codec(data, entry)?,
            None => AudioCodec::Other,
        };
        return Ok(Some(Track::Audio(AudioTrack {
            codec,
            default,
            id,
            position: 0,
        })));
    }
    let codec = match entry.map(|e| e.kind) {
        Some(kind) if &kind == b"tx3g" || &kind == b"text" => SubtitleCodec::Text,
        Some(kind) if &kind == b"wvtt" => SubtitleCodec::WebVtt,
        Some(kind) if &kind == b"stpp" => SubtitleCodec::Ttml,
        Some(kind) if &kind == b"c608" || &kind == b"c708" => SubtitleCodec::ClosedCaptions,
        _ => SubtitleCodec::Other,
    };
    Ok(Some(Track::Subtitle(SubtitleTrack {
        codec,
        language: media_language(data, mdia)?,
        name: None,
        default,
        forced: false,
    })))
}

/// The ISO 639-2/T language in `mdia/mdhd`: three letters packed in 15 bits.
fn media_language(data: &Sparse, mdia: Bx) -> Result<Option<String>, Stop> {
    let Some(mdhd) = child(data, mdia, *b"mdhd")? else {
        return Ok(None);
    };
    // FullBox, then times and durations of 32 bits (version 0) or 64 bits (version 1).
    let at = if mdhd.body_len() >= 1 && data.u8_at(mdhd.body)? == 1 {
        32
    } else {
        20
    };
    if mdhd.body_len() < at + 2 {
        return Err(Stop::Invalid("mdhd box too short"));
    }
    let packed = u16::from_be_bytes(data.array(mdhd.body + at)?);
    // Values below 0x400 are old QuickTime (Macintosh) language codes.
    if packed < 0x400 {
        return Ok(None);
    }
    let letter = |shift: u16| u8::try_from(((packed >> shift) & 0x1F) + 0x60).unwrap_or(b'?');
    Ok(language_tag(&[letter(10), letter(5), letter(0)]))
}

/// The first sample entry in `mdia/minf/stbl/stsd`.
fn sample_entry(data: &Sparse, mdia: Bx) -> Result<Option<Bx>, Stop> {
    let Some(minf) = child(data, mdia, *b"minf")? else {
        return Ok(None);
    };
    let Some(stbl) = child(data, minf, *b"stbl")? else {
        return Ok(None);
    };
    let Some(stsd) = child(data, stbl, *b"stsd")? else {
        return Ok(None);
    };
    // FullBox version and flags, entry count, then the entries.
    if stsd.body_len() < 8 {
        return Err(Stop::Invalid("stsd box too short"));
    }
    if data.u32_at(stsd.body + 4)? == 0 {
        return Ok(None);
    }
    header(data, stsd.body + 8, stsd.end).map(Some)
}

/// The audio codec of a sound sample entry.
fn audio_codec(data: &Sparse, entry: Bx) -> Result<AudioCodec, Stop> {
    Ok(match &entry.kind {
        b"mp4a" => mp4a_codec(data, entry)?,
        b"ac-3" => AudioCodec::Ac3,
        b"ec-3" => AudioCodec::Eac3,
        b"ac-4" => AudioCodec::Ac4,
        b"mlpa" => AudioCodec::TrueHd,
        b"dtsc" | b"dtsh" | b"dtsl" | b"dtse" | b"dtsx" | b"dtsy" => AudioCodec::Dts,
        b"Opus" => AudioCodec::Opus,
        b"fLaC" => AudioCodec::Flac,
        b".mp3" => AudioCodec::Mp3,
        b"alac" => AudioCodec::Alac,
        b"sowt" | b"twos" | b"lpcm" | b"ipcm" | b"fpcm" | b"in24" | b"in32" | b"fl32" | b"fl64"
        | b"raw " => AudioCodec::Pcm,
        _ => AudioCodec::Other,
    })
}

/// The codec in an `mp4a` sample entry, from the object type in its `esds` box (directly or
/// inside a `QuickTime` `wave` box). AAC if there is none.
fn mp4a_codec(data: &Sparse, entry: Bx) -> Result<AudioCodec, Stop> {
    // SampleEntry (6 reserved bytes, data reference index), then the sound description. Its
    // QuickTime version sets where the child boxes start: version 1 adds 16 bytes, version 2
    // adds 36. ISO files may use version 1 without the extra fields, so both are tried.
    if entry.body_len() < 28 {
        return Err(Stop::Invalid("mp4a sample entry too short"));
    }
    let version = u16::from_be_bytes(data.array(entry.body + 8)?);
    let starts: &[u64] = match version {
        1 => &[44, 28],
        2 => &[64],
        _ => &[28],
    };
    for &skip in starts {
        let found = walk(data, entry.body + skip, entry.end, |b| match &b.kind {
            b"esds" => esds_codec(data, b),
            b"wave" => walk(data, b.body, b.end, |w| {
                if &w.kind == b"esds" {
                    esds_codec(data, w)
                } else {
                    Ok(None)
                }
            }),
            _ => Ok(None),
        });
        match found {
            Ok(Some(codec)) => return Ok(codec),
            // Not the layout this version suggested; try the next one.
            Ok(None) | Err(Stop::Invalid(_)) => {}
            Err(stop) => return Err(stop),
        }
    }
    Ok(AudioCodec::Aac)
}

/// The codec named by the object type in an `esds` box, or `None` if it cannot be found.
fn esds_codec(data: &Sparse, esds: Bx) -> Result<Option<AudioCodec>, Stop> {
    // FullBox version and flags, then an ES_Descriptor (tag 3).
    let (tag, es_end, mut at) = descriptor(data, esds.body + 4, esds.end)?;
    if tag != 3 || es_end - at < 3 {
        return Ok(None);
    }
    // ES_ID, then flags saying which optional fields follow.
    let flags = data.u8_at(at + 2)?;
    at += 3;
    if flags & 0x80 != 0 {
        at += 2; // dependsOn_ES_ID
    }
    if flags & 0x40 != 0 {
        at = at
            .checked_add(1 + u64::from(data.u8_at(at)?))
            .ok_or(Stop::Invalid("esds URL too long"))?;
    }
    if flags & 0x20 != 0 {
        at += 2; // OCR_ES_Id
    }
    // DecoderConfigDescriptor (tag 4); its first byte is the object type.
    let (tag, end, at) = descriptor(data, at, es_end)?;
    if tag != 4 || end == at {
        return Ok(None);
    }
    Ok(Some(object_type(data.u8_at(at)?)))
}

/// Reads an MPEG-4 descriptor header at `at`, inside a parent ending at `end`: its tag, where
/// its payload ends, and where the payload starts. The length takes 1 to 4 bytes of 7 bits.
fn descriptor(data: &Sparse, at: u64, end: u64) -> Result<(u8, u64, u64), Stop> {
    if at >= end {
        return Err(Stop::Invalid("truncated esds descriptor"));
    }
    let tag = data.u8_at(at)?;
    let mut pos = at + 1;
    let mut len = 0u64;
    for i in 0..4 {
        if pos >= end {
            return Err(Stop::Invalid("truncated esds descriptor"));
        }
        let b = data.u8_at(pos)?;
        pos += 1;
        len = (len << 7) | u64::from(b & 0x7F);
        if b & 0x80 == 0 {
            break;
        }
        if i == 3 {
            return Err(Stop::Invalid("esds descriptor length too long"));
        }
    }
    if len > end - pos {
        return Err(Stop::Invalid("esds descriptor runs past its box"));
    }
    Ok((tag, pos + len, pos))
}

/// Maps an MPEG-4 `objectTypeIndication` to a codec.
fn object_type(oti: u8) -> AudioCodec {
    match oti {
        0x40 | 0x66..=0x68 => AudioCodec::Aac,
        0x69 | 0x6B => AudioCodec::Mp3,
        0xA5 => AudioCodec::Ac3,
        0xA6 => AudioCodec::Eac3,
        0xA9..=0xAC => AudioCodec::Dts,
        0xAD => AudioCodec::Opus,
        0xDD => AudioCodec::Vorbis,
        _ => AudioCodec::Other,
    }
}

/// The layout of an MP4 or MOV file (see [`crate::layout`]): the top-level boxes a player
/// reads up to `moov`, the first chunk of any track, and the first video track's sync samples.
pub(crate) fn layout(data: &Sparse) -> Result<MediaLayout, Stop> {
    let len = data.len();
    let mut ranges = Vec::new();
    let mut mdat_first = false;
    let mut found = None;
    let mut at = 0;
    for _ in 0..MAX_TOP_LEVEL {
        if len - at < 8 {
            break;
        }
        let b = header(data, at, len)?;
        match &b.kind {
            b"moov" => {
                push_range(&mut ranges, at..b.end);
                found = Some(b);
                break;
            }
            // A player reads the header and skips the payload.
            b"mdat" | b"free" | b"skip" | b"wide" => {
                mdat_first |= &b.kind == b"mdat";
                push_range(&mut ranges, at..b.body);
            }
            _ => push_range(&mut ranges, at..b.end),
        }
        at = b.end;
    }
    let moov = found.ok_or(Stop::Invalid("no moov box"))?;
    let mut duration = None;
    let mut media_start: Option<u64> = None;
    let mut video: Option<(String, Vec<SeekPoint>)> = None;
    walk(data, moov.body, moov.end, |b| {
        if &b.kind == b"mvhd" {
            duration = movie_duration(data, b)?;
        } else if &b.kind == b"trak"
            && let Some(t) = track_layout(data, b, video.is_none())?
        {
            media_start = Some(media_start.map_or(t.first_chunk, |m| m.min(t.first_chunk)));
            if video.is_none() {
                video = t.video;
            }
        }
        Ok(None::<()>)
    })?;
    let media_start =
        media_start.ok_or(Stop::Invalid("no samples in moov (a fragmented MP4 file?)"))?;
    let (video_codec, seek_points) = match video {
        Some((codec, points)) => (Some(codec), thin(points)),
        None => (None, Vec::new()),
    };
    Ok(MediaLayout {
        container: Container::Mp4,
        video_codec,
        header: ranges,
        media_start,
        seek_index: None,
        seek_points,
        duration_ms: duration,
        index_at_end: mdat_first,
    })
}

/// What [`layout`] needs from one track.
struct TrackLayout {
    /// Offset of its first chunk.
    first_chunk: u64,
    /// For a video track (when asked for): its codec and sync samples.
    video: Option<(String, Vec<SeekPoint>)>,
}

/// The track's first chunk and, if it is a video track and `want_video`, its keyframes.
/// `None` for a track without samples.
fn track_layout(data: &Sparse, trak: Bx, want_video: bool) -> Result<Option<TrackLayout>, Stop> {
    let Some(mdia) = child(data, trak, *b"mdia")? else {
        return Ok(None);
    };
    let Some(stbl) = child(data, mdia, *b"minf")?
        .map(|minf| child(data, minf, *b"stbl"))
        .transpose()?
        .flatten()
    else {
        return Ok(None);
    };
    let chunks = match (child(data, stbl, *b"stco")?, child(data, stbl, *b"co64")?) {
        (Some(stco), _) => Table::new(data, stco, 4, 4)?,
        (None, Some(co64)) => Table::new(data, co64, 4, 8)?,
        (None, None) => return Ok(None),
    };
    if chunks.count == 0 {
        return Ok(None);
    }
    let first_chunk = chunks.uint(data, 0)?;
    let is_video = match child(data, mdia, *b"hdlr")? {
        Some(hdlr) if hdlr.body_len() >= 12 => &data.array::<4>(hdlr.body + 8)? == b"vide",
        _ => false,
    };
    let video = if want_video && is_video {
        let codec = sample_entry(data, mdia)?
            .map_or_else(|| "unknown".to_owned(), |e| video_codec_name(e.kind));
        Some((codec, sync_points(data, mdia, stbl, &chunks)?))
    } else {
        None
    };
    Ok(Some(TrackLayout { first_chunk, video }))
}

/// A short name for a video sample entry type.
fn video_codec_name(kind: [u8; 4]) -> String {
    let name = match &kind {
        b"avc1" | b"avc2" | b"avc3" | b"avc4" => "H.264",
        b"hvc1" | b"hev1" => "HEVC",
        b"dvh1" | b"dvhe" => "HEVC (Dolby Vision)",
        b"av01" => "AV1",
        b"vp08" => "VP8",
        b"vp09" => "VP9",
        b"mp4v" => "MPEG-4 Part 2",
        b"mjpa" | b"mjpb" | b"jpeg" => "Motion JPEG",
        b"apch" | b"apcn" | b"apcs" | b"apco" | b"ap4h" => "ProRes",
        other => return String::from_utf8_lossy(other).trim().to_owned(),
    };
    name.to_owned()
}

/// A sample table: `count` entries of `width` bytes, after `skip` bytes of the box and the
/// 4-byte count.
struct Table {
    start: u64,
    count: u64,
    width: u64,
}

impl Table {
    fn new(data: &Sparse, b: Bx, skip: u64, width: u64) -> Result<Self, Stop> {
        if b.body_len() < skip + 4 {
            return Err(Stop::Invalid("sample table too short"));
        }
        let count = u64::from(data.u32_at(b.body + skip)?);
        let start = b.body + skip + 4;
        if count.saturating_mul(width) > b.end - start {
            return Err(Stop::Invalid("sample table runs past its box"));
        }
        Ok(Self {
            start,
            count,
            width,
        })
    }

    /// The table's bytes, read in one piece.
    fn bytes<'a>(&self, data: &'a Sparse) -> Result<&'a [u8], Stop> {
        let len = usize::try_from(self.count * self.width)
            .map_err(|_| Stop::Invalid("sample table too large"))?;
        data.get(self.start, len)
    }

    /// Entry `i` as an unsigned integer (4 or 8 bytes wide).
    fn uint(&self, data: &Sparse, i: u64) -> Result<u64, Stop> {
        let at = self.start + i * self.width;
        if self.width == 8 {
            data.u64_at(at)
        } else {
            data.u32_at(at).map(u64::from)
        }
    }
}

/// Big-endian `u32` number `i` of `bytes` (0 past the end).
fn be32(bytes: &[u8], i: usize) -> u32 {
    bytes
        .get(i * 4..i * 4 + 4)
        .and_then(|b| b.try_into().ok())
        .map_or(0, u32::from_be_bytes)
}

/// The sync samples (keyframes) of a video track, with their decode times and offsets, from
/// `stts`, `stss`, `stsc`, `stsz`, and the chunk offsets.
// One pass over the sample tables, in step with each other.
#[allow(clippy::too_many_lines)]
fn sync_points(data: &Sparse, mdia: Bx, stbl: Bx, chunks: &Table) -> Result<Vec<SeekPoint>, Stop> {
    let mdhd = child(data, mdia, *b"mdhd")?.ok_or(Stop::Invalid("no mdhd box"))?;
    let scale_at = if mdhd.body_len() >= 1 && data.u8_at(mdhd.body)? == 1 {
        20
    } else {
        12
    };
    if mdhd.body_len() < scale_at + 4 {
        return Err(Stop::Invalid("mdhd box too short"));
    }
    let scale = u128::from(data.u32_at(mdhd.body + scale_at)?);
    if scale == 0 {
        return Err(Stop::Invalid("zero time scale"));
    }
    let need =
        |kind: &[u8; 4]| child(data, stbl, *kind)?.ok_or(Stop::Invalid("missing sample table"));
    let size_box = need(b"stsz")?;
    if size_box.body_len() < 12 {
        return Err(Stop::Invalid("stsz box too short"));
    }
    let fixed_size = u64::from(data.u32_at(size_box.body + 4)?);
    let samples = u64::from(data.u32_at(size_box.body + 8)?);
    if samples > MAX_SAMPLES {
        return Err(Stop::Invalid("too many samples"));
    }
    let sizes = if fixed_size == 0 {
        Table::new(data, size_box, 8, 4)?.bytes(data)?
    } else {
        &[]
    };
    let stts = Table::new(data, need(b"stts")?, 4, 8)?;
    let stts_bytes = stts.bytes(data)?;
    let runs = Table::new(data, need(b"stsc")?, 4, 12)?;
    let runs_bytes = runs.bytes(data)?;
    let syncs = child(data, stbl, *b"stss")?
        .map(|b| Table::new(data, b, 4, 4))
        .transpose()?;
    let sync_bytes = syncs.as_ref().map(|t| t.bytes(data)).transpose()?;
    let offsets = chunks.bytes(data)?;
    let chunk_offset = |i: usize| -> u64 {
        if chunks.width == 8 {
            offsets
                .get(i * 8..i * 8 + 8)
                .and_then(|b| b.try_into().ok())
                .map_or(0, u64::from_be_bytes)
        } else {
            u64::from(be32(offsets, i))
        }
    };
    // Without `stss` every sample is a sync sample; keep a bounded, even selection.
    let stride = if sync_bytes.is_none() {
        samples.div_ceil(MAX_SEEK_POINTS as u64).max(1)
    } else {
        1
    };
    let to_usize = |n: u64| usize::try_from(n).map_err(|_| Stop::Invalid("table too large"));
    let (stts_len, runs_len) = (to_usize(stts.count)?, to_usize(runs.count)?);
    let sync_len = syncs.as_ref().map_or(Ok(0), |t| to_usize(t.count))?;

    let mut points = Vec::new();
    let (mut stts_at, mut stts_left, mut delta) = (0usize, 0u64, 0u64);
    let (mut run_at, mut sync_at) = (0usize, 0usize);
    let mut time: u128 = 0;
    let mut sample: u64 = 0;
    for chunk in 0..to_usize(chunks.count)? {
        // Entries give 1-based first chunks; the last one that has started applies.
        while run_at + 1 < runs_len
            && u64::from(be32(runs_bytes, (run_at + 1) * 3)) <= chunk as u64 + 1
        {
            run_at += 1;
        }
        let per_chunk = u64::from(be32(runs_bytes, run_at * 3 + 1));
        let mut offset = chunk_offset(chunk);
        for _ in 0..per_chunk {
            if sample >= samples {
                return Ok(points);
            }
            let sync = match sync_bytes {
                Some(sync_table) => {
                    while sync_at < sync_len && u64::from(be32(sync_table, sync_at)) < sample + 1 {
                        sync_at += 1;
                    }
                    sync_at < sync_len && u64::from(be32(sync_table, sync_at)) == sample + 1
                }
                None => sample.is_multiple_of(stride),
            };
            if sync {
                points.push(SeekPoint {
                    time_ms: u64::try_from(time * 1000 / scale).unwrap_or(u64::MAX),
                    offset,
                });
            }
            let size = if fixed_size == 0 {
                u64::from(be32(sizes, to_usize(sample)?))
            } else {
                fixed_size
            };
            offset = offset.saturating_add(size);
            while stts_left == 0 && stts_at < stts_len {
                stts_left = u64::from(be32(stts_bytes, stts_at * 2));
                delta = u64::from(be32(stts_bytes, stts_at * 2 + 1));
                stts_at += 1;
            }
            stts_left = stts_left.saturating_sub(1);
            time += u128::from(delta);
            sample += 1;
        }
    }
    Ok(points)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A box with a 32-bit size.
    pub(crate) fn bx(kind: &[u8], body: &[u8]) -> Vec<u8> {
        let mut out = u32::try_from(body.len() + 8)
            .unwrap()
            .to_be_bytes()
            .to_vec();
        out.extend_from_slice(kind);
        out.extend_from_slice(body);
        out
    }

    fn boxes(kind: &[u8], children: &[Vec<u8>]) -> Vec<u8> {
        bx(kind, &children.concat())
    }

    fn hdlr(handler: &[u8]) -> Vec<u8> {
        let mut body = vec![0; 8];
        body.extend_from_slice(handler);
        body.extend_from_slice(&[0; 13]);
        bx(b"hdlr", &body)
    }

    fn tkhd(enabled: bool) -> Vec<u8> {
        let mut body = vec![0; 84];
        body[3] = if enabled { 3 } else { 2 };
        bx(b"tkhd", &body)
    }

    /// A sound sample entry (version 0) with optional child boxes.
    pub(crate) fn sound_entry(kind: &[u8], children: &[Vec<u8>]) -> Vec<u8> {
        let mut body = vec![0; 28];
        body[7] = 1; // data reference index
        body[17] = 2; // channels
        body.extend_from_slice(&children.concat());
        bx(kind, &body)
    }

    /// An `esds` box with the given object type.
    pub(crate) fn esds(object_type: u8) -> Vec<u8> {
        let decoder = [&[0x04, 13, object_type, 0x15][..], &[0; 11]].concat();
        let es = [
            &[0x03, u8::try_from(decoder.len() + 3).unwrap(), 0, 1, 0][..],
            &decoder,
        ]
        .concat();
        bx(b"esds", &[&[0, 0, 0, 0][..], &es].concat())
    }

    /// A track: `handler` is `soun` or `vide`; `table_bytes` pads the sample tables.
    pub(crate) fn trak(handler: &[u8], enabled: bool, entry: &[u8], table_bytes: usize) -> Vec<u8> {
        let stsd = bx(b"stsd", &[&[0, 0, 0, 0, 0, 0, 0, 1][..], entry].concat());
        let stbl = boxes(b"stbl", &[stsd, bx(b"stsz", &vec![0; table_bytes])]);
        let minf = boxes(
            b"minf",
            &[bx(b"smhd", &[0; 8]), bx(b"dinf", &[0; 28]), stbl],
        );
        let mdia = boxes(b"mdia", &[bx(b"mdhd", &[0; 24]), hdlr(handler), minf]);
        boxes(b"trak", &[tkhd(enabled), mdia])
    }

    pub(crate) fn moov(traks: &[Vec<u8>]) -> Vec<u8> {
        let mut children = vec![bx(b"mvhd", &[0; 100])];
        children.extend_from_slice(traks);
        boxes(b"moov", &children)
    }

    pub(crate) fn ftyp() -> Vec<u8> {
        bx(b"ftyp", b"isom\0\0\x02\0isomiso2avc1mp41")
    }

    fn parse_all(file: Vec<u8>) -> Result<TrackReport, Stop> {
        parse(&Sparse::whole(file))
    }

    fn codecs(report: &TrackReport) -> Vec<(AudioCodec, bool)> {
        report.audio.iter().map(|t| (t.codec, t.default)).collect()
    }

    /// An `mvhd` box of `version` with a time scale and duration.
    fn mvhd(version: u8, scale: u32, duration: u64) -> Vec<u8> {
        let mut body = vec![version, 0, 0, 0];
        let time_len = if version == 1 { 8 } else { 4 };
        body.extend_from_slice(&vec![0; 2 * time_len]);
        body.extend_from_slice(&scale.to_be_bytes());
        if version == 1 {
            body.extend_from_slice(&duration.to_be_bytes());
        } else {
            body.extend_from_slice(&u32::try_from(duration).unwrap().to_be_bytes());
        }
        body.extend_from_slice(&[0; 80]);
        bx(b"mvhd", &body)
    }

    fn duration_of(mvhd: Vec<u8>) -> Option<u64> {
        let audio = trak(b"soun", true, &sound_entry(b"ac-3", &[]), 100);
        let file = [ftyp(), boxes(b"moov", &[mvhd, audio])].concat();
        let report = parse_all(file).unwrap();
        assert_eq!(codecs(&report), [(AudioCodec::Ac3, true)]);
        report.duration_ms
    }

    #[test]
    fn reads_the_movie_duration() {
        assert_eq!(duration_of(mvhd(0, 600, 600 * 95)), Some(95_000));
        assert_eq!(
            duration_of(mvhd(1, 90_000, 90_000 * 3_600)),
            Some(3_600_000)
        );
        assert_eq!(duration_of(mvhd(0, 0, 1000)), None, "no time scale");
        assert_eq!(duration_of(mvhd(0, 1000, 0)), None, "fragmented");
        assert_eq!(
            duration_of(mvhd(0, 1000, u64::from(u32::MAX))),
            None,
            "unknown"
        );
        assert_eq!(duration_of(mvhd(1, 1000, u64::MAX)), None, "unknown");
        assert_eq!(
            duration_of(bx(b"mvhd", &[1, 0, 0, 0, 0])),
            None,
            "short box"
        );
        assert_eq!(duration_of(mvhd(2, 1000, 1000)), None, "bad version");
    }

    #[test]
    fn finds_sound_tracks_and_skips_video() {
        let video = trak(b"vide", true, &bx(b"avc1", &[0; 78]), 10_000);
        let file = [
            ftyp(),
            moov(&[
                video,
                trak(b"soun", true, &sound_entry(b"ac-3", &[]), 100),
                trak(b"soun", false, &sound_entry(b"mp4a", &[esds(0x40)]), 100),
            ]),
            bx(b"mdat", &[0; 64]),
        ]
        .concat();
        let report = parse_all(file).unwrap();
        assert_eq!(report.container, Container::Mp4);
        assert_eq!(
            codecs(&report),
            [(AudioCodec::Ac3, true), (AudioCodec::Aac, false)]
        );
        assert_eq!(report.primary().unwrap().codec, AudioCodec::Ac3);
        assert_eq!(
            report.playable_alternative().unwrap().codec,
            AudioCodec::Aac
        );
    }

    #[test]
    fn maps_sample_entry_formats() {
        for (kind, codec) in [
            (b"ec-3", AudioCodec::Eac3),
            (b"dtsc", AudioCodec::Dts),
            (b"dtsh", AudioCodec::Dts),
            (b"dtsl", AudioCodec::Dts),
            (b"dtse", AudioCodec::Dts),
            (b"mlpa", AudioCodec::TrueHd),
            (b"Opus", AudioCodec::Opus),
            (b"fLaC", AudioCodec::Flac),
            (b".mp3", AudioCodec::Mp3),
            (b"sowt", AudioCodec::Pcm),
            (b"zzzz", AudioCodec::Other),
        ] {
            let file = [
                ftyp(),
                moov(&[trak(b"soun", true, &sound_entry(kind, &[]), 0)]),
            ]
            .concat();
            assert_eq!(
                codecs(&parse_all(file).unwrap()),
                [(codec, true)],
                "{kind:?}"
            );
        }
    }

    #[test]
    fn reads_the_object_type_of_mp4a() {
        for (oti, codec) in [
            (0x40, AudioCodec::Aac),
            (0x67, AudioCodec::Aac),
            (0x6B, AudioCodec::Mp3),
            (0xA5, AudioCodec::Ac3),
            (0xA6, AudioCodec::Eac3),
            (0xA9, AudioCodec::Dts),
            (0xAD, AudioCodec::Opus),
            (0xFE, AudioCodec::Other),
        ] {
            let entry = sound_entry(b"mp4a", &[esds(oti)]);
            let file = [ftyp(), moov(&[trak(b"soun", true, &entry, 0)])].concat();
            assert_eq!(
                codecs(&parse_all(file).unwrap()),
                [(codec, true)],
                "{oti:#x}"
            );
        }
        // No esds: assume AAC.
        let entry = sound_entry(b"mp4a", &[]);
        let file = [ftyp(), moov(&[trak(b"soun", true, &entry, 0)])].concat();
        assert_eq!(codecs(&parse_all(file).unwrap()), [(AudioCodec::Aac, true)]);
    }

    #[test]
    fn reads_quicktime_sound_descriptions() {
        // Version 1 with its 16 extra bytes, and the esds inside a `wave` box.
        let mut body = vec![0; 44];
        body[9] = 1; // version 1
        body.extend_from_slice(&bx(
            b"wave",
            &[bx(b"frma", b"mp4a"), esds(0x6B), bx(b"\0\0\0\0", &[])].concat(),
        ));
        let entry = bx(b"mp4a", &body);
        let file = [moov(&[trak(b"soun", true, &entry, 0)]), bx(b"mdat", &[])].concat();
        assert_eq!(codecs(&parse_all(file).unwrap()), [(AudioCodec::Mp3, true)]);

        // ISO version 1 entries have no extra fields.
        let mut body = vec![0; 28];
        body[9] = 1;
        body.extend_from_slice(&esds(0xA6));
        let entry = bx(b"mp4a", &body);
        let file = [ftyp(), moov(&[trak(b"soun", true, &entry, 0)])].concat();
        assert_eq!(
            codecs(&parse_all(file).unwrap()),
            [(AudioCodec::Eac3, true)]
        );
    }

    #[test]
    fn handles_moov_after_a_large_mdat() {
        // A 64-bit size: a 16-byte header and 8 bytes of data.
        let mut mdat = vec![0, 0, 0, 1];
        mdat.extend_from_slice(b"mdat");
        mdat.extend_from_slice(&24u64.to_be_bytes());
        mdat.extend_from_slice(&[0; 8]);
        let file = [
            ftyp(),
            mdat,
            moov(&[trak(b"soun", true, &sound_entry(b"dtsl", &[]), 0)]),
        ]
        .concat();
        assert_eq!(codecs(&parse_all(file).unwrap()), [(AudioCodec::Dts, true)]);
    }

    fn mdhd(version: u8, language: [u8; 3]) -> Vec<u8> {
        let at = if version == 1 { 32 } else { 20 };
        let mut body = vec![0; at + 4];
        body[0] = version;
        let packed = language
            .iter()
            .fold(0u16, |v, c| (v << 5) | u16::from(c - 0x60));
        body[at..at + 2].copy_from_slice(&packed.to_be_bytes());
        bx(b"mdhd", &body)
    }

    fn subtitle_trak(handler: &[u8], enabled: bool, entry: &[u8], mdhd: Vec<u8>) -> Vec<u8> {
        let stsd = bx(
            b"stsd",
            &[&[0, 0, 0, 0, 0, 0, 0, 1][..], &bx(entry, &[0; 8])].concat(),
        );
        let minf = boxes(b"minf", &[bx(b"nmhd", &[0; 4]), boxes(b"stbl", &[stsd])]);
        boxes(
            b"trak",
            &[tkhd(enabled), boxes(b"mdia", &[mdhd, hdlr(handler), minf])],
        )
    }

    #[test]
    fn lists_subtitle_tracks_with_languages() {
        let file = [
            ftyp(),
            moov(&[
                trak(b"soun", true, &sound_entry(b"mp4a", &[esds(0x40)]), 0),
                subtitle_trak(b"sbtl", true, b"wvtt", mdhd(0, *b"fra")),
                subtitle_trak(b"text", false, b"tx3g", mdhd(1, *b"eng")),
                subtitle_trak(b"clcp", false, b"c608", mdhd(0, *b"und")),
                subtitle_trak(b"subt", false, b"stpp", bx(b"mdhd", &[0; 24])),
            ]),
        ]
        .concat();
        let report = parse_all(file).unwrap();
        assert_eq!(codecs(&report), [(AudioCodec::Aac, true)]);
        let subs: Vec<_> = report
            .subtitles
            .iter()
            .map(|s| (s.codec, s.language.as_deref(), s.default))
            .collect();
        assert_eq!(
            subs,
            [
                (SubtitleCodec::WebVtt, Some("fra"), true),
                (SubtitleCodec::Text, Some("eng"), false),
                (SubtitleCodec::ClosedCaptions, None, false),
                (SubtitleCodec::Ttml, None, false),
            ]
        );
        // A truncated mdhd is malformed.
        let file = [
            ftyp(),
            moov(&[subtitle_trak(b"sbtl", true, b"wvtt", bx(b"mdhd", &[0; 8]))]),
        ]
        .concat();
        assert!(matches!(parse_all(file), Err(Stop::Invalid(_))));
    }

    #[test]
    fn a_file_without_sound_tracks_has_no_audio() {
        let file = [
            ftyp(),
            moov(&[trak(b"vide", true, &bx(b"avc1", &[0; 78]), 0)]),
        ]
        .concat();
        assert_eq!(parse_all(file).unwrap().audio, []);
    }

    #[test]
    fn rejects_malformed_files() {
        let invalid = |file: Vec<u8>| matches!(parse_all(file), Err(Stop::Invalid(_)));
        // No moov at all; mdat to the end of the file.
        assert!(invalid([ftyp(), bx(b"mdat", &[1; 32])].concat()));
        assert!(invalid(
            [ftyp(), vec![0, 0, 0, 0], b"mdat".to_vec(), vec![0; 8]].concat()
        ));
        // A box larger than the file, or smaller than its header.
        let mut file = ftyp();
        file.extend_from_slice(&[0xFF, 0xFF, 0, 0]);
        file.extend_from_slice(b"moov");
        assert!(invalid(file));
        assert!(invalid(
            [ftyp(), vec![0, 0, 0, 4], b"moov".to_vec()].concat()
        ));
        // A 64-bit size that is too small.
        assert!(invalid(
            [
                vec![0, 0, 0, 1],
                b"moov".to_vec(),
                3u64.to_be_bytes().to_vec()
            ]
            .concat()
        ));
        // A child box that overflows its parent.
        let mut moov_box = moov(&[trak(b"soun", true, &sound_entry(b"ac-3", &[]), 0)]);
        moov_box[8 + 2] = 0xFF; // mvhd size
        assert!(invalid([ftyp(), moov_box].concat()));
        // A short hdlr.
        let bad = boxes(b"trak", &[boxes(b"mdia", &[bx(b"hdlr", &[0; 4])])]);
        assert!(invalid([ftyp(), moov(&[bad])].concat()));
        // An esds whose descriptor length runs past the box falls back to the next layout.
        let mut e = esds(0xA5);
        e[13] = 0x7F;
        let file = [
            ftyp(),
            moov(&[trak(b"soun", true, &sound_entry(b"mp4a", &[e]), 0)]),
        ]
        .concat();
        assert_eq!(codecs(&parse_all(file).unwrap()), [(AudioCodec::Aac, true)]);
    }

    #[test]
    fn limits_the_number_of_boxes() {
        let mut file = ftyp();
        for _ in 0..100 {
            file.extend_from_slice(&bx(b"free", &[]));
        }
        assert!(matches!(parse_all(file), Err(Stop::Invalid(_))));
        let many: Vec<Vec<u8>> = (0..300).map(|_| bx(b"udta", &[])).collect();
        assert!(matches!(
            parse_all([ftyp(), boxes(b"moov", &many)].concat()),
            Err(Stop::Invalid(_))
        ));
    }

    #[test]
    fn asks_only_for_headers_it_needs() {
        // The first chunk holds only the ftyp and the mdat header: the parser asks for the
        // bytes right after the mdat, not for the mdat itself.
        let file = [ftyp(), bx(b"mdat", &vec![0; 100_000]), moov(&[])].concat();
        let mut data = Sparse::new(file.len() as u64);
        data.insert(0, file[..40].to_vec());
        let moov_at = (ftyp().len() + 100_008) as u64;
        assert_eq!(parse(&data), Err(Stop::Need(moov_at..moov_at + 8)));
    }

    /// A `FullBox` with `count` entries, given as `u32` words.
    fn table(kind: &[u8], count: usize, words: &[u32]) -> Vec<u8> {
        let mut body = vec![0; 4];
        body.extend_from_slice(&u32::try_from(count).unwrap().to_be_bytes());
        for e in words {
            body.extend_from_slice(&e.to_be_bytes());
        }
        bx(kind, &body)
    }

    /// An H.264 track of samples of `size` bytes, one per chunk at `offsets`, one second
    /// apart, with the 1-based sync samples `sync`.
    pub(crate) fn video_trak(offsets: &[u32], size: u32, sync: &[u32]) -> Vec<u8> {
        let n = u32::try_from(offsets.len()).unwrap();
        let stsd = bx(
            b"stsd",
            &[&[0, 0, 0, 0, 0, 0, 0, 1][..], &bx(b"avc1", &[0; 78])].concat(),
        );
        let stbl = boxes(
            b"stbl",
            &[
                stsd,
                table(b"stts", 1, &[n, 1000]),
                table(b"stss", sync.len(), sync),
                // One run: from chunk 1, one sample per chunk, description 1.
                table(b"stsc", 1, &[1, 1, 1]),
                bx(
                    b"stsz",
                    &[[0; 4], size.to_be_bytes(), n.to_be_bytes()].concat(),
                ),
                table(b"stco", offsets.len(), offsets),
            ],
        );
        let mut mdhd = vec![0; 24];
        mdhd[12..16].copy_from_slice(&1000u32.to_be_bytes());
        let mdia = boxes(
            b"mdia",
            &[bx(b"mdhd", &mdhd), hdlr(b"vide"), boxes(b"minf", &[stbl])],
        );
        boxes(b"trak", &[tkhd(true), mdia])
    }

    /// Four 100-byte samples with keyframes at 0 s and 2 s, in a file with `moov` before
    /// (`faststart`) or after the media.
    fn layout_file(faststart: bool) -> (Vec<u8>, u64, u64) {
        let mdat = bx(b"mdat", &[0; 400]);
        let moov_len = moov(&[video_trak(&[0; 4], 100, &[1, 3])]).len() as u64;
        let base = ftyp().len() as u64 + 8 + if faststart { moov_len } else { 0 };
        let offsets: Vec<u32> = (0..4)
            .map(|i| u32::try_from(base + i * 100).unwrap())
            .collect();
        let moov = moov(&[video_trak(&offsets, 100, &[1, 3])]);
        let file = if faststart {
            [ftyp(), moov, mdat].concat()
        } else {
            [ftyp(), mdat, moov].concat()
        };
        (file, base, moov_len)
    }

    #[test]
    fn layout_of_a_faststart_file() {
        let (file, base, moov_len) = layout_file(true);
        let layout = layout(&Sparse::whole(file)).unwrap();
        let ftyp_len = ftyp().len() as u64;
        assert_eq!(layout.header, vec![0..ftyp_len + moov_len]);
        assert_eq!(layout.media_start, base);
        assert!(!layout.index_at_end);
        assert_eq!(layout.video_codec.as_deref(), Some("H.264"));
        assert_eq!(
            layout.seek_points,
            [
                SeekPoint {
                    time_ms: 0,
                    offset: base
                },
                SeekPoint {
                    time_ms: 2000,
                    offset: base + 200
                }
            ]
        );
    }

    #[test]
    fn layout_of_a_file_with_moov_at_the_end() {
        let (file, base, moov_len) = layout_file(false);
        let len = file.len() as u64;
        let layout = layout(&Sparse::whole(file)).unwrap();
        // The head up to the mdat header, then moov at the end.
        assert_eq!(layout.header, [0..base, len - moov_len..len]);
        assert_eq!(layout.media_start, base);
        assert!(layout.index_at_end);
        assert_eq!(layout.seek_points[1].offset, base + 200);
        assert_eq!(layout.seek_point(3500), Some(layout.seek_points[1]));
    }

    #[test]
    fn layout_without_samples_is_an_error() {
        // Fragmented files keep their samples outside moov.
        let file = [ftyp(), moov(&[]), bx(b"moof", &[0; 16])].concat();
        assert!(matches!(
            layout(&Sparse::whole(file)),
            Err(Stop::Invalid(_))
        ));
    }
}
