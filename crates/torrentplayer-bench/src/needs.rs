// SPDX-License-Identifier: MIT OR Apache-2.0

//! What a player reads at start-up and after a seek, from the file's container layout
//! ([`MediaLayout`]), instead of a flat amount at each end.
//!
//! - **Start-up:** the container header in the order a player reads it (for MP4, the boxes up
//!   to `moov`, wherever `moov` is; for Matroska, everything before the first `Cluster`), then
//!   [`READY_SECS`] of media from where the media data starts.
//! - **Seek** to a fraction of the duration: the index first if the player loads it only now
//!   (Matroska `Cues` after the clusters), then the media from the keyframe at or before the
//!   target through the target plus [`READY_SECS`], since the decoder starts at the keyframe.
//!
//! Each step is read after the one before it, as a player parses one structure before it
//! knows where the next is. Steps that start within [`MERGE_GAP`] of the previous one's end are
//! read in one request, as a player reading on through a faststart file does.

use std::ops::Range;

use serde::{Deserialize, Serialize};
use torrentplayer_media::{MediaLayout, SeekPoint};

/// Seconds of media a player buffers before it shows a frame.
pub const READY_SECS: f64 = 1.0;
/// Reads that start this close after the previous one's end are merged into it.
pub const MERGE_GAP: u64 = 64 * 1024;

/// One read: `len` bytes at `start`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Step {
    /// First byte.
    pub start: u64,
    /// Bytes to read.
    pub len: u64,
}

impl Step {
    fn end(self) -> u64 {
        self.start + self.len
    }
}

/// The bytes needed to play `secs` of media at `byte_rate`.
fn media_bytes(secs: f64, byte_rate: u64) -> u64 {
    // Non-negative and far below 2^53.
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_precision_loss
    )]
    let bytes = (secs.max(0.0) * byte_rate as f64).ceil() as u64;
    bytes.max(1)
}

/// Appends `range` (clamped to `len`), merging it into the last step if it starts within
/// [`MERGE_GAP`] of its end.
fn push(steps: &mut Vec<Step>, range: Range<u64>, len: u64) {
    let (start, end) = (range.start.min(len), range.end.min(len));
    if start >= end {
        return;
    }
    if let Some(last) = steps.last_mut()
        && start >= last.start
        && start <= last.end() + MERGE_GAP
    {
        last.len = last.len.max(end - last.start);
        return;
    }
    steps.push(Step {
        start,
        len: end - start,
    });
}

/// The reads before the first frame of a `len`-byte file with media at `byte_rate`.
#[must_use]
pub fn startup_steps(layout: &MediaLayout, len: u64, byte_rate: u64) -> Vec<Step> {
    let mut steps = Vec::new();
    for range in &layout.header {
        push(&mut steps, range.clone(), len);
    }
    let media = layout.media_start;
    push(
        &mut steps,
        media..media.saturating_add(media_bytes(READY_SECS, byte_rate)),
        len,
    );
    steps
}

/// A seek planned from the layout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeekPlan {
    /// The target time, in milliseconds.
    pub target_ms: u64,
    /// The keyframe decoding starts from.
    pub keyframe: SeekPoint,
    /// The reads, in order; the last one is the media from the keyframe.
    pub steps: Vec<Step>,
}

/// The reads for a seek to `fraction` of `duration_ms`. `index_read` says whether the player
/// has already read the layout's seek index (with the header, or for an earlier seek). `None`
/// if the layout has no keyframes.
#[must_use]
pub fn seek_plan(
    layout: &MediaLayout,
    len: u64,
    byte_rate: u64,
    duration_ms: u64,
    fraction: f64,
    index_read: bool,
) -> Option<SeekPlan> {
    // Bounded by the duration.
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_precision_loss
    )]
    let target_ms = (fraction.clamp(0.0, 1.0) * duration_ms as f64) as u64;
    let keyframe = layout.seek_point(target_ms)?;
    let mut steps = Vec::new();
    if let Some(index) = layout.seek_index.clone()
        && !index_read
        && !in_header(layout, &index)
    {
        push(&mut steps, index, len);
    }
    #[allow(clippy::cast_precision_loss)]
    let secs = target_ms.saturating_sub(keyframe.time_ms) as f64 / 1000.0 + READY_SECS;
    let media = keyframe.offset.min(len.saturating_sub(1));
    // Never merged with the index read: the player reopens at the keyframe.
    let media_len = media_bytes(secs, byte_rate).min(len - media);
    steps.push(Step {
        start: media,
        len: media_len,
    });
    Some(SeekPlan {
        target_ms,
        keyframe,
        steps,
    })
}

