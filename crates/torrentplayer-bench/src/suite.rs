// SPDX-License-Identifier: MIT OR Apache-2.0

//! `torrentplayer-bench suite`: a benchmark matrix over a list of files, with a Markdown and
//! JSON report.
//!
//! The list is plain text, one entry per line. Blank lines and lines starting with `#` are
//! ignored. An entry is the whole line (so Windows paths with spaces need no quotes; surrounding
//! quotes are removed), optionally followed by ` | label`:
//!
//! ```text
//! # Local media: absolute, or relative to the list file
//! D:\Videos\My Lecture 01.mkv | lecture (MKV)
//! clips/faststart.mp4
//! # Public swarms (run only with --public)
//! https://webtorrent.io/torrents/sintel.torrent | Sintel
//! magnet:?xt=urn:btih:...
//! downloads/some.torrent
//! ```

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use anyhow::bail;
use serde::Serialize;

use crate::media::MediaInfo;
use crate::{Report, RunResult, Summary};

const MIB: f64 = 1024.0 * 1024.0;

/// Where an entry's media comes from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", content = "source", rename_all = "snake_case")]
pub enum Source {
    /// A local media file, seeded on `127.0.0.1` (`local --media`).
    Media(PathBuf),
    /// A `.torrent` file, played from its public swarm.
    TorrentFile(PathBuf),
    /// An `http(s)://` link to a `.torrent` file, played from its public swarm.
    Url(String),
    /// A magnet link, played from its public swarm.
    Magnet(String),
}

impl Source {
    /// Whether the entry downloads from (and uploads to) a public swarm.
    #[must_use]
    pub fn is_public(&self) -> bool {
        !matches!(self, Self::Media(_))
    }

    /// The input for `torrent` mode, or the media path.
    #[must_use]
    pub fn input(&self) -> String {
        match self {
            Self::Media(p) | Self::TorrentFile(p) => p.display().to_string(),
            Self::Url(s) | Self::Magnet(s) => s.clone(),
        }
    }
}

/// One line of a list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Entry {
    /// Line number in the list (1-based).
    pub line: usize,
    /// Its label: the one given, or a name taken from the source.
    pub label: String,
    /// Where its media comes from.
    #[serde(flatten)]
    pub source: Source,
}

/// Parses a list; relative paths are relative to `base` (the list file's folder).
///
/// # Errors
///
/// Returns an error naming the line of an entry that is not a path, URL, or magnet link.
pub fn parse_list(text: &str, base: &Path) -> anyhow::Result<Vec<Entry>> {
    let mut entries = Vec::new();
    for (i, raw) in text.lines().enumerate() {
        let line = raw.trim().trim_start_matches('\u{feff}');
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        // Paths, URLs, and magnet links never contain " | ".
        let (value, label) = match line.rsplit_once(" | ") {
            Some((value, label)) => (value.trim(), Some(label.trim())),
            // An empty label: the line was trimmed after the separator.
            None => match line.strip_suffix(" |") {
                Some(value) => (value.trim(), None),
                None => (line, None),
            },
        };
        let value = unquote(value);
        if value.is_empty() {
            bail!("line {}: no file, URL, or magnet link", i + 1);
        }
        let lower = value.to_ascii_lowercase();
        let source = if lower.starts_with("magnet:") {
            Source::Magnet(value.to_owned())
        } else if lower.starts_with("https://") || lower.starts_with("http://") {
            Source::Url(value.to_owned())
        } else if lower.contains("://") {
            bail!(
                "line {}: only https:// links and magnet links are supported",
                i + 1
            );
        } else {
            let path = base.join(value);
            if lower.ends_with(".torrent") {
                Source::TorrentFile(path)
            } else {
                Source::Media(path)
            }
        };
        let label = label
            .filter(|l| !l.is_empty())
            .map(str::to_owned)
            .or_else(|| default_label(&source))
            .unwrap_or_else(|| format!("line {}", i + 1));
        entries.push(Entry {
            line: i + 1,
            label,
            source,
        });
    }
    Ok(entries)
}

/// Removes one pair of surrounding quotes, as Explorer's "Copy as path" adds.
fn unquote(s: &str) -> &str {
    for q in ['"', '\''] {
        if let Some(inner) = s.strip_prefix(q).and_then(|s| s.strip_suffix(q)) {
            return inner.trim();
        }
    }
    s
}

