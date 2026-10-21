// SPDX-License-Identifier: MIT OR Apache-2.0

//! The prioritization policy: turns playback state into an ordered list of anchors.

use std::ops::Range;
use std::time::Duration;

use crate::geometry::{FileSpan, PieceGeometry};

/// Tunable prioritization settings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    /// How much playback time to keep downloaded ahead of the playhead.
    pub read_ahead: Duration,
    /// Media byte rate assumed until the real bitrate is known (bytes per second).
    pub fallback_byte_rate: u64,
    /// Smallest read-ahead window, so very low bitrates still buffer a useful amount.
    pub min_window_bytes: u64,
    /// Largest read-ahead window, so very high bitrates cannot starve everything else.
    pub max_window_bytes: u64,
    /// Until this many bytes are buffered at the playhead, only the playhead is prioritized.
    /// Right after pressing play or seeking, every other request competes for bandwidth with
    /// the bytes the player is waiting for, so speculative work waits until playback is safe.
    pub critical_bytes: u64,
    /// Bytes at the start of a file that hold container metadata (headers, the MP4 `moov`
    /// atom when it is at the front, MKV `SeekHead`).
    pub head_bytes: u64,
    /// Bytes at the end of a file that hold container indexes (MP4 `moov` when written last,
    /// MKV `Cues`).
    pub tail_bytes: u64,
    /// Start prefetching the next lecture when this much playback time remains.
    pub next_lecture_lead: Duration,
    /// How far ahead of each parked stream the engine prioritizes pieces. This mirrors a
    /// constant inside librqbit (32 MiB in librqbit 9) and must be kept in sync with it.
    pub engine_lookahead_bytes: u64,
    /// Maximum number of anchors in a plan. Each anchor holds an engine stream, and streams
    /// share a limited pool with disk I/O, so this is kept small.
    pub max_anchors: usize,
}

impl Default for Config {
    fn default() -> Self {
        const MIB: u64 = 1024 * 1024;
        Self {
            read_ahead: Duration::from_secs(30),
            // 2.5 Mbit/s: typical for 720p lecture video.
            fallback_byte_rate: 312_500,
            min_window_bytes: 8 * MIB,
            critical_bytes: 2 * MIB,
            max_window_bytes: 1024 * MIB,
            head_bytes: 2 * MIB,
            // An hour-long MP4 has a `moov` atom of roughly 1–3 MiB.
            tail_bytes: 4 * MIB,
            next_lecture_lead: Duration::from_secs(60),
            engine_lookahead_bytes: 32 * MIB,
            max_anchors: 4,
        }
    }
}

impl Config {
    /// Size of the read-ahead window for media consumed at `byte_rate` bytes per second.
    #[must_use]
    pub fn window_bytes(&self, byte_rate: u64) -> u64 {
        let secs = self.read_ahead.as_secs_f64();
        // Saturating float-to-int conversion is intended: absurd bitrates clamp to the max.
        #[allow(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            clippy::cast_precision_loss
        )]
        let bytes = (byte_rate as f64 * secs) as u64;
        bytes.clamp(
            self.min_window_bytes,
            self.max_window_bytes.max(self.min_window_bytes),
        )
    }
}

/// What the viewer is watching right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Playback {
    /// The file being played.
    pub file: FileSpan,
    /// Playhead position as a byte offset within the file.
    pub position: u64,
    /// Average media byte rate (bitrate / 8), if known.
    pub byte_rate: Option<u64>,
    /// The next lecture in the course, if any, for prefetching.
    pub next_file: Option<FileSpan>,
}

/// Which file an anchor belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AnchorFile {
    /// The file being played.
    Current,
    /// The next lecture.
    Next,
}

/// Why an anchor exists. Useful for the debug overlay and for tests.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Purpose {
    /// Data the player needs next.
    Playhead,
    /// Container metadata at the start of the file.
    ContainerHead,
    /// Container indexes at the end of the file.
    ContainerTail,
    /// The rest of the read-ahead window.
    ReadAhead,
    /// Start of the next lecture.
    NextLectureHead,
    /// End of the next lecture (its container index).
    NextLectureTail,
}

/// A position where the engine adapter should park a stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Anchor {
    /// The file the position refers to.
    pub file: AnchorFile,
    /// Byte offset within that file. Always the first missing byte of the region it serves.
    pub offset: u64,
    /// Why this anchor exists.
    pub purpose: Purpose,
}

