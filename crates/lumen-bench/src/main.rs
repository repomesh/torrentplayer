// SPDX-License-Identifier: MIT OR Apache-2.0

//! `lumen-bench`: measure streaming start-up and seek latency.
//!
//! ```text
//! lumen-bench local   [--size-mib 256] [--piece-kib 256] [--upload-mibps 8] [--rtt-ms 40]
//!                     [--seeders 1 | --peers SPEC] [--tracker-interval SECS] [--runs 5] [OPTIONS]
//! lumen-bench torrent <magnet | https URL | path.torrent>        [--runs 3] [OPTIONS]
//!
//! OPTIONS:
//!   --file N             file index to play (default: largest video)
//!   --seeks 0.1,0.5,...  seek positions as fractions of the file
//!   --duration SECS      media duration, so the bitrate can be used for read-ahead
//!   --startup-mib N      MiB a player needs at the head and at the tail to start (default 1)
//!   --seek-mib N         MiB a player needs at a seek target to resume (default 1)
//!   --json PATH          write the full report as JSON
//!   --max-startup-ms MS  exit with an error if the median start-up exceeds MS
//!   --max-seek-ms MS     exit with an error if the median seek exceeds MS
//!   --anchors N          anchor budget for prioritization; 0 = librqbit's built-in only
//!   --no-dht             public swarms: use trackers only
//!   --no-web-seeds       swarm engine: ignore the torrent's web seeds
//!   --download-mibps N   cap the download rate (MiB/s)
//!   --peer-limit N       maximum connected peers
//!   --max-connecting N   swarm engine: maximum simultaneous connection attempts
//!   --warm SECS          wait between adding the torrent and pressing play
//!   --queue-ms N         swarm engine: per-peer request queue, in milliseconds of download
//!   --relaxed-ms N       swarm engine: request queue once only background work remains
//!   --relax-horizon-secs N  swarm engine: the queue relaxes unless data is due within N s
//!   --dup-ms N           swarm engine: duplicate blocks due within N ms (0 disables)
//!   --dup-policy P       swarm engine: `time` (after duplicate_after) or `snubbed` (only from
//!                        peers that delivered nothing for 1 s)
//!   --engine NAME        rqbit (default) or swarm (TorrentStream's own engine)
//!   --watch SECS         seconds of playback after start-up and after each seek (default 3)
//!   --shared-cache       measure all seeks in one session; by default each seek gets its own
//!                        session from an empty download directory, so no seek lands on data
//!                        downloaded for an earlier one
//!
//! LOCAL SWARM SHAPES (`local` only):
//!   --upload-mibps N     the viewer's downlink, shared by all seeders
//!   --peers SPEC         one seeder per `;`-separated entry, each a `,`-separated list of
//!                        `up=MIBPS` (its own upload rate), `rtt=MS`, `have=K/N` (a partial
//!                        seed holding every N-th run of 8 pieces, starting at run K),
//!                        `choke=ON/OFF[/OFFSET]` (seconds unchoked, then choked, repeating),
//!                        `offline=FROM/UNTIL` (seconds since the run started; repeatable),
//!                        `late=S` (offline until S), `announce=S` (the tracker lists it after
//!                        S), and `x=N` (N such seeders). `-` is a plain seeder.
//!   --tracker-interval S run a local tracker (interval and min interval S) and find every
//!                        seeder through it instead of connecting to them directly
//!   --web-seed SPEC      add a web seed (BEP 19) serving the file over HTTP, shaped like a
//!                        `--peers` entry (`up=`, `rtt=`); `--peers none` leaves only it
//! ```

#![forbid(unsafe_code)]

use std::io::Write as _;
use std::num::NonZeroU32;
use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{Context as _, bail};
use lumen_bench::link::{ChokeCycle, Clock, Link, PeerProfile};
use lumen_bench::seeder::LocalSeeder;
use lumen_bench::tracker::LocalTracker;
use lumen_bench::webseed::{WebSeedServer, with_web_seed};
use lumen_bench::{BenchConfig, EngineKind, Report, Summary, run_once};
use lumen_torrent::TorrentSource;
use tracing_subscriber::EnvFilter;

const MIB: u64 = 1024 * 1024;

