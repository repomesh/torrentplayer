// SPDX-License-Identifier: MIT OR Apache-2.0

//! Preloading the start of a video before the user plays it.
//!
//! When a search finishes, the app can preload its best result so that pressing play feels
//! instant (see [`StreamEngine::preload`](crate::StreamEngine::preload)): the magnet link's
//! metadata is fetched (librqbit, metadata only), the torrent is added to torrentplayer-swarm, and the
//! first minutes of the best video file plus its last 2 MiB (where MP4 and MKV indexes often
//! live) are downloaded. Nothing is uploaded: preload torrents stay choked, and librqbit only
//! exchanges metadata.
//!
//! Limits: one preload at a time (a new one evicts the old one unless the user added it), at
//! most [`MAX_HEAD_BYTES`] + [`TAIL_BYTES`] of target data, a byte cap on everything received
//! ([`byte_cap`]), and a [`PRELOAD_TIMEOUT`]. An unplayed preload's cache is deleted when it is
//! evicted, and at the next start if the app exits first.
//!
//! This module holds the pure parts: the [`PreloadStatus`] state machine, file choice, and
//! progress arithmetic.

use std::cmp::Ordering;
use std::time::Duration;

use torrentplayer_prioritize::{FileSpan, PieceGeometry, buffered_bytes_from};

use crate::engine::FileEntry;
use crate::media::FileKind;

const MIB: u64 = 1024 * 1024;
/// Smallest head preloaded (or the whole file, if smaller).
pub const MIN_HEAD_BYTES: u64 = 24 * MIB;
/// Largest head preloaded.
pub const MAX_HEAD_BYTES: u64 = 96 * MIB;
/// The head is this fraction of the file: about three minutes of a 90-minute video.
const HEAD_DIVISOR: u64 = 30;
/// Bytes preloaded at the end of the file.
pub const TAIL_BYTES: u64 = 2 * MIB;
/// A preload that has not finished by then stops, keeping what it has.
pub const PRELOAD_TIMEOUT: Duration = Duration::from_secs(180);

const HEX_DIGITS: &[u8; 16] = b"0123456789abcdef";

/// Containers the webview player cannot open (as in the frontend's `playableInWebview`).
const NEEDS_FULL_PLAYER: &[&str] = &[
    "ogv", "avi", "wmv", "flv", "ts", "m2ts", "mts", "mpg", "mpeg", "wma", "ape", "aif", "aiff",
    "mka",
];

/// What a preload is doing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PreloadPhase {
    /// Fetching the torrent's metadata.
    Resolving,
    /// Downloading the start and end of the chosen file.
    Buffering,
    /// Done: the target is downloaded, or it stopped at the time or byte limit with part of it.
    Ready,
    /// Nothing could be preloaded (no metadata, no playable video, or no data in time).
    Failed,
    /// Evicted by a newer preload or shut down.
    Cancelled,
}

/// The current preload.
///
/// Transitions (anything else is ignored):
///
/// ```text
/// Resolving ──resolved──▶ Buffering ──progress ≥ target──▶ Ready
///     │                     │  └──stopped (time/bytes)──▶ Ready if any data, else Failed
///     └──fail──▶ Failed ◀───┘
/// Resolving | Buffering | Ready ──cancel (unless claimed)──▶ Cancelled
/// Buffering | Ready ──claim──▶ same phase, claimed: the user added it; never evicted
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreloadStatus {
    /// Lowercase hex info hash.
    pub info_hash: String,
    /// Phase.
    pub phase: PreloadPhase,
    /// Whether the user has added this torrent (it is theirs now, not a preload).
    pub claimed: bool,
    /// The file being preloaded, once chosen.
    pub file: Option<usize>,
    /// Its path within the torrent (untrusted text from the torrent).
    pub file_name: Option<String>,
    /// Target bytes downloaded so far.
    pub buffered_bytes: u64,
    /// Bytes to preload: head plus tail.
    pub target_bytes: u64,
    /// Why it failed, in words safe to show.
    pub error: Option<String>,
}

impl PreloadStatus {
    /// A new preload, resolving metadata.
    #[must_use]
    pub fn new(info_hash: String) -> Self {
        Self {
            info_hash,
            phase: PreloadPhase::Resolving,
            claimed: false,
            file: None,
            file_name: None,
            buffered_bytes: 0,
            target_bytes: 0,
            error: None,
        }
    }