/// Whether `range` lies inside the header, so the player read it at start-up.
fn in_header(layout: &MediaLayout, range: &Range<u64>) -> bool {
    layout
        .header
        .iter()
        .any(|h| h.start <= range.start && range.end <= h.end)
}

#[cfg(test)]
// Layouts with one header range.
#[allow(clippy::single_range_in_vec_init)]
mod tests {
    use torrentplayer_media::Container;

    use super::*;

    const MIB: u64 = 1024 * 1024;

    fn point(time_ms: u64, offset: u64) -> SeekPoint {
        SeekPoint { time_ms, offset }
    }

    /// A 100 MiB, 100 s file (1 MiB/s) with keyframes every 2 s.
    fn mp4(faststart: bool) -> MediaLayout {
        let (header, media_start) = if faststart {
            (vec![0..300_000], 300_008)
        } else {
            (vec![0..40, 100 * MIB - 300_000..100 * MIB], 40)
        };
        MediaLayout {
            container: Container::Mp4,
            video_codec: Some("H.264".into()),
            header,
            media_start,
            seek_index: None,
            seek_points: (0..50)
                .map(|i| point(i * 2000, media_start + i * 2 * MIB))
                .collect(),
            duration_ms: Some(100_000),
            index_at_end: !faststart,
        }
    }

    #[test]
    fn faststart_reads_the_header_and_media_in_one_request() {
        let steps = startup_steps(&mp4(true), 100 * MIB, MIB);
        assert_eq!(
            steps,
            [Step {
                start: 0,
                len: 300_008 + MIB
            }]
        );
    }

    #[test]
    fn moov_at_the_end_needs_the_tail_before_the_media() {
        let steps = startup_steps(&mp4(false), 100 * MIB, MIB);
        assert_eq!(
            steps,
            [
                Step { start: 0, len: 40 },
                Step {
                    start: 100 * MIB - 300_000,
                    len: 300_000
                },
                Step {
                    start: 40,
                    len: MIB
                },
            ]
        );
    }

    #[test]
    fn seeks_start_at_the_keyframe_before_the_target() {
        let layout = mp4(true);
        // 50% is 50 s, a keyframe; 51.5 s needs 1.5 s more from the keyframe at 50 s.
        let plan = seek_plan(&layout, 100 * MIB, MIB, 100_000, 0.5, true).unwrap();
        assert_eq!(plan.keyframe, point(50_000, 300_008 + 50 * MIB));
        assert_eq!(plan.steps.len(), 1);
        assert_eq!(plan.steps[0].len, MIB);
        let plan = seek_plan(&layout, 100 * MIB, MIB, 100_000, 0.515, true).unwrap();
        assert_eq!(plan.target_ms, 51_500);
        assert_eq!(plan.keyframe.time_ms, 50_000);
        assert_eq!(plan.steps[0].len, MIB * 5 / 2);
        // Near the end, the read stops at the end of the file.
        let plan = seek_plan(&layout, 100 * MIB, MIB, 100_000, 1.0, true).unwrap();
        let last = plan.steps[0];
        assert_eq!(last.start + last.len, 100 * MIB);
        let none = MediaLayout {
            seek_points: Vec::new(),
            ..layout
        };
        assert!(seek_plan(&none, 100 * MIB, MIB, 100_000, 0.5, true).is_none());
    }

    #[test]
    fn matroska_reads_cues_after_the_clusters_before_the_first_seek() {
        let cues = 100 * MIB - 20_000..100 * MIB;
        let layout = MediaLayout {
            container: Container::Matroska,
            video_codec: Some("H.264".into()),
            header: vec![0..5000],
            media_start: 5000,
            seek_index: Some(cues.clone()),
            seek_points: vec![point(0, 5000), point(5000, 5 * MIB)],
            duration_ms: Some(100_000),
            index_at_end: true,
        };
        // Start-up does not read the cues.
        assert_eq!(
            startup_steps(&layout, 100 * MIB, MIB),
            [Step {
                start: 0,
                len: 5000 + MIB
            }]
        );
        let plan = seek_plan(&layout, 100 * MIB, MIB, 100_000, 0.06, false).unwrap();
        assert_eq!(
            plan.steps,
            [
                Step {
                    start: cues.start,
                    len: 20_000
                },
                Step {
                    start: 5 * MIB,
                    len: 2 * MIB
                }
            ]
        );
        // Once read, or when they sit in the header, the cues are not read again.
        let plan = seek_plan(&layout, 100 * MIB, MIB, 100_000, 0.06, true).unwrap();
        assert_eq!(plan.steps.len(), 1);
        let early = MediaLayout {
            header: vec![0..cues.end],
            ..layout
        };
        let plan = seek_plan(&early, 100 * MIB, MIB, 100_000, 0.06, false).unwrap();
        assert_eq!(plan.steps.len(), 1);
    }
}
