// SPDX-License-Identifier: MIT OR Apache-2.0

//! `torrentplayer-bench`: measure streaming start-up and seek latency.
//!
//! ```text
//! torrentplayer-bench local   [--size-mib 256 | --media FILE] [--piece-kib 256] [--upload-mibps 8]
//!                     [--rtt-ms 40] [--seeders 1 | --peers SPEC] [--tracker-interval SECS]
//!                     [--inbound] [--runs 5] [OPTIONS]
//! torrentplayer-bench torrent <magnet | https URL | path.torrent>        [--runs 3] [OPTIONS]
//! torrentplayer-bench limit   [--limit-mibps 2] [--then-mibps 1] [--change-after 10]
//!                     [--measure-after 20] [--churn 8] [local swarm shape] [--runs 3] [OPTIONS]
//! torrentplayer-bench seed    <media file> <output .torrent>
//! torrentplayer-bench suite   <list file> [public | --public] [--quick] [--out DIR]
//!                     [--engines rqbit,swarm] [--links 2,8,25] [--seeders 1,4] [--rtt-ms 40]
//!                     [--runs 3] [--piece-kib 256] [--tracker-interval SECS] [OPTIONS]
//!
//! OPTIONS:
//!   --file N             file index to play (default: largest video)
//!   --seeks 0.1,0.5,...  seek positions as fractions of the file (of the duration with --media)
//!   --duration SECS      media duration, so the bitrate can be used for read-ahead (with
//!                        --media: the container's duration unless given)
//!   --startup-mib N      MiB a player needs at the head and at the tail to start (default 1;
//!                        with --media, given only to override the container-aware reads)
//!   --seek-mib N         MiB a player needs at a seek target to resume (default 1; with
//!                        --media, given only to override the container-aware reads)
//!   --json PATH          write the full report as JSON
//!   --max-startup-ms MS  exit with an error if the median start-up exceeds MS
//!   --max-seek-ms MS     exit with an error if the median seek exceeds MS
//!   --anchors N          anchor budget for prioritization; 0 = librqbit's built-in only
//!   --no-dht             public swarms: use trackers only
//!   --no-trackers        public swarms: use the DHT only (as for a trackerless magnet link)
//!   --dht-state DIR      keep the swarm engine's DHT routing tables in DIR between sessions,
//!                        as the app does in its data folder (`swarm.json`); the first session
//!                        without a saved state starts cold. librqbit keeps none, as in the app
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
//!   --engine NAME        rqbit (default) or swarm (TorrentPlayer's own engine)
//!   --watch SECS         seconds of playback after start-up and after each seek (default 3)
//!   --shared-cache       measure all seeks in one session; by default each seek gets its own
//!                        session from an empty download directory, so no seek lands on data
//!                        downloaded for an earlier one
//!
//! LOCAL SWARM SHAPES (`local` and `limit`):
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
//!   --inbound            incoming peers only: the viewer accepts incoming connections (the
//!                        swarm engine through the network manager's listener, as with the
//!                        app's `network.incomingPeers` setting; librqbit on its own
//!                        listener; both on every interface, so Windows may show a firewall
//!                        prompt, which loopback traffic does not need) and announces its port
//!                        to the local tracker (interval 30 s unless --tracker-interval is
//!                        given), which lists no seeders; each
//!                        seeder connects to the viewer once the tracker has its address (and
//!                        its `announce=` delay has passed), through the same link shaping.
//!                        Compare with `--tracker-interval 30` alone (outgoing only)
//!   --web-seed SPEC      add a web seed (BEP 19) serving the file over HTTP, shaped like a
//!                        `--peers` entry (`up=`, `rtt=`); `--peers none` leaves only it
//!
//! LIMIT (`limit` only; defaults: 128 MiB in 16 KiB pieces, 8 seeders, 16 MiB/s downlink):
//!   --limit-mibps N      the download limit at the start (MiB/s, fractional allowed)
//!   --then-mibps N       the limit after the change
//!   --change-after S     seconds after the file is selected when the limit changes
//!   --measure-after S    seconds to keep measuring after the change
//!   --churn S            peer churn: each seeder goes offline for a quarter of every S
//!                        seconds, staggered (0 disables)
//! ```
//!
//! `limit` reports the delivered rate (verified bytes per second) and the engine's reported
//! `download_bps` before and after the change, the highest one-second rate after it, and the
//! time until the rate stays within 5% of the new limit (see `torrentplayer_bench::limit`).
//!
//! `seed` seeds a media file on `127.0.0.1` until stopped, writes its `.torrent`, and logs the
//! seeder's address, for playing real media in a development build of the app (see
//! `TORRENTPLAYER_DEV_AUTOPLAY` in docs/building.md).
//!
//! `local --media FILE` seeds a real media file instead of generated data, through the same
//! link shaping, and reads what a player needs from its container (MP4, MOV, Matroska, `WebM`;
//! see `torrentplayer_bench::needs`): the header wherever it is (`moov` at the end costs a read
//! of the tail before the first frame) and, for a seek, the media from the keyframe before the
//! target. Seek fractions are of the duration, which comes from the container. If the container
//! cannot be parsed, the flat `--startup-mib`/`--seek-mib` reads are used and the report says
//! so; giving either flag uses the flat reads for that part.
//!
//! `suite LIST` runs a matrix for every entry of a list file (see `torrentplayer_bench::suite`
//! for the format) and writes `report.md` and `results.json` to `--out` (default
//! `bench-results/<UTC timestamp>/`). Local media: both engines × `--links` (MiB/s, default
//! 2,8,25) × `--seeders` (default 1,4) at `--rtt-ms` (default 40), plus 8 MiB/s with 4 seeders at
//! 120 ms, 3 runs each. Torrents and magnet links: their public swarm, both engines, a 10 MiB/s
//! download limit (`--download-mibps`), 3 runs; these download from and upload to strangers,
//! so they run only with `public` or `--public`. `--quick`: 1 run, and for local media only
//! 2 MiB/s with 1 seeder and 25 MiB/s with 4 seeders. `--engines` picks engines; other options
//! apply to every run.

