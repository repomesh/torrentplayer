// SPDX-License-Identifier: MIT OR Apache-2.0

//! Streaming benchmarks for TorrentPlayer.
//!
//! Each run starts from an empty download directory, adds a torrent, and then acts like a
//! player through the real loopback stream server:
//!
//! 1. **Start-up:** request the file's head and tail at the same time (players need container
//!    metadata from both ends) and time until both have arrived.
//! 2. **Seeks:** for each seek position, report the new playhead to the playback session and
//!    request data there, timing until it has arrived.
//!
//! These are *data-ready* latencies: the time until a player has the bytes it needs. Decoder
//! start-up is measured separately in the app's debug overlay. See `docs/performance.md`.

#![forbid(unsafe_code)]

mod engines;
pub mod http;
pub mod link;
pub mod seeder;
pub mod tracker;
pub mod webseed;

pub use engines::EngineKind;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::Context as _;
use serde::Serialize;
use torrentplayer_stream::{ServerConfig, StreamServer};
use torrentplayer_torrent::TorrentSource;

use crate::engines::{BenchEngine, Playhead};
use crate::http::{Player, Target, timed_range};

/// What to measure.
// Independent switches for the swarm under test.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone)]
pub struct BenchConfig {
    /// File index to play; the largest video file if `None`.
    pub file: Option<usize>,
    /// Seek positions as fractions of the file length.
    pub seeks: Vec<f64>,
    /// Bytes a player needs at the head and tail of the file to start.
    pub startup_bytes: u64,
    /// Bytes a player needs at a seek target to resume.
    pub seek_bytes: u64,
    /// Media duration in seconds, if known, so the byte rate can be derived.
    pub duration_secs: Option<f64>,
    /// Give up on a single measurement after this long.
    pub timeout: Duration,
    /// How long to "watch" after start-up and after each seek, so every seek happens in
    /// steady state, as when a viewer watches for a while and then jumps.
    pub watch: Duration,
    /// Peers to connect to directly (for the local seeder).
    pub peers: Vec<SocketAddr>,
    /// Use DHT and trackers (off for the local seeder).
    pub public_swarm: bool,
    /// Use the torrent's trackers even though the swarm is local (a local tracker).
    pub local_tracker: bool,
    /// Let the swarm engine use web seeds on loopback (the local web seed).
    pub local_web_seeds: bool,
    /// Use the torrent's web seeds (swarm engine).
    pub web_seeds: bool,
    /// Use the DHT when on a public swarm.
    pub dht: bool,
    /// Cap the download rate (bytes per second), to model a home connection and limit usage.
    pub download_limit_bps: Option<std::num::NonZeroU32>,
    /// Maximum connected peers.
    pub peer_limit: Option<usize>,
    /// Maximum simultaneous connection attempts (swarm engine).
    pub max_connecting: Option<usize>,
    /// Wait this long between adding the torrent and pressing play, as when a viewer opens a
    /// course page before choosing a lecture. Peers are discovered meanwhile.
    pub warm: Duration,
    /// Measure every seek in its own session, from an empty download directory: add, start
    /// up, watch, then seek once. Otherwise all seeks share one session, and a later seek can
    /// land on data already downloaded for an earlier one.
    pub cold_seeks: bool,
    /// Prioritization settings (librqbit engine).
    pub prioritization: torrentplayer_prioritize::Config,
    /// Scheduler settings (swarm engine).
    pub scheduler: torrentplayer_swarm::SchedulerConfig,
}

impl Default for BenchConfig {
    fn default() -> Self {
        Self {
            file: None,
            seeks: vec![0.1, 0.5, 0.9, 0.25],
            startup_bytes: 1024 * 1024,
            seek_bytes: 1024 * 1024,
            duration_secs: None,
            timeout: Duration::from_secs(120),
            watch: Duration::from_secs(3),
            peers: Vec::new(),
            public_swarm: true,
            local_tracker: false,
            local_web_seeds: false,
            web_seeds: true,
            dht: true,
            download_limit_bps: None,
            peer_limit: None,
            max_connecting: None,
            warm: Duration::ZERO,
            cold_seeks: true,
            prioritization: torrentplayer_prioritize::Config::default(),
            scheduler: torrentplayer_swarm::SchedulerConfig::default(),
        }
    }
}