/// The result of [`plan`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    /// Anchors in priority order, at most [`Config::max_anchors`].
    pub anchors: Vec<Anchor>,
    /// The read-ahead window as a byte range within the current file.
    pub window: Range<u64>,
    /// Contiguous downloaded bytes starting at the playhead.
    pub buffered_bytes: u64,
}

/// Computes which positions to prioritize for the given playback state.
///
/// Until [`Config::critical_bytes`] are buffered at the playhead, the plan contains only the
/// playhead anchor; after that it adds container metadata, read-ahead, and the next lecture.
///
/// `have(piece)` must report whether a piece (by torrent-wide index) is downloaded and
/// verified. The function is pure and cheap: it only inspects pieces inside the regions it
/// considers.
#[must_use]
pub fn plan(
    geometry: &PieceGeometry,
    config: &Config,
    playback: &Playback,
    have: impl Fn(u64) -> bool,
) -> Plan {
    let file = playback.file;
    let position = playback.position.min(file.len);
    let byte_rate = playback
        .byte_rate
        .filter(|&r| r > 0)
        .unwrap_or(config.fallback_byte_rate)
        .max(1);
    let window = file.clamp(position..position.saturating_add(config.window_bytes(byte_rate)));

    let buffered_bytes = buffered_bytes_from(geometry, file, position, &have);
    let critical = buffered_bytes < config.critical_bytes.min(file.len - position);

    let mut planner = Planner {
        geometry,
        lookahead: config.engine_lookahead_bytes.max(1),
        // In the critical phase only the playhead anchor is placed.
        max: if critical {
            config.max_anchors.min(1)
        } else {
            config.max_anchors
        },
        have: &have,
        anchors: Vec::new(),
    };

    // 1. The first missing byte ahead of the playhead.
    let playhead_limit = position.saturating_add(planner.lookahead);
    if let Some(offset) = planner.first_missing(AnchorFile::Current, file, window.clone()) {
        let purpose = if offset < playhead_limit {
            Purpose::Playhead
        } else {
            Purpose::ReadAhead
        };
        planner.push(AnchorFile::Current, offset, purpose);
    }

    // 2. Container metadata at both ends of the file.
    planner.place(
        AnchorFile::Current,
        file,
        file.clamp(0..config.head_bytes),
        Purpose::ContainerHead,
    );
    planner.place(
        AnchorFile::Current,
        file,
        file.clamp(file.len.saturating_sub(config.tail_bytes)..file.len),
        Purpose::ContainerTail,
    );

    // 3. The rest of the read-ahead window, one engine lookahead at a time.
    while planner.place(
        AnchorFile::Current,
        file,
        window.clone(),
        Purpose::ReadAhead,
    ) {}

    // 4. The next lecture, once the current one is nearly over.
    let remaining_secs = (file.len - position) / byte_rate;
    if let Some(next) = playback.next_file
        && remaining_secs <= config.next_lecture_lead.as_secs()
    {
        planner.place(
            AnchorFile::Next,
            next,
            next.clamp(0..config.head_bytes),
            Purpose::NextLectureHead,
        );
        planner.place(
            AnchorFile::Next,
            next,
            next.clamp(next.len.saturating_sub(config.tail_bytes)..next.len),
            Purpose::NextLectureTail,
        );
    }

    Plan {
        anchors: planner.anchors,
        window,
        buffered_bytes,
    }
}

/// Counts the contiguous downloaded bytes in `file` starting at the file-relative `position`.
#[must_use]
pub fn buffered_bytes_from(
    geometry: &PieceGeometry,
    file: FileSpan,
    position: u64,
    have: impl Fn(u64) -> bool,
) -> u64 {
    let position = position.min(file.len);
    let pieces = geometry.pieces_overlapping(file.to_abs(position)..file.to_abs(file.len));
    for piece in pieces {
        if !have(piece) {
            let missing_abs = geometry.piece_start(piece).max(file.to_abs(position));
            return missing_abs - file.to_abs(position);
        }
    }
    file.len - position
}

struct Planner<'a, H> {
    geometry: &'a PieceGeometry,
    lookahead: u64,
    max: usize,
    have: &'a H,
    anchors: Vec<Anchor>,
}