    /// Downloaded fraction of the target, 0–1.
    #[must_use]
    pub fn fraction(&self) -> f64 {
        if self.target_bytes == 0 {
            return 0.0;
        }
        // Precision loss is irrelevant for a progress fraction.
        #[allow(clippy::cast_precision_loss)]
        let f = self.buffered_bytes as f64 / self.target_bytes as f64;
        f.clamp(0.0, 1.0)
    }

    /// Whether it is still working (resolving or buffering).
    #[must_use]
    pub fn is_active(&self) -> bool {
        matches!(
            self.phase,
            PreloadPhase::Resolving | PreloadPhase::Buffering
        )
    }

    /// Metadata arrived and a file was chosen.
    pub(crate) fn resolved(&mut self, file: usize, name: String, target: u64) -> bool {
        if self.phase != PreloadPhase::Resolving {
            return false;
        }
        self.phase = PreloadPhase::Buffering;
        self.file = Some(file);
        self.file_name = Some(name);
        self.target_bytes = target;
        true
    }

    /// New progress. Returns `true` when this completes the target.
    pub(crate) fn progress(&mut self, buffered: u64) -> bool {
        if self.phase != PreloadPhase::Buffering {
            return false;
        }
        self.buffered_bytes = buffered.min(self.target_bytes);
        if self.buffered_bytes >= self.target_bytes {
            self.phase = PreloadPhase::Ready;
            return true;
        }
        false
    }

    /// Stopped at the time or byte limit before completing.
    pub(crate) fn stopped(&mut self) {
        if self.phase != PreloadPhase::Buffering {
            return;
        }
        if self.buffered_bytes > 0 {
            self.phase = PreloadPhase::Ready;
        } else {
            self.phase = PreloadPhase::Failed;
            self.error = Some("No peers sent data in time.".into());
        }
    }

    /// Failed while resolving or buffering.
    pub(crate) fn fail(&mut self, error: &str) {
        if self.is_active() {
            self.phase = PreloadPhase::Failed;
            self.error = Some(error.to_owned());
        }
    }

    /// Evicted. Returns `false` (and changes nothing) if the user added the torrent.
    pub(crate) fn cancel(&mut self) -> bool {
        if self.claimed {
            return false;
        }
        if matches!(
            self.phase,
            PreloadPhase::Resolving | PreloadPhase::Buffering | PreloadPhase::Ready
        ) {
            self.phase = PreloadPhase::Cancelled;
        }
        true
    }

    /// The user added the torrent. Returns `true` if it has a torrent to hand over.
    pub(crate) fn claim(&mut self) -> bool {
        if matches!(self.phase, PreloadPhase::Buffering | PreloadPhase::Ready) {
            self.claimed = true;
        }
        self.claimed
    }
}

/// Head and tail bytes to preload for a file of `len` bytes, given the tail the scheduler
/// prefetches ([`TAIL_BYTES`] by default).
#[must_use]
pub fn targets(len: u64, tail_bytes: u64) -> (u64, u64) {
    let head = (len / HEAD_DIVISOR).clamp(MIN_HEAD_BYTES, MAX_HEAD_BYTES);
    if len <= head + tail_bytes {
        (len, 0)
    } else {
        (head, tail_bytes)
    }
}

/// Everything received (duplicates and discarded data included) before a preload stops early.
#[must_use]
pub fn byte_cap(target: u64) -> u64 {
    target + target / 2 + 8 * MIB
}

/// Target bytes present: the contiguous head from the start of the file (up to `head`) plus
/// the downloaded part of the last `tail` bytes.
#[must_use]
pub fn preloaded_bytes(
    geometry: &PieceGeometry,
    span: FileSpan,
    head: u64,
    tail: u64,
    have: impl Fn(u64) -> bool,
) -> u64 {
    let head_done = buffered_bytes_from(geometry, span, 0, &have).min(head);
    let tail_start = span.to_abs(span.len.saturating_sub(tail));
    let tail_end = span.to_abs(span.len);
    let mut tail_done = 0;
    for piece in geometry.pieces_overlapping(tail_start..tail_end) {
        if have(piece) {
            let start = geometry.piece_start(piece).max(tail_start);
            let end = (geometry.piece_start(piece) + geometry.piece_len()).min(tail_end);
            tail_done += end.saturating_sub(start);
        }
    }
    head_done + tail_done
}

