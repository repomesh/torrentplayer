// SPDX-License-Identifier: MIT OR Apache-2.0

//! Matroska and `WebM`: the audio and subtitle `TrackEntry` elements in `Segment/Tracks`.
//!
//! `Tracks` normally comes before the first `Cluster`, within the first few kilobytes. If a
//! `Cluster` comes first, the `SeekHead` says where `Tracks` is. Elements are skipped by their
//! size without reading their contents. The `Info` element before `Tracks` gives the duration.

use crate::audio::{AudioCodec, AudioTrack, MAX_AUDIO_TRACKS};
use crate::sparse::{Sparse, Stop};
use crate::tracks::{
    Container, MAX_SUBTITLE_TRACKS, SubtitleCodec, SubtitleTrack, TrackReport, duration_ms,
    language_tag, track_name,
};

const EBML: u32 = 0x1A45_DFA3;
const SEGMENT: u32 = 0x1853_8067;
const SEEK_HEAD: u32 = 0x114D_9B74;
const SEEK: u32 = 0x4DBB;
const SEEK_ID: u32 = 0x53AB;
const SEEK_POSITION: u32 = 0x53AC;
const INFO: u32 = 0x1549_A966;
const TIMESTAMP_SCALE: u32 = 0x2A_D7B1;
const DURATION: u32 = 0x4489;
const TRACKS: u32 = 0x1654_AE6B;
const TRACK_ENTRY: u32 = 0xAE;
const TRACK_NUMBER: u32 = 0xD7;
const TRACK_TYPE: u32 = 0x83;
const FLAG_ENABLED: u32 = 0xB9;
const FLAG_DEFAULT: u32 = 0x88;
const CODEC_ID: u32 = 0x86;
const FLAG_FORCED: u32 = 0x55AA;
const LANGUAGE: u32 = 0x0022_B59C;
const LANGUAGE_BCP47: u32 = 0x0022_B59D;
const NAME: u32 = 0x536E;
const CLUSTER: u32 = 0x1F43_B675;

/// `TrackType` of audio tracks.
const AUDIO: u64 = 2;
/// `TrackType` of subtitle tracks.
const SUBTITLE: u64 = 0x11;

/// Top-level elements visited before the `Segment`.
const MAX_TOP_LEVEL: usize = 16;
/// `Segment` children visited before `Tracks`.
const MAX_SEGMENT_CHILDREN: usize = 256;
/// Children visited per master element.
const MAX_CHILDREN: usize = 512;
/// Longest `CodecID` read; longer ones are reported as [`AudioCodec::Other`].
const MAX_CODEC_ID: usize = 64;
/// Longest name or language string read; longer ones are ignored.
const MAX_STRING: usize = 1024;

/// Whether `head` starts with the EBML magic number.
pub(crate) fn sniff(head: &[u8]) -> bool {
    head.starts_with(&EBML.to_be_bytes())
}

/// An element: its ID and where its payload starts and ends (`None`: unknown size).
#[derive(Debug, Clone, Copy)]
struct Element {
    id: u32,
    body: u64,
    end: Option<u64>,
}

impl Element {
    fn known_end(self) -> Result<u64, Stop> {
        self.end.ok_or(Stop::Invalid(
            "unknown-size element where a size is required",
        ))
    }
}

/// Reads one variable-length integer's bytes at `at` (inside `..end`): the leading zeros of
/// the first byte give its width, at most `max`.
fn vint(data: &Sparse, at: u64, end: u64, max: u32) -> Result<&[u8], Stop> {
    if at >= end {
        return Err(Stop::Invalid("truncated element header"));
    }
    let width = data.u8_at(at)?.leading_zeros() + 1;
    if width > max {
        return Err(Stop::Invalid("element header too long"));
    }
    if u64::from(width) > end - at {
        return Err(Stop::Invalid("truncated element header"));
    }
    // `width` is at most 8.
    data.get(at, usize::try_from(width).unwrap_or(usize::MAX))
}