/// A label from the source: the file name, the last URL segment, or a magnet link's `dn`, else
/// its info hash (shortened). `None` if the source gives none.
fn default_label(source: &Source) -> Option<String> {
    let label = match source {
        // Split on both separators: a list written on Windows gets the same labels on any OS.
        Source::Media(p) | Source::TorrentFile(p) => {
            let path = p.to_string_lossy();
            path.trim_end_matches(['/', '\\'])
                .rsplit(['/', '\\'])
                .next()
                .filter(|n| !n.is_empty())
                .map_or_else(|| path.clone().into_owned(), str::to_owned)
        }
        Source::Url(url) => url
            .split(['?', '#'])
            .next()
            .and_then(|u| u.trim_end_matches('/').rsplit('/').next())
            .filter(|s| !s.is_empty())
            .unwrap_or(url)
            .to_owned(),
        Source::Magnet(link) => {
            let param = |key: &str| {
                link.split(['?', '&'])
                    .find_map(|p| p.strip_prefix(key))
                    .map(|v| percent_decode(&v.replace('+', " ")).trim().to_owned())
                    .filter(|v| !v.is_empty())
            };
            param("dn=")
                .or_else(|| {
                    param("xt=").map(|xt| {
                        let hash = xt.rsplit(':').next().unwrap_or(&xt);
                        format!("magnet {}", hash.chars().take(12).collect::<String>())
                    })
                })
                .unwrap_or_default()
        }
    };
    let label = label.trim();
    (!label.is_empty()).then(|| label.to_owned())
}

/// Decodes `%XX` escapes (invalid ones stay as they are).
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && let Some(byte) = s
                .get(i + 1..i + 3)
                .and_then(|h| u8::from_str_radix(h, 16).ok())
        {
            out.push(byte);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// One cell of the matrix.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Scenario {
    /// Local swarm: the viewer's downlink (MiB/s), shared by the seeders.
    pub link_mibps: Option<u32>,
    /// Local swarm: the number of seeders.
    pub seeders: Option<usize>,
    /// Local swarm: the round-trip time to each seeder.
    pub rtt_ms: Option<u64>,
    /// Public swarm: the download limit (MiB/s).
    pub download_mibps: Option<u32>,
    /// Public swarm: the swarm engine also used the torrent's HTTP web seeds (BEP 19); otherwise
    /// peers only.
    pub web_seeds: bool,
}

impl Scenario {
    /// A short description for report tables.
    #[must_use]
    pub fn label(&self) -> String {
        match (self.link_mibps, self.seeders, self.rtt_ms) {
            (Some(link), Some(seeders), Some(rtt)) => {
                let s = if seeders == 1 { "" } else { "s" };
                format!("{link} MiB/s, {seeders} seeder{s}, {rtt} ms")
            }
            _ => match self.download_mibps {
                Some(limit) => format!("public swarm, {}, {limit} MiB/s limit", self.source()),
                None => format!("public swarm, {}", self.source()),
            },
        }
    }
}

/// The local-swarm cells: every link × seeders at `rtt_ms`, plus `extra` cells.
#[must_use]
pub fn local_scenarios(
    links: &[u32],
    seeders: &[usize],
    rtt_ms: u64,
    extra: &[(u32, usize, u64)],
) -> Vec<Scenario> {
    let cell = |link, seeders, rtt| Scenario {
        link_mibps: Some(link),
        seeders: Some(seeders),
        rtt_ms: Some(rtt),
        download_mibps: None,
        web_seeds: false,
    };
    let mut out: Vec<Scenario> = links
        .iter()
        .flat_map(|&link| seeders.iter().map(move |&s| cell(link, s, rtt_ms)))
        .collect();
    for &(link, s, rtt) in extra {
        let extra = cell(link, s, rtt);
        if !out.contains(&extra) {
            out.push(extra);
        }
    }
    out
}

impl Scenario {
    fn source(&self) -> &'static str {
        if self.web_seeds {
            "peers + web seeds"
        } else {
            "peers only"
        }
    }

    /// Whether this is a public-swarm cell.
    #[must_use]
    pub fn is_public(&self) -> bool {
        self.link_mibps.is_none()
    }
}

/// What a torrent says about itself, for public entries.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct TorrentInfo {
    /// The torrent's name (a magnet link's `dn`).
    pub name: Option<String>,
    /// Total size in bytes (unknown for magnet links).
    pub size: Option<u64>,
    /// Number of files (unknown for magnet links).
    pub files: Option<usize>,
    /// Trackers listed.
    pub trackers: usize,
    /// HTTP web seeds listed (BEP 19 `url-list`, or a magnet link's `ws=`).
    pub web_seeds: usize,
}

impl TorrentInfo {
    /// What a magnet link lists.
    #[must_use]
    pub fn from_magnet(link: &str) -> Self {
        let params: Vec<&str> = link
            .split_once('?')
            .map_or("", |(_, q)| q)
            .split('&')
            .collect();
        let count = |key: &str| params.iter().filter(|p| p.starts_with(key)).count();
        Self {
            name: params
                .iter()
                .find_map(|p| p.strip_prefix("dn="))
                .map(|n| percent_decode(&n.replace('+', " "))),
            size: None,
            files: None,
            trackers: count("tr="),
            web_seeds: count("ws="),
        }
    }