#![forbid(unsafe_code)]

use std::io::Write as _;
use std::num::NonZeroU32;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context as _, bail};
use torrentplayer_bench::limit::{LimitPlan, LimitReport, run_limit};
use torrentplayer_bench::link::{ChokeCycle, Clock, Dialers, Link, PeerProfile};
use torrentplayer_bench::media::MediaInfo;
use torrentplayer_bench::seeder::LocalSeeder;
use torrentplayer_bench::suite::{
    Entry, EntryResult, Scenario, ScenarioResult, Source, SuiteReport, local_scenarios, parse_list,
};
use torrentplayer_bench::tracker::LocalTracker;
use torrentplayer_bench::webseed::{WebSeedServer, with_web_seed};
use torrentplayer_bench::{BenchConfig, EngineKind, Report, Summary, run_once};
use torrentplayer_torrent::TorrentSource;
use tracing_subscriber::EnvFilter;

const MIB: u64 = 1024 * 1024;
/// The local tracker's interval with `--inbound` when none is given.
const INBOUND_TRACKER_INTERVAL: Duration = Duration::from_secs(30);

struct Args {
    mode: Mode,
    engine: EngineKind,
    runs: usize,
    json: Option<PathBuf>,
    max_startup_ms: Option<f64>,
    max_seek_ms: Option<f64>,
    bench: BenchConfig,
}

/// A local swarm: a seeder, the link between it and the viewer, and how the viewer finds it.
struct Shape {
    size_mib: u64,
    piece_kib: u32,
    upload_mibps: u32,
    rtt_ms: u64,
    peers: Vec<PeerProfile>,
    tracker_interval: Option<Duration>,
    web_seed: Option<PeerProfile>,
    inbound: bool,
    /// Seed this media file instead of generated data.
    media: Option<PathBuf>,
}

enum Mode {
    Local(Shape),
    Limit(Shape, LimitPlan),
    Torrent(String),
}

/// Options that take no value.
const FLAGS: &[&str] = &[
    "--no-dht",
    "--no-trackers",
    "--no-web-seeds",
    "--shared-cache",
    "--inbound",
    "--quick",
    "--public",
];

type Options = std::collections::HashMap<String, String>;

