// SPDX-License-Identifier: MIT OR Apache-2.0

//! Download-limit accuracy: how closely an engine holds a download limit, and how it reacts
//! when the limit is lowered mid-transfer, as when the user changes the setting while a video
//! downloads.
//!
//! A run adds the torrent with the first limit, selects the file (both engines then download
//! it in the background), and samples the engine every 100 ms: verified bytes (what was
//! actually delivered) and the engine's own `download_bps` (what the app shows). After
//! [`LimitPlan::change_after`] the limit changes to [`LimitPlan::then_bps`]. The report gives,
//! for the steady second half of each phase, the delivered and the reported rate; the highest
//! and the lowest delivered rate over any one-second window after the change (a burst, and a
//! dip while debt is repaid); the bytes delivered in the two seconds after the change beyond
//! the new limit; and how long until the delivered rate over every later two-second window
//! stays within 5% of the new limit, and of the rate it settles at (a swarm with churn may
//! settle below the limit). The reported rate is an estimate already smoothed by the engine,
//! so its settling time uses each sample.
//!
//! Verified bytes arrive a piece at a time, so a window can be off by a piece: use small
//! pieces (the `limit` command defaults to 16 KiB) to keep that under 2%. The engines' token
//! buckets allow bursts of a tenth of a second (swarm) or more, so one-second windows vary by
//! several percent even at a steady limit; hence the two-second windows for settling.

use std::num::NonZeroU32;
use std::time::{Duration, Instant};

use anyhow::Context as _;
use serde::Serialize;
use torrentplayer_torrent::TorrentSource;

use crate::engines::BenchEngine;
use crate::{BenchConfig, EngineKind, ms, pick_file};

/// How far a window may be from its target and still count as settled.
pub const SETTLE_TOLERANCE: f64 = 0.05;
/// Sampling period.
const SAMPLE_EVERY: Duration = Duration::from_millis(100);
/// Window for the burst and the dip.
const WINDOW: Duration = Duration::from_secs(1);
/// Window for settling.
const SETTLE_WINDOW: Duration = Duration::from_secs(2);
/// Window after the change for [`LimitRun::excess_bytes`].
const EXCESS_WINDOW: Duration = Duration::from_secs(2);

/// The limit schedule of one run.
#[derive(Debug, Clone, Copy)]
pub struct LimitPlan {
    /// The limit at the start, bytes per second.
    pub first_bps: NonZeroU32,
    /// The limit after the change, bytes per second.
    pub then_bps: NonZeroU32,
    /// When the limit changes, after the file is selected.
    pub change_after: Duration,
    /// How long to keep measuring after the change.
    pub measure_after: Duration,
}

/// One sample of the engine.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct LimitSample {
    /// Since the file was selected, in milliseconds.
    pub t_ms: f64,
    /// Verified bytes so far.
    pub verified_bytes: u64,
    /// The engine's own download rate, bytes per second.
    pub reported_bps: u64,
    /// Connected peers.
    pub peers_live: u32,
}

/// Rates over one steady phase.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct Phase {
    /// The limit in force, bytes per second.
    pub limit_bps: u64,
    /// Verified bytes per second.
    pub delivered_bps: f64,
    /// Mean of the engine's reported rate.
    pub reported_bps: f64,
}

/// One run.
#[derive(Debug, Clone, Serialize)]
pub struct LimitRun {
    /// Engine measured.
    pub engine: String,
    /// Before the change: its second half.
    pub before: Phase,
    /// After the change: its second half.
    pub after: Phase,
    /// The highest verified rate over any one-second window starting at or after the change.
    pub max_burst_bps: f64,
    /// The lowest verified rate over any one-second window starting at or after the change.
    pub min_after_bps: f64,
    /// Bytes verified in the two seconds after the change beyond what the new limit allows
    /// (negative if fewer).
    pub excess_bytes: f64,
    /// From the change until every later two-second window of verified bytes is within 5% of
    /// the new limit; `None` if that never happened.
    pub settle_ms: Option<f64>,
    /// The same, within 5% of [`LimitRun::after`]'s delivered rate (where the rate settled).
    pub settle_steady_ms: Option<f64>,
    /// The same for the engine's reported rate (each sample, not a window).
    pub reported_settle_ms: Option<f64>,
    /// Fewest and most connected peers after the first second.
    pub peers_live_range: (u32, u32),
    /// Peers learned about during the run.
    pub peers_seen: u32,
    /// Every sample, for plotting.
    pub samples: Vec<LimitSample>,
}

