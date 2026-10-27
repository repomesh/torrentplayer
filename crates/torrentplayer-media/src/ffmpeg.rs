// SPDX-License-Identifier: MIT OR Apache-2.0

//! Probing and seek-bar thumbnails with the user's own FFmpeg (`ffprobe` and `ffmpeg`).
//!
//! FFmpeg is not bundled yet, so everything here is optional: [`FfmpegTools::locate`] finds the
//! programs in a folder the user chose, or on `PATH`. The media is untrusted, so the programs
//! run locked down:
//!
//! - They read only the URL they are given (a loopback stream): `-protocol_whitelist http,tcp`
//!   and a `-format_whitelist` of plain media containers, so a playlist (HLS, DASH, concat) or
//!   a reference inside the file cannot make them open another file or host, which would also
//!   get around the network mode and kill switch.
//! - No proxy: FFmpeg's HTTP client follows `http_proxy`, which would send the loopback URL
//!   and its token to the proxy, so [`ffmpeg_command`] removes those variables.
//! - No standard input, no console window, a time limit, and bounded output. The process is
//!   killed when the call is dropped (a newer request, a timeout).
//! - Their output is parsed strictly: the probe's JSON into bounded numbers, a thumbnail only
//!   if it is a JPEG.
//! - Error messages never contain the URL's token.

use std::ffi::{OsStr, OsString};
use std::io;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use serde::Deserialize;
use tokio::io::{AsyncRead, AsyncReadExt as _};
use tokio::process::Command;

use crate::tracks::duration_ms;

/// Demuxers FFmpeg may use: the containers of the video and audio files TorrentPlayer plays.
/// Playlists, image sequences, and other formats that open further files are left out.
pub const FORMAT_WHITELIST: &str = "mov,matroska,avi,mpegts,flv,asf,mpeg,ogg,mp3,flac,wav,aac";
/// Longest probe output read.
const MAX_PROBE_OUTPUT: usize = 64 * 1024;
/// Largest thumbnail accepted.
pub const MAX_THUMBNAIL_BYTES: usize = 512 * 1024;
/// Error output kept for the message.
const MAX_STDERR: usize = 4 * 1024;
/// Longest error message reported.
const MAX_MESSAGE_CHARS: usize = 200;
/// Widest thumbnail asked for, in pixels.
pub const MAX_THUMBNAIL_WIDTH: u32 = 640;
/// Lowest and highest bitrate believed, in bits per second.
const BIT_RATES: std::ops::RangeInclusive<u64> = 1_000..=2_000_000_000;
/// How long `-version` may take.
const VERSION_TIMEOUT: Duration = Duration::from_secs(5);
/// Network read timeout handed to FFmpeg, in microseconds (its unit).
const READ_TIMEOUT_US: &str = "15000000";