    /// One line for reports.
    #[must_use]
    pub fn summary(&self) -> String {
        let mut parts = vec![self.name.clone().unwrap_or_else(|| "unnamed".into())];
        match (self.size, self.files) {
            (Some(size), Some(files)) => {
                let s = if files == 1 { "" } else { "s" };
                parts.push(format!("{} in {files} file{s}", bytes(size)));
            }
            _ => parts.push("size unknown before the metadata arrives".into()),
        }
        parts.push(format!("{} trackers", self.trackers));
        parts.push(match self.web_seeds {
            0 => "no web seeds (peers only)".to_owned(),
            1 => "1 HTTP web seed".to_owned(),
            n => format!("{n} HTTP web seeds"),
        });
        parts.join(", ")
    }
}

/// One engine in one scenario.
#[derive(Debug, Clone, Serialize)]
pub struct ScenarioResult {
    /// Engine measured.
    pub engine: String,
    /// The cell of the matrix.
    pub scenario: Scenario,
    /// All runs and their summaries, if it ran.
    pub report: Option<Report>,
    /// Why it failed.
    pub error: Option<String>,
    /// Why it was not run (an engine the app does not use for this source in this mode).
    pub skipped: Option<String>,
}

/// Everything measured for one list entry.
#[derive(Debug, Clone, Serialize)]
pub struct EntryResult {
    /// The list entry.
    pub entry: Entry,
    /// Local media: container, codecs, duration, and layout.
    pub media: Option<MediaInfo>,
    /// Public entries: what the torrent lists (trackers, web seeds).
    pub torrent: Option<TorrentInfo>,
    /// Notes for the reader (container not parsed, flat needs, ...).
    pub warnings: Vec<String>,
    /// Why the entry did not run at all.
    pub error: Option<String>,
    /// Why the entry was skipped (public entries without `--public`).
    pub skipped: Option<String>,
    /// One per engine and scenario.
    pub results: Vec<ScenarioResult>,
}

/// The whole suite, as written to `results.json`.
#[derive(Debug, Clone, Serialize)]
pub struct SuiteReport {
    /// When the suite started (UTC).
    pub started: String,
    /// The list file.
    pub list: String,
    /// The options, as given.
    pub options: Vec<String>,
    /// The protected network mode public entries ran in (`WireGuard via host:port`), without
    /// secrets; `None` if they connected directly. Local media always runs directly.
    pub network: Option<String>,
    /// Runs per engine and scenario.
    pub runs: usize,
    /// One per list entry.
    pub entries: Vec<EntryResult>,
}

/// Median start-up target on a healthy swarm, in milliseconds (docs/performance.md).
pub const STARTUP_TARGET_MS: f64 = 3000.0;
/// Median seek target on a healthy swarm, in milliseconds.
pub const SEEK_TARGET_MS: f64 = 1500.0;
/// A public swarm where an engine saw fewer peers than this is called thin.
pub const THIN_SWARM_PEERS: u32 = 5;

/// The generated guide at the top of `report.md` (mirrored in docs/performance.md).
const GUIDE: &str = "\
## How to read this report

- **Start-up** is the time from pressing play until the player has the bytes it needs for the \
first frame: the container header, wherever it is, and about a second of media. **Seek** is the \
time from a seek until the data from the keyframe before the target through one second past the \
target has arrived. Both are data-ready times at the stream server, not decoding or the first \
frame on screen, which add a little on top.
- **Targets** on a healthy swarm: median start-up under 3 s and median seek under 1.5 s. Each \
entry's verdict counts the rows that meet both.
- **Median and p95:** the median is the typical case, p95 nearly the worst. With 3 runs (12 \
seeks) p95 is close to the maximum, so one slow run moves it.
- **Local scenarios** run on this machine and repeat well. The link speed is the viewer's \
downlink in MiB/s, shared by all seeders: 2 is a slow home connection, 8 a typical one, 25 a \
fast one (a 1080p film needs about 1). Seeders hold the whole file; the round trip to each is \
40 ms (nearby) or 120 ms (far away).
- **Public swarms** are real peers on the internet and vary from run to run and day to day. \
Rows are peers only unless marked *peers + web seeds*: the torrent's HTTP servers, which only \
the swarm engine uses, so those rows partly measure a web server. *Peers per run* is live at the \
end / seen during the run; a handful means a thin swarm, whose result depends on a few peers.
- **Comparing engines:** compare rqbit and swarm within one scenario, not across scenarios. With \
few runs, differences under about 15%, or with overlapping median-to-p95 ranges, are likely \
noise; rerun with `--runs 5` before drawing conclusions.
- **Containers:** an MP4 with `moov` at the end needs a read at the end of the file before any \
media, so it starts slower than the same file with `moov` first (faststart); a Matroska file \
reads its Cues before the first seek when they come after the media. The line under each file \
says what start-up and seeks read.
- **Seeks of a few milliseconds** mean the data was already downloaded (a short file on a fast \
link). **Throughput** is the median of each run's average download rate. **Stalls** count the \
times playback ran out of data in the 3 s watched after start-up and after each seek.
";