/// Runs one limit measurement from an empty download directory.
///
/// # Errors
///
/// Returns an error if the torrent cannot be added or the file is too small to keep the
/// download going for the whole run.
pub async fn run_limit(
    kind: EngineKind,
    source: TorrentSource,
    config: &BenchConfig,
    plan: LimitPlan,
) -> anyhow::Result<LimitRun> {
    let dir = tempfile::tempdir().context("creating a download directory")?;
    let config = BenchConfig {
        download_limit_bps: Some(plan.first_bps),
        ..config.clone()
    };
    let mut engine = BenchEngine::start(kind, dir.path().to_path_buf(), &config).await?;
    let (_, files) = engine.add(source, config.peers.clone()).await?;
    let entry = pick_file(&files, config.file)?;
    let byte_rate = config.prioritization.fallback_byte_rate.max(1);
    let playhead = engine.play(&entry, &config, byte_rate).await?;
    playhead.update(0);

    let start = Instant::now();
    let change_at = start + plan.change_after;
    let end = change_at + plan.measure_after;
    let mut changed = false;
    let mut samples = Vec::new();
    let mut next = start;
    while next <= end {
        tokio::time::sleep_until(next.into()).await;
        let now = Instant::now();
        if !changed && now >= change_at {
            engine.set_download_limit(Some(plan.then_bps));
            changed = true;
        }
        let stats = engine.stats()?;
        samples.push(LimitSample {
            t_ms: ms(now - start),
            verified_bytes: stats.downloaded_bytes,
            reported_bps: stats.download_bps,
            peers_live: stats.peers_live,
        });
        next += SAMPLE_EVERY;
    }
    let stats = engine.stats()?;
    drop(playhead);
    engine.finish().await?;
    anyhow::ensure!(
        stats.downloaded_bytes < entry.len,
        "the whole file downloaded before the run ended; use a larger --size-mib"
    );

    let mut run = analyze(&samples, plan);
    run.engine = kind.to_string();
    run.peers_seen = stats.peers_seen;
    run.samples = samples;
    Ok(run)
}

/// Computes a run's figures from its samples (see [`LimitRun`]).
#[must_use]
pub fn analyze(samples: &[LimitSample], plan: LimitPlan) -> LimitRun {
    let change = ms(plan.change_after);
    let end = change + ms(plan.measure_after);
    let then = f64::from(plan.then_bps.get());
    let phase = |from: f64, to: f64, limit: NonZeroU32| Phase {
        limit_bps: u64::from(limit.get()),
        delivered_bps: delivered(samples, from, to).unwrap_or(0.0),
        reported_bps: mean(
            samples
                .iter()
                .filter(|s| s.t_ms >= from && s.t_ms <= to)
                .map(|s| reported(s.reported_bps)),
        ),
    };
    // Delivered rate over windows starting at each sample after the change.
    let windows = |len: Duration| -> Vec<(f64, f64)> {
        samples
            .iter()
            .filter(|s| s.t_ms >= change)
            .filter_map(|s| {
                delivered(samples, s.t_ms, s.t_ms + ms(len)).map(|bps| (s.t_ms - change, bps))
            })
            .collect()
    };
    let (short, long) = (windows(WINDOW), windows(SETTLE_WINDOW));
    let near = |target: f64| move |bps: f64| (bps / target - 1.0).abs() <= SETTLE_TOLERANCE;
    let after = phase(change + (end - change) / 2.0, end, plan.then_bps);
    let reported_after: Vec<(f64, f64)> = samples
        .iter()
        .filter(|s| s.t_ms >= change)
        .map(|s| (s.t_ms - change, reported(s.reported_bps)))
        .collect();
    let excess = bytes_at(samples, change + ms(EXCESS_WINDOW))
        .zip(bytes_at(samples, change))
        .map_or(0.0, |(b, a)| b - a - then * EXCESS_WINDOW.as_secs_f64());
    let live = samples
        .iter()
        .filter(|s| s.t_ms >= 1000.0)
        .map(|s| s.peers_live);
    LimitRun {
        engine: String::new(),
        before: phase(change / 2.0, change, plan.first_bps),
        max_burst_bps: short.iter().map(|w| w.1).fold(0.0, f64::max),
        min_after_bps: short.iter().map(|w| w.1).reduce(f64::min).unwrap_or(0.0),
        excess_bytes: excess,
        settle_ms: settled(&long, near(then)),
        settle_steady_ms: settled(&long, near(after.delivered_bps)),
        reported_settle_ms: settled(&reported_after, near(then)),
        after,
        peers_live_range: (live.clone().min().unwrap_or(0), live.max().unwrap_or(0)),
        peers_seen: 0,
        samples: Vec::new(),
    }
}