/// Splits arguments into positional ones and `--name value` options ([`FLAGS`] take no value).
fn tokenize(mut args: impl Iterator<Item = String>) -> anyhow::Result<(Vec<String>, Options)> {
    let mut positional = Vec::new();
    let mut options = Options::new();
    while let Some(arg) = args.next() {
        if FLAGS.contains(&arg.as_str()) {
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
    Ok((positional, options))
}

fn parse_args() -> anyhow::Result<Args> {
    let mut args = std::env::args().skip(1);
    let mode = args
        .next()
        .context("expected `local`, `torrent`, `limit`, `seed`, or `suite`; see --help")?;
    let (mut positional, mut options) = tokenize(args)?;
    let mut take = |name: &str| options.remove(name);

    let (mode, default_runs) = match mode.as_str() {
        "local" => (Mode::Local(shape(&mut take, (256, 256, 8, 1))?), 5),
        "limit" => {
            let mut shape = shape(&mut take, (128, 16, 16, 8))?;
            let plan = LimitPlan {
                first_bps: limit_bps(take("limit-mibps"), 2.0)?,
                then_bps: limit_bps(take("then-mibps"), 1.0)?,
                change_after: secs(&take("change-after").unwrap_or_else(|| "10".into()))?,
                measure_after: secs(&take("measure-after").unwrap_or_else(|| "20".into()))?,
            };
            let churn = secs(&take("churn").unwrap_or_else(|| "8".into()))?;
            add_churn(
                &mut shape.peers,
                churn,
                plan.change_after + plan.measure_after,
            );
            (Mode::Limit(shape, plan), 3)
        }
        "torrent" => (
            Mode::Torrent(
                positional
                    .pop()
                    .context("expected a torrent link or file")?,
            ),
            3,
        ),
        "--help" | "-h" | "help" => bail!("usage: see the crate documentation in src/main.rs"),
        other => bail!("unknown mode {other:?}; expected `local`, `torrent`, or `limit`"),
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

/// Parses an optional whole number.
fn number(v: Option<String>, default: u64) -> anyhow::Result<u64> {
    v.map_or(Ok(default), |v| v.parse().context("expected a number"))
}

/// Parses the local swarm shape; `defaults` are the size (MiB), piece size (KiB), downlink
/// (MiB/s), and number of seeders.
fn shape(
    take: &mut dyn FnMut(&str) -> Option<String>,
    defaults: (u64, u64, u64, u64),
) -> anyhow::Result<Shape> {
    let (size, piece, upload, seeders) = defaults;
    Ok(Shape {
        size_mib: number(take("size-mib"), size)?,
        piece_kib: u32::try_from(number(take("piece-kib"), piece)?)?,
        upload_mibps: u32::try_from(number(take("upload-mibps"), upload)?)?,
        rtt_ms: number(take("rtt-ms"), 40)?,
        peers: match take("peers") {
            Some(spec) => parse_peers(&spec)?,
            None => vec![
                PeerProfile::default();
                usize::try_from(number(take("seeders"), seeders)?)?.max(1)
            ],
        },
        tracker_interval: float(take("tracker-interval"))?
            .map(|s| Duration::from_secs_f64(s.max(1.0))),
        web_seed: take("web-seed")
            .map(|spec| parse_profile(&spec).map(|(p, _)| p))
            .transpose()?,
        inbound: take("inbound").is_some(),
        media: take("media").map(PathBuf::from),
    })
}

/// A limit in MiB/s (fractional allowed) as bytes per second.
fn limit_bps(v: Option<String>, default: f64) -> anyhow::Result<NonZeroU32> {
    let mibps = float(v)?.unwrap_or(default);
    let bps = u32::try_from(mib_to_bytes(mibps)).context("the limit is too high")?;
    NonZeroU32::new(bps).context("the limit must be above zero")
}

/// Takes each seeder offline for a quarter of every `period`, staggered so that some are
/// always online, for `total` (plus a margin).
fn add_churn(peers: &mut [PeerProfile], period: Duration, total: Duration) {
    if period.is_zero() || peers.is_empty() {
        return;
    }
    let n = u32::try_from(peers.len()).unwrap_or(u32::MAX);
    let end = total + period;
    for (i, peer) in (0..n).zip(peers.iter_mut()) {
        // The first outage comes a second in, after the first connections.
        let mut from = Duration::from_secs(1) + period * i / n;
        while from < end {
            peer.offline.push((from, from + period / 4));
            from += period;
        }
    }
}

/// Parses an optional number.
fn float(v: Option<String>) -> anyhow::Result<Option<f64>> {
    v.map(|v| v.parse().context("expected a number"))
        .transpose()
}

fn secs(v: &str) -> anyhow::Result<Duration> {
    let s: f64 = v
        .trim()
        .parse()
        .with_context(|| format!("expected seconds, got {v:?}"))?;
    Ok(Duration::from_secs_f64(s.max(0.0)))
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
                    offset: rest.first().map_or(Ok(Duration::ZERO), |o| secs(o))?,
                });
            }
            ("offline", [from, until]) => p.offline.push((secs(from)?, secs(until)?)),
            ("late", [s]) => p.offline.push((Duration::ZERO, secs(s)?)),
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

fn millis(ms: f64) -> Duration {
    Duration::from_secs_f64(ms.max(0.0) / 1000.0)
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
        bench.flat_startup = true;
    }
    if let Some(mib) = float(take("seek-mib"))? {
        bench.seek_bytes = mib_to_bytes(mib);
        bench.flat_seeks = true;
    }
    if let Some(anchors) = take("anchors") {
        bench.prioritization.max_anchors = anchors.parse().context("--anchors expects a count")?;
    }
    bench.dht = take("no-dht").is_none();
    bench.trackers = take("no-trackers").is_none();
    bench.dht_state_dir = take("dht-state").map(PathBuf::from);
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
        bench.warm = Duration::from_secs_f64(warm.max(0.0));
    }
    if let Some(watch) = float(take("watch"))? {
        bench.watch = Duration::from_secs_f64(watch.max(0.0));
    }
    scheduler_options(take, &mut bench.scheduler)?;
    Ok(bench)
}

/// Tuning options for the swarm engine's scheduler.
fn scheduler_options(
    take: &mut dyn FnMut(&str) -> Option<String>,
    s: &mut torrentplayer_swarm::SchedulerConfig,
) -> anyhow::Result<()> {
    if let Some(ms) = float(take("queue-ms"))? {
        s.queue_time = millis(ms.max(1.0));
    }
    if let Some(ms) = float(take("relaxed-ms"))? {
        s.relaxed_queue_time = millis(ms.max(1.0));
    }
    if let Some(secs) = float(take("relax-horizon-secs"))? {
        s.relax_horizon = Duration::from_secs_f64(secs.max(0.0));
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
        s.critical_secs = Duration::from_secs_f64(secs.max(0.0));
    }
    Ok(())
}

fn source_from(input: &str) -> anyhow::Result<TorrentSource> {
    if input.contains(':') && !Path::new(input).exists() {
        Ok(TorrentSource::parse_link(input)?)
    } else {
        let bytes = std::fs::read(input).with_context(|| format!("reading {input}"))?;
        Ok(TorrentSource::from_file_bytes(bytes)?)
    }
}

