// SPDX-License-Identifier: MIT OR Apache-2.0

//! Playing Dolby and DTS audio, which the built-in player cannot decode, by converting it to AAC
//! with the bundled FFmpeg sidecar (see docs/adr/0007-dolby-and-dts-audio.md).
//!
//! FFmpeg reads the video from a loopback server of its own, copies the video, converts the
//! audio track the player would play, and writes fragmented MP4 with the original timestamps.
//! Its output is published as a live stream on the app's stream server, and the player feeds it
//! to Media Source Extensions. Each seek outside what is buffered starts a new conversion from
//! there; the previous one is stopped.

use std::collections::VecDeque;
use std::io;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::task::{Context, Poll};
use std::time::Duration;

use serde::Serialize;
use tauri::State;
use tokio::io::{AsyncRead, AsyncReadExt as _, ReadBuf};
use tokio::process::{Child, ChildStdout};
use torrentplayer_stream::{LiveSource, MediaSource, ServerConfig, StreamId, StreamServer};
use ts_rs::TS;

use crate::core::{AppState, Core};
use crate::error::{ErrorKind, IpcError};

/// The sidecar's file name, without the `.exe` suffix on Windows.
const SIDECAR: &str = "torrentplayer-ffmpeg";
/// How long FFmpeg may take to write the initialization segment. It must first read the
/// container's index and the first frames, which may still be downloading.
const INIT_TIMEOUT: Duration = Duration::from_secs(60);
/// AAC bitrate of the converted audio, which is mixed down to stereo.
const AAC_BITRATE: &str = "192k";
/// Lines of FFmpeg's log kept for the error message when a conversion fails.
const LOG_LINES: usize = 8;
/// Bytes retained per log line; media metadata can contain arbitrarily long lines.
const LOG_LINE_BYTES: usize = 2048;
/// Read size while waiting for the initialization segment.
const READ_CHUNK: usize = 64 * 1024;

/// A converted stream, for the player's Media Source Extensions.
#[derive(Debug, Clone, Serialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub(crate) struct ConvertedStream {
    /// The live stream's URL, read once with `fetch`. Treat it as a secret: do not log it.
    pub url: String,
    /// The type for `MediaSource.addSourceBuffer`, such as
    /// `video/mp4; codecs="avc1.64001f,mp4a.40.2"`.
    pub mime: String,
    /// The video's length in seconds, if the container states it.
    pub duration: Option<f64>,
}

/// The bundled FFmpeg, if this build has one: next to the app's executable, where Tauri puts
/// sidecars. Debug builds also accept a path in `TORRENTPLAYER_FFMPEG`.
pub(crate) fn sidecar() -> Option<&'static Path> {
    static PATH: OnceLock<Option<PathBuf>> = OnceLock::new();
    PATH.get_or_init(|| {
        #[cfg(debug_assertions)]
        if let Some(path) = std::env::var_os("TORRENTPLAYER_FFMPEG") {
            return Some(PathBuf::from(path));
        }
        let exe = std::env::current_exe().ok()?;
        let path = exe
            .parent()?
            .join(format!("{SIDECAR}{}", std::env::consts::EXE_SUFFIX));
        path.is_file().then_some(path)
    })
    .as_deref()
}

/// A stream's conversions: FFmpeg's input server and the current output.
pub(crate) struct Converter {
    /// A loopback server of its own that serves only this stream. Its URL is on FFmpeg's command
    /// line, which other local users may be able to see, so it must not grant access to anything
    /// else. Dropped, it stops serving.
    _input: StreamServer,
    input_url: String,
    /// The current conversion's output on the app's stream server.
    pub(crate) output: Option<StreamId>,
}

impl Converter {
    async fn start(media: Arc<dyn MediaSource>) -> Result<Self, IpcError> {
        let input = StreamServer::start(ServerConfig {
            max_connections: 4,
            ..ServerConfig::default()
        })
        .await
        .map_err(|e| IpcError::internal(&e))?;
        let input_url = input.publish(media).url;
        Ok(Self {
            _input: input,
            input_url,
            output: None,
        })
    }
}

