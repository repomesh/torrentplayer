// SPDX-License-Identifier: MIT OR Apache-2.0

//! `ffprobe` and `ffmpeg` against media served by the loopback stream server. Skipped (with a
//! note) when FFmpeg is not installed: set `TORRENTPLAYER_FFMPEG_DIR` or put it on `PATH`.

// Test code: panicking on failure is the point, and a skip is reported on the console.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::print_stderr
)]

use std::io::{self, Cursor, SeekFrom};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use futures_util::future::BoxFuture;
use tokio::io::{AsyncRead, AsyncSeek, ReadBuf};
use torrentplayer_media::{FfmpegError, FfmpegTools};
use torrentplayer_stream::{MediaReader, MediaSource, ServerConfig, StreamServer};

const TIMEOUT: Duration = Duration::from_secs(20);

fn tools() -> Option<FfmpegTools> {
    let dir = std::env::var_os("TORRENTPLAYER_FFMPEG_DIR").map(PathBuf::from);
    let tools = FfmpegTools::locate(dir.as_deref());
    if tools.ffprobe.is_none() || tools.ffmpeg.is_none() {
        eprintln!("FFmpeg not found; skipping (set TORRENTPLAYER_FFMPEG_DIR)");
        return None;
    }
    Some(tools)
}

/// Ten seconds of a test pattern with sound, encoded with FFmpeg's own (LGPL) encoders.
fn test_video(ffmpeg: &Path, dir: &Path, name: &str) -> Vec<u8> {
    let out = dir.join(name);
    let status = std::process::Command::new(ffmpeg)
        .args(["-hide_banner", "-loglevel", "error", "-f", "lavfi", "-i"])
        .arg("testsrc2=size=320x240:rate=25:duration=10")
        .args(["-f", "lavfi", "-i", "sine=frequency=440:duration=10"])
        .args([
            "-c:v",
            "mpeg4",
            "-g",
            "25",
            "-q:v",
            "5",
            "-c:a",
            "aac",
            "-shortest",
        ])
        .arg(&out)
        .status()
        .unwrap();
    assert!(status.success());
    std::fs::read(out).unwrap()
}

/// A file in memory; reads inside `missing` fail as a stored-only torrent reader's do.
struct Memory {
    data: Arc<Vec<u8>>,
    missing: Range<u64>,
}

impl MediaSource for Memory {
    fn byte_len(&self) -> u64 {
        self.data.len() as u64
    }

    fn content_type(&self) -> &'static str {
        "video/mp4"
    }

    fn open(&self) -> BoxFuture<'static, io::Result<Box<dyn MediaReader>>> {
        let reader = Partial {
            inner: Cursor::new(Bytes(Arc::clone(&self.data))),
            missing: self.missing.clone(),
        };
        Box::pin(async move { Ok(Box::new(reader) as Box<dyn MediaReader>) })
    }
}

/// Shared bytes a `Cursor` can read.
struct Bytes(Arc<Vec<u8>>);

impl AsRef<[u8]> for Bytes {
    fn as_ref(&self) -> &[u8] {
        &self.0
    }
}

struct Partial {
    inner: Cursor<Bytes>,
    missing: Range<u64>,
}

impl AsyncRead for Partial {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let pos = self.inner.position();
        if self.missing.contains(&pos) {
            return Poll::Ready(Err(io::ErrorKind::WouldBlock.into()));
        }
        // Stop at the gap, so the next read hits it.
        let room = if pos < self.missing.start {
            usize::try_from(self.missing.start - pos).unwrap_or(usize::MAX)
        } else {
            usize::MAX
        };
        let mut chunk = vec![0; room.min(buf.remaining())];
        let mut limited = ReadBuf::new(&mut chunk);
        let poll = Pin::new(&mut self.inner).poll_read(cx, &mut limited);
        buf.put_slice(limited.filled());
        poll
    }
}