fn ms(summary: Option<Summary>) -> String {
    summary.map_or_else(
        || "-".into(),
        |s| format!("{:.0} / {:.0}", s.median_ms, s.p95_ms),
    )
}

/// Error text on one line, without the table's column separator.
fn one_line(text: &str) -> String {
    text.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .replace('|', "/")
}

/// The seek column: median / p95, and how many seeks hit the timeout (counted at the timeout).
fn seek_cell(report: &Report) -> String {
    let seeks = report.runs.iter().flat_map(|r| &r.seeks);
    let timed_out = seeks.clone().filter(|s| s.timed_out).count();
    if timed_out == 0 {
        return ms(report.seek);
    }
    format!(
        "{} ({timed_out} of {} timed out)",
        ms(report.seek),
        seeks.count()
    )
}

/// Why the app would not use librqbit for `source` in this network mode, if it would not: in
/// its WireGuard and SOCKS5 modes, librqbit only fetches magnet links' metadata, and `.torrent`
/// files and links open in TorrentPlayer's engine (`torrentplayer_torrent::Engine` refuses
/// them in a protected session).
#[must_use]
pub fn rqbit_not_used(protected: bool, source: &Source) -> Option<&'static str> {
    (protected && !matches!(source, Source::Magnet(_))).then_some(
        "not used in this network mode (the app plays .torrent files and links on its own \
         engine there)",
    )
}

/// The median of `values` (the upper one for an even count); `None` if empty.
fn median<T: Copy + PartialOrd>(values: &[T]) -> Option<T> {
    let mut sorted = values.to_vec();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    sorted.get(sorted.len() / 2).copied()
}

/// Bytes for display: `B`, `KiB`, or `MiB`.
#[must_use]
pub fn bytes(n: u64) -> String {
    #[allow(clippy::cast_precision_loss)]
    let f = n as f64;
    if n < 1024 {
        format!("{n} B")
    } else if f < MIB {
        format!("{:.0} KiB", f / 1024.0)
    } else {
        format!("{:.1} MiB", f / MIB)
    }
}

/// How the first run read at start-up and per seek, for the report.
fn needs_line(run: &RunResult) -> String {
    let reads: Vec<String> = run
        .startup_reads
        .iter()
        .map(|s| format!("{} at {}", bytes(s.len), bytes(s.start)))
        .collect();
    let seek_bytes: Vec<u64> = run.seeks.iter().map(|s| s.bytes).collect();
    let seek = median(&seek_bytes).map_or_else(String::new, |b| {
        format!("; seeks read a median {} each", bytes(b))
    });
    if run.container_aware {
        format!(
            "Container-aware: start-up reads {}{seek}.",
            if reads.len() > 1 {
                format!("{}, one after another", reads.join(", then "))
            } else {
                reads.join("")
            }
        )
    } else {
        format!(
            "Flat: start-up reads {} at once{seek}.",
            reads.join(" and ")
        )
    }
}

/// Whether a row meets both targets (by median).
fn meets_targets(report: &Report) -> bool {
    report
        .startup
        .is_some_and(|s| s.median_ms < STARTUP_TARGET_MS)
        && report.seek.is_none_or(|s| s.median_ms < SEEK_TARGET_MS)
}

/// The most peers an engine saw in any run of a row.
fn most_peers_seen(report: &Report) -> u32 {
    report.runs.iter().map(|r| r.peers_seen).max().unwrap_or(0)
}

/// One sentence per entry: how many rows meet the targets, the slowest rows, and thin swarms.
#[must_use]
pub fn verdict(results: &[ScenarioResult]) -> Option<String> {
    let ran: Vec<(&ScenarioResult, &Report)> = results
        .iter()
        .filter_map(|r| r.report.as_ref().map(|report| (r, report)))
        .collect();
    if ran.is_empty() {
        return None;
    }
    let meets = ran
        .iter()
        .filter(|(_, report)| meets_targets(report))
        .count();
    let row = |r: &ScenarioResult| format!("{}, {}", r.engine, r.scenario.label());
    let slowest = |pick: fn(&Report) -> Option<Summary>| {
        ran.iter()
            .filter_map(|(r, report)| pick(report).map(|s| (s.median_ms, *r)))
            .max_by(|a, b| a.0.total_cmp(&b.0))
    };
    let mut out = format!(
        "**Verdict:** meets the targets (median start-up < 3 s, median seek < 1.5 s) in {meets} of {} rows",
        ran.len()
    );
    let failed = results.iter().filter(|r| r.error.is_some()).count();
    if failed > 0 {
        let _ = write!(out, "; {failed} more failed");
    }
    out.push('.');
    if let Some((ms, r)) = slowest(|report| report.startup) {
        let _ = write!(out, " Slowest start-up: {} ({:.1} s).", row(r), ms / 1000.0);
    }
    if let Some((ms, r)) = slowest(|report| report.seek) {
        let _ = write!(out, " Slowest seek: {} ({:.1} s).", row(r), ms / 1000.0);
    }
    let thin: Vec<String> = ran
        .iter()
        .filter(|(r, report)| r.scenario.is_public() && most_peers_seen(report) < THIN_SWARM_PEERS)
        .map(|(r, report)| format!("{} saw at most {}", r.engine, most_peers_seen(report)))
        .collect();
    if !thin.is_empty() {
        let _ = write!(
            out,
            " Thin swarm ({} peers): these numbers depend on a few peers and vary between runs.",
            thin.join(", ")
        );
    }
    Some(out)
}