/// How ready a file is to play, from the pieces present (see [`file_readiness`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileReadiness {
    /// The file's index.
    pub file: usize,
    /// Verified bytes of the file.
    pub downloaded: u64,
    /// The file's length.
    pub len: u64,
    /// Its start and end are present, as much as a preload downloads ([`targets`]), so
    /// playing it starts at once.
    pub startable: bool,
}

impl FileReadiness {
    /// Whether the whole file is downloaded.
    #[must_use]
    pub fn complete(&self) -> bool {
        self.downloaded >= self.len
    }
}

/// How ready `file` (at `span`) is to play: its downloaded bytes, and whether the head and
/// tail of `(head, tail)` bytes (from [`targets`]) are present. Only reads `have`.
#[must_use]
pub fn file_readiness(
    geometry: &PieceGeometry,
    span: FileSpan,
    file: usize,
    (head, tail): (u64, u64),
    have: impl Fn(u64) -> bool,
) -> FileReadiness {
    let (start, end) = (span.to_abs(0), span.to_abs(span.len));
    let mut downloaded = 0;
    for piece in geometry.pieces_overlapping(start..end) {
        if have(piece) {
            let from = geometry.piece_start(piece).max(start);
            let to = (geometry.piece_start(piece) + geometry.piece_len()).min(end);
            downloaded += to.saturating_sub(from);
        }
    }
    let startable =
        downloaded >= span.len || preloaded_bytes(geometry, span, head, tail, &have) >= head + tail;
    FileReadiness {
        file,
        downloaded,
        len: span.len,
        startable,
    }
}

/// The file to preload: the largest playable video, or the first episode of a series pack
/// (several videos of similar size). Samples and trailers are skipped. A torrent without
/// video (an album, a podcast, an audiobook) starts at its first track. `None` if nothing
/// would play in the built-in player.
#[must_use]
pub fn choose_file(files: &[FileEntry]) -> Option<&FileEntry> {
    let playable = |kind: FileKind| {
        move |f: &&FileEntry| {
            f.kind == kind && f.len > 0 && webview_playable(&f.path) && !is_extra(&f.path)
        }
    };
    let mut videos: Vec<&FileEntry> = files.iter().filter(playable(FileKind::Video)).collect();
    if videos.is_empty() {
        let mut tracks: Vec<&FileEntry> = files.iter().filter(playable(FileKind::Audio)).collect();
        tracks.sort_by(|a, b| natural_cmp(&a.path, &b.path));
        return tracks.first().copied();
    }
    let largest = videos.iter().map(|f| f.len).max().unwrap_or(0);
    if videos.len() >= 3 {
        let mut sizes: Vec<u64> = videos.iter().map(|f| f.len).collect();
        sizes.sort_unstable();
        let median = sizes[sizes.len() / 2];
        if largest < median.saturating_mul(2) {
            videos.sort_by(|a, b| natural_cmp(&a.path, &b.path));
            return videos.first().copied();
        }
    }
    videos.into_iter().find(|f| f.len == largest)
}

/// The file to preload when the caller may name one (the play queue knows which lecture is
/// next): `wanted` if it is a non-empty video or audio file, otherwise [`choose_file`]'s
/// choice.
#[must_use]
pub fn preload_target(files: &[FileEntry], wanted: Option<usize>) -> Option<&FileEntry> {
    wanted
        .and_then(|index| {
            files
                .iter()
                .find(|f| f.index == index && f.kind.is_playable() && f.len > 0)
        })
        .or_else(|| choose_file(files))
}

/// Whether the built-in player can open a file, by extension.
fn webview_playable(path: &str) -> bool {
    let ext = path.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
    !NEEDS_FULL_PLAYER.contains(&ext.as_str())
}

/// Samples, trailers, and extras that come with a release.
fn is_extra(path: &str) -> bool {
    path.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .any(|w| matches!(w, "sample" | "samples" | "trailer" | "trailers" | "extras"))
}