/// Reads the header of the element at `at`, inside a parent that ends at `end`.
fn element(data: &Sparse, at: u64, end: u64) -> Result<Element, Stop> {
    // IDs keep their length marker; they are at most 4 bytes.
    let id_bytes = vint(data, at, end, 4)?;
    let id = id_bytes.iter().fold(0u32, |v, b| (v << 8) | u32::from(*b));
    let size_at = at + id_bytes.len() as u64;
    let size_bytes = vint(data, size_at, end, 8)?;
    let width = size_bytes.len();
    // Clear the length marker; a width of 8 leaves no marker bits in the first byte.
    let mask = u32::try_from(width)
        .ok()
        .and_then(|w| 0xFFu8.checked_shr(w))
        .unwrap_or(0);
    let first = size_bytes.first().map_or(0, |b| b & mask);
    let size = size_bytes
        .iter()
        .skip(1)
        .fold(u64::from(first), |v, b| (v << 8) | u64::from(*b));
    let body = size_at + width as u64;
    // All value bits set means the size is unknown (live streams, unfinished files).
    let unknown = size == (1u64 << (7 * width)) - 1;
    let element_end = if unknown {
        None
    } else {
        Some(
            body.checked_add(size)
                .filter(|e| *e <= end)
                .ok_or(Stop::Invalid("element size out of range"))?,
        )
    };
    Ok(Element {
        id,
        body,
        end: element_end,
    })
}

/// The children of a master element with a known size.
fn children(data: &Sparse, parent: Element) -> Result<Vec<Element>, Stop> {
    let end = parent.known_end()?;
    let mut out = Vec::new();
    let mut at = parent.body;
    while at < end {
        if out.len() == MAX_CHILDREN {
            return Err(Stop::Invalid("too many child elements"));
        }
        let child = element(data, at, end)?;
        at = child.known_end()?;
        out.push(child);
    }
    Ok(out)
}

/// An unsigned integer element (0 to 8 bytes).
fn uint(data: &Sparse, field: Element) -> Result<u64, Stop> {
    let len = usize::try_from(field.known_end()? - field.body)
        .ok()
        .filter(|n| *n <= 8)
        .ok_or(Stop::Invalid("integer element longer than 8 bytes"))?;
    let bytes = data.get(field.body, len)?;
    Ok(bytes.iter().fold(0, |v, b| (v << 8) | u64::from(*b)))
}

/// Lists the audio and subtitle tracks of a Matroska or `WebM` file.
pub(crate) fn parse(data: &Sparse) -> Result<TrackReport, Stop> {
    let len = data.len();
    let header = element(data, 0, len)?;
    if header.id != EBML {
        return Err(Stop::Unsupported);
    }
    let mut at = header.known_end()?;
    for _ in 0..MAX_TOP_LEVEL {
        let el = element(data, at, len)?;
        if el.id == SEGMENT {
            return segment(data, el.body, el.end.unwrap_or(len));
        }
        at = el.known_end()?;
    }
    Err(Stop::Invalid("no Segment element"))
}

fn segment(data: &Sparse, start: u64, end: u64) -> Result<TrackReport, Stop> {
    let mut at = start;
    let mut tracks_at = None;
    let mut duration = None;
    for _ in 0..MAX_SEGMENT_CHILDREN {
        if at >= end {
            break;
        }
        let el = element(data, at, end)?;
        match el.id {
            TRACKS => return tracks(data, el, duration),
            INFO if duration.is_none() => duration = info_duration(data, el)?,
            SEEK_HEAD if tracks_at.is_none() => tracks_at = seek_tracks(data, el, start)?,
            CLUSTER => match tracks_at.take() {
                // Positions only move forward, so the walk ends.
                Some(pos) if pos > at && pos < end => {
                    at = pos;
                    continue;
                }
                _ => return Err(Stop::Invalid("no Tracks before the first Cluster")),
            },
            _ => {}
        }
        at = el.known_end()?;
    }
    Err(Stop::Invalid("no Tracks element"))
}