/// One seek measurement.
#[derive(Debug, Clone, Serialize)]
pub struct SeekResult {
    /// Seek target as a fraction of the file.
    pub fraction: f64,
    /// Seek target in bytes.
    pub offset: u64,
    /// Until the first byte at the target arrived.
    pub first_byte_ms: f64,
    /// Until [`BenchConfig::seek_bytes`] at the target arrived.
    pub ready_ms: f64,
}

/// One complete run.
#[derive(Debug, Clone, Serialize)]
pub struct RunResult {
    /// Engine measured.
    pub engine: String,
    /// Name of the torrent.
    pub torrent: String,
    /// Path of the file played.
    pub file: String,
    /// File length in bytes.
    pub file_bytes: u64,
    /// Adding the torrent, including fetching metadata for magnet links.
    pub metadata_ms: f64,
    /// From pressing play until the first byte of the file arrived.
    pub first_byte_ms: f64,
    /// From pressing play until the head and tail were both available.
    pub startup_ms: f64,
    /// Seek measurements, in order.
    pub seeks: Vec<SeekResult>,
    /// Connected peers when play was pressed.
    pub peers_at_play: u32,
    /// Connected peers at the end of the run.
    pub peers_live: u32,
    /// Peers learned about during the run.
    pub peers_seen: u32,
    /// Download rate at the end of the run, bytes per second.
    pub download_bps: u64,
    /// Average download rate from pressing play to the end of the run (verified bytes over
    /// elapsed time), bytes per second. Unlike [`RunResult::download_bps`], a single sample of
    /// each engine's own estimate, this is measured the same way for both engines.
    pub avg_download_bps: u64,
    /// Bytes received but not needed (swarm engine only).
    pub wasted_bytes: Option<u64>,
    /// Duplicate requests sent (swarm engine only).
    pub duplicate_requests: Option<u64>,
    /// Media byte rate used for the run (bytes per second): from `--duration`, else the
    /// scheduler's fallback.
    pub byte_rate: u64,
    /// For each watch period, the smallest player-side buffer while playing: seconds of media
    /// the player had received ahead of the playhead. The simulated player reads at most twice
    /// the byte rate, so the buffer grows by at most one second per second of watching.
    pub buffer_secs: Vec<f64>,
    /// The lowest of [`RunResult::buffer_secs`].
    pub min_buffer_secs: Option<f64>,
    /// Times playback stalled (ran out of data) while watching. The viewer's playhead stops
    /// while stalled and resumes once [`REBUFFER_SECS`] are buffered, like a real player.
    pub stalls: u32,
    /// Total time spent stalled while watching, in milliseconds.
    pub stall_ms: f64,
    /// Total time spent watching (after start-up and after each seek), in milliseconds.
    pub watch_ms: f64,
}

/// A stalled player resumes once it has this much media buffered (or reaches the end).
pub const REBUFFER_SECS: f64 = 1.0;

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

/// Runs one benchmark from an empty cache. `source` is called before every session: once, or
/// once per seek with [`BenchConfig::cold_seeks`].
///
/// # Errors
///
/// Returns an error if the torrent cannot be added or a measurement times out.
pub async fn run_once(
    kind: EngineKind,
    mut source: impl FnMut() -> anyhow::Result<TorrentSource>,
    config: &BenchConfig,
) -> anyhow::Result<RunResult> {
    if !config.cold_seeks || config.seeks.len() < 2 {
        return run_session(kind, source()?, config).await;
    }
    let mut merged: Option<RunResult> = None;
    for &fraction in &config.seeks {
        let session = BenchConfig {
            seeks: vec![fraction],
            ..config.clone()
        };
        let run = run_session(kind, source()?, &session).await?;
        merged = Some(match merged {
            Some(m) => m.merge(run),
            None => run,
        });
    }
    merged.context("no sessions ran")
}