impl AsyncSeek for Partial {
    fn start_seek(mut self: Pin<&mut Self>, position: SeekFrom) -> io::Result<()> {
        Pin::new(&mut self.inner).start_seek(position)
    }

    fn poll_complete(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<u64>> {
        Pin::new(&mut self.inner).poll_complete(cx)
    }
}

fn serve(server: &StreamServer, data: &Arc<Vec<u8>>, missing: Range<u64>) -> String {
    let source = Memory {
        data: Arc::clone(data),
        missing,
    };
    server.publish_private(Arc::new(source)).unwrap().url
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn probes_and_takes_thumbnails_through_the_stream_server() {
    let Some(tools) = tools() else { return };
    let dir = tempfile::tempdir().unwrap();
    let server = StreamServer::start(ServerConfig::default()).await.unwrap();

    for name in ["clip.mp4", "clip.mkv"] {
        let data = Arc::new(test_video(tools.ffmpeg.as_ref().unwrap(), dir.path(), name));
        let url = serve(&server, &data, 0..0);

        let info = tools.probe(&url, TIMEOUT).await.unwrap();
        let ms = info.duration_ms.unwrap();
        assert!((9_900..=10_100).contains(&ms), "{name}: {ms} ms");
        let rate = info.byte_rate(data.len() as u64).unwrap();
        let average = data.len() as u64 * 1000 / ms;
        assert!(
            rate.abs_diff(average) * 10 < average,
            "{name}: {rate} vs {average}"
        );

        let jpeg = tools.thumbnail(&url, 5.0, 160, TIMEOUT).await.unwrap();
        assert!(
            jpeg.starts_with(&[0xFF, 0xD8]) && jpeg.len() > 500,
            "{name}"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_missing_part_fails_fast() {
    let Some(tools) = tools() else { return };
    let dir = tempfile::tempdir().unwrap();
    let server = StreamServer::start(ServerConfig::default()).await.unwrap();
    let data = Arc::new(test_video(
        tools.ffmpeg.as_ref().unwrap(),
        dir.path(),
        "clip.mkv",
    ));
    let len = data.len() as u64;
    // Start and end downloaded, the middle missing.
    let url = serve(&server, &data, len / 5..len * 9 / 10);

    let jpeg = tools.thumbnail(&url, 0.5, 160, TIMEOUT).await.unwrap();
    assert!(jpeg.starts_with(&[0xFF, 0xD8]));
    let started = Instant::now();
    let error = tools.thumbnail(&url, 5.0, 160, TIMEOUT).await.unwrap_err();
    assert!(
        matches!(error, FfmpegError::Failed { .. } | FfmpegError::Output(_)),
        "{error}"
    );
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "{:?}",
        started.elapsed()
    );
    assert!(!error.to_string().contains("?t="), "{error}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn playlists_are_refused() {
    let Some(tools) = tools() else { return };
    let server = StreamServer::start(ServerConfig::default()).await.unwrap();
    // A playlist that would make FFmpeg read a local file and another host.
    let playlist = "#EXTM3U\n#EXT-X-TARGETDURATION:10\n#EXTINF:10,\nfile:///C:/Windows/win.ini\n\
                    #EXTINF:10,\nhttp://192.0.2.1/segment.ts\n#EXT-X-ENDLIST\n";
    let data = Arc::new(playlist.as_bytes().to_vec());
    let url = serve(&server, &data, 0..0);
    let started = Instant::now();
    let error = tools.probe(&url, TIMEOUT).await.unwrap_err();
    assert!(matches!(error, FfmpegError::Failed { .. }), "{error}");
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "{:?}",
        started.elapsed()
    );

    let concat = b"ffconcat version 1.0\nfile 'http://192.0.2.1/a.mp4'\n".to_vec();
    let url = serve(&server, &Arc::new(concat), 0..0);
    let error = tools.probe(&url, TIMEOUT).await.unwrap_err();
    assert!(matches!(error, FfmpegError::Failed { .. }), "{error}");
}