/// Where the `SeekHead` says `Tracks` is, as an offset in the file.
fn seek_tracks(data: &Sparse, head: Element, segment_start: u64) -> Result<Option<u64>, Stop> {
    for seek in children(data, head)? {
        if seek.id != SEEK {
            continue;
        }
        let (mut id, mut pos) = (None, None);
        for field in children(data, seek)? {
            match field.id {
                SEEK_ID => id = Some(uint(data, field)?),
                SEEK_POSITION => pos = Some(uint(data, field)?),
                _ => {}
            }
        }
        if id == Some(u64::from(TRACKS))
            && let Some(pos) = pos
        {
            return Ok(segment_start.checked_add(pos));
        }
    }
    Ok(None)
}

/// The segment's duration in `Info`, in milliseconds. A malformed `Info` gives `None` rather
/// than failing the probe: the tracks matter more.
fn info_duration(data: &Sparse, info: Element) -> Result<Option<u64>, Stop> {
    let fields = match children(data, info) {
        Ok(fields) => fields,
        Err(Stop::Invalid(_)) => return Ok(None),
        Err(other) => return Err(other),
    };
    // Matroska's default: one timestamp unit is a millisecond.
    let (mut scale, mut duration) = (1_000_000, None);
    for field in fields {
        match field.id {
            TIMESTAMP_SCALE => scale = uint(data, field).unwrap_or(0),
            DURATION => duration = float(data, field)?,
            _ => {}
        }
    }
    // Precision loss is irrelevant at millisecond resolution.
    #[allow(clippy::cast_precision_loss)]
    let ms = duration.and_then(|d| duration_ms(d * scale as f64, 1e9));
    Ok(ms)
}

/// A float element (4 or 8 bytes); `None` for any other size.
fn float(data: &Sparse, field: Element) -> Result<Option<f64>, Stop> {
    let Some(end) = field.end else {
        return Ok(None);
    };
    Ok(match end - field.body {
        4 => Some(f64::from(f32::from_be_bytes(data.array(field.body)?))),
        8 => Some(f64::from_be_bytes(data.array(field.body)?)),
        _ => None,
    })
}

fn tracks(data: &Sparse, el: Element, duration_ms: Option<u64>) -> Result<TrackReport, Stop> {
    let mut audio = Vec::new();
    let mut audio_entries = 0;
    let mut subtitles = Vec::new();
    for entry in children(data, el)? {
        if entry.id != TRACK_ENTRY {
            continue;
        }
        let e = track_entry(data, entry)?;
        // FFmpeg keeps every audio track with an audio codec ID, enabled or not.
        let position = audio_entries;
        if e.kind == Some(AUDIO) && e.codec.first() == Some(&b'A') {
            audio_entries += 1;
        }
        if !e.enabled {
            continue;
        }
        match e.kind {
            Some(AUDIO) if audio.len() < MAX_AUDIO_TRACKS => audio.push(AudioTrack {
                codec: codec_from_id(&e.codec),
                default: e.default,
                id: e.number,
                position,
            }),
            Some(SUBTITLE) if subtitles.len() < MAX_SUBTITLE_TRACKS => {
                subtitles.push(SubtitleTrack {
                    codec: subtitle_codec(&e.codec),
                    language: e.bcp47.or(e.language),
                    name: e.name,
                    default: e.default,
                    forced: e.forced,
                });
            }
            _ => {}
        }
    }
    Ok(TrackReport {
        container: Container::Matroska,
        audio,
        subtitles,
        duration_ms,
    })
}

/// The fields of a `TrackEntry` that matter here.
struct Entry {
    number: Option<u64>,
    kind: Option<u64>,
    enabled: bool,
    default: bool,
    forced: bool,
    /// `CodecID`, empty if missing or too long.
    codec: Vec<u8>,
    language: Option<String>,
    bcp47: Option<String>,
    name: Option<String>,
}