/// One session in a fresh download directory.
async fn run_session(
    kind: EngineKind,
    source: TorrentSource,
    config: &BenchConfig,
) -> anyhow::Result<RunResult> {
    // The download directory is deleted when `dir` is dropped.
    let dir = tempfile::tempdir().context("creating a download directory")?;
    run_in(kind, dir.path().to_path_buf(), source, config).await
}

impl RunResult {
    /// Folds a later cold-seek session into this run. Metadata and start-up come from the
    /// first session; seeks, buffers, and stalls accumulate; peers and rates cover all
    /// sessions.
    fn merge(mut self, next: Self) -> Self {
        let sessions = self.seeks.len().max(1) as u64;
        self.seeks.extend(next.seeks);
        self.peers_live = next.peers_live;
        self.peers_seen = self.peers_seen.max(next.peers_seen);
        self.download_bps = next.download_bps;
        self.avg_download_bps =
            (self.avg_download_bps * sessions + next.avg_download_bps) / (sessions + 1);
        self.wasted_bytes = add(self.wasted_bytes, next.wasted_bytes);
        self.duplicate_requests = add(self.duplicate_requests, next.duplicate_requests);
        self.buffer_secs.extend(next.buffer_secs);
        self.min_buffer_secs = self.buffer_secs.iter().copied().reduce(f64::min);
        self.stalls += next.stalls;
        self.stall_ms += next.stall_ms;
        self.watch_ms += next.watch_ms;
        self
    }
}

fn add(a: Option<u64>, b: Option<u64>) -> Option<u64> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a + b),
        (a, b) => a.or(b),
    }
}

// One benchmark run is a linear script (add, start-up, seeks, clean-up); splitting it would
// scatter the timing logic.
#[allow(clippy::too_many_lines)]
async fn run_in(
    kind: EngineKind,
    dir: PathBuf,
    source: TorrentSource,
    config: &BenchConfig,
) -> anyhow::Result<RunResult> {
    let mut engine = BenchEngine::start(kind, dir, config).await?;

    let started = Instant::now();
    let (name, files) = engine.add(source, config.peers.clone()).await?;
    let metadata_ms = ms(started.elapsed());

    let entry = match config.file {
        Some(index) => files
            .iter()
            .find(|f| f.index == index)
            .context("no file with that index")?,
        None => files
            .iter()
            .filter(|f| f.video)
            .max_by_key(|f| f.len)
            .context("the torrent has no video files")?,
    }
    .clone();
    let len = entry.len;

    let server = StreamServer::start(ServerConfig::default()).await?;
    let published = server.publish(engine.media(&entry)?);
    let target = Target::parse(server.local_addr(), &published.url)?;

    // The simulated player reads at twice the media rate, like a player filling its buffer.
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_precision_loss
    )]
    let byte_rate = config
        .duration_secs
        .filter(|d| *d > 0.0)
        .map_or(config.prioritization.fallback_byte_rate, |d| {
            (len as f64 / d) as u64
        })
        .max(1);
    let pace = byte_rate * 2;

    tokio::time::sleep(config.warm).await;
    let peers_at_play = engine.stats()?.peers_live;

    // Start-up: the user presses play. The player reads from the start while the tail (the
    // container index) is fetched alongside, as players do for MP4 files with `moov` at the end.
    let play = Instant::now();
    let playhead = engine.play(&entry, config, byte_rate).await?;
    playhead.update(0);
    let head_len = config.startup_bytes.min(len);
    let tail_len = config.startup_bytes.min(len);
    let requested = play.elapsed();
    let (mut player, head_ready) = Player::start(target.clone(), 0, head_len, pace);
    let (head, tail) = tokio::join!(
        tokio::time::timeout(config.timeout, head_ready),
        timed_range(&target, len - tail_len, tail_len, config.timeout),
    );
    let head = head
        .context("start-up (head) timed out")?
        .context("player stopped")?
        .context("start-up (head)")?;
    tail.context("start-up (tail)")?;
    let startup_ms = ms(play.elapsed());
    let first_byte_ms = ms(requested + head.first_byte);
    let mut watched = vec![
        watch(
            &*playhead,
            &player,
            0,
            head_len,
            byte_rate,
            len,
            config.watch,
        )
        .await,
    ];

    // Seeks: the player drops its connection and reopens at the target, as `<video>` does.
    let mut seeks = Vec::new();
    for &fraction in &config.seeks {
        #[allow(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            clippy::cast_precision_loss
        )]
        let offset = ((fraction.clamp(0.0, 1.0) * len as f64) as u64)
            .min(len.saturating_sub(config.seek_bytes));
        let seek_len = config.seek_bytes.min(len - offset);
        let seek = Instant::now();
        drop(player);
        playhead.update(offset);
        let requested = seek.elapsed();
        let (next, ready) = Player::start(target.clone(), offset, seek_len, pace);
        player = next;
        let timing = tokio::time::timeout(config.timeout, ready)
            .await
            .with_context(|| format!("seek to {fraction} timed out"))?
            .context("player stopped")?
            .with_context(|| format!("seek to {fraction}"))?;
        seeks.push(SeekResult {
            fraction,
            offset,
            first_byte_ms: ms(requested + timing.first_byte),
            ready_ms: ms(requested + timing.complete),
        });
        watched.push(
            watch(
                &*playhead,
                &player,
                offset,
                seek_len,
                byte_rate,
                len,
                config.watch,
            )
            .await,
        );
    }
    drop(player);
    let buffers: Vec<f64> = watched.iter().filter_map(|w| w.min_buffer).collect();
    let stalls = watched.iter().map(|w| w.stalls).sum();
    let stall_ms = watched.iter().map(|w| ms(w.stalled)).sum();
    let watch_ms = watched.iter().map(|w| ms(w.elapsed)).sum();

    let stats = engine.stats()?;
    let run_time = play.elapsed();
    drop(playhead);
    server.unpublish(published.id);
    engine.finish().await?;

    Ok(RunResult {
        engine: kind.to_string(),
        torrent: name,
        file: entry.path,
        file_bytes: len,
        metadata_ms,
        first_byte_ms,
        startup_ms,
        seeks,
        peers_at_play,
        peers_live: stats.peers_live,
        peers_seen: stats.peers_seen,
        download_bps: stats.download_bps,
        #[allow(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            clippy::cast_precision_loss
        )]
        avg_download_bps: (stats.downloaded_bytes as f64 / run_time.as_secs_f64().max(1e-3)) as u64,
        wasted_bytes: stats.wasted_bytes,
        duplicate_requests: stats.duplicate_requests,
        min_buffer_secs: buffers.iter().copied().reduce(f64::min),
        buffer_secs: buffers,
        stalls,
        stall_ms,
        watch_ms,
        byte_rate,
    })
}