struct Args {
    mode: Mode,
    engine: EngineKind,
    runs: usize,
    json: Option<PathBuf>,
    max_startup_ms: Option<f64>,
    max_seek_ms: Option<f64>,
    bench: BenchConfig,
}

enum Mode {
    Local {
        size_mib: u64,
        piece_kib: u32,
        upload_mibps: u32,
        rtt_ms: u64,
        peers: Vec<PeerProfile>,
        tracker_interval: Option<std::time::Duration>,
        web_seed: Option<PeerProfile>,
    },
    Torrent(String),
}

fn parse_args() -> anyhow::Result<Args> {
    let mut args = std::env::args().skip(1);
    let mode = args
        .next()
        .context("expected `local` or `torrent`; see --help")?;
    let mut positional = Vec::new();
    let mut options = std::collections::HashMap::new();
    while let Some(arg) = args.next() {
        if arg == "--no-dht" || arg == "--no-web-seeds" || arg == "--shared-cache" {
            options.insert(arg[2..].to_owned(), String::new());
        } else if let Some(name) = arg.strip_prefix("--") {
            let value = args
                .next()
                .with_context(|| format!("--{name} needs a value"))?;
            options.insert(name.to_owned(), value);
        } else {
            positional.push(arg);
        }
    }
    let mut take = |name: &str| options.remove(name);
    let number = |v: Option<String>, default: u64| -> anyhow::Result<u64> {
        v.map_or(Ok(default), |v| v.parse().context("expected a number"))
    };

    let (mode, default_runs) = match mode.as_str() {
        "local" => (
            Mode::Local {
                size_mib: number(take("size-mib"), 256)?,
                piece_kib: u32::try_from(number(take("piece-kib"), 256)?)?,
                upload_mibps: u32::try_from(number(take("upload-mibps"), 8)?)?,
                rtt_ms: number(take("rtt-ms"), 40)?,
                peers: match take("peers") {
                    Some(spec) => parse_peers(&spec)?,
                    None => vec![
                        PeerProfile::default();
                        usize::try_from(number(take("seeders"), 1)?)?.max(1)
                    ],
                },
                tracker_interval: float(take("tracker-interval"))?
                    .map(|s| std::time::Duration::from_secs_f64(s.max(1.0))),
                web_seed: take("web-seed")
                    .map(|spec| parse_profile(&spec).map(|(p, _)| p))
                    .transpose()?,
            },
            5,
        ),
        "torrent" => (
            Mode::Torrent(
                positional
                    .pop()
                    .context("expected a torrent link or file")?,
            ),
            3,
        ),
        "--help" | "-h" | "help" => bail!("usage: see the crate documentation in src/main.rs"),
        other => bail!("unknown mode {other:?}; expected `local` or `torrent`"),
    };
    let bench = bench_options(&mut take)?;
    let engine = take("engine").map_or(Ok(EngineKind::Rqbit), |e| e.parse())?;
    let args = Args {
        mode,
        engine,
        runs: usize::try_from(number(take("runs"), default_runs)?)?.max(1),
        json: take("json").map(PathBuf::from),
        max_startup_ms: float(take("max-startup-ms"))?,
        max_seek_ms: float(take("max-seek-ms"))?,
        bench,
    };
    if let Some(unknown) = options.keys().next() {
        bail!("unknown option --{unknown}");
    }
    Ok(args)
}

/// Parses an optional number.
fn float(v: Option<String>) -> anyhow::Result<Option<f64>> {
    v.map(|v| v.parse().context("expected a number"))
        .transpose()
}

fn secs(v: &str) -> anyhow::Result<std::time::Duration> {
    let s: f64 = v
        .trim()
        .parse()
        .with_context(|| format!("expected seconds, got {v:?}"))?;
    Ok(std::time::Duration::from_secs_f64(s.max(0.0)))
}

/// Parses `--peers` (see the crate documentation).
fn parse_peers(spec: &str) -> anyhow::Result<Vec<PeerProfile>> {
    if spec.trim() == "none" {
        return Ok(Vec::new());
    }
    let mut peers = Vec::new();
    for entry in spec.split(';').map(str::trim).filter(|e| !e.is_empty()) {
        let (p, copies) = parse_profile(entry)?;
        peers.extend(std::iter::repeat_n(p, copies));
    }
    anyhow::ensure!(!peers.is_empty(), "--peers lists no seeders");
    Ok(peers)
}

