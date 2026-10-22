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
            None => (line, None),
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
            .map_or_else(|| default_label(&source), str::to_owned);
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

/// A label from the source: the file name, the last URL segment, or a magnet link's `dn`.
fn default_label(source: &Source) -> String {
    match source {
        Source::Media(p) | Source::TorrentFile(p) => p.file_name().map_or_else(
            || p.display().to_string(),
            |n| n.to_string_lossy().into_owned(),
        ),
        Source::Url(url) => url
            .split(['?', '#'])
            .next()
            .and_then(|u| u.trim_end_matches('/').rsplit('/').next())
            .filter(|s| !s.is_empty())
            .unwrap_or(url)
            .to_owned(),
        Source::Magnet(link) => link
            .split(['?', '&'])
            .find_map(|p| p.strip_prefix("dn="))
            .map_or_else(
                || link.chars().take(60).collect(),
                |name| percent_decode(&name.replace('+', " ")),
            ),
    }
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
                Some(limit) => format!("public swarm, {limit} MiB/s limit"),
                None => "public swarm".to_owned(),
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
}

/// Everything measured for one list entry.
#[derive(Debug, Clone, Serialize)]
pub struct EntryResult {
    /// The list entry.
    pub entry: Entry,
    /// Local media: container, codecs, duration, and layout.
    pub media: Option<MediaInfo>,
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
    /// Runs per engine and scenario.
    pub runs: usize,
    /// One per list entry.
    pub entries: Vec<EntryResult>,
}

fn ms(summary: Option<Summary>) -> String {
    summary.map_or_else(
        || "-".into(),
        |s| format!("{:.0} / {:.0}", s.median_ms, s.p95_ms),
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

/// One table row per engine and scenario.
fn table(out: &mut String, results: &[ScenarioResult]) {
    out.push_str(
        "| Engine | Scenario | Start-up ms (median / p95) | Seek ms (median / p95) | Throughput MiB/s | Peers (live / seen) | Stalls |\n",
    );
    out.push_str("| --- | --- | --- | --- | --- | --- | --- |\n");
    for r in results {
        let scenario = r.scenario.label();
        let Some(report) = &r.report else {
            let error = r.error.as_deref().unwrap_or("did not run");
            let _ = writeln!(
                out,
                "| {} | {scenario} | failed: {} | | | | |",
                r.engine,
                error.replace('|', "/")
            );
            continue;
        };
        let rates: Vec<u64> = report.runs.iter().map(|r| r.avg_download_bps).collect();
        #[allow(clippy::cast_precision_loss)]
        let rate = median(&rates).map_or_else(|| "-".into(), |b| format!("{:.2}", b as f64 / MIB));
        let live: Vec<u32> = report.runs.iter().map(|r| r.peers_live).collect();
        let seen = report.runs.iter().map(|r| r.peers_seen).max().unwrap_or(0);
        let stalls: u32 = report.runs.iter().map(|r| r.stalls).sum();
        let stalled_secs: f64 = report.runs.iter().map(|r| r.stall_ms).sum::<f64>() / 1000.0;
        let _ = writeln!(
            out,
            "| {} | {scenario} | {} | {} | {rate} | {} / {seen} | {stalls} ({stalled_secs:.1} s) |",
            r.engine,
            ms(report.startup),
            ms(report.seek),
            median(&live).unwrap_or(0),
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
    out.push_str(
        "Times are data-ready latencies (see docs/performance.md): start-up from pressing play \
         until the player has what it needs for the first frame; seeks from the seek until the \
         data from the keyframe through the target (plus one second) has arrived. Throughput is \
         the median of each run's average download rate.\n",
    );
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
        table(&mut out, &e.results);
    }
    out
}

/// A plain-text summary for the terminal: one line per engine and scenario.
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
        for r in &e.results {
            let line = match (&r.report, &r.error) {
                (Some(report), _) => format!(
                    "start-up {} ms, seek {} ms (median / p95)",
                    ms(report.startup),
                    ms(report.seek)
                ),
                (None, error) => format!("failed: {}", error.as_deref().unwrap_or("?")),
            };
            let _ = writeln!(out, "  {:<6} {:<32} {line}", r.engine, r.scenario.label());
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
}