/// What one watch period saw.
#[derive(Debug, Clone, Copy, Default)]
struct Watched {
    /// Smallest buffer while playing, in seconds; `None` if nothing was played or the end of
    /// the file was buffered.
    min_buffer: Option<f64>,
    stalls: u32,
    stalled: Duration,
    elapsed: Duration,
}

/// Plays for `duration` like a viewer: the playhead advances at the media byte rate while the
/// player has data, stops when it runs out (a stall), and resumes once [`REBUFFER_SECS`] are
/// buffered. The playhead is reported to the engine as it moves, as the app does. The player
/// started at `from` and had `ready` bytes when playback began.
async fn watch(
    playhead: &dyn Playhead,
    player: &Player,
    from: u64,
    ready: u64,
    byte_rate: u64,
    len: u64,
    duration: Duration,
) -> Watched {
    #[allow(clippy::cast_precision_loss)]
    let rate = byte_rate.max(1) as f64;
    let started = Instant::now();
    let mut last = started;
    #[allow(clippy::cast_precision_loss)]
    let mut position = from as f64;
    let mut stalled_since: Option<Instant> = None;
    let mut out = Watched::default();
    let mut min_buffer: Option<f64> = None;
    while started.elapsed() < duration {
        tokio::time::sleep(Duration::from_millis(250)).await;
        let now = Instant::now();
        let dt = now.saturating_duration_since(last).as_secs_f64();
        last = now;
        // Data the player holds runs from `from` to here.
        #[allow(clippy::cast_precision_loss)]
        let available = (from + ready + player.bytes_read()).min(len) as f64;
        #[allow(clippy::cast_precision_loss)]
        let at_end = available >= len as f64;
        match stalled_since {
            None => {
                let next = position + dt * rate;
                if next > available && !at_end {
                    // Ran dry part-way through this tick.
                    let played_secs = ((available - position) / rate).max(0.0);
                    let unplayed = Duration::from_secs_f64((dt - played_secs).max(0.0));
                    stalled_since = Some(now.checked_sub(unplayed).unwrap_or(now));
                    position = available;
                    out.stalls += 1;
                } else {
                    position = next.min(available);
                    if !at_end {
                        let ahead = (available - position) / rate;
                        min_buffer = Some(min_buffer.map_or(ahead, |m: f64| m.min(ahead)));
                    }
                }
            }
            Some(since) => {
                if at_end || available - position >= REBUFFER_SECS * rate {
                    out.stalled += now.saturating_duration_since(since);
                    stalled_since = None;
                }
            }
        }
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        playhead.update(position as u64);
    }
    if let Some(since) = stalled_since {
        out.stalled += Instant::now().saturating_duration_since(since);
    }
    out.elapsed = started.elapsed();
    out.min_buffer = min_buffer;
    out
}