/// Parses one `--peers` entry (or `--web-seed`): the profile and how many such seeders.
fn parse_profile(entry: &str) -> anyhow::Result<(PeerProfile, usize)> {
    let mut p = PeerProfile::default();
    let mut copies = 1;
    for item in entry
        .split(',')
        .map(str::trim)
        .filter(|i| !i.is_empty() && *i != "-")
    {
        let (key, value) = item
            .split_once('=')
            .with_context(|| format!("expected key=value in --peers, got {item:?}"))?;
        let parts: Vec<&str> = value.split('/').collect();
        match (key, parts.as_slice()) {
            ("up", [mibps]) => {
                let mibps: f64 = mibps.parse().context("up= expects MiB/s")?;
                p.up_bps = Some(mib_to_bytes(mibps).max(1));
            }
            ("rtt", [ms]) => p.rtt = Some(millis(ms.parse().context("rtt= expects ms")?)),
            ("have", [k, n]) => {
                let (k, n): (u32, u32) = (k.parse()?, n.parse()?);
                anyhow::ensure!(n > 0 && k < n, "have=K/N needs K < N");
                p.have = Some((k, n));
            }
            ("choke", [on, off, rest @ ..]) => {
                p.choke = Some(ChokeCycle {
                    on: secs(on)?,
                    off: secs(off)?,
                    offset: rest
                        .first()
                        .map_or(Ok(std::time::Duration::ZERO), |o| secs(o))?,
                });
            }
            ("offline", [from, until]) => p.offline.push((secs(from)?, secs(until)?)),
            ("late", [s]) => p.offline.push((std::time::Duration::ZERO, secs(s)?)),
            ("announce", [s]) => p.announce_after = secs(s)?,
            ("x", [n]) => copies = n.parse::<usize>().context("x= expects a count")?.max(1),
            _ => bail!("unknown --peers item {item:?}"),
        }
    }
    Ok((p, copies))
}

/// Converts MiB (possibly fractional) to bytes.
fn mib_to_bytes(mib: f64) -> u64 {
    // Saturating conversion of a small, non-negative value.
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_precision_loss
    )]
    let bytes = (mib.max(0.0) * MIB as f64) as u64;
    bytes
}

fn millis(ms: f64) -> std::time::Duration {
    std::time::Duration::from_secs_f64(ms.max(0.0) / 1000.0)
}

/// Options that shape what each run measures.
fn bench_options(take: &mut dyn FnMut(&str) -> Option<String>) -> anyhow::Result<BenchConfig> {
    let mut bench = BenchConfig::default();
    if let Some(file) = take("file") {
        bench.file = Some(file.parse().context("--file expects an index")?);
    }
    if let Some(seeks) = take("seeks") {
        bench.seeks = seeks
            .split(',')
            .map(|s| s.trim().parse::<f64>())
            .collect::<Result<_, _>>()
            .context("--seeks expects comma-separated fractions")?;
    }
    bench.duration_secs = float(take("duration"))?;
    if let Some(mib) = float(take("startup-mib"))? {
        bench.startup_bytes = mib_to_bytes(mib);
    }
    if let Some(mib) = float(take("seek-mib"))? {
        bench.seek_bytes = mib_to_bytes(mib);
    }
    if let Some(anchors) = take("anchors") {
        bench.prioritization.max_anchors = anchors.parse().context("--anchors expects a count")?;
    }
    bench.dht = take("no-dht").is_none();
    bench.web_seeds = take("no-web-seeds").is_none();
    bench.cold_seeks = take("shared-cache").is_none();
    if let Some(limit) = take("download-mibps") {
        let mibps: u32 = limit.parse().context("--download-mibps expects a number")?;
        bench.download_limit_bps = NonZeroU32::new(mibps.saturating_mul(1024 * 1024));
    }
    if let Some(limit) = take("peer-limit") {
        bench.peer_limit = Some(limit.parse().context("--peer-limit expects a count")?);
    }
    if let Some(connecting) = take("max-connecting") {
        bench.max_connecting = Some(
            connecting
                .parse()
                .context("--max-connecting expects a count")?,
        );
    }
    if let Some(warm) = float(take("warm"))? {
        bench.warm = std::time::Duration::from_secs_f64(warm.max(0.0));
    }
    if let Some(watch) = float(take("watch"))? {
        bench.watch = std::time::Duration::from_secs_f64(watch.max(0.0));
    }
    scheduler_options(take, &mut bench.scheduler)?;
    Ok(bench)
}

