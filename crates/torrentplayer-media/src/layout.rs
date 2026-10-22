// SPDX-License-Identifier: MIT OR Apache-2.0

//! Where a player finds things in a file: the container header it reads before the first
//! frame, where the media data starts, and the keyframes (MP4 sync samples, Matroska cue
//! points) it seeks to.
//!
//! The streaming benchmark uses this to request the bytes a real player needs at start-up and
//! after a seek, instead of a flat amount at each end.

use std::ops::Range;

use tokio::io::{AsyncRead, AsyncSeek};

use crate::probe::{ProbeError, by_container, read_parsed};
use crate::sparse::{Sparse, Stop};
use crate::tracks::Container;
use crate::{mkv, mp4};

/// Most reads per layout probe. Unlike [`probe_tracks`](crate::probe_tracks), this reads the
/// video track's sample tables (or the Matroska `Cues`), one read each.
pub const MAX_LAYOUT_READS: usize = 256;
/// Most keyframes recorded; with more (an all-intra file), every n-th is kept.
pub const MAX_SEEK_POINTS: usize = 100_000;
/// Most samples in a video track whose sample tables are walked.
pub(crate) const MAX_SAMPLES: u64 = 50_000_000;

/// A position a player can start decoding from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SeekPoint {
    /// Presentation time, in milliseconds.
    pub time_ms: u64,
    /// Byte offset in the file where its data starts (the keyframe sample, or the cluster
    /// that holds it).
    pub offset: u64,
}

/// The parts of a file a player reads, as found in its container.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaLayout {
    /// The container format.
    pub container: Container,
    /// The first video track's codec, as a short name (`H.264`, `HEVC`, `AV1`, `VP9`, ...), or
    /// the container's own identifier for others. `None` without a video track.
    pub video_codec: Option<String>,
    /// The byte ranges a player reads to parse the header, in the order it reads them: for
    /// MP4, every top-level box up to and including `moov` (only the header of an `mdat` box
    /// before it); for Matroska, everything before the first `Cluster`. Adjacent ranges are
    /// merged.
    pub header: Vec<Range<u64>>,
    /// Where the media data starts: the first chunk of any track (MP4) or the first `Cluster`.
    pub media_start: u64,
    /// An index a player reads before its first seek, if it reads it only then (Matroska
    /// `Cues`, which FFmpeg-based players load on the first seek).
    pub seek_index: Option<Range<u64>>,
    /// Keyframes of the first video track, by time.
    pub seek_points: Vec<SeekPoint>,
    /// The duration in the header, in milliseconds.
    pub duration_ms: Option<u64>,
    /// The index (MP4 `moov`, Matroska `Cues`) comes after the media data, so a player reads
    /// the end of the file first.
    pub index_at_end: bool,
}

impl MediaLayout {
    /// The last keyframe at or before `time_ms` (the first one if there is none before).
    #[must_use]
    pub fn seek_point(&self, time_ms: u64) -> Option<SeekPoint> {
        let after = self.seek_points.partition_point(|p| p.time_ms <= time_ms);
        self.seek_points.get(after.saturating_sub(1)).copied()
    }
}

/// Reads the layout of the `len`-byte file behind `reader`: the header, the media start, and
/// the video keyframes. Reads the container header and the video sample tables (MP4) or the
/// `Cues` (Matroska), not the media.
///
/// # Errors
///
/// Returns an error if the container is not supported or is malformed (including fragmented
/// MP4 files, whose samples are not in `moov`), or reading fails.
pub async fn probe_layout<R>(reader: &mut R, len: u64) -> Result<MediaLayout, ProbeError>
where
    R: AsyncRead + AsyncSeek + Unpin + ?Sized,
{
    read_parsed(reader, len, MAX_LAYOUT_READS, parse).await
}

pub(crate) fn parse(data: &Sparse) -> Result<MediaLayout, Stop> {
    by_container(data, mkv::layout, mp4::layout)
}

/// Adds `range` to `ranges`, merging it with the last one if they touch.
pub(crate) fn push_range(ranges: &mut Vec<Range<u64>>, range: Range<u64>) {
    if range.is_empty() {
        return;
    }
    match ranges.last_mut() {
        Some(last) if last.end == range.start => last.end = range.end,
        _ => ranges.push(range),
    }
}

/// Keeps at most [`MAX_SEEK_POINTS`], evenly spaced, and sorts them by time.
pub(crate) fn thin(mut points: Vec<SeekPoint>) -> Vec<SeekPoint> {
    points.sort_by_key(|p| (p.time_ms, p.offset));
    if points.len() > MAX_SEEK_POINTS {
        let step = points.len().div_ceil(MAX_SEEK_POINTS);
        points = points.into_iter().step_by(step).collect();
    }
    points
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_the_keyframe_at_or_before_a_time() {
        let point = |time_ms, offset| SeekPoint { time_ms, offset };
        let layout = MediaLayout {
            container: Container::Mp4,
            video_codec: None,
            header: Vec::new(),
            media_start: 0,
            seek_index: None,
            seek_points: vec![point(0, 10), point(2000, 500), point(4000, 900)],
            duration_ms: None,
            index_at_end: false,
        };
        assert_eq!(layout.seek_point(0), Some(point(0, 10)));
        assert_eq!(layout.seek_point(1999), Some(point(0, 10)));
        assert_eq!(layout.seek_point(2000), Some(point(2000, 500)));
        assert_eq!(layout.seek_point(99_999), Some(point(4000, 900)));
        let empty = MediaLayout {
            seek_points: Vec::new(),
            ..layout
        };
        assert_eq!(empty.seek_point(5), None);
    }

    #[test]
    fn merges_touching_ranges_and_thins_points() {
        let mut ranges = Vec::new();
        push_range(&mut ranges, 0..10);
        push_range(&mut ranges, 10..20);
        push_range(&mut ranges, 5..5);
        push_range(&mut ranges, 40..50);
        assert_eq!(ranges, [0..20, 40..50]);
        let many = (0..=(MAX_SEEK_POINTS as u64 * 2))
            .rev()
            .map(|i| SeekPoint {
                time_ms: i,
                offset: i,
            })
            .collect();
        let thinned = thin(many);
        assert!(thinned.len() <= MAX_SEEK_POINTS);
        assert_eq!(thinned[0].time_ms, 0);
        assert!(thinned.windows(2).all(|w| w[0].time_ms < w[1].time_ms));
    }
}