/// Median and 95th percentile of a set of measurements.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct Summary {
    /// Number of samples.
    pub samples: usize,
    /// Median, in milliseconds.
    pub median_ms: f64,
    /// 95th percentile (nearest rank), in milliseconds.
    pub p95_ms: f64,
    /// Worst sample, in milliseconds.
    pub max_ms: f64,
}

impl Summary {
    /// Summarizes `values`; `None` if there are none.
    #[must_use]
    pub fn of(values: &[f64]) -> Option<Self> {
        if values.is_empty() {
            return None;
        }
        let mut sorted = values.to_vec();
        sorted.sort_by(f64::total_cmp);
        let at = |q: f64| {
            // Nearest-rank percentile; the cast is bounded by the length.
            #[allow(
                clippy::cast_possible_truncation,
                clippy::cast_sign_loss,
                clippy::cast_precision_loss
            )]
            let rank = (q * sorted.len() as f64).ceil() as usize;
            sorted[rank.clamp(1, sorted.len()) - 1]
        };
        Some(Self {
            samples: sorted.len(),
            median_ms: at(0.5),
            p95_ms: at(0.95),
            max_ms: sorted[sorted.len() - 1],
        })
    }
}

/// All runs plus their summaries.
#[derive(Debug, Clone, Serialize)]
pub struct Report {
    /// Individual runs.
    pub runs: Vec<RunResult>,
    /// Time to metadata.
    pub metadata: Option<Summary>,
    /// Time to first byte after pressing play.
    pub first_byte: Option<Summary>,
    /// Start-up readiness.
    pub startup: Option<Summary>,
    /// Seek readiness, pooled across all seeks.
    pub seek: Option<Summary>,
}

impl Report {
    /// Builds summaries from runs.
    #[must_use]
    pub fn new(runs: Vec<RunResult>) -> Self {
        let collect = |f: &dyn Fn(&RunResult) -> f64| runs.iter().map(f).collect::<Vec<_>>();
        let seeks: Vec<f64> = runs
            .iter()
            .flat_map(|r| r.seeks.iter().map(|s| s.ready_ms))
            .collect();
        Self {
            metadata: Summary::of(&collect(&|r| r.metadata_ms)),
            first_byte: Summary::of(&collect(&|r| r.first_byte_ms)),
            startup: Summary::of(&collect(&|r| r.startup_ms)),
            seek: Summary::of(&seeks),
            runs,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summary_uses_nearest_rank() {
        let s = Summary::of(&[5.0, 1.0, 3.0, 2.0, 4.0]).unwrap();
        assert_eq!(s.samples, 5);
        assert!((s.median_ms - 3.0).abs() < f64::EPSILON);
        assert!((s.p95_ms - 5.0).abs() < f64::EPSILON);
        assert!((s.max_ms - 5.0).abs() < f64::EPSILON);
        assert!(Summary::of(&[]).is_none());
    }
}