/// The earliest time from which every later value is within tolerance.
fn settled(series: &[(f64, f64)], within: impl Fn(f64) -> bool) -> Option<f64> {
    let last_out = series.iter().rposition(|&(_, v)| !within(v));
    match last_out {
        None => series.first().map(|s| s.0),
        Some(i) => series.get(i + 1).map(|s| s.0),
    }
}

/// Verified bytes at time `t`, interpolated between samples; `None` outside the samples.
fn bytes_at(samples: &[LimitSample], t: f64) -> Option<f64> {
    let after = samples.iter().position(|s| s.t_ms >= t)?;
    #[allow(clippy::cast_precision_loss)]
    let b = samples[after].verified_bytes as f64;
    if after == 0 || samples[after].t_ms <= t {
        return Some(b);
    }
    let prev = &samples[after - 1];
    #[allow(clippy::cast_precision_loss)]
    let a = prev.verified_bytes as f64;
    let f = (t - prev.t_ms) / (samples[after].t_ms - prev.t_ms);
    Some(a + (b - a) * f)
}

/// Verified bytes per second between `from` and `to` (milliseconds).
fn delivered(samples: &[LimitSample], from: f64, to: f64) -> Option<f64> {
    if to <= from {
        return None;
    }
    let (a, b) = (bytes_at(samples, from)?, bytes_at(samples, to)?);
    Some((b - a) / ((to - from) / 1000.0))
}

#[allow(clippy::cast_precision_loss)]
fn reported(bps: u64) -> f64 {
    bps as f64
}

fn mean(values: impl Iterator<Item = f64>) -> f64 {
    let (sum, n) = values.fold((0.0, 0u32), |(s, n), v| (s + v, n + 1));
    if n == 0 { 0.0 } else { sum / f64::from(n) }
}

/// All runs plus medians of their figures.
#[derive(Debug, Clone, Serialize)]
pub struct LimitReport {
    /// Individual runs.
    pub runs: Vec<LimitRun>,
    /// Median delivered rate before the change, bytes per second.
    pub before_delivered_bps: f64,
    /// Median reported rate before the change.
    pub before_reported_bps: f64,
    /// Median delivered rate after the change.
    pub after_delivered_bps: f64,
    /// Median reported rate after the change.
    pub after_reported_bps: f64,
    /// Median of the highest one-second rate after the change.
    pub max_burst_bps: f64,
    /// Median of the lowest one-second rate after the change.
    pub min_after_bps: f64,
    /// Median excess bytes in the two seconds after the change.
    pub excess_bytes: f64,
    /// Median settling time (to the new limit) over the runs that settled.
    pub settle_ms: Option<f64>,
    /// Runs that settled.
    pub settled_runs: usize,
    /// Median settling time to the steady rate after the change.
    pub settle_steady_ms: Option<f64>,
    /// Runs that settled to their steady rate.
    pub steady_runs: usize,
    /// Median settling time of the reported rate.
    pub reported_settle_ms: Option<f64>,
    /// Runs whose reported rate settled.
    pub reported_settled_runs: usize,
}