/// A running local swarm. Everything in it stops when it is dropped.
struct LocalSwarm {
    _dir: tempfile::TempDir,
    _seeder: Arc<LocalSeeder>,
    _tracker: Option<LocalTracker>,
    _server: Option<WebSeedServer>,
    _links: Vec<Link>,
    _web_link: Option<Link>,
    _dialers: Option<Dialers>,
    clock: Clock,
    torrent: Vec<u8>,
}

impl LocalSwarm {
    /// Starts the seeder, tracker, web seed, and links for `shape`, and points `bench` at them.
    /// `shared` seeds the torrent instead of a new seeder (it must list no tracker and have at
    /// least as many sessions as `shape` has seeders).
    // A linear set-up of the swarm's parts; splitting it would scatter what each mode needs.
    #[allow(clippy::too_many_lines)]
    async fn start(
        shape: &Shape,
        bench: &mut BenchConfig,
        shared: Option<Arc<LocalSeeder>>,
    ) -> anyhow::Result<Self> {
        let Shape {
            size_mib,
            piece_kib,
            upload_mibps,
            rtt_ms,
            peers,
            tracker_interval,
            web_seed,
            inbound,
            media,
        } = shape;
        anyhow::ensure!(
            !peers.is_empty() || web_seed.is_some(),
            "no seeders and no web seed"
        );
        anyhow::ensure!(
            !(*inbound && web_seed.is_some()),
            "--inbound does not combine with --web-seed"
        );
        let clock = Clock::default();
        let tracker_interval = tracker_interval.or(inbound.then_some(INBOUND_TRACKER_INTERVAL));
        let tracker = match tracker_interval {
            Some(interval) => Some(LocalTracker::start(interval, clock.clone()).await?),
            None => None,
        };
        // The seeder is unthrottled; the link emulator provides bandwidth and latency.
        let dir = tempfile::tempdir()?;
        let trackers: Vec<String> = tracker.iter().map(|t| t.url.clone()).collect();
        let seeder = match (shared, media) {
            (Some(seeder), _) => {
                anyhow::ensure!(
                    trackers.is_empty() && seeder.addrs.len() >= peers.len(),
                    "the shared seeder does not fit this swarm"
                );
                seeder
            }
            (None, Some(file)) => Arc::new(
                LocalSeeder::seed(
                    std::path::absolute(file)?,
                    piece_kib * 1024,
                    None,
                    peers.len().max(1),
                    trackers,
                )
                .await
                .with_context(|| format!("seeding {}", file.display()))?,
            ),
            (None, None) => Arc::new(
                LocalSeeder::start(
                    dir.path(),
                    usize::try_from(size_mib * MIB)?,
                    piece_kib * 1024,
                    None,
                    peers.len().max(1),
                    trackers,
                )
                .await?,
            ),
        };
        let mut profiles: Vec<(std::net::SocketAddr, PeerProfile)> = seeder
            .addrs
            .iter()
            .copied()
            .zip(peers.iter().cloned())
            .collect();
        let down_bps = u64::from(*upload_mibps) * MIB;
        let rtt = Duration::from_millis(*rtt_ms);
        let mut torrent = seeder.torrent.to_vec();
        bench.public_swarm = false;
        bench.local_tracker = tracker.is_some();
        if *inbound {
            // The tracker lists nobody; the seeders dial the address the viewer announces.
            let announcer = tracker
                .as_ref()
                .context("--inbound needs the local tracker")?
                .announcer();
            let info_hash = torrentplayer_swarm::Metainfo::from_bytes(&torrent)?.info_hash;
            let dialers = Dialers::start(&profiles, down_bps, rtt, &clock, info_hash, &announcer);
            bench.incoming = true;
            return Ok(Self {
                _dir: dir,
                _seeder: seeder,
                _tracker: tracker,
                _server: None,
                _links: Vec::new(),
                _web_link: None,
                _dialers: Some(dialers),
                clock,
                torrent,
            });
        }
        // The web seed shares the viewer's downlink with the seeders.
        let server = match web_seed {
            Some(profile) => {
                let server = WebSeedServer::start(seeder.file.clone()).await?;
                profiles.push((server.addr, profile.clone()));
                Some(server)
            }
            None => None,
        };
        let mut links = Link::start_profiles(&profiles, down_bps, rtt, &clock).await?;
        let web_link = server.as_ref().and_then(|_| links.pop());
        if let Some(link) = &web_link {
            torrent = with_web_seed(&torrent, &format!("http://{}/lecture.mp4", link.addr));
            bench.local_web_seeds = true;
        }
        if let Some(tracker) = &tracker {
            // Every seeder is found through the tracker, some only after a while.
            tracker.set_peers(
                links
                    .iter()
                    .zip(peers)
                    .map(|(l, p)| (l.addr, p.announce_after))
                    .collect(),
            );
        } else {
            bench.peers = links.iter().map(|l| l.addr).collect();
        }
        Ok(Self {
            _dir: dir,
            _seeder: seeder,
            _tracker: tracker,
            _server: server,
            _links: links,
            _web_link: web_link,
            _dialers: None,
            clock,
            torrent,
        })
    }