fn track_entry(data: &Sparse, entry: Element) -> Result<Entry, Stop> {
    // Matroska's defaults: enabled and default 1, forced 0.
    let mut e = Entry {
        number: None,
        kind: None,
        enabled: true,
        default: true,
        forced: false,
        codec: Vec::new(),
        // Matroska's default language is English; `und` (undetermined) clears it.
        language: Some("eng".to_owned()),
        bcp47: None,
        name: None,
    };
    for field in children(data, entry)? {
        match field.id {
            TRACK_NUMBER => e.number = Some(uint(data, field)?),
            TRACK_TYPE => e.kind = Some(uint(data, field)?),
            FLAG_ENABLED => e.enabled = uint(data, field)? != 0,
            FLAG_DEFAULT => e.default = uint(data, field)? != 0,
            FLAG_FORCED => e.forced = uint(data, field)? != 0,
            CODEC_ID => {
                e.codec = string(data, field, MAX_CODEC_ID)?
                    .map(<[u8]>::to_vec)
                    .unwrap_or_default();
            }
            LANGUAGE => e.language = string(data, field, MAX_STRING)?.and_then(language_tag),
            LANGUAGE_BCP47 => e.bcp47 = string(data, field, MAX_STRING)?.and_then(language_tag),
            NAME => e.name = string(data, field, MAX_STRING)?.and_then(track_name),
            _ => {}
        }
    }
    Ok(e)
}

/// A string element's bytes without zero padding, or `None` if longer than `max` bytes.
fn string(data: &Sparse, field: Element, max: usize) -> Result<Option<&[u8]>, Stop> {
    let Some(len) = usize::try_from(field.known_end()? - field.body)
        .ok()
        .filter(|n| *n <= max)
    else {
        return Ok(None);
    };
    let bytes = data.get(field.body, len)?;
    let end = bytes.iter().position(|b| *b == 0).unwrap_or(bytes.len());
    Ok(bytes.get(..end))
}

/// Maps a Matroska subtitle `CodecID` to a format.
fn subtitle_codec(id: &[u8]) -> SubtitleCodec {
    match id {
        b"S_TEXT/UTF8" | b"S_TEXT/ASCII" => SubtitleCodec::Text,
        b"S_TEXT/WEBVTT" => SubtitleCodec::WebVtt,
        b"S_TEXT/ASS" | b"S_TEXT/SSA" | b"S_ASS" | b"S_SSA" => SubtitleCodec::Ass,
        b"S_HDMV/PGS" | b"S_VOBSUB" | b"S_DVBSUB" | b"S_IMAGE/BMP" => SubtitleCodec::Image,
        _ => SubtitleCodec::Other,
    }
}