/// Compares paths with digit runs as numbers, so `E2` sorts before `E10`.
fn natural_cmp(a: &str, b: &str) -> Ordering {
    let (mut a, mut b) = (a.as_bytes(), b.as_bytes());
    loop {
        match (a.first(), b.first()) {
            (None, None) => return Ordering::Equal,
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(x), Some(y)) if x.is_ascii_digit() && y.is_ascii_digit() => {
                let da = a.iter().take_while(|c| c.is_ascii_digit()).count();
                let db = b.iter().take_while(|c| c.is_ascii_digit()).count();
                let (na, nb) = (trim_zeros(&a[..da]), trim_zeros(&b[..db]));
                let order = na.len().cmp(&nb.len()).then_with(|| na.cmp(nb));
                if order != Ordering::Equal {
                    return order;
                }
                a = &a[da..];
                b = &b[db..];
            }
            (Some(x), Some(y)) => {
                let order = x.to_ascii_lowercase().cmp(&y.to_ascii_lowercase());
                if order != Ordering::Equal {
                    return order;
                }
                a = &a[1..];
                b = &b[1..];
            }
        }
    }
}

fn trim_zeros(digits: &[u8]) -> &[u8] {
    let zeros = digits.iter().take_while(|&&d| d == b'0').count();
    &digits[zeros.min(digits.len().saturating_sub(1))..]
}