    /// The torrent for a new session, with every seeder's schedule (choke cycles, outages)
    /// restarted.
    fn session(&self) -> anyhow::Result<TorrentSource> {
        self.clock.reset();
        Ok(TorrentSource::from_file_bytes(self.torrent.clone())?)
    }
}

async fn run(args: &Args) -> anyhow::Result<Report> {
    let mut bench = args.bench.clone();
    let mut runs = Vec::new();
    match &args.mode {
        Mode::Local(shape) => {
            if let Some(file) = &shape.media {
                let info = MediaInfo::inspect(file).await?;
                apply_media(&info, &mut bench);
                let mut out = std::io::stdout().lock();
                writeln!(out, "media: {}", info.summary())?;
                for warning in &info.warnings {
                    writeln!(out, "warning: {warning}")?;
                }
            }
            let swarm = LocalSwarm::start(shape, &mut bench, None).await?;
            for i in 0..args.runs {
                runs.push(
                    run_once(args.engine, || swarm.session(), &bench)
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
        Mode::Limit(..) => bail!("`limit` has its own report"),
    }
    Ok(Report::new(runs))
}

async fn run_limits(args: &Args, shape: &Shape, plan: LimitPlan) -> anyhow::Result<LimitReport> {
    let mut bench = args.bench.clone();
    let swarm = LocalSwarm::start(shape, &mut bench, None).await?;
    let mut runs = Vec::new();
    for i in 0..args.runs {
        runs.push(
            run_limit(args.engine, swarm.session()?, &bench, plan)
                .await
                .with_context(|| format!("run {}", i + 1))?,
        );
        progress(i + 1, args.runs);
    }
    Ok(LimitReport::new(runs))
}

fn progress(done: usize, total: usize) {
    tracing::info!("run {done}/{total} finished");
}

/// Bytes to MiB, for display only.
#[allow(clippy::cast_precision_loss)]
fn mib(bytes: u64) -> f64 {
    bytes as f64 / MIB as f64
}

/// Bytes (as a float) to MiB, for display only.
#[allow(clippy::cast_precision_loss)]
fn mib_f(bytes: f64) -> f64 {
    bytes / MIB as f64
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
        ("first peer", report.first_peer),
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
        let dht = match run.dht_state_loaded {
            Some(true) => ", saved DHT state",
            Some(false) => ", no saved DHT state",
            None => "",
        };
        writeln!(
            out,
            "run {}: {} peers at play, start-up {:.0} ms, seeks [{}], peers {} live / {} seen, {:.2} MiB/s at the end, {:.2} MiB/s average, {} stalls ({:.1} s of {:.0} s watched){dht}",
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

fn print_limit_report(report: &LimitReport, plan: LimitPlan) -> std::io::Result<()> {
    let mut out = std::io::stdout().lock();
    let engine = report.runs.first().map_or("", |r| r.engine.as_str());
    writeln!(
        out,
        "[{engine}] download limit {:.2} -> {:.2} MiB/s after {:.0} s, {} runs (medians)",
        mib(u64::from(plan.first_bps.get())),
        mib(u64::from(plan.then_bps.get())),
        plan.change_after.as_secs_f64(),
        report.runs.len()
    )?;
    writeln!(
        out,
        "{:<8} {:>12} {:>16} {:>15}",
        "phase", "limit MiB/s", "delivered MiB/s", "reported MiB/s"
    )?;
    for (name, limit, delivered, reported) in [
        (
            "before",
            plan.first_bps,
            report.before_delivered_bps,
            report.before_reported_bps,
        ),
        (
            "after",
            plan.then_bps,
            report.after_delivered_bps,
            report.after_reported_bps,
        ),
    ] {
        writeln!(
            out,
            "{name:<8} {:>12.2} {:>16.3} {:>15.3}",
            mib(u64::from(limit.get())),
            mib_f(delivered),
            mib_f(reported)
        )?;
    }
    let settle = |ms: Option<f64>, n: usize| match ms {
        Some(ms) => format!("{ms:.0} ms ({n}/{} runs settled)", report.runs.len()),
        None => format!("never (0/{} runs)", report.runs.len()),
    };
    writeln!(
        out,
        "1-s rate after the change: max {:.3}, min {:.3} MiB/s; {:.0} KiB over the new limit in the 2 s after it",
        mib_f(report.max_burst_bps),
        mib_f(report.min_after_bps),
        report.excess_bytes / 1024.0
    )?;
    writeln!(
        out,
        "settled within 5%: delivered to the limit {}, to its steady rate {}; reported {}",
        settle(report.settle_ms, report.settled_runs),
        settle(report.settle_steady_ms, report.steady_runs),
        settle(report.reported_settle_ms, report.reported_settled_runs)
    )?;
    let ms = |ms: Option<f64>| ms.map_or("never".into(), |ms| format!("{ms:.0} ms"));
    for (i, run) in report.runs.iter().enumerate() {
        writeln!(
            out,
            "run {}: before {:.3}/{:.3}, after {:.3}/{:.3} MiB/s delivered/reported, max {:.3} min {:.3} MiB/s, excess {:.0} KiB, settle {} / steady {} / reported {}, peers {}-{} live, {} seen, {} wasted",
            i + 1,
            mib_f(run.before.delivered_bps),
            mib_f(run.before.reported_bps),
            mib_f(run.after.delivered_bps),
            mib_f(run.after.reported_bps),
            mib_f(run.max_burst_bps),
            mib_f(run.min_after_bps),
            run.excess_bytes / 1024.0,
            ms(run.settle_ms),
            ms(run.settle_steady_ms),
            ms(run.reported_settle_ms),
            run.peers_live_range.0,
            run.peers_live_range.1,
            run.peers_seen,
            run.wasted_bytes
                .map_or("-".into(), |b| format!("{:.0} KiB", mib(b) * 1024.0))
        )?;
    }
    Ok(())
}

/// `torrentplayer-bench seed <file> <torrent>`: seeds `file` until the process is stopped.
fn seed(file: PathBuf, torrent: &Path) -> anyhow::Result<()> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    runtime.block_on(async {
        let seeder =
            LocalSeeder::seed(std::path::absolute(file)?, 256 * 1024, None, 1, Vec::new()).await?;
        std::fs::write(torrent, &seeder.torrent)
            .with_context(|| format!("writing {}", torrent.display()))?;
        let addr = seeder.addrs.first().context("no seeder")?;
        tracing::info!("seeding at {addr}");
        std::future::pending::<()>().await;
        Ok(())
    })
}

/// Points `bench` at a media file's layout and, unless `--duration` was given, its duration.
fn apply_media(info: &MediaInfo, bench: &mut BenchConfig) {
    bench.layout.clone_from(&info.layout);
    if bench.duration_secs.is_none() {
        bench.duration_secs = info.duration_secs;
    }
}

/// The download limit for public entries in `suite` unless `--download-mibps` is given.
const PUBLIC_LIMIT_MIBPS: u32 = 10;

/// `suite` options.
struct SuiteArgs {
    list: PathBuf,
    public: bool,
    out: PathBuf,
    engines: Vec<EngineKind>,
    scenarios: Vec<Scenario>,
    piece_kib: u32,
    tracker_interval: Option<Duration>,
    runs: usize,
    bench: BenchConfig,
    options: Vec<String>,
}

/// Parses a comma-separated list.
fn numbers<T: std::str::FromStr>(v: Option<String>, name: &str) -> anyhow::Result<Option<Vec<T>>> {
    v.map(|v| {
        v.split(',')
            .map(|n| n.trim().parse::<T>())
            .collect::<Result<Vec<T>, _>>()
            .map_err(|_| anyhow::anyhow!("--{name} expects comma-separated numbers"))
    })
    .transpose()
}

fn parse_suite(args: impl Iterator<Item = String>) -> anyhow::Result<SuiteArgs> {
    let raw: Vec<String> = args.collect();
    let (positional, mut options) = tokenize(raw.iter().cloned())?;
    let mut take = |name: &str| options.remove(name);
    let mut positional = positional.into_iter();
    let list = positional
        .next()
        .context("usage: torrentplayer-bench suite <list file> [public] [options]")?;
    let mut public = take("public").is_some();
    for extra in positional {
        anyhow::ensure!(extra == "public", "unexpected argument {extra:?}");
        public = true;
    }
    let quick = take("quick").is_some();
    let links = numbers::<u32>(take("links"), "links")?;
    let seeders = numbers::<usize>(take("seeders"), "seeders")?;
    let rtt = take("rtt-ms")
        .map(|v| v.parse::<u64>())
        .transpose()
        .context("--rtt-ms expects milliseconds")?;
    let custom = links.is_some() || seeders.is_some() || rtt.is_some();
    let rtt_ms = rtt.unwrap_or(40);
    let scenarios = if quick && !custom {
        local_scenarios(&[], &[], rtt_ms, &[(2, 1, rtt_ms), (25, 4, rtt_ms)])
    } else {
        // The high-latency row only with the default matrix.
        let extra: &[(u32, usize, u64)] = if quick || custom { &[] } else { &[(8, 4, 120)] };
        local_scenarios(
            &links.unwrap_or_else(|| vec![2, 8, 25]),
            &seeders.unwrap_or_else(|| vec![1, 4]),
            rtt_ms,
            extra,
        )
    };
    anyhow::ensure!(
        scenarios
            .iter()
            .all(|s| s.link_mibps > Some(0) && s.seeders > Some(0)),
        "--links and --seeders must be above zero"
    );
    let engines = match take("engines") {
        Some(list) => list
            .split(',')
            .map(|e| e.trim().parse())
            .collect::<anyhow::Result<Vec<EngineKind>>>()?,
        None => vec![EngineKind::Rqbit, EngineKind::Swarm],
    };
    anyhow::ensure!(!engines.is_empty(), "--engines lists no engine");
    let runs = usize::try_from(number(take("runs"), if quick { 1 } else { 3 })?)?.max(1);
    let out = take("out").map_or_else(
        || {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default();
            Path::new("bench-results").join(torrentplayer_bench::suite::timestamp(now))
        },
        PathBuf::from,
    );
    let piece_kib = u32::try_from(number(take("piece-kib"), 256)?)?;
    let tracker_interval =
        float(take("tracker-interval"))?.map(|s| Duration::from_secs_f64(s.max(1.0)));
    let bench = bench_options(&mut take)?;
    if let Some(unknown) = options.keys().next() {
        bail!("unknown option --{unknown} for suite");
    }
    Ok(SuiteArgs {
        list: PathBuf::from(list),
        public,
        out,
        engines,
        scenarios,
        piece_kib,
        tracker_interval,
        runs,
        bench,
        options: raw.into_iter().skip(1).collect(),
    })
}

/// `torrentplayer-bench suite <list>`: runs the matrix for every entry, writing the reports
/// after each. `Ok(false)` if any entry or cell failed.
fn suite(args: impl Iterator<Item = String>) -> anyhow::Result<bool> {
    let args = parse_suite(args)?;
    let text = std::fs::read_to_string(&args.list)
        .with_context(|| format!("reading {}", args.list.display()))?;
    let base = args
        .list
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_default();
    let entries = parse_list(&text, &base)?;
    anyhow::ensure!(
        !entries.is_empty(),
        "{} lists no entries",
        args.list.display()
    );
    let public = entries.iter().filter(|e| e.source.is_public()).count();
    if public > 0 && args.public {
        writeln!(
            std::io::stdout(),
            "NOTICE: {public} entr{} play from public BitTorrent swarms. The benchmark downloads \
             from and uploads to other people's computers, and your IP address is visible to \
             them. Only list content you have the right to download and share, such as openly \
             licensed films or free-software images.",
            if public == 1 { "y" } else { "ies" }
        )?;
    } else if public > 0 {
        writeln!(
            std::io::stdout(),
            "{public} public-swarm entr{} will be skipped; add `public` or --public to run them \
             (they download from and upload to a public swarm).",
            if public == 1 { "y" } else { "ies" }
        )?;
    }
    std::fs::create_dir_all(&args.out)
        .with_context(|| format!("creating {}", args.out.display()))?;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let mut report = SuiteReport {
        started: torrentplayer_bench::suite::timestamp(now),
        list: args.list.display().to_string(),
        options: args.options.clone(),
        runs: args.runs,
        entries: Vec::new(),
    };
    let total = entries.len();
    for (i, entry) in entries.into_iter().enumerate() {
        tracing::info!("entry {}/{total}: {}", i + 1, entry.label);
        let result = if entry.source.is_public() && !args.public {
            EntryResult {
                skipped: Some("a public swarm; run with `public` or --public".into()),
                ..entry_result(entry)
            }
        } else {
            runtime.block_on(run_entry(&args, entry))
        };
        if let Some(error) = &result.error {
            tracing::error!("{}: {error}", result.entry.label);
        }
        report.entries.push(result);
        write_suite(&args.out, &report)?;
    }
    let mut out = std::io::stdout().lock();
    write!(
        out,
        "
{}",
        torrentplayer_bench::suite::render_summary(&report)
    )?;
    writeln!(
        out,
        "
report: {}
raw runs: {}",
        args.out.join("report.md").display(),
        args.out.join("results.json").display()
    )?;
    Ok(report
        .entries
        .iter()
        .all(|e| e.error.is_none() && e.results.iter().all(|r| r.error.is_none())))
}

fn entry_result(entry: Entry) -> EntryResult {
    EntryResult {
        entry,
        media: None,
        warnings: Vec::new(),
        error: None,
        skipped: None,
        results: Vec::new(),
    }
}

fn write_suite(out: &Path, report: &SuiteReport) -> anyhow::Result<()> {
    std::fs::write(
        out.join("report.md"),
        torrentplayer_bench::suite::render_markdown(report),
    )?;
    std::fs::write(out.join("results.json"), serde_json::to_vec_pretty(report)?)?;
    Ok(())
}

async fn run_entry(args: &SuiteArgs, entry: Entry) -> EntryResult {
    let mut result = entry_result(entry);
    let outcome = match result.entry.source.clone() {
        Source::Media(path) => run_media(args, &path, &mut result).await,
        source => {
            run_public(args, &source.input(), &mut result).await;
            Ok(())
        }
    };
    if let Err(error) = outcome {
        result.error = Some(format!("{error:#}"));
    }
    result
}

/// Runs `runs` sessions with `engine`.
async fn runs_of(
    runs: usize,
    engine: EngineKind,
    mut source: impl FnMut() -> anyhow::Result<TorrentSource>,
    bench: &BenchConfig,
) -> anyhow::Result<Report> {
    let mut out = Vec::new();
    for i in 0..runs {
        out.push(
            run_once(engine, &mut source, bench)
                .await
                .with_context(|| format!("run {}", i + 1))?,
        );
        progress(i + 1, runs);
    }
    Ok(Report::new(out))
}

fn cell(engine: EngineKind, scenario: Scenario, outcome: anyhow::Result<Report>) -> ScenarioResult {
    let (report, error) = match outcome {
        Ok(report) => (Some(report), None),
        Err(error) => {
            tracing::error!("{engine}, {}: {error:#}", scenario.label());
            (None, Some(format!("{error:#}")))
        }
    };
    ScenarioResult {
        engine: engine.to_string(),
        scenario,
        report,
        error,
    }
}

/// A local media entry: every scenario with every engine, from one set of seeders.
async fn run_media(args: &SuiteArgs, path: &Path, result: &mut EntryResult) -> anyhow::Result<()> {
    let info = MediaInfo::inspect(path).await?;
    let mut bench = args.bench.clone();
    apply_media(&info, &mut bench);
    result.warnings.extend(info.warnings.iter().cloned());
    if bench.layout.is_some() && (bench.flat_startup || bench.flat_seeks) {
        result.warnings.push(
            "--startup-mib or --seek-mib given: that part uses flat reads instead of the container"
                .into(),
        );
    }
    tracing::info!("media: {}", info.summary());
    result.media = Some(info);
    // Hash the file once: the seeders serve every scenario (a tracker needs its own torrent).
    let most = args
        .scenarios
        .iter()
        .filter_map(|s| s.seeders)
        .max()
        .unwrap_or(1);
    let shared = match args.tracker_interval {
        None => Some(Arc::new(
            LocalSeeder::seed(
                std::path::absolute(path)?,
                args.piece_kib * 1024,
                None,
                most,
                Vec::new(),
            )
            .await
            .with_context(|| format!("seeding {}", path.display()))?,
        )),
        Some(_) => None,
    };
    for &scenario in &args.scenarios {
        let shape = Shape {
            size_mib: 0,
            piece_kib: args.piece_kib,
            upload_mibps: scenario.link_mibps.unwrap_or(8),
            rtt_ms: scenario.rtt_ms.unwrap_or(40),
            peers: vec![PeerProfile::default(); scenario.seeders.unwrap_or(1)],
            tracker_interval: args.tracker_interval,
            web_seed: None,
            inbound: false,
            media: Some(path.to_path_buf()),
        };
        let mut cell_bench = bench.clone();
        let swarm = match LocalSwarm::start(&shape, &mut cell_bench, shared.clone()).await {
            Ok(swarm) => swarm,
            Err(error) => {
                let error = format!("{error:#}");
                for &engine in &args.engines {
                    result
                        .results
                        .push(cell(engine, scenario, Err(anyhow::anyhow!("{error}"))));
                }
                continue;
            }
        };
        for &engine in &args.engines {
            tracing::info!("{}: {engine}, {}", result.entry.label, scenario.label());
            let outcome = runs_of(args.runs, engine, || swarm.session(), &cell_bench).await;
            result.results.push(cell(engine, scenario, outcome));
        }
    }
    Ok(())
}

/// A torrent or magnet entry: its public swarm with every engine.
async fn run_public(args: &SuiteArgs, input: &str, result: &mut EntryResult) {
    let mut bench = args.bench.clone();
    let limit = bench
        .download_limit_bps
        .and_then(|l| u32::try_from(u64::from(l.get()) / MIB).ok())
        .unwrap_or(PUBLIC_LIMIT_MIBPS);
    bench.download_limit_bps = bench
        .download_limit_bps
        .or(NonZeroU32::new(PUBLIC_LIMIT_MIBPS * 1024 * 1024));
    let scenario = Scenario {
        link_mibps: None,
        seeders: None,
        rtt_ms: None,
        download_mibps: Some(limit),
    };
    for &engine in &args.engines {
        tracing::info!("{}: {engine}, {}", result.entry.label, scenario.label());
        let outcome = runs_of(args.runs, engine, || source_from(input), &bench).await;
        result.results.push(cell(engine, scenario, outcome));
    }
}

fn main() -> ExitCode {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .try_init();

    let mut positional = std::env::args().skip(1);
    let mode = positional.next();
    if mode.as_deref() == Some("suite") {
        return match suite(positional) {
            Ok(true) => ExitCode::SUCCESS,
            Ok(false) => ExitCode::FAILURE,
            Err(error) => {
                tracing::error!("{error:#}");
                ExitCode::from(2)
            }
        };
    }
    if mode.as_deref() == Some("seed") {
        let (Some(file), Some(torrent)) = (positional.next(), positional.next()) else {
            tracing::error!("usage: torrentplayer-bench seed <media file> <output .torrent>");
            return ExitCode::from(2);
        };
        return match seed(file.into(), Path::new(&torrent)) {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                tracing::error!("{error:#}");
                ExitCode::from(2)
            }
        };
    }

    let result = (|| -> anyhow::Result<bool> {
        let args = parse_args()?;
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()?;
        if let Mode::Limit(shape, plan) = &args.mode {
            let report = runtime.block_on(run_limits(&args, shape, *plan))?;
            print_limit_report(&report, *plan)?;
            if let Some(path) = &args.json {
                std::fs::write(path, serde_json::to_vec_pretty(&report)?)?;
            }
            return Ok(true);
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn churn_staggers_outages() {
        let mut peers = vec![PeerProfile::default(); 4];
        add_churn(&mut peers, Duration::from_secs(8), Duration::from_secs(16));
        assert_eq!(
            peers[0].offline[0],
            (Duration::from_secs(1), Duration::from_secs(3))
        );
        assert_eq!(
            peers[1].offline[0],
            (Duration::from_secs(3), Duration::from_secs(5))
        );
        // Through the run plus one period.
        assert_eq!(peers[0].offline.len(), 3);
        add_churn(&mut peers[..1], Duration::ZERO, Duration::from_secs(16));
        assert_eq!(peers[0].offline.len(), 3);
    }
}