/// One table row per engine and scenario.
fn table(out: &mut String, results: &[ScenarioResult]) {
    out.push_str(
        "| Engine | Scenario | Start-up ms (median / p95) | Seek ms (median / p95) | Throughput MiB/s | Peers per run (live / seen) | Stalls |\n",
    );
    out.push_str("| --- | --- | --- | --- | --- | --- | --- |\n");
    for r in results {
        let scenario = r.scenario.label();
        if let Some(reason) = &r.skipped {
            let _ = writeln!(out, "| {} | {scenario} | {reason} | | | | |", r.engine);
            continue;
        }
        let Some(report) = &r.report else {
            let error = r.error.as_deref().unwrap_or("did not run");
            let _ = writeln!(
                out,
                "| {} | {scenario} | failed: {} | | | | |",
                r.engine,
                one_line(error)
            );
            continue;
        };
        let rates: Vec<u64> = report.runs.iter().map(|r| r.avg_download_bps).collect();
        #[allow(clippy::cast_precision_loss)]
        let rate = median(&rates).map_or_else(|| "-".into(), |b| format!("{:.2}", b as f64 / MIB));
        let peers: Vec<String> = report
            .runs
            .iter()
            .map(|r| format!("{} / {}", r.peers_live, r.peers_seen))
            .collect();
        let stalls: u32 = report.runs.iter().map(|r| r.stalls).sum();
        let stalled_secs: f64 = report.runs.iter().map(|r| r.stall_ms).sum::<f64>() / 1000.0;
        let _ = writeln!(
            out,
            "| {} | {scenario} | {} | {} | {rate} | {} | {stalls} ({stalled_secs:.1} s) |",
            r.engine,
            ms(report.startup),
            seek_cell(report),
            peers.join(", "),
        );
    }
}

/// The Markdown report.
#[must_use]
pub fn render_markdown(suite: &SuiteReport) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "# TorrentPlayer benchmark suite\n");
    let _ = writeln!(
        out,
        "Started {} from `{}` with {} run(s) per engine and scenario. Options: `{}`.\n",
        suite.started,
        suite.list,
        suite.runs,
        if suite.options.is_empty() {
            "(defaults)".to_owned()
        } else {
            suite.options.join(" ")
        }
    );
    if let Some(network) = &suite.network {
        let _ = writeln!(
            out,
            "Network: torrents and magnet links ran through {network}, as the app's protected \
             mode does (kill switch on; librqbit without its own DHT). Local media ran \
             directly.\n"
        );
    }
    out.push_str(GUIDE);
    for e in &suite.entries {
        let _ = writeln!(out, "\n## {}\n", e.entry.label);
        let _ = writeln!(
            out,
            "`{}` (line {})\n",
            e.entry.source.input(),
            e.entry.line
        );
        if let Some(media) = &e.media {
            let _ = writeln!(out, "Media: {}.\n", media.summary());
        }
        if let Some(torrent) = &e.torrent {
            let _ = writeln!(out, "Torrent: {}.\n", torrent.summary());
        }
        if let Some(run) = e
            .results
            .iter()
            .find_map(|r| r.report.as_ref().and_then(|r| r.runs.first()))
            && !e.entry.source.is_public()
        {
            let _ = writeln!(out, "{}\n", needs_line(run));
        }
        for w in &e.warnings {
            let _ = writeln!(out, "> Warning: {w}\n");
        }
        if let Some(skipped) = &e.skipped {
            let _ = writeln!(out, "Skipped: {skipped}");
            continue;
        }
        if let Some(error) = &e.error {
            let _ = writeln!(out, "Error: {error}");
            continue;
        }
        if let Some(verdict) = verdict(&e.results) {
            let _ = writeln!(out, "{verdict}\n");
        }
        table(&mut out, &e.results);
    }
    out
}