/// The lowercase hex info hash of a magnet link (`btih` as 40 hex or 32 base32 characters).
#[must_use]
pub fn magnet_info_hash(magnet: &str) -> Option<String> {
    let query = magnet.strip_prefix("magnet:?")?;
    let value = query.split('&').find_map(|pair| {
        let (key, value) = pair.split_once('=')?;
        let urn = value.get(..9)?;
        (key == "xt" && urn.eq_ignore_ascii_case("urn:btih:")).then(|| &value[9..])
    })?;
    if value.len() == 40 && value.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Some(value.to_ascii_lowercase());
    }
    if value.len() != 32 {
        return None;
    }
    let mut bits: u64 = 0;
    let mut nbits = 0;
    let mut bytes: Vec<u8> = Vec::with_capacity(20);
    for c in value.bytes() {
        let v = match c.to_ascii_uppercase() {
            c @ b'A'..=b'Z' => c - b'A',
            c @ b'2'..=b'7' => c - b'2' + 26,
            _ => return None,
        };
        bits = (bits << 5) | u64::from(v);
        nbits += 5;
        if nbits >= 8 {
            nbits -= 8;
            bytes.push(((bits >> nbits) & 0xff).to_le_bytes()[0]);
        }
    }
    (bytes.len() == 20).then(|| {
        bytes
            .iter()
            .flat_map(|b| {
                [
                    HEX_DIGITS[usize::from(b >> 4)],
                    HEX_DIGITS[usize::from(b & 0x0f)],
                ]
            })
            .map(char::from)
            .collect()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn video(index: usize, path: &str, len: u64) -> FileEntry {
        FileEntry {
            index,
            path: path.into(),
            len,
            offset_in_torrent: 0,
            kind: FileKind::from_name(path),
        }
    }

    #[test]
    fn state_machine_happy_path() {
        let mut s = PreloadStatus::new("ab".repeat(20));
        assert!(s.is_active());
        assert!(!s.claim(), "nothing to claim while resolving");
        assert!(s.resolved(0, "a.mp4".into(), 100));
        assert!(!s.resolved(1, "b.mp4".into(), 5), "resolves once");
        assert!(!s.progress(40));
        assert!((s.fraction() - 0.4).abs() < 1e-9);
        assert!(s.progress(150), "completes, clamped to the target");
        assert_eq!(s.phase, PreloadPhase::Ready);
        assert_eq!(s.buffered_bytes, 100);
        assert!(!s.progress(10), "ignored once ready");
        assert!(s.claim());
        assert!(!s.cancel(), "claimed torrents are never evicted");
        assert_eq!(s.phase, PreloadPhase::Ready);
    }

    #[test]
    fn state_machine_stops_fails_and_cancels() {
        let mut s = PreloadStatus::new(String::new());
        s.resolved(0, "a.mp4".into(), 100);
        s.progress(30);
        s.stopped();
        assert_eq!(s.phase, PreloadPhase::Ready, "partial data is kept");

        let mut s = PreloadStatus::new(String::new());
        s.resolved(0, "a.mp4".into(), 100);
        s.stopped();
        assert_eq!(s.phase, PreloadPhase::Failed);
        assert!(s.error.is_some());

        let mut s = PreloadStatus::new(String::new());
        s.fail("no metadata");
        assert_eq!(s.phase, PreloadPhase::Failed);
        assert!(!s.claim());
        assert!(s.cancel(), "failed preloads are evictable");
        assert_eq!(s.phase, PreloadPhase::Failed, "terminal phase kept");

        let mut s = PreloadStatus::new(String::new());
        assert!(s.cancel());
        assert_eq!(s.phase, PreloadPhase::Cancelled);
        assert!(
            !s.resolved(0, "a.mp4".into(), 1),
            "cancelled preloads stay cancelled"
        );
        s.fail("late");
        assert_eq!(s.phase, PreloadPhase::Cancelled);
        assert_eq!(s.error, None);
    }

    #[test]
    fn targets_scale_with_file_size_within_limits() {
        assert_eq!(
            targets(10 * MIB, TAIL_BYTES),
            (10 * MIB, 0),
            "small files entirely"
        );
        assert_eq!(targets(25 * MIB, TAIL_BYTES), (25 * MIB, 0));
        assert_eq!(targets(300 * MIB, TAIL_BYTES), (MIN_HEAD_BYTES, TAIL_BYTES));
        assert_eq!(targets(1500 * MIB, TAIL_BYTES), (50 * MIB, TAIL_BYTES));
        assert_eq!(
            targets(40_000 * MIB, TAIL_BYTES),
            (MAX_HEAD_BYTES, TAIL_BYTES)
        );
        assert!(byte_cap(98 * MIB) > 98 * MIB);
    }

    #[test]
    fn readiness_counts_the_file_and_its_ends() {
        // 10 pieces of 100 bytes; the file spans bytes 50..950 (pieces 0..=9).
        let geometry = PieceGeometry::new(100, 1000).unwrap();
        let span = FileSpan {
            offset_in_torrent: 50,
            len: 900,
        };
        let have = |pieces: &'static [u64]| move |p: u64| pieces.contains(&p);
        let r = file_readiness(&geometry, span, 3, (300, 100), have(&[]));
        assert_eq!((r.file, r.downloaded, r.len), (3, 0, 900));
        assert!(!r.startable && !r.complete());

        // Head 0..300 (pieces 0..=3, 350 bytes of the file) and tail 800..900 (pieces 8, 9).
        let r = file_readiness(&geometry, span, 3, (300, 100), have(&[0, 1, 2, 3, 8, 9]));
        assert_eq!(r.downloaded, 50 + 300 + 100 + 50);
        assert!(r.startable, "head and tail present");
        assert!(!r.complete());

        let r = file_readiness(&geometry, span, 3, (300, 100), have(&[0, 1, 2, 3]));
        assert!(!r.startable, "no tail yet");

        let all = file_readiness(&geometry, span, 3, (300, 100), |_| true);
        assert_eq!(all.downloaded, 900);
        assert!(all.complete() && all.startable);
    }

    #[test]
    fn counts_head_and_tail_progress() {
        // 10 pieces of 100 bytes; the file spans bytes 50..950.
        let geometry = PieceGeometry::new(100, 1000).unwrap();
        let span = FileSpan {
            offset_in_torrent: 50,
            len: 900,
        };
        let have = |pieces: &'static [u64]| move |p: u64| pieces.contains(&p);
        assert_eq!(preloaded_bytes(&geometry, span, 300, 100, have(&[])), 0);
        // Head: pieces 0..=2 cover file bytes 0..250 contiguously.
        assert_eq!(
            preloaded_bytes(&geometry, span, 300, 100, have(&[0, 1, 2])),
            250
        );
        assert_eq!(
            preloaded_bytes(&geometry, span, 200, 100, have(&[0, 1, 2])),
            200
        );
        // Tail: file bytes 800..900 are absolute 850..950, in piece 8 (50 bytes) and 9 (50).
        assert_eq!(preloaded_bytes(&geometry, span, 300, 100, have(&[9])), 50);
        assert_eq!(
            preloaded_bytes(&geometry, span, 300, 100, have(&[0, 1, 2, 3, 8, 9])),
            300 + 100
        );
    }

    #[test]
    fn chooses_the_main_video_or_the_first_episode() {
        let movie = [
            video(0, "Movie/Movie.2020.1080p.mkv", 4_000_000_000),
            video(1, "Movie/Sample/movie-sample.mkv", 50_000_000),
            video(2, "Movie/Movie.nfo", 1000),
        ];
        assert_eq!(choose_file(&movie).unwrap().index, 0);

        let pack = [
            video(0, "Show/Show.S01E10.mp4", 510_000_000),
            video(1, "Show/Show.S01E02.mp4", 500_000_000),
            video(2, "Show/Show.S01E01.mp4", 490_000_000),
            video(3, "Show/Show.S01E03.mp4", 520_000_000),
        ];
        assert_eq!(
            choose_file(&pack).unwrap().index,
            2,
            "first episode, not the largest"
        );

        let lectures = [
            video(0, "Course/Lecture 2.mp4", 300_000_000),
            video(1, "Course/Lecture 10.mp4", 320_000_000),
            video(2, "Course/Lecture 1.mp4", 310_000_000),
        ];
        assert_eq!(choose_file(&lectures).unwrap().index, 2);

        let mixed = [
            video(0, "a.avi", 900_000_000),
            video(1, "b.mp4", 100_000_000),
        ];
        assert_eq!(
            choose_file(&mixed).unwrap().index,
            1,
            "playable beats larger"
        );
        assert!(choose_file(&[video(0, "a.avi", 1)]).is_none());
        assert!(choose_file(&[video(0, "notes.pdf", 1)]).is_none());

        // Without video, an album or audiobook starts at its first track.
        let album = [
            video(0, "Album/10 - Finale.flac", 40_000_000),
            video(1, "Album/02 - Second.flac", 30_000_000),
            video(2, "Album/01 - Opening.flac", 20_000_000),
            video(3, "Album/cover.jpg", 500_000),
        ];
        assert_eq!(choose_file(&album).unwrap().index, 2);
        // Any video still wins over audio.
        let film = [
            video(0, "Film/Soundtrack/01.mp3", 9_000_000),
            video(1, "Film/Film.mp4", 900_000_000),
        ];
        assert_eq!(choose_file(&film).unwrap().index, 1);
        assert!(
            choose_file(&[video(0, "song.wma", 1_000_000)]).is_none(),
            "needs the full player"
        );
    }

    #[test]
    fn preloads_the_named_file_if_it_is_a_video() {
        let course = [
            video(0, "Course/Lecture 1.mp4", 320_000_000),
            video(1, "Course/Lecture 2.mkv", 310_000_000),
            video(2, "Course/notes.pdf", 1000),
            video(3, "Course/Lecture 3.avi", 0),
        ];
        assert_eq!(preload_target(&course, Some(1)).unwrap().index, 1);
        assert_eq!(
            preload_target(&course, None).unwrap().index,
            0,
            "default choice"
        );
        assert_eq!(
            preload_target(&course, Some(2)).unwrap().index,
            0,
            "not a video"
        );
        assert_eq!(preload_target(&course, Some(3)).unwrap().index, 0, "empty");
        assert_eq!(
            preload_target(&course, Some(9)).unwrap().index,
            0,
            "unknown"
        );
        let avi = [video(0, "a.avi", 5)];
        assert_eq!(
            preload_target(&avi, Some(0)).unwrap().index,
            0,
            "a queued file is preloaded even if the built-in player may not play it"
        );
        assert!(preload_target(&avi, None).is_none());
    }

    #[test]
    fn natural_order() {
        assert_eq!(natural_cmp("E2", "E10"), Ordering::Less);
        assert_eq!(natural_cmp("e02", "E2"), Ordering::Equal);
        assert_eq!(natural_cmp("Lecture 1", "Lecture 1a"), Ordering::Less);
        assert_eq!(natural_cmp("b", "A"), Ordering::Greater);
        assert_eq!(natural_cmp("0", "00"), Ordering::Equal);
    }

    #[test]
    fn parses_magnet_info_hashes() {
        let hex = "dd8255ecdc7ca55fb0bbf81323d87062db1f6d1c";
        assert_eq!(
            magnet_info_hash(&format!("magnet:?xt=urn:btih:{}&dn=x", hex.to_uppercase())),
            Some(hex.into())
        );
        assert_eq!(
            magnet_info_hash(&format!("magnet:?dn=x&xt=urn:btih:{hex}")),
            Some(hex.into())
        );
        assert_eq!(
            magnet_info_hash(&format!("magnet:?xt=urn:btih:{}", "A".repeat(32))),
            Some("0".repeat(40))
        );
        assert_eq!(magnet_info_hash("magnet:?xt=urn:btih:zz"), None);
        assert_eq!(magnet_info_hash("https://example.org/a.torrent"), None);
    }
}