impl LimitReport {
    /// Builds the medians from runs.
    #[must_use]
    pub fn new(runs: Vec<LimitRun>) -> Self {
        let med = |f: &dyn Fn(&LimitRun) -> f64| median(runs.iter().map(f).collect());
        let settle: Vec<f64> = runs.iter().filter_map(|r| r.settle_ms).collect();
        let steady: Vec<f64> = runs.iter().filter_map(|r| r.settle_steady_ms).collect();
        let reported_settle: Vec<f64> = runs.iter().filter_map(|r| r.reported_settle_ms).collect();
        Self {
            before_delivered_bps: med(&|r| r.before.delivered_bps).unwrap_or(0.0),
            before_reported_bps: med(&|r| r.before.reported_bps).unwrap_or(0.0),
            after_delivered_bps: med(&|r| r.after.delivered_bps).unwrap_or(0.0),
            after_reported_bps: med(&|r| r.after.reported_bps).unwrap_or(0.0),
            max_burst_bps: med(&|r| r.max_burst_bps).unwrap_or(0.0),
            min_after_bps: med(&|r| r.min_after_bps).unwrap_or(0.0),
            excess_bytes: med(&|r| r.excess_bytes).unwrap_or(0.0),
            settled_runs: settle.len(),
            settle_ms: median(settle),
            steady_runs: steady.len(),
            settle_steady_ms: median(steady),
            reported_settled_runs: reported_settle.len(),
            reported_settle_ms: median(reported_settle),
            runs,
        }
    }
}

/// The median (the lower middle for an even count).
fn median(mut values: Vec<f64>) -> Option<f64> {
    values.sort_by(f64::total_cmp);
    let mid = values.len().checked_sub(1)? / 2;
    values.get(mid).copied()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plan() -> LimitPlan {
        LimitPlan {
            first_bps: NonZeroU32::new(2_000_000).unwrap(),
            then_bps: NonZeroU32::new(1_000_000).unwrap(),
            change_after: Duration::from_secs(4),
            measure_after: Duration::from_secs(6),
        }
    }

    /// 2 MB/s until 4 s, a 1 MB burst at the change, then 1 MB/s; reported 10% high.
    fn samples() -> Vec<LimitSample> {
        let mut out = Vec::new();
        let mut bytes = 0u64;
        for i in 0..=100u32 {
            let t = f64::from(i) * 100.0;
            if i > 0 {
                bytes += if t <= 4000.0 { 200_000 } else { 100_000 };
                if i == 41 {
                    bytes += 1_000_000;
                }
            }
            let rate = if t <= 4000.0 { 2_200_000 } else { 1_100_000 };
            out.push(LimitSample {
                t_ms: t,
                verified_bytes: bytes,
                reported_bps: rate,
                peers_live: 4,
            });
        }
        out
    }

    #[test]
    fn analysis_finds_rates_the_burst_and_settling() {
        let run = analyze(&samples(), plan());
        assert!((run.before.delivered_bps - 2_000_000.0).abs() < 1.0);
        assert!((run.before.reported_bps - 2_200_000.0).abs() < 1.0);
        assert!((run.after.delivered_bps - 1_000_000.0).abs() < 1.0);
        // The burst lands in the windows starting before 4.1 s.
        assert!((run.max_burst_bps - 2_000_000.0).abs() < 1.0, "{run:?}");
        assert!((run.excess_bytes - 1_000_000.0).abs() < 1.0);
        assert!((run.min_after_bps - 1_000_000.0).abs() < 1.0);
        // Windows starting at 4.1 s and later hold the limit, which is also the steady rate.
        let settle = run.settle_ms.unwrap();
        assert!((settle - 100.0).abs() < 1.0, "{settle}");
        assert_eq!(run.settle_steady_ms, run.settle_ms);
        // The reported rate stays 10% high, so it never settles within 5%.
        assert_eq!(run.reported_settle_ms, None);
        assert_eq!(run.peers_live_range, (4, 4));

        let report = LimitReport::new(vec![run.clone(), run]);
        assert_eq!(report.settled_runs, 2);
        assert!((report.after_delivered_bps - 1_000_000.0).abs() < 1.0);
    }

    #[test]
    fn median_takes_the_lower_middle() {
        assert_eq!(median(vec![3.0, 1.0, 2.0]), Some(2.0));
        assert_eq!(median(vec![4.0, 1.0, 3.0, 2.0]), Some(2.0));
        assert_eq!(median(Vec::new()), None);
    }
}