/// Tuning options for the swarm engine's scheduler.
fn scheduler_options(
    take: &mut dyn FnMut(&str) -> Option<String>,
    s: &mut lumen_swarm::SchedulerConfig,
) -> anyhow::Result<()> {
    if let Some(ms) = float(take("queue-ms"))? {
        s.queue_time = millis(ms.max(1.0));
    }
    if let Some(ms) = float(take("relaxed-ms"))? {
        s.relaxed_queue_time = millis(ms.max(1.0));
    }
    if let Some(secs) = float(take("relax-horizon-secs"))? {
        s.relax_horizon = std::time::Duration::from_secs_f64(secs.max(0.0));
    }
    if let Some(policy) = take("dup-policy") {
        s.duplicate_only_snubbed = match policy.as_str() {
            "snubbed" => true,
            "time" => false,
            other => bail!("unknown --dup-policy {other:?}; expected time or snubbed"),
        };
    }
    if let Some(ms) = float(take("dup-ms"))? {
        s.endgame_horizon = millis(ms);
    }
    if let Some(mib) = float(take("critical-mib"))? {
        s.critical_bytes = mib_to_bytes(mib);
    }
    if let Some(secs) = float(take("critical-secs"))? {
        s.critical_secs = std::time::Duration::from_secs_f64(secs.max(0.0));
    }
    Ok(())
}

fn source_from(input: &str) -> anyhow::Result<TorrentSource> {
    if input.contains(':') && !std::path::Path::new(input).exists() {
        Ok(TorrentSource::parse_link(input)?)
    } else {
        let bytes = std::fs::read(input).with_context(|| format!("reading {input}"))?;
        Ok(TorrentSource::from_file_bytes(bytes)?)
    }
}

async fn run(args: &Args) -> anyhow::Result<Report> {
    let mut bench = args.bench.clone();
    let mut runs = Vec::new();
    match &args.mode {
        Mode::Local {
            size_mib,
            piece_kib,
            upload_mibps,
            rtt_ms,
            peers,
            tracker_interval,
            web_seed,
        } => {
            anyhow::ensure!(
                !peers.is_empty() || web_seed.is_some(),
                "no seeders and no web seed"
            );
            let clock = Clock::default();
            let tracker = match tracker_interval {
                Some(interval) => Some(LocalTracker::start(*interval, clock.clone()).await?),
                None => None,
            };
            // The seeder is unthrottled; the link emulator provides bandwidth and latency.
            let dir = tempfile::tempdir()?;
            let seeder = LocalSeeder::start(
                dir.path(),
                usize::try_from(size_mib * MIB)?,
                piece_kib * 1024,
                None,
                peers.len().max(1),
                tracker.iter().map(|t| t.url.clone()).collect(),
            )
            .await?;
            let mut profiles: Vec<(std::net::SocketAddr, PeerProfile)> = seeder
                .addrs
                .iter()
                .copied()
                .zip(peers.iter().cloned())
                .collect();
            // The web seed shares the viewer's downlink with the seeders.
            let server = match web_seed {
                Some(profile) => {
                    let server = WebSeedServer::start(seeder.file.clone()).await?;
                    profiles.push((server.addr, profile.clone()));
                    Some(server)
                }
                None => None,
            };
            let mut links = Link::start_profiles(
                &profiles,
                u64::from(*upload_mibps) * MIB,
                std::time::Duration::from_millis(*rtt_ms),
                &clock,
            )
            .await?;
            let mut torrent = seeder.torrent.to_vec();
            let web_link = server.as_ref().and_then(|_| links.pop());
            if let Some(link) = &web_link {
                torrent = with_web_seed(&torrent, &format!("http://{}/lecture.mp4", link.addr));
                bench.local_web_seeds = true;
            }
            bench.public_swarm = false;
            if let Some(tracker) = &tracker {
                // Every seeder is found through the tracker, some only after a while.
                tracker.set_peers(
                    links
                        .iter()
                        .zip(peers)
                        .map(|(l, p)| (l.addr, p.announce_after))
                        .collect(),
                );
                bench.local_tracker = true;
            } else {
                bench.peers = links.iter().map(|l| l.addr).collect();
            }
            for i in 0..args.runs {
                // Every session restarts the seeders' schedules (choke cycles, outages).
                let session = || {
                    clock.reset();
                    Ok(TorrentSource::from_file_bytes(torrent.clone())?)
                };
                runs.push(
                    run_once(args.engine, session, &bench)
                        .await
                        .with_context(|| format!("run {}", i + 1))?,
                );
                progress(i + 1, args.runs);
            }
        }
        Mode::Torrent(input) => {
            for i in 0..args.runs {
                runs.push(
                    run_once(args.engine, || source_from(input), &bench)
                        .await
                        .with_context(|| format!("run {}", i + 1))?,
                );
                progress(i + 1, args.runs);
            }
        }
    }
    Ok(Report::new(runs))
}