/// A plain-text summary for the terminal: the verdict and one line per engine and scenario.
#[must_use]
pub fn render_summary(suite: &SuiteReport) -> String {
    let mut out = String::new();
    for e in &suite.entries {
        let _ = writeln!(out, "{}", e.entry.label);
        if let Some(skipped) = &e.skipped {
            let _ = writeln!(out, "  skipped: {skipped}");
            continue;
        }
        if let Some(error) = &e.error {
            let _ = writeln!(out, "  error: {error}");
            continue;
        }
        if let Some(verdict) = verdict(&e.results) {
            let _ = writeln!(out, "  {}", verdict.replace("**", ""));
        }
        for r in &e.results {
            let line = match (&r.report, &r.error) {
                (Some(report), _) => format!(
                    "start-up {} ms, seek {} ms (median / p95)",
                    ms(report.startup),
                    ms(report.seek)
                ),
                (None, _) if r.skipped.is_some() => r.skipped.clone().unwrap_or_default(),
                (None, error) => format!("failed: {}", one_line(error.as_deref().unwrap_or("?"))),
            };
            let _ = writeln!(out, "  {:<6} {:<45} {line}", r.engine, r.scenario.label());
        }
    }
    out
}

/// `YYYYMMDD-HHMMSS` in UTC, for result folders.
#[must_use]
pub fn timestamp(since_epoch: std::time::Duration) -> String {
    let secs = since_epoch.as_secs();
    let (days, rest) = (secs / 86_400, secs % 86_400);
    // Days to a civil date (Howard Hinnant's algorithm), for dates after 1970.
    let z = days + 719_468;
    let era = z / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + u64::from(month <= 2);
    format!(
        "{year:04}{month:02}{day:02}-{:02}{:02}{:02}",
        rest / 3600,
        rest % 3600 / 60,
        rest % 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_comments_labels_paths_with_spaces_and_links() {
        let base = Path::new("C:/lists");
        let text = "\u{feff}# a comment\n\
                    \n\
                    D:\\My Videos\\Lecture 01 (final).mkv | Lecture one\n\
                    \"C:\\Users\\me\\Copied path.mp4\"\n\
                    clips/sub dir/clip.mp4\n\
                    \x20  # an indented comment\n\
                    https://webtorrent.io/torrents/sintel.torrent | Sintel\n\
                    https://example.org/files/big.torrent?x=1\n\
                    magnet:?xt=urn:btih:dd8255ecdc7ca55fb0bbf81323d87062db1f6d1c&dn=Big+Buck%20Bunny&tr=udp%3A%2F%2Fexample\n\
                    downloads/ubuntu.iso.TORRENT\n";
        let entries = parse_list(text, base).unwrap();
        let summary: Vec<(usize, &str, &Source)> = entries
            .iter()
            .map(|e| (e.line, e.label.as_str(), &e.source))
            .collect();
        assert_eq!(
            summary,
            [
                (
                    3,
                    "Lecture one",
                    &Source::Media(base.join("D:\\My Videos\\Lecture 01 (final).mkv"))
                ),
                (
                    4,
                    "Copied path.mp4",
                    &Source::Media(base.join("C:\\Users\\me\\Copied path.mp4"))
                ),
                (
                    5,
                    "clip.mp4",
                    &Source::Media(base.join("clips/sub dir/clip.mp4"))
                ),
                (
                    7,
                    "Sintel",
                    &Source::Url("https://webtorrent.io/torrents/sintel.torrent".into())
                ),
                (
                    8,
                    "big.torrent",
                    &Source::Url("https://example.org/files/big.torrent?x=1".into())
                ),
                (
                    9,
                    "Big Buck Bunny",
                    &Source::Magnet(
                        "magnet:?xt=urn:btih:dd8255ecdc7ca55fb0bbf81323d87062db1f6d1c&dn=Big+Buck%20Bunny&tr=udp%3A%2F%2Fexample".into()
                    )
                ),
                (
                    10,
                    "ubuntu.iso.TORRENT",
                    &Source::TorrentFile(base.join("downloads/ubuntu.iso.TORRENT"))
                ),
            ]
        );
        assert!(!entries[0].source.is_public());
        assert!(entries[3].source.is_public() && entries[6].source.is_public());
    }

    #[test]
    fn absolute_windows_paths_ignore_the_list_folder() {
        let entries = parse_list("C:\\Media\\a b.mkv", Path::new("D:\\lists")).unwrap();
        let Source::Media(path) = &entries[0].source else {
            panic!("not media");
        };
        if cfg!(windows) {
            assert_eq!(path, Path::new("C:\\Media\\a b.mkv"));
        }
    }

    #[test]
    fn rejects_other_schemes_and_empty_entries() {
        assert!(parse_list("ftp://example.org/a.torrent", Path::new(".")).is_err());
        assert!(parse_list("\"\" | label", Path::new(".")).is_err());
        assert!(
            parse_list("# only comments\n\n", Path::new("."))
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn builds_the_local_matrix() {
        let cells = local_scenarios(&[2, 8, 25], &[1, 4], 40, &[(8, 4, 120), (2, 1, 40)]);
        assert_eq!(cells.len(), 7);
        assert_eq!(cells[0].label(), "2 MiB/s, 1 seeder, 40 ms");
        assert_eq!(cells[6].label(), "8 MiB/s, 4 seeders, 120 ms");
    }

    #[test]
    fn formats_utc_timestamps() {
        assert_eq!(
            timestamp(std::time::Duration::from_secs(0)),
            "19700101-000000"
        );
        // 2026-10-22 09:30:05 UTC.
        assert_eq!(
            timestamp(std::time::Duration::from_secs(1_792_661_405)),
            "20261022-093005"
        );
        // A leap day.
        assert_eq!(
            timestamp(std::time::Duration::from_secs(1_709_164_800)),
            "20240229-000000"
        );
    }

    #[test]
    fn reads_web_seeds_and_trackers_from_magnet_links() {
        let info = TorrentInfo::from_magnet(
            "magnet:?xt=urn:btih:08ada5a7a6183aae1e09d831df6748d566095a10&dn=Sintel&tr=udp%3A%2F%2Fa&tr=wss%3A%2F%2Fb&ws=https%3A%2F%2Fwebtorrent.io%2Ftorrents%2F",
        );
        assert_eq!(info.name.as_deref(), Some("Sintel"));
        assert_eq!((info.trackers, info.web_seeds), (2, 1));
        assert!(info.summary().contains("1 HTTP web seed"));
        let bare = TorrentInfo::from_magnet("magnet:?xt=urn:btih:08ada5a7");
        assert_eq!((bare.trackers, bare.web_seeds), (0, 0));
        assert!(bare.summary().contains("no web seeds (peers only)"));
    }

    fn run(startup_ms: f64, seek_ms: f64, peers_seen: u32) -> RunResult {
        RunResult {
            engine: "rqbit".into(),
            torrent: String::new(),
            file: String::new(),
            file_bytes: 0,
            metadata_ms: 0.0,
            first_peer_ms: None,
            dht_state_loaded: None,
            first_byte_ms: 0.0,
            startup_ms,
            startup_reads: Vec::new(),
            container_aware: true,
            raw_bytes: false,
            seeks: vec![crate::SeekResult {
                fraction: 0.5,
                offset: 0,
                target_ms: None,
                keyframe_ms: None,
                bytes: 0,
                first_byte_ms: 0.0,
                ready_ms: seek_ms,
                timed_out: false,
            }],
            peers_at_play: 0,
            peers_live: peers_seen,
            peers_seen,
            download_bps: 0,
            avg_download_bps: 0,
            wasted_bytes: None,
            duplicate_requests: None,
            byte_rate: 0,
            buffer_secs: Vec::new(),
            min_buffer_secs: None,
            stalls: 0,
            stall_ms: 0.0,
            watch_ms: 0.0,
        }
    }

    fn row(engine: &str, scenario: Scenario, runs: Vec<RunResult>) -> ScenarioResult {
        ScenarioResult {
            engine: engine.into(),
            scenario,
            report: Some(Report::new(runs)),
            error: None,
            skipped: None,
        }
    }

    #[test]
    fn verdicts_count_rows_within_the_targets_and_flag_thin_swarms() {
        let local = local_scenarios(&[2, 25], &[1], 40, &[]);
        let public = Scenario {
            link_mibps: None,
            seeders: None,
            rtt_ms: None,
            download_mibps: Some(10),
            web_seeds: false,
        };
        let results = vec![
            row("rqbit", local[0], vec![run(4000.0, 900.0, 1)]),
            row("swarm", local[1], vec![run(700.0, 2000.0, 1)]),
            row("swarm", local[1], vec![run(700.0, 300.0, 1)]),
            row("rqbit", public, vec![run(1500.0, 800.0, 3)]),
            ScenarioResult {
                engine: "swarm".into(),
                scenario: public,
                report: None,
                error: Some("timed out".into()),
                skipped: None,
            },
        ];
        let verdict = verdict(&results).unwrap();
        assert!(
            verdict.contains("in 2 of 4 rows; 1 more failed"),
            "{verdict}"
        );
        assert!(
            verdict.contains("Slowest start-up: rqbit, 2 MiB/s, 1 seeder, 40 ms (4.0 s)"),
            "{verdict}"
        );
        assert!(
            verdict.contains("Slowest seek: swarm, 25 MiB/s"),
            "{verdict}"
        );
        assert!(
            verdict.contains("Thin swarm (rqbit saw at most 3 peers)"),
            "{verdict}"
        );
        assert!(super::verdict(&[]).is_none());
        assert_eq!(public.label(), "public swarm, peers only, 10 MiB/s limit");
        assert_eq!(
            Scenario {
                web_seeds: true,
                ..public
            }
            .label(),
            "public swarm, peers + web seeds, 10 MiB/s limit"
        );
    }

    #[test]
    fn reports_start_with_the_guide_and_give_peers_per_run() {
        let entry = parse_list("a.mp4", Path::new(".")).unwrap().remove(0);
        let cell = local_scenarios(&[8], &[4], 40, &[])[0];
        let suite = SuiteReport {
            started: "20261022-000000".into(),
            list: "list.txt".into(),
            options: Vec::new(),
            network: None,
            runs: 2,
            entries: vec![EntryResult {
                entry,
                media: None,
                torrent: None,
                warnings: Vec::new(),
                error: None,
                skipped: None,
                results: vec![row(
                    "swarm",
                    cell,
                    vec![run(500.0, 400.0, 4), run(600.0, 500.0, 3)],
                )],
            }],
        };
        let md = render_markdown(&suite);
        assert!(md.contains("## How to read this report"));
        assert!(md.find("How to read").unwrap() < md.find("## a.mp4").unwrap());
        assert!(md.contains("meets the targets"));
        assert!(md.contains("| 4 / 4, 3 / 3 |"), "{md}");
    }

    #[test]
    fn labels_never_end_up_empty() {
        let labels = |text: &str| -> Vec<String> {
            parse_list(text, Path::new("."))
                .unwrap()
                .into_iter()
                .map(|e| e.label)
                .collect()
        };
        assert_eq!(
            labels(
                "magnet:?xt=urn:btih:08ada5a7a6183aae1e09d831df6748d566095a10&dn=\n\
                 magnet:?xt=urn:btih:08ada5a7a6183aae1e09d831df6748d566095a10 |  \n\
                 magnet:?dn=+&tr=x\n\
                 magnet:?xt=urn:btih:abc&dn=Big%20Buck | \n"
            ),
            [
                "magnet 08ada5a7a618",
                "magnet 08ada5a7a618",
                "line 3",
                "Big Buck"
            ]
        );
    }

    #[test]
    fn protected_modes_leave_torrent_files_to_the_swarm_engine() {
        let file = Source::TorrentFile("a.torrent".into());
        let url = Source::Url("https://example.org/a.torrent".into());
        let magnet = Source::Magnet("magnet:?xt=urn:btih:abc".into());
        assert!(rqbit_not_used(true, &file).is_some());
        assert!(rqbit_not_used(true, &url).is_some());
        assert!(rqbit_not_used(true, &magnet).is_none(), "as in the app");
        assert!(
            rqbit_not_used(false, &file).is_none(),
            "direct: both engines"
        );
    }

    #[test]
    fn skipped_rows_and_timed_out_seeks_are_reported_not_failed() {
        let public = Scenario {
            link_mibps: None,
            seeders: None,
            rtt_ms: None,
            download_mibps: Some(10),
            web_seeds: false,
        };
        let mut slow = run(900.0, 120_000.0, 30);
        slow.seeks[0].timed_out = true;
        let results = vec![
            ScenarioResult {
                engine: "rqbit".into(),
                scenario: public,
                report: None,
                error: None,
                skipped: rqbit_not_used(true, &Source::Url("https://x/a.torrent".into()))
                    .map(str::to_owned),
            },
            row("swarm", public, vec![run(900.0, 400.0, 30), slow]),
        ];
        let verdict = verdict(&results).unwrap();
        assert!(verdict.contains("of 1 rows."), "{verdict}");
        let mut md = String::new();
        table(&mut md, &results);
        assert!(
            md.contains(
                "| rqbit | public swarm, peers only, 10 MiB/s limit | not used in this network mode"
            ),
            "{md}"
        );
        assert!(md.contains("(1 of 2 timed out)"), "{md}");
        assert!(one_line("a.        b | c\n d") == "a. b / c d");
    }

    #[test]
    fn the_repository_benchmark_list_parses() {
        let base = Path::new("lists");
        let entries = parse_list(include_str!("../lists/benchmarks.txt"), base).unwrap();
        assert_eq!(entries.len(), 12);
        let media = entries
            .iter()
            .filter(|e| matches!(e.source, Source::Media(_)))
            .count();
        assert_eq!(media, 4);
        assert!(entries.iter().all(|e| !e.label.is_empty()));
        assert_eq!(
            entries[0].source,
            Source::Media(base.join("media/bbb_sunflower_2160p_60fps_normal.mp4"))
        );
        assert!(matches!(entries.last().unwrap().source, Source::Magnet(_)));
        // The example list is all comments.
        assert!(
            parse_list(include_str!("../lists/example.txt"), base)
                .unwrap()
                .is_empty()
        );
    }
}