impl<H: Fn(u64) -> bool> Planner<'_, H> {
    fn full(&self) -> bool {
        self.anchors.len() >= self.max
    }

    fn push(&mut self, file: AnchorFile, offset: u64, purpose: Purpose) {
        if !self.full() {
            self.anchors.push(Anchor {
                file,
                offset,
                purpose,
            });
        }
    }

    /// The end of the coverage of an existing anchor that contains `offset`, if any.
    fn covered_until(&self, file: AnchorFile, offset: u64) -> Option<u64> {
        self.anchors
            .iter()
            .filter(|a| a.file == file && a.offset <= offset)
            .map(|a| a.offset.saturating_add(self.lookahead))
            .filter(|&end| offset < end)
            .max()
    }

    /// First missing, not-yet-covered byte of `region` (file-relative), if any.
    fn first_missing(&self, which: AnchorFile, file: FileSpan, region: Range<u64>) -> Option<u64> {
        let mut cursor = region.start;
        while cursor < region.end {
            if let Some(end) = self.covered_until(which, cursor) {
                cursor = end;
                continue;
            }
            let pieces = self
                .geometry
                .pieces_overlapping(file.to_abs(cursor)..file.to_abs(region.end));
            let missing = pieces.clone().find(|&p| !(self.have)(p))?;
            let offset = (self.geometry.piece_start(missing).max(file.to_abs(cursor)))
                - file.offset_in_torrent;
            // The missing piece may start inside an existing anchor's coverage.
            match self.covered_until(which, offset) {
                Some(end) => cursor = end,
                None => return Some(offset),
            }
        }
        None
    }

    /// Adds an anchor at the first missing byte of `region`. Returns whether one was added.
    fn place(
        &mut self,
        which: AnchorFile,
        file: FileSpan,
        region: Range<u64>,
        purpose: Purpose,
    ) -> bool {
        if self.full() {
            return false;
        }
        match self.first_missing(which, file, region) {
            Some(offset) => {
                self.push(which, offset, purpose);
                true
            }
            None => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    const MIB: u64 = 1024 * 1024;

    /// A 1 GiB single-file torrent with 1 MiB pieces.
    fn single_file() -> (PieceGeometry, FileSpan) {
        let geometry = PieceGeometry::new(u32::try_from(MIB).unwrap(), 1024 * MIB).unwrap();
        let file = FileSpan {
            offset_in_torrent: 0,
            len: 1024 * MIB,
        };
        (geometry, file)
    }

    fn playback(file: FileSpan, position: u64) -> Playback {
        Playback {
            file,
            position,
            byte_rate: Some(MIB),
            next_file: None,
        }
    }

    fn purposes(plan: &Plan) -> Vec<Purpose> {
        plan.anchors.iter().map(|a| a.purpose).collect()
    }

    /// Pieces below `end_mib` are downloaded (1 MiB pieces).
    fn have_until(end_mib: u64) -> impl Fn(u64) -> bool {
        move |piece| piece < end_mib
    }

    #[test]
    fn fresh_start_prioritizes_only_the_playhead() {
        let (g, f) = single_file();
        let p = plan(&g, &Config::default(), &playback(f, 0), |_| false);
        assert_eq!(purposes(&p), vec![Purpose::Playhead]);
        assert_eq!(p.anchors[0].offset, 0);
        assert_eq!(p.window, 0..30 * MIB);
        assert_eq!(p.buffered_bytes, 0);
    }

    #[test]
    fn after_the_critical_buffer_adds_tail_and_read_ahead() {
        let (g, f) = single_file();
        let p = plan(&g, &Config::default(), &playback(f, 0), have_until(2));
        // The playhead anchor (at the first missing byte) already covers the head.
        assert_eq!(
            purposes(&p),
            vec![Purpose::Playhead, Purpose::ContainerTail],
            "1 MiB/s for 30 s fits in one 32 MiB lookahead"
        );
        assert_eq!(p.anchors[0].offset, 2 * MIB);
        assert_eq!(p.anchors[1].offset, f.len - 4 * MIB);
        assert_eq!(p.buffered_bytes, 2 * MIB);
    }

    #[test]
    fn high_bitrate_extends_read_ahead_beyond_one_lookahead() {
        let (g, f) = single_file();
        let mut pb = playback(f, 0);
        pb.byte_rate = Some(4 * MIB); // 30 s => 120 MiB window
        let p = plan(&g, &Config::default(), &pb, have_until(2));
        assert_eq!(
            purposes(&p),
            vec![
                Purpose::Playhead,
                Purpose::ContainerTail,
                Purpose::ReadAhead,
                Purpose::ReadAhead,
            ]
        );
        assert_eq!(p.anchors[2].offset, 34 * MIB);
        assert_eq!(p.anchors[3].offset, 66 * MIB);
    }

    #[test]
    fn seek_focuses_on_the_target_then_requests_both_container_ends() {
        let (g, f) = single_file();
        let p = plan(&g, &Config::default(), &playback(f, 500 * MIB), |_| false);
        assert_eq!(purposes(&p), vec![Purpose::Playhead]);
        assert_eq!(p.anchors[0].offset, 500 * MIB);

        let buffered = |piece: u64| (500..502).contains(&piece);
        let p = plan(&g, &Config::default(), &playback(f, 500 * MIB), buffered);
        assert_eq!(
            purposes(&p),
            vec![
                Purpose::Playhead,
                Purpose::ContainerHead,
                Purpose::ContainerTail
            ]
        );
        assert_eq!(p.anchors[0].offset, 502 * MIB);
    }

    #[test]
    fn critical_phase_ends_early_near_the_end_of_the_file() {
        let (g, f) = single_file();
        // Only the last 1 MiB remains and it is downloaded, so playback is safe.
        let have = |piece: u64| piece == 1023;
        let p = plan(&g, &Config::default(), &playback(f, f.len - MIB), have);
        assert!(p.anchors.iter().all(|a| a.purpose != Purpose::Playhead));
        assert_eq!(p.buffered_bytes, MIB);
    }

    #[test]
    fn anchors_skip_downloaded_data() {
        let (g, f) = single_file();
        // Head and tail done; the first 10 MiB after the playhead also done.
        let have = |piece: u64| !(4..1020).contains(&piece) || (500..510).contains(&piece);
        let p = plan(&g, &Config::default(), &playback(f, 500 * MIB), have);
        assert_eq!(purposes(&p), vec![Purpose::Playhead]);
        assert_eq!(p.anchors[0].offset, 510 * MIB);
        assert_eq!(p.buffered_bytes, 10 * MIB);
    }

    #[test]
    fn mid_piece_position_anchors_at_position_not_piece_start() {
        let (g, f) = single_file();
        let p = plan(
            &g,
            &Config::default(),
            &playback(f, 500 * MIB + 123),
            |_| false,
        );
        assert_eq!(p.anchors[0].offset, 500 * MIB + 123);
    }

    #[test]
    fn fully_downloaded_file_needs_no_anchors() {
        let (g, f) = single_file();
        let p = plan(&g, &Config::default(), &playback(f, 100 * MIB), |_| true);
        assert!(p.anchors.is_empty());
        assert_eq!(p.buffered_bytes, f.len - 100 * MIB);
    }

    #[test]
    fn next_lecture_is_prefetched_only_near_the_end() {
        let geometry = PieceGeometry::new(u32::try_from(MIB).unwrap(), 2048 * MIB).unwrap();
        let current = FileSpan {
            offset_in_torrent: 0,
            len: 1024 * MIB,
        };
        let next = FileSpan {
            offset_in_torrent: 1024 * MIB,
            len: 1024 * MIB,
        };
        let have_current = |piece: u64| piece < 1024;
        let mut pb = Playback {
            file: current,
            position: 100 * MIB,
            byte_rate: Some(MIB),
            next_file: Some(next),
        };
        let early = plan(&geometry, &Config::default(), &pb, have_current);
        assert!(early.anchors.is_empty());

        pb.position = current.len - 30 * MIB; // 30 s left at 1 MiB/s
        let late = plan(&geometry, &Config::default(), &pb, have_current);
        assert_eq!(
            purposes(&late),
            vec![Purpose::NextLectureHead, Purpose::NextLectureTail]
        );
        assert!(late.anchors.iter().all(|a| a.file == AnchorFile::Next));
        assert_eq!(late.anchors[1].offset, next.len - 4 * MIB);
    }

    #[test]
    fn unknown_bitrate_uses_fallback_and_window_is_clamped() {
        let config = Config::default();
        assert_eq!(config.window_bytes(1), config.min_window_bytes);
        assert_eq!(config.window_bytes(u64::MAX), config.max_window_bytes);
        let (g, f) = single_file();
        let mut pb = playback(f, 0);
        pb.byte_rate = None;
        let p = plan(&g, &config, &pb, |_| false);
        assert_eq!(p.window.end, config.window_bytes(config.fallback_byte_rate));
    }

    #[test]
    fn respects_anchor_budget() {
        let (g, f) = single_file();
        let config = Config {
            max_anchors: 2,
            ..Config::default()
        };
        let buffered = |piece: u64| (500..502).contains(&piece);
        let p = plan(&g, &config, &playback(f, 500 * MIB), buffered);
        assert_eq!(
            purposes(&p),
            vec![Purpose::Playhead, Purpose::ContainerHead]
        );
    }

    #[test]
    fn file_inside_multi_file_torrent_uses_file_relative_offsets() {
        let geometry = PieceGeometry::new(u32::try_from(MIB).unwrap(), 300 * MIB).unwrap();
        // The file starts half-way through piece 100.
        let file = FileSpan {
            offset_in_torrent: 100 * MIB + MIB / 2,
            len: 100 * MIB,
        };
        let p = plan(&geometry, &Config::default(), &playback(file, 0), |_| false);
        assert_eq!(p.anchors[0].offset, 0);
        assert!(p.anchors.iter().all(|a| a.offset < file.len));
    }

    prop_compose! {
        fn arb_case()(
            piece_len in 1u32..=(4 * 1024 * 1024),
            pieces in 1u64..2_000,
        )(
            piece_len in Just(piece_len),
            total in (u64::from(piece_len) * (pieces - 1) + 1)..=(u64::from(piece_len) * pieces),
            file_start_frac in 0.0f64..1.0,
            file_len_frac in 0.0f64..=1.0,
            position_frac in 0.0f64..=1.0,
            byte_rate in proptest::option::of(1u64..50 * 1024 * 1024),
            have_seed in any::<u64>(),
            have_density in 0u64..=100,
            max_anchors in 0usize..8,
        ) -> (PieceGeometry, Config, Playback, u64, u64) {
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss, clippy::cast_precision_loss)]
            let frac = |x: f64, of: u64| ((x * of as f64) as u64).min(of);
            let geometry = PieceGeometry::new(piece_len, total).unwrap();
            let offset = frac(file_start_frac, total - 1);
            let len = frac(file_len_frac, total - offset).max(1);
            let file = FileSpan { offset_in_torrent: offset, len };
            let config = Config { max_anchors, ..Config::default() };
            let playback = Playback {
                file,
                position: frac(position_frac, len),
                byte_rate,
                next_file: None,
            };
            (geometry, config, playback, have_seed, have_density)
        }
    }

    /// Deterministic pseudo-random availability so failures are reproducible.
    fn have_fn(seed: u64, density: u64) -> impl Fn(u64) -> bool {
        move |piece| {
            let mut x = piece ^ seed;
            x = x.wrapping_mul(0x9E37_79B9_7F4A_7C15);
            x ^= x >> 31;
            x % 100 < density
        }
    }

    proptest! {
        #[test]
        fn plans_are_well_formed((g, config, pb, seed, density) in arb_case()) {
            let have = have_fn(seed, density);
            let p = plan(&g, &config, &pb, &have);

            prop_assert!(p.anchors.len() <= config.max_anchors);
            prop_assert!(p.window.start <= p.window.end && p.window.end <= pb.file.len);
            prop_assert!(p.buffered_bytes <= pb.file.len - pb.position.min(pb.file.len));

            for (i, a) in p.anchors.iter().enumerate() {
                prop_assert_eq!(a.file, AnchorFile::Current);
                prop_assert!(a.offset < pb.file.len, "anchor past end of file");
                // Every anchor sits on data we do not have yet.
                prop_assert!(!have(g.piece_at(pb.file.to_abs(a.offset))));
                // No anchor starts inside an earlier anchor's lookahead.
                for earlier in &p.anchors[..i] {
                    let covered = earlier.offset..earlier.offset.saturating_add(config.engine_lookahead_bytes);
                    prop_assert!(!covered.contains(&a.offset), "duplicate coverage");
                }
            }
        }

        #[test]
        fn critical_phase_has_at_most_one_anchor((g, config, pb, seed, density) in arb_case()) {
            let have = have_fn(seed, density);
            let p = plan(&g, &config, &pb, &have);
            let remaining = pb.file.len - pb.position.min(pb.file.len);
            if p.buffered_bytes < config.critical_bytes.min(remaining) {
                prop_assert!(p.anchors.len() <= 1);
            }
        }

        #[test]
        fn complete_files_need_nothing((g, config, pb, _seed, _density) in arb_case()) {
            let p = plan(&g, &config, &pb, |_| true);
            prop_assert!(p.anchors.is_empty());
            prop_assert_eq!(p.buffered_bytes, pb.file.len - pb.position.min(pb.file.len));
        }

        #[test]
        fn empty_files_start_at_the_playhead((g, config, pb, _seed, _density) in arb_case()) {
            prop_assume!(config.max_anchors > 0 && pb.position < pb.file.len);
            let p = plan(&g, &config, &pb, |_| false);
            prop_assert_eq!(p.anchors[0].offset, pb.position);
            prop_assert_eq!(p.anchors[0].purpose, Purpose::Playhead);
            prop_assert_eq!(p.buffered_bytes, 0);
        }
    }
}