/// Why running FFmpeg failed.
#[derive(Debug, thiserror::Error)]
pub enum FfmpegError {
    /// The program is not installed (or not found).
    #[error("{0} was not found")]
    Missing(&'static str),
    /// The program could not be started.
    #[error("could not start {0}: {1}")]
    Spawn(&'static str, #[source] io::Error),
    /// The program did not finish in time and was stopped.
    #[error("{0} took too long")]
    Timeout(&'static str),
    /// The program reported an error (the media is unsupported, malformed, or not downloaded
    /// where it was read). The message is FFmpeg's, without the URL's token.
    #[error("{program} failed: {message}")]
    Failed {
        /// `ffprobe` or `ffmpeg`.
        program: &'static str,
        /// The first line of its error output.
        message: String,
    },
    /// The program's output was too large or not what was asked for.
    #[error("{0} gave unexpected output")]
    Output(&'static str),
    /// Talking to the program failed.
    #[error("reading from {0} failed: {1}")]
    Io(&'static str, #[source] io::Error),
}

/// Where the user's `ffprobe` and `ffmpeg` are, if installed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FfmpegTools {
    /// `ffprobe`, for duration and bitrate.
    pub ffprobe: Option<PathBuf>,
    /// `ffmpeg`, for thumbnails.
    pub ffmpeg: Option<PathBuf>,
}

impl FfmpegTools {
    /// Finds the programs in `dir` (or its `bin` folder) if given, otherwise on `PATH` and, on
    /// macOS and Linux, in the usual package manager folders (apps started from the desktop
    /// often get a short `PATH`). Relative folders are ignored, so nothing is picked up from
    /// the working directory. Touches only the file system.
    #[must_use]
    pub fn locate(dir: Option<&Path>) -> Self {
        let dirs = match dir {
            Some(dir) => vec![dir.to_path_buf(), dir.join("bin")],
            None => search_path(std::env::var_os("PATH").as_deref()),
        };
        Self {
            ffprobe: find("ffprobe", &dirs),
            ffmpeg: find("ffmpeg", &dirs),
        }
    }

    /// Duration and bitrate of the media at `url`, from `ffprobe`.
    ///
    /// # Errors
    ///
    /// Returns an error if `ffprobe` is missing, fails, takes longer than `timeout`, or prints
    /// something unexpected.
    pub async fn probe(&self, url: &str, timeout: Duration) -> Result<MediaInfo, FfmpegError> {
        let program = self
            .ffprobe
            .as_deref()
            .ok_or(FfmpegError::Missing(FFPROBE))?;
        let out = run(
            FFPROBE,
            program,
            &probe_args(url),
            MAX_PROBE_OUTPUT,
            timeout,
        )
        .await?;
        parse_probe(&out)
    }

    /// A JPEG of the key frame at or just before `at_secs` of the video at `url`, `width`
    /// pixels wide (at most [`MAX_THUMBNAIL_WIDTH`]), from `ffmpeg`.
    ///
    /// # Errors
    ///
    /// Returns an error if `ffmpeg` is missing, fails (for example because the bytes it needs
    /// are not downloaded), takes longer than `timeout`, or gives no JPEG.
    pub async fn thumbnail(
        &self,
        url: &str,
        at_secs: f64,
        width: u32,
        timeout: Duration,
    ) -> Result<Vec<u8>, FfmpegError> {
        let program = self.ffmpeg.as_deref().ok_or(FfmpegError::Missing(FFMPEG))?;
        let args = thumbnail_args(url, at_secs, width);
        let out = run(FFMPEG, program, &args, MAX_THUMBNAIL_BYTES, timeout).await?;
        if is_jpeg(&out) {
            Ok(out)
        } else {
            Err(FfmpegError::Output(FFMPEG))
        }
    }
}

/// The version an FFmpeg program reports (`7.1.1`, `N-118000-g1234abcd`), which also shows
/// that it is FFmpeg. `None` if it does not run or answer like FFmpeg.
pub async fn version(program: &Path) -> Option<String> {
    let args = [OsString::from("-hide_banner"), OsString::from("-version")];
    let out = run("ffmpeg", program, &args, MAX_PROBE_OUTPUT, VERSION_TIMEOUT)
        .await
        .ok()?;
    parse_version(&String::from_utf8_lossy(&out))
}

/// What `ffprobe` says about a file.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MediaInfo {
    /// Duration in milliseconds, if known and plausible.
    pub duration_ms: Option<u64>,
    /// Overall bitrate (all streams and the container), bits per second, if known.
    pub bit_rate: Option<u64>,
}

impl MediaInfo {
    /// Bytes per second of playback: the bitrate, or else `len` over the duration.
    #[must_use]
    pub fn byte_rate(&self, len: u64) -> Option<u64> {
        self.bit_rate
            .map(|b| b / 8)
            .or_else(|| {
                self.duration_ms
                    .filter(|ms| *ms > 0)
                    .map(|ms| len.saturating_mul(1000) / ms)
            })
            .filter(|r| *r > 0)
    }
}

const FFPROBE: &str = "ffprobe";
const FFMPEG: &str = "ffmpeg";

/// Options that keep FFmpeg on the one URL and quiet.
fn input_args() -> Vec<OsString> {
    [
        "-hide_banner",
        "-loglevel",
        "error",
        "-protocol_whitelist",
        "http,tcp",
        "-format_whitelist",
        FORMAT_WHITELIST,
        "-rw_timeout",
        READ_TIMEOUT_US,
    ]
    .map(OsString::from)
    .to_vec()
}

fn probe_args(url: &str) -> Vec<OsString> {
    let mut args = input_args();
    args.extend(
        [
            "-show_entries",
            "format=duration,bit_rate",
            "-print_format",
            "json",
            "-i",
            url,
        ]
        .map(OsString::from),
    );
    args
}

fn thumbnail_args(url: &str, at_secs: f64, width: u32) -> Vec<OsString> {
    let at = if at_secs.is_finite() {
        at_secs.max(0.0)
    } else {
        0.0
    };
    let width = width.clamp(16, MAX_THUMBNAIL_WIDTH);
    let mut args = input_args();
    args.push("-nostdin".into());
    args.extend(
        [
            // Seek to the key frame before the position and decode only key frames: fast, and
            // reads only around that frame.
            "-ss".to_owned(),
            format!("{at:.3}"),
            "-noaccurate_seek".to_owned(),
            "-skip_frame".to_owned(),
            "nokey".to_owned(),
            "-i".to_owned(),
            url.to_owned(),
            "-map".to_owned(),
            "0:v:0".to_owned(),
            "-an".to_owned(),
            "-sn".to_owned(),
            "-dn".to_owned(),
            "-frames:v".to_owned(),
            "1".to_owned(),
            "-vf".to_owned(),
            // Even height, aspect ratio kept.
            format!("scale={width}:-2"),
            "-c:v".to_owned(),
            "mjpeg".to_owned(),
            "-q:v".to_owned(),
            "5".to_owned(),
            "-f".to_owned(),
            "image2pipe".to_owned(),
            "pipe:1".to_owned(),
        ]
        .map(OsString::from),
    );
    args
}

/// Environment variables FFmpeg's HTTP client follows. The input is a loopback URL with a
/// private token, so with a proxy set FFmpeg would send that URL (token included) to the proxy.
pub const PROXY_ENV: [&str; 4] = ["http_proxy", "HTTP_PROXY", "no_proxy", "NO_PROXY"];

/// A command that runs `program` (ffmpeg or ffprobe) on a loopback stream: the proxy
/// variables ([`PROXY_ENV`]) removed, no standard input, standard output and error piped, no
/// console window on Windows, and the process killed when the child is dropped. Every
/// TorrentPlayer FFmpeg run starts from this; the caller adds the arguments.
#[must_use]
pub fn ffmpeg_command(program: &Path) -> Command {
    let mut command = Command::new(program);
    for name in PROXY_ENV {
        command.env_remove(name);
    }
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(windows)]
    {
        /// `CREATE_NO_WINDOW`: no console window flashes up.
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    command
}

/// Runs `program` and returns its standard output, at most `max_out` bytes.
async fn run(
    name: &'static str,
    program: &Path,
    args: &[OsString],
    max_out: usize,
    timeout: Duration,
) -> Result<Vec<u8>, FfmpegError> {
    let mut command = ffmpeg_command(program);
    command.args(args);
    let mut child = command.spawn().map_err(|e| FfmpegError::Spawn(name, e))?;
    let (stdout, stderr) = (child.stdout.take(), child.stderr.take());
    let work = async {
        let (out, err) = tokio::try_join!(
            read_capped(stdout, max_out, true),
            read_capped(stderr, MAX_STDERR, false),
        )
        .map_err(|e| match e.kind() {
            io::ErrorKind::FileTooLarge => FfmpegError::Output(name),
            _ => FfmpegError::Io(name, e),
        })?;
        let status = child.wait().await.map_err(|e| FfmpegError::Io(name, e))?;
        if status.success() {
            Ok(out)
        } else {
            Err(FfmpegError::Failed {
                program: name,
                message: message(&err),
            })
        }
    };
    // Dropping `work` drops the child, which kills it.
    tokio::time::timeout(timeout, work)
        .await
        .map_err(|_| FfmpegError::Timeout(name))?
}

/// Reads all of `pipe`, keeping at most `max` bytes. With `strict`, more than `max` bytes is
/// an error ([`io::ErrorKind::FileTooLarge`]); otherwise the rest is read and dropped, so the
/// program never blocks on a full pipe.
async fn read_capped<R: AsyncRead + Unpin>(
    pipe: Option<R>,
    max: usize,
    strict: bool,
) -> io::Result<Vec<u8>> {
    let Some(mut pipe) = pipe else {
        return Ok(Vec::new());
    };
    let mut out = Vec::new();
    // On the heap: two of these run at once, inside every caller's future.
    let mut chunk = vec![0u8; 8 * 1024];
    loop {
        let n = pipe.read(&mut chunk).await?;
        if n == 0 {
            return Ok(out);
        }
        let keep = n.min(max - out.len());
        if strict && keep < n {
            return Err(io::ErrorKind::FileTooLarge.into());
        }
        out.extend_from_slice(chunk.get(..keep).unwrap_or_default());
    }
}

/// The first non-empty line of FFmpeg's error output, without tokens, at most 200 characters.
fn message(stderr: &[u8]) -> String {
    let text = String::from_utf8_lossy(stderr);
    let line = text
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("no message");
    let clean: String = redact_tokens(line)
        .chars()
        .filter(|c| !c.is_control())
        .take(MAX_MESSAGE_CHARS)
        .collect();
    clean
}

/// Replaces runs of 32 or more hexadecimal digits (stream tokens) with `…`.
fn redact_tokens(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut run = String::new();
    let flush = |run: &mut String, out: &mut String| {
        if run.len() >= 32 {
            out.push('…');
        } else {
            out.push_str(run);
        }
        run.clear();
    };
    for c in text.chars() {
        if c.is_ascii_hexdigit() {
            run.push(c);
        } else {
            flush(&mut run, &mut out);
            out.push(c);
        }
    }
    flush(&mut run, &mut out);
    out
}

#[derive(Deserialize)]
struct ProbeOutput {
    #[serde(default)]
    format: Option<ProbeFormat>,
}

#[derive(Deserialize)]
struct ProbeFormat {
    #[serde(default)]
    duration: Option<String>,
    #[serde(default)]
    bit_rate: Option<String>,
}

/// Parses `ffprobe -show_entries format=duration,bit_rate -print_format json`.
fn parse_probe(json: &[u8]) -> Result<MediaInfo, FfmpegError> {
    let out: ProbeOutput =
        serde_json::from_slice(json).map_err(|_| FfmpegError::Output(FFPROBE))?;
    let format = out.format.ok_or(FfmpegError::Output(FFPROBE))?;
    let duration_ms = format
        .duration
        .and_then(|d| d.trim().parse::<f64>().ok())
        .and_then(|secs| duration_ms(secs, 1.0));
    let bit_rate = format
        .bit_rate
        .and_then(|b| b.trim().parse::<u64>().ok())
        .filter(|b| BIT_RATES.contains(b));
    Ok(MediaInfo {
        duration_ms,
        bit_rate,
    })
}

/// The version in the first line of `-version` output: `ffprobe version 7.1.1 Copyright …`.
fn parse_version(text: &str) -> Option<String> {
    let first = text.lines().next()?;
    let mut words = first.split_whitespace();
    let program = words.next()?;
    if !matches!(program, "ffprobe" | "ffmpeg") || words.next()? != "version" {
        return None;
    }
    let version: String = words
        .next()?
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | '+'))
        .take(40)
        .collect();
    (!version.is_empty()).then_some(version)
}

/// Whether `bytes` is a whole JPEG file.
fn is_jpeg(bytes: &[u8]) -> bool {
    bytes.starts_with(&[0xFF, 0xD8, 0xFF]) && bytes.ends_with(&[0xFF, 0xD9])
}

/// The absolute folders of a `PATH` value, then the usual package manager folders.
fn search_path(path: Option<&OsStr>) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = path
        .map(|p| {
            std::env::split_paths(p)
                .filter(|d| d.is_absolute())
                .collect()
        })
        .unwrap_or_default();
    if cfg!(unix) {
        for extra in ["/opt/homebrew/bin", "/usr/local/bin", "/usr/bin"] {
            let extra = PathBuf::from(extra);
            if !dirs.contains(&extra) {
                dirs.push(extra);
            }
        }
    }
    dirs
}