/// Maps a Matroska `CodecID` to a codec.
fn codec_from_id(id: &[u8]) -> AudioCodec {
    // `A_AAC` and `A_AAC/MPEG4/LC`, but not `A_AACX`.
    let family = |name: &[u8]| {
        id.strip_prefix(name)
            .is_some_and(|rest| rest.is_empty() || rest.starts_with(b"/"))
    };
    if family(b"A_AAC") {
        AudioCodec::Aac
    } else if family(b"A_AC3") {
        AudioCodec::Ac3
    } else if id == b"A_EAC3" {
        AudioCodec::Eac3
    } else if family(b"A_DTS") {
        AudioCodec::Dts
    } else if id == b"A_TRUEHD" || id == b"A_MLP" {
        AudioCodec::TrueHd
    } else if family(b"A_OPUS") {
        AudioCodec::Opus
    } else if id == b"A_VORBIS" {
        AudioCodec::Vorbis
    } else if id == b"A_FLAC" {
        AudioCodec::Flac
    } else if id == b"A_MPEG/L3" {
        AudioCodec::Mp3
    } else if id == b"A_MPEG/L2" || id == b"A_MPEG/L1" {
        AudioCodec::Mp2
    } else if family(b"A_PCM") {
        AudioCodec::Pcm
    } else if id == b"A_ALAC" {
        AudioCodec::Alac
    } else {
        AudioCodec::Other
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// An element with an 8-byte size.
    pub(crate) fn el(id: u32, body: &[u8]) -> Vec<u8> {
        let mut out: Vec<u8> = id
            .to_be_bytes()
            .into_iter()
            .skip_while(|b| *b == 0)
            .collect();
        out.push(0x01);
        out.extend_from_slice(&(body.len() as u64).to_be_bytes()[1..]);
        out.extend_from_slice(body);
        out
    }

    fn uint_el(id: u32, value: u8) -> Vec<u8> {
        el(id, &[value])
    }

    pub(crate) fn track(kind: u8, codec: &str, default: Option<bool>) -> Vec<u8> {
        let mut fields = [
            uint_el(0xD7, 1),
            uint_el(TRACK_TYPE, kind),
            el(CODEC_ID, codec.as_bytes()),
            el(0x63A2, &[0; 300]), // CodecPrivate
        ]
        .concat();
        if let Some(d) = default {
            fields.extend_from_slice(&uint_el(FLAG_DEFAULT, u8::from(d)));
        }
        el(TRACK_ENTRY, &fields)
    }

    pub(crate) fn ebml_header() -> Vec<u8> {
        el(EBML, &el(0x4282, b"matroska"))
    }

    /// A file: EBML header and a Segment with the given children.
    pub(crate) fn file(children: &[Vec<u8>]) -> Vec<u8> {
        [ebml_header(), el(SEGMENT, &children.concat())].concat()
    }

    fn parse_all(file: Vec<u8>) -> Result<TrackReport, Stop> {
        parse(&Sparse::whole(file))
    }

    fn codecs(report: &TrackReport) -> Vec<(AudioCodec, bool)> {
        report.audio.iter().map(|t| (t.codec, t.default)).collect()
    }

    fn info() -> Vec<u8> {
        el(0x1549_A966, &el(0x2A_D7B1, &[0x0F, 0x42, 0x40]))
    }

    #[test]
    fn lists_audio_tracks_with_default_flags() {
        let f = file(&[
            el(0xEC, &[0; 50]), // Void
            info(),
            el(
                TRACKS,
                &[
                    track(1, "V_MPEG4/ISO/AVC", None),
                    track(2, "A_AC3", Some(true)),
                    track(2, "A_AAC/MPEG4/LC", Some(false)),
                    track(17, "S_TEXT/UTF8", None),
                ]
                .concat(),
            ),
            el(CLUSTER, &[0; 64]),
        ]);
        let report = parse_all(f).unwrap();
        assert_eq!(report.container, Container::Matroska);
        assert_eq!(
            codecs(&report),
            [(AudioCodec::Ac3, true), (AudioCodec::Aac, false)]
        );
        assert_eq!(
            report.playable_alternative().unwrap().codec,
            AudioCodec::Aac
        );
    }

    #[test]
    fn reads_the_duration_from_info() {
        let with_info = |info: Vec<u8>| {
            let f = file(&[info, el(TRACKS, &track(2, "A_AAC", None))]);
            parse_all(f).unwrap().duration_ms
        };
        let scale = |ns: u32| el(TIMESTAMP_SCALE, &ns.to_be_bytes());
        let f64_duration = |d: f64| el(DURATION, &d.to_be_bytes());
        let f32_duration = |d: f32| el(DURATION, &d.to_be_bytes());
        assert_eq!(
            with_info(el(INFO, &f64_duration(5_400_000.0))),
            Some(5_400_000)
        );
        assert_eq!(
            with_info(el(
                INFO,
                &[scale(1_000_000), f32_duration(95_500.0)].concat()
            )),
            Some(95_500)
        );
        // Microsecond units.
        assert_eq!(
            with_info(el(INFO, &[scale(1000), f64_duration(2_000_000.0)].concat())),
            Some(2000)
        );
        assert_eq!(with_info(info()), None, "no Duration");
        assert_eq!(with_info(el(INFO, &f64_duration(f64::NAN))), None);
        assert_eq!(
            with_info(el(INFO, &el(DURATION, &[1, 2, 3]))),
            None,
            "bad size"
        );
        assert_eq!(with_info(el(INFO, &[0xFF; 3])), None, "malformed Info");
    }

    #[test]
    fn maps_codec_ids() {
        for (id, codec) in [
            ("A_AAC", AudioCodec::Aac),
            ("A_AAC/MPEG2/LC/SBR", AudioCodec::Aac),
            ("A_AC3/BSID10", AudioCodec::Ac3),
            ("A_EAC3", AudioCodec::Eac3),
            ("A_DTS", AudioCodec::Dts),
            ("A_DTS/LOSSLESS", AudioCodec::Dts),
            ("A_DTS/EXPRESS", AudioCodec::Dts),
            ("A_TRUEHD", AudioCodec::TrueHd),
            ("A_OPUS", AudioCodec::Opus),
            ("A_VORBIS", AudioCodec::Vorbis),
            ("A_FLAC", AudioCodec::Flac),
            ("A_MPEG/L3", AudioCodec::Mp3),
            ("A_MPEG/L2", AudioCodec::Mp2),
            ("A_PCM/INT/LIT", AudioCodec::Pcm),
            ("A_AACX", AudioCodec::Other),
            ("A_EAC3\0\0", AudioCodec::Eac3),
            ("", AudioCodec::Other),
        ] {
            let f = file(&[el(TRACKS, &track(2, id, None))]);
            assert_eq!(codecs(&parse_all(f).unwrap()), [(codec, true)], "{id:?}");
        }
        let long = "A_".to_owned() + &"X".repeat(100);
        let f = file(&[el(TRACKS, &track(2, &long, None))]);
        assert_eq!(codecs(&parse_all(f).unwrap()), [(AudioCodec::Other, true)]);
    }

    #[test]
    fn skips_disabled_tracks() {
        let disabled = el(
            TRACK_ENTRY,
            &[
                uint_el(TRACK_TYPE, 2),
                uint_el(FLAG_ENABLED, 0),
                el(CODEC_ID, b"A_DTS"),
            ]
            .concat(),
        );
        let f = file(&[el(
            TRACKS,
            &[disabled, track(2, "A_OPUS", Some(false))].concat(),
        )]);
        assert_eq!(codecs(&parse_all(f).unwrap()), [(AudioCodec::Opus, false)]);
    }

    #[test]
    fn follows_the_seek_head_when_tracks_come_after_a_cluster() {
        // An unknown-size Segment, and Tracks after the first Cluster.
        let cluster = el(CLUSTER, &[0; 1000]);
        let seek_head_len = el(
            SEEK_HEAD,
            &el(
                SEEK,
                &[
                    el(SEEK_ID, &TRACKS.to_be_bytes()),
                    el(SEEK_POSITION, &[0; 2]),
                ]
                .concat(),
            ),
        )
        .len();
        let position = u16::try_from(seek_head_len + cluster.len()).unwrap();
        let seek_head = el(
            SEEK_HEAD,
            &el(
                SEEK,
                &[
                    el(SEEK_ID, &TRACKS.to_be_bytes()),
                    el(SEEK_POSITION, &position.to_be_bytes()),
                ]
                .concat(),
            ),
        );
        let mut f = ebml_header();
        f.extend_from_slice(&[0x18, 0x53, 0x80, 0x67, 0xFF]); // Segment, unknown size
        f.extend_from_slice(&seek_head);
        f.extend_from_slice(&cluster);
        f.extend_from_slice(&el(TRACKS, &track(2, "A_TRUEHD", None)));
        assert_eq!(codecs(&parse_all(f).unwrap()), [(AudioCodec::TrueHd, true)]);

        // Without a SeekHead, a Cluster before Tracks gives up.
        let f = file(&[el(CLUSTER, &[0; 10]), el(TRACKS, &track(2, "A_AAC", None))]);
        assert!(matches!(parse_all(f), Err(Stop::Invalid(_))));
    }

    #[test]
    fn a_file_without_audio_has_no_tracks() {
        let f = file(&[el(TRACKS, &track(1, "V_VP9", None))]);
        assert_eq!(parse_all(f).unwrap().audio, []);
    }

    fn subtitle(codec: &str, fields: &[Vec<u8>]) -> Vec<u8> {
        let mut body = [uint_el(TRACK_TYPE, 0x11), el(CODEC_ID, codec.as_bytes())].concat();
        for f in fields {
            body.extend_from_slice(f);
        }
        el(TRACK_ENTRY, &body)
    }

    #[test]
    fn lists_subtitle_tracks() {
        let f = file(&[el(
            TRACKS,
            &[
                track(2, "A_AAC", None),
                subtitle("S_TEXT/UTF8", &[]),
                subtitle(
                    "S_TEXT/ASS",
                    &[
                        el(LANGUAGE, b"fre"),
                        el(NAME, b"Fran\xc3\xa7ais\n(forced)"),
                        uint_el(FLAG_FORCED, 1),
                        uint_el(FLAG_DEFAULT, 0),
                    ],
                ),
                subtitle(
                    "S_HDMV/PGS",
                    &[el(LANGUAGE, b"ger"), el(LANGUAGE_BCP47, b"de-CH")],
                ),
                subtitle("S_TEXT/WEBVTT", &[el(LANGUAGE, b"und")]),
                subtitle("S_KATE", &[uint_el(FLAG_ENABLED, 0)]),
            ]
            .concat(),
        )]);
        let report = parse_all(f).unwrap();
        assert_eq!(report.audio.len(), 1);
        let subs = &report.subtitles;
        assert_eq!(subs.len(), 4, "the disabled track is left out");
        assert_eq!(subs[0].codec, SubtitleCodec::Text);
        assert_eq!(
            subs[0].language.as_deref(),
            Some("eng"),
            "Matroska's default"
        );
        assert!(subs[0].default && !subs[0].forced);
        assert_eq!(subs[1].codec, SubtitleCodec::Ass);
        assert_eq!(subs[1].language.as_deref(), Some("fre"));
        assert_eq!(subs[1].name.as_deref(), Some("Français (forced)"));
        assert!(subs[1].forced && !subs[1].default);
        assert_eq!(subs[2].codec, SubtitleCodec::Image);
        assert_eq!(subs[2].language.as_deref(), Some("de-CH"), "BCP 47 wins");
        assert_eq!(subs[3].codec, SubtitleCodec::WebVtt);
        assert_eq!(subs[3].language, None, "`und` is no language");
    }

    #[test]
    fn rejects_malformed_files() {
        let invalid = |f: Vec<u8>| matches!(parse_all(f), Err(Stop::Invalid(_)));
        // A child larger than its parent.
        let mut entry = el(TRACK_ENTRY, &uint_el(TRACK_TYPE, 2));
        entry[1 + 7] += 5; // size runs past the Tracks element
        assert!(invalid(file(&[el(TRACKS, &entry)])));
        // A zero first byte (no length marker), and an integer longer than 8 bytes.
        assert!(invalid(file(&[vec![0x00, 0x81, 0x00]])));
        let wide = el(TRACK_ENTRY, &el(TRACK_TYPE, &[0; 9]));
        assert!(invalid(file(&[el(TRACKS, &wide)])));
        // No Segment, no Tracks.
        assert!(invalid(ebml_header()));
        assert!(invalid(file(&[info()])));
        // Too many children.
        let many: Vec<Vec<u8>> = (0..600).map(|_| el(0xEC, &[])).collect();
        assert!(invalid(file(&[el(TRACKS, &many.concat())])));
        // An unknown-size element inside a master element.
        assert!(invalid(file(&[el(TRACKS, &[0xAE, 0xFF])])));
    }
}