fn progress(done: usize, total: usize) {
    tracing::info!("run {done}/{total} finished");
}

/// Bytes to MiB, for display only.
#[allow(clippy::cast_precision_loss)]
fn mib(bytes: u64) -> f64 {
    bytes as f64 / MIB as f64
}

fn print_report(report: &Report) -> std::io::Result<()> {
    let mut out = std::io::stdout().lock();
    if let Some(first) = report.runs.first() {
        writeln!(
            out,
            "[{}] {} / {} ({:.1} MiB)",
            first.engine,
            first.torrent,
            first.file,
            mib(first.file_bytes)
        )?;
    }
    writeln!(
        out,
        "{:<22} {:>7} {:>10} {:>10} {:>10}",
        "metric", "samples", "median ms", "p95 ms", "max ms"
    )?;
    for (name, summary) in [
        ("metadata", report.metadata),
        ("first byte after play", report.first_byte),
        ("start-up ready", report.startup),
        ("seek ready", report.seek),
    ] {
        if let Some(Summary {
            samples,
            median_ms,
            p95_ms,
            max_ms,
        }) = summary
        {
            writeln!(
                out,
                "{name:<22} {samples:>7} {median_ms:>10.0} {p95_ms:>10.0} {max_ms:>10.0}"
            )?;
        }
    }
    for (i, run) in report.runs.iter().enumerate() {
        let seeks: Vec<String> = run
            .seeks
            .iter()
            .map(|s| format!("{:.0}%={:.0}ms", s.fraction * 100.0, s.ready_ms))
            .collect();
        writeln!(
            out,
            "run {}: {} peers at play, start-up {:.0} ms, seeks [{}], peers {} live / {} seen, {:.2} MiB/s at the end, {:.2} MiB/s average, {} stalls ({:.1} s of {:.0} s watched)",
            i + 1,
            run.peers_at_play,
            run.startup_ms,
            seeks.join(", "),
            run.peers_live,
            run.peers_seen,
            mib(run.download_bps),
            mib(run.avg_download_bps),
            run.stalls,
            run.stall_ms / 1000.0,
            run.watch_ms / 1000.0
        )?;
    }
    Ok(())
}

fn main() -> ExitCode {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .try_init();

    let result = (|| -> anyhow::Result<bool> {
        let args = parse_args()?;
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()?;
        let report = runtime.block_on(run(&args))?;
        print_report(&report)?;
        if let Some(path) = &args.json {
            std::fs::write(path, serde_json::to_vec_pretty(&report)?)?;
        }
        let over = |summary: Option<Summary>, max: Option<f64>| matches!((summary, max), (Some(s), Some(max)) if s.median_ms > max);
        let failed =
            over(report.startup, args.max_startup_ms) || over(report.seek, args.max_seek_ms);
        if failed {
            tracing::error!("median latency exceeded the configured budget");
        }
        Ok(!failed)
    })();

    match result {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::FAILURE,
        Err(error) => {
            tracing::error!("{error:#}");
            ExitCode::from(2)
        }
    }
}