/// The first `name` (with the platform's executable suffix) that is a file in `dirs`.
fn find(name: &str, dirs: &[PathBuf]) -> Option<PathBuf> {
    let file = format!("{name}{}", std::env::consts::EXE_SUFFIX);
    dirs.iter()
        .map(|d| d.join(&file))
        .find(|p| p.is_absolute() && p.is_file())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commands_ignore_proxy_variables() {
        let command = ffmpeg_command(Path::new("ffprobe"));
        let removed: Vec<_> = command
            .as_std()
            .get_envs()
            .filter(|(_, value)| value.is_none())
            .map(|(name, _)| name.to_string_lossy().into_owned())
            .collect();
        for name in PROXY_ENV {
            // Windows compares names case-insensitively, so `http_proxy` and `HTTP_PROXY` may be
            // one entry there.
            assert!(
                removed.iter().any(|r| r.eq_ignore_ascii_case(name)),
                "{name} is removed: {removed:?}"
            );
        }
        assert!(command.get_kill_on_drop());
    }

    fn strings(args: &[OsString]) -> Vec<String> {
        args.iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn parses_probe_output() {
        let json = br#"{
            "programs": [],
            "format": { "duration": "5400.123000", "bit_rate": "2516582" }
        }"#;
        let info = parse_probe(json).unwrap();
        assert_eq!(info.duration_ms, Some(5_400_123));
        assert_eq!(info.bit_rate, Some(2_516_582));
        assert_eq!(info.byte_rate(0), Some(314_572));
    }

    #[test]
    fn implausible_or_missing_values_are_unknown() {
        for json in [
            r#"{"format": {}}"#,
            r#"{"format": {"duration": "N/A", "bit_rate": "N/A"}}"#,
            r#"{"format": {"duration": "-3", "bit_rate": "-1"}}"#,
            r#"{"format": {"duration": "nan", "bit_rate": "99999999999999"}}"#,
            r#"{"format": {"duration": "1e300", "bit_rate": "12"}}"#,
        ] {
            let info = parse_probe(json.as_bytes()).unwrap();
            assert_eq!(info, MediaInfo::default(), "{json}");
            assert_eq!(info.byte_rate(1000), None);
        }
        for bad in [
            "",
            "not json",
            "[]",
            r#"{"streams": []}"#,
            r#"{"format": 5}"#,
        ] {
            assert!(parse_probe(bad.as_bytes()).is_err(), "{bad}");
        }
    }

    #[test]
    fn byte_rate_falls_back_to_length_over_duration() {
        let info = MediaInfo {
            duration_ms: Some(100_000),
            bit_rate: None,
        };
        assert_eq!(info.byte_rate(50_000_000), Some(500_000));
        assert_eq!(info.byte_rate(0), None);
    }

    #[test]
    fn commands_read_only_the_url_through_whitelists() {
        let url = "http://127.0.0.1:5000/s/3?t=abc";
        for args in [probe_args(url), thumbnail_args(url, 61.5, 320)] {
            let args = strings(&args);
            let after = |flag: &str| {
                let i = args.iter().position(|a| a == flag).unwrap();
                args[i + 1].clone()
            };
            assert_eq!(after("-protocol_whitelist"), "http,tcp");
            assert_eq!(after("-format_whitelist"), FORMAT_WHITELIST);
            assert_eq!(after("-i"), url);
            assert_eq!(args.iter().filter(|a| a.contains(url)).count(), 1);
            // Whitelists come before the input they apply to.
            assert!(
                args.iter().position(|a| a == "-format_whitelist")
                    < args.iter().position(|a| a == "-i")
            );
        }
        let thumb = strings(&thumbnail_args(url, 61.5, 320));
        assert!(thumb.contains(&"61.500".to_owned()));
        assert!(thumb.contains(&"scale=320:-2".to_owned()));
        assert_eq!(thumb.last().map(String::as_str), Some("pipe:1"));
        assert!(!FORMAT_WHITELIST.contains("hls") && !FORMAT_WHITELIST.contains("concat"));
    }

    #[test]
    fn thumbnail_arguments_are_clamped() {
        let args = strings(&thumbnail_args("u", f64::NAN, 100_000));
        assert!(args.contains(&"0.000".to_owned()));
        assert!(args.contains(&format!("scale={MAX_THUMBNAIL_WIDTH}:-2")));
        let args = strings(&thumbnail_args("u", -5.0, 0));
        assert!(args.contains(&"0.000".to_owned()));
        assert!(args.contains(&"scale=16:-2".to_owned()));
    }

    #[test]
    fn error_messages_drop_tokens() {
        let token = "0123456789abcdef".repeat(4);
        let stderr = format!(
            "\n  http://127.0.0.1:5000/s/3?t={token}: Server returned 403 Forbidden\nmore\n"
        );
        let m = message(stderr.as_bytes());
        assert!(!m.contains(&token), "{m}");
        assert_eq!(
            m,
            "http://127.0.0.1:5000/s/3?t=…: Server returned 403 Forbidden"
        );
        assert_eq!(message(b""), "no message");
        assert_eq!(message(&[b'x'; 1000]).len(), MAX_MESSAGE_CHARS);
        assert_eq!(redact_tokens("deadbeef cafe"), "deadbeef cafe");
    }

    #[test]
    fn parses_versions() {
        assert_eq!(
            parse_version("ffprobe version 7.1.1-full_build-www.gyan.dev Copyright (c) 2007"),
            Some("7.1.1-full_build-www.gyan.dev".to_owned())
        );
        assert_eq!(
            parse_version("ffmpeg version N-118000-g1234abcd Copyright"),
            Some("N-118000-g1234abcd".to_owned())
        );
        assert_eq!(parse_version("something version 1"), None);
        assert_eq!(parse_version("ffmpeg"), None);
        assert_eq!(parse_version(""), None);
    }

    #[test]
    fn recognizes_whole_jpegs_only() {
        assert!(is_jpeg(&[0xFF, 0xD8, 0xFF, 0xE0, 0, 0xFF, 0xD9]));
        assert!(!is_jpeg(b"\x89PNG\r\n\x1a\n"));
        assert!(!is_jpeg(&[0xFF, 0xD8, 0xFF, 0xE0]), "truncated");
        assert!(!is_jpeg(&[]));
    }

    #[test]
    fn finds_programs_in_absolute_folders_only() {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("bin");
        std::fs::create_dir(&bin).unwrap();
        let exe = |name: &str| format!("{name}{}", std::env::consts::EXE_SUFFIX);
        std::fs::write(bin.join(exe("ffprobe")), b"").unwrap();

        let tools = FfmpegTools::locate(Some(dir.path()));
        assert_eq!(tools.ffprobe, Some(bin.join(exe("ffprobe"))));
        assert_eq!(tools.ffmpeg, None);

        let path = std::env::join_paths([PathBuf::from("relative"), bin.clone()]).unwrap();
        let dirs = search_path(Some(&path));
        assert_eq!(dirs.first(), Some(&bin));
        assert!(dirs.iter().all(|d| d.is_absolute()));
        assert_eq!(search_path(None).is_empty(), !cfg!(unix));
    }

    #[tokio::test]
    async fn a_missing_program_is_reported() {
        let tools = FfmpegTools::default();
        assert!(matches!(
            tools.probe("http://x", Duration::from_secs(1)).await,
            Err(FfmpegError::Missing("ffprobe"))
        ));
        let gone = FfmpegTools {
            ffprobe: Some(PathBuf::from("/nonexistent/ffprobe")),
            ffmpeg: None,
        };
        assert!(matches!(
            gone.probe("http://x", Duration::from_secs(1)).await,
            Err(FfmpegError::Spawn("ffprobe", _))
        ));
        assert_eq!(version(Path::new("/nonexistent/ffmpeg")).await, None);
    }
}