/// Starts converting a stream's audio from `start` seconds and returns where to read the
/// result. Stops the stream's previous conversion.
#[tauri::command]
pub(crate) async fn convert_audio(
    state: State<'_, AppState>,
    stream_id: StreamId,
    start: f64,
) -> Result<ConvertedStream, IpcError> {
    let Some(ffmpeg) = sidecar() else {
        return Err(IpcError::new(
            ErrorKind::AudioConversion,
            "This build can't convert audio.",
        ));
    };
    if !start.is_finite() || !(0.0..=1_000_000.0).contains(&start) {
        return Err(IpcError::invalid(
            "The start position must be zero or more.",
        ));
    }
    let core = state.core().await?;
    let cancelled = core.with_stream(stream_id, |s| s.conversion_request.begin())?;
    tokio::select! {
        biased;
        () = cancelled => Err(IpcError::new(ErrorKind::NotFound, "The conversion was cancelled.")),
        result = convert(core, ffmpeg, stream_id, start) => result,
    }
}

/// Performs the cancellable work; dropping this future kills a child still writing its init.
async fn convert(
    core: &Core,
    ffmpeg: &Path,
    stream_id: StreamId,
    start: f64,
) -> Result<ConvertedStream, IpcError> {
    let mut probe = core.with_stream(stream_id, |s| s.tracks.subscribe())?;
    let probed = probe
        .wait_for(Option::is_some)
        .await
        .map_err(|_| IpcError::new(ErrorKind::NotFound, "Unknown stream."))?
        .clone();
    let track = match probed {
        Some(p)
            if p.audio
                .primary
                .as_ref()
                .is_some_and(|t| supported_codec(&t.codec)) =>
        {
            p.primary_stream
        }
        _ => {
            return Err(IpcError::invalid(
                "This video's audio doesn't need converting.",
            ));
        }
    };

    let (media, input_url) = core.with_stream(stream_id, |s| {
        (
            Arc::clone(&s.source),
            s.converter.as_ref().map(|c| c.input_url.clone()),
        )
    })?;
    let input_url = if let Some(url) = input_url {
        url
    } else {
        let converter = Converter::start(media).await?;
        // Another call may have started one meanwhile; the first one stays.
        core.with_stream(stream_id, |s| {
            s.converter.get_or_insert(converter).input_url.clone()
        })?
    };

    let started = Conversion::start(ffmpeg, &input_url, track, start)
        .await
        .map_err(|error| {
            tracing::warn!(stream = stream_id, %error, "could not convert the audio");
            IpcError::new(
                ErrorKind::AudioConversion,
                "The audio couldn't be converted.",
            )
        })?;
    let published = core.server.publish_live(Arc::new(started.output));
    let previous = core.with_stream(stream_id, |s| {
        s.converter
            .as_mut()
            .and_then(|c| c.output.replace(published.id))
    });
    if let Ok(Some(previous)) = previous {
        core.server.unpublish(previous);
    }
    if previous.is_err() {
        // The stream stopped meanwhile.
        core.server.unpublish(published.id);
        return Err(IpcError::new(ErrorKind::NotFound, "Unknown stream."));
    }
    tracing::info!(
        stream = stream_id,
        start,
        mime = %started.mime,
        "converting the audio"
    );
    Ok(ConvertedStream {
        url: published.url,
        mime: started.mime,
        duration: started.duration,
    })
}

/// Decoders included in the playback sidecar. AC-4 is detected but cannot be converted.
pub(crate) fn supported_codec(codec: &str) -> bool {
    matches!(codec, "ac3" | "eac3" | "truehd" | "dts")
}

/// A running conversion whose initialization segment has arrived.
struct Conversion {
    output: Output,
    mime: String,
    duration: Option<f64>,
}

/// Why a conversion did not start.
#[derive(Debug, thiserror::Error)]
enum ConversionError {
    #[error("could not run FFmpeg: {0}")]
    Spawn(#[source] io::Error),
    #[error("FFmpeg did not start writing in time")]
    Timeout,
    #[error("FFmpeg stopped before writing any video: {0}")]
    Ended(String),
    #[error("{0}")]
    Init(#[from] torrentplayer_media::InitError),
    #[error("the video's codec can't be played: {0}")]
    Codec(String),
    #[error("reading FFmpeg's output failed: {0}")]
    Read(#[source] io::Error),
}

impl Conversion {
    /// Runs FFmpeg from `start` seconds and waits for the initialization segment. `track` is
    /// FFmpeg's specifier for the audio track to convert (the first audio track if unknown).
    async fn start(
        ffmpeg: &Path,
        input_url: &str,
        track: Option<String>,
        start: f64,
    ) -> Result<Self, ConversionError> {
        // No proxy (the input is a loopback URL with a token), no stdin, no console window.
        let mut command = torrentplayer_media::ffmpeg_command(ffmpeg);
        command.args(arguments(input_url, track, start));
        let mut child = command.spawn().map_err(ConversionError::Spawn)?;
        let log = Arc::new(Log::default());
        if let Some(stderr) = child.stderr.take() {
            let log = Arc::clone(&log);
            tauri::async_runtime::spawn(async move {
                read_log(stderr, &log).await;
            });
        }
        let Some(mut stdout) = child.stdout.take() else {
            return Err(ConversionError::Spawn(io::Error::other("no stdout")));
        };

        let init = tokio::time::timeout(INIT_TIMEOUT, read_init(&mut stdout, &log))
            .await
            .map_err(|_| ConversionError::Timeout)??;
        let (len, bytes) = init;
        let mime = torrentplayer_media::mse_type(&bytes[..len])
            .ok_or_else(|| ConversionError::Codec(log.tail()))?;
        Ok(Self {
            output: Output::new(bytes, stdout, child),
            mime,
            duration: log.duration(),
        })
    }
}

/// FFmpeg's arguments: copy the first video stream (not cover art) if there is one, convert one
/// audio track to stereo AAC, and write fragmented MP4 that keeps the original timestamps, so
/// the player can place each conversion on the video's timeline.
fn arguments(input_url: &str, track: Option<String>, start: f64) -> Vec<String> {
    let audio = track.unwrap_or_else(|| "0:a:0".to_owned());
    let mut args: Vec<String> = ["-nostdin", "-hide_banner", "-nostats", "-loglevel", "info"]
        .map(str::to_owned)
        .to_vec();
    if start > 0.0 {
        args.extend(["-ss".to_owned(), format!("{start:.3}")]);
    }
    args.extend(
        [
            "-copyts",
            "-protocol_whitelist",
            "http,tcp",
            "-format_whitelist",
            "matroska,webm,mov,mp4,m4a,3gp,3g2,mj2",
            "-i",
            input_url,
            // Optional, so audio-only files convert too.
            "-map",
            "0:V:0?",
            "-map",
            &audio,
            "-c:v",
            "copy",
            "-c:a",
            "aac",
            "-b:a",
            AAC_BITRATE,
            "-ac",
            "2",
            "-avoid_negative_ts",
            "make_non_negative",
            "-f",
            "mp4",
            "-movflags",
            "frag_keyframe+empty_moov+default_base_moof+frag_discont",
            "-frag_duration",
            "1000000",
            "pipe:1",
        ]
        .map(str::to_owned),
    );
    args
}

/// Reads FFmpeg's output until the initialization segment is complete. Returns its length and
/// everything read so far.
async fn read_init(
    stdout: &mut ChildStdout,
    log: &Log,
) -> Result<(usize, Vec<u8>), ConversionError> {
    let mut bytes = Vec::new();
    let mut chunk = vec![0; READ_CHUNK];
    loop {
        if let Some(len) = torrentplayer_media::init_segment_len(&bytes) {
            return Ok((len?, bytes));
        }
        let n = stdout
            .read(&mut chunk)
            .await
            .map_err(ConversionError::Read)?;
        if n == 0 {
            // Let the log catch up with FFmpeg's last words.
            tokio::time::sleep(Duration::from_millis(100)).await;
            return Err(ConversionError::Ended(log.tail()));
        }
        bytes.extend_from_slice(&chunk[..n]);
    }
}

/// FFmpeg's log: the input's duration, and the last lines for error messages. The input URL's
/// token is removed from every line kept.
#[derive(Default)]
struct Log {
    lines: Mutex<VecDeque<String>>,
    duration: Mutex<Option<f64>>,
}

/// Drains stderr without allocating in proportion to a hostile metadata line.
async fn read_log(mut stderr: impl AsyncRead + Unpin, log: &Log) {
    let mut chunk = [0; 4096];
    let mut line = Vec::with_capacity(LOG_LINE_BYTES);
    while let Ok(n) = stderr.read(&mut chunk).await {
        if n == 0 {
            break;
        }
        for &byte in &chunk[..n] {
            if byte == b'\n' {
                log.push(&String::from_utf8_lossy(&line));
                line.clear();
            } else if line.len() < LOG_LINE_BYTES {
                line.push(byte);
            }
        }
    }
    if !line.is_empty() {
        log.push(&String::from_utf8_lossy(&line));
    }
}

impl Log {
    fn push(&self, line: &str) {
        if let Some(rest) = line.trim_start().strip_prefix("Duration: ") {
            let mut duration = self.duration.lock().unwrap_or_else(PoisonError::into_inner);
            if duration.is_none() {
                *duration = parse_duration(rest);
            }
        }
        let mut lines = self.lines.lock().unwrap_or_else(PoisonError::into_inner);
        if lines.len() == LOG_LINES {
            lines.pop_front();
        }
        lines.push_back(redact(line));
    }

    fn tail(&self) -> String {
        let lines = self.lines.lock().unwrap_or_else(PoisonError::into_inner);
        lines
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>()
            .join(" | ")
    }

    fn duration(&self) -> Option<f64> {
        *self.duration.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// Seconds from FFmpeg's `HH:MM:SS.ss, start: …` (`N/A` when the container has none).
fn parse_duration(text: &str) -> Option<f64> {
    let clock = text.split(',').next()?.trim();
    let mut parts = clock.split(':');
    let hours: f64 = parts.next()?.parse().ok()?;
    let minutes: f64 = parts.next()?.parse().ok()?;
    let seconds: f64 = parts.next()?.parse().ok()?;
    let total = hours * 3600.0 + minutes * 60.0 + seconds;
    (parts.next().is_none() && total.is_finite() && total > 0.0).then_some(total)
}

/// `line` without the value of any `t=` (token) query parameter.
fn redact(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut rest = line;
    while let Some(at) = rest.find("t=") {
        let (before, after) = rest.split_at(at + 2);
        out.push_str(before);
        let preceded = before[..before.len() - 2].ends_with(['?', '&']);
        if preceded {
            out.push_str("<redacted>");
            let end = after
                .find(|c: char| !c.is_ascii_alphanumeric() && c != '-' && c != '_')
                .unwrap_or(after.len());
            rest = &after[end..];
        } else {
            rest = after;
        }
    }
    out.push_str(rest);
    out
}

/// A conversion's output: the initialization segment read so far, then the rest of FFmpeg's
/// output. Owns FFmpeg, which is stopped when the output is dropped: when the stream is
/// unpublished before being read, or when the player stops reading it.
struct Output(Mutex<Option<OutputReader>>);

impl Output {
    fn new(read: Vec<u8>, stdout: ChildStdout, child: Child) -> Self {
        Self(Mutex::new(Some(OutputReader {
            reader: io::Cursor::new(read).chain(stdout),
            _child: child,
        })))
    }
}

impl LiveSource for Output {
    fn content_type(&self) -> &'static str {
        "video/mp4"
    }

    fn take(&self) -> Option<Box<dyn AsyncRead + Unpin + Send>> {
        let reader = self
            .0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()?;
        Some(Box::new(reader))
    }
}

struct OutputReader {
    reader: tokio::io::Chain<io::Cursor<Vec<u8>>, ChildStdout>,
    _child: Child,
}

impl AsyncRead for OutputReader {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.reader).poll_read(cx, buf)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Optional real-sidecar smoke test, with a locally generated Dolby/DTS fixture.
    #[tokio::test]
    async fn real_sidecar_converts_and_seeks() {
        type Open = Pin<
            Box<dyn Future<Output = io::Result<Box<dyn torrentplayer_stream::MediaReader>>> + Send>,
        >;
        struct Memory(Arc<[u8]>);
        impl MediaSource for Memory {
            fn byte_len(&self) -> u64 {
                self.0.len() as u64
            }
            fn content_type(&self) -> &'static str {
                "application/octet-stream"
            }
            fn open(&self) -> Open {
                let data = Arc::clone(&self.0);
                Box::pin(async move {
                    Ok(Box::new(io::Cursor::new(data))
                        as Box<dyn torrentplayer_stream::MediaReader>)
                })
            }
        }
        let (Some(ffmpeg), Some(fixture)) = (
            std::env::var_os("TORRENTPLAYER_TEST_FFMPEG"),
            std::env::var_os("TORRENTPLAYER_TEST_DOLBY_FILE"),
        ) else {
            return;
        };
        let source = Memory(tokio::fs::read(fixture).await.unwrap().into());
        let input = Converter::start(Arc::new(source)).await.unwrap();
        for start in [0.0, 60.0] {
            let converted = Conversion::start(
                Path::new(&ffmpeg),
                &input.input_url,
                Some("0:a:0".to_owned()),
                start,
            )
            .await
            .unwrap();
            assert!(converted.mime.contains("mp4a.40.2"));
            let mut output = converted.output.take().unwrap().take(64 * 1024 * 1024);
            let mut bytes = Vec::new();
            tokio::time::timeout(Duration::from_secs(20), output.read_to_end(&mut bytes))
                .await
                .unwrap()
                .unwrap();
            assert!(bytes.windows(4).any(|w| w == b"moof"));
            assert!(bytes.windows(4).any(|w| w == b"mdat"));
        }
    }

    #[tokio::test]
    async fn stderr_lines_are_bounded_and_still_drained() {
        let input = format!(
            "{}\n  Duration: 00:01:00.00, start: 0\nlast",
            "x".repeat(100_000)
        );
        let log = Log::default();
        read_log(input.as_bytes(), &log).await;
        assert_eq!(log.duration(), Some(60.0));
        let lines = log.lines.lock().unwrap();
        assert_eq!(lines[0].len(), LOG_LINE_BYTES);
        assert_eq!(lines.back().unwrap(), "last");
    }

    #[test]
    fn only_the_bundled_decoders_are_offered() {
        for codec in ["ac3", "eac3", "truehd", "dts"] {
            assert!(supported_codec(codec));
        }
        for codec in ["ac4", "aac", "other"] {
            assert!(!supported_codec(codec));
        }
    }

    #[test]
    fn arguments_map_the_track_and_keep_timestamps() {
        let args = arguments(
            "http://127.0.0.1:1/s/1?t=secret",
            Some("0:i:2".to_owned()),
            31.5,
        );
        let text = args.join(" ");
        assert!(text.contains("-ss 31.500 -copyts"));
        assert!(text.contains("-protocol_whitelist http,tcp"));
        assert!(text.contains("-format_whitelist matroska,webm,mov,mp4,m4a,3gp,3g2,mj2 -i http://127.0.0.1:1/s/1?t=secret"));
        assert!(text.contains("-map 0:V:0? -map 0:i:2"));
        assert!(text.contains("-c:v copy -c:a aac"));
        assert!(text.ends_with("pipe:1"));

        let from_start = arguments("u", None, 0.0).join(" ");
        assert!(!from_start.contains("-ss"));
        assert!(from_start.contains("-map 0:a:0"));
    }

    #[test]
    fn parses_durations() {
        assert_eq!(
            parse_duration("00:01:30.08, start: 0.000000, bitrate: 2773 kb/s"),
            Some(90.08)
        );
        assert_eq!(parse_duration("02:00:00.00, start: 0"), Some(7200.0));
        assert_eq!(parse_duration("N/A, start: 0.000000, bitrate: N/A"), None);
        assert_eq!(parse_duration("garbage"), None);
    }

    #[test]
    fn log_keeps_the_duration_and_redacts_tokens() {
        let log = Log::default();
        log.push("Input #0, matroska,webm, from 'http://127.0.0.1:5/s/1?t=AbC-12_x':");
        log.push("  Duration: 00:00:10.00, start: 0.000000, bitrate: 100 kb/s");
        for i in 0..LOG_LINES {
            log.push(&format!("line {i}"));
        }
        assert_eq!(log.duration(), Some(10.0));
        let expected: Vec<String> = (0..LOG_LINES).map(|i| format!("line {i}")).collect();
        assert_eq!(log.tail(), expected.join(" | "));

        let log = Log::default();
        log.push("Input #0, matroska,webm, from 'http://127.0.0.1:5/s/1?t=AbC-12_x':");
        assert_eq!(
            log.tail(),
            "Input #0, matroska,webm, from 'http://127.0.0.1:5/s/1?t=<redacted>':"
        );
        assert_eq!(
            redact("from 'http://127.0.0.1:5/s/1?t=AbC-12_x': at=3 &t=zz"),
            "from 'http://127.0.0.1:5/s/1?t=<redacted>': at=3 &t=<redacted>"
        );
    }
}
