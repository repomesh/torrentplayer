// SPDX-License-Identifier: MIT OR Apache-2.0

//! FFmpeg, when the user has it installed: the duration of streams whose container header gives
//! none (so the playhead and read-ahead work for them), and seek-bar thumbnails.
//!
//! `ffprobe` and `ffmpeg` read a stream through the loopback server, published for the one call
//! under a token of its own (`StreamServer::publish_private`; the URL is on their command line,
//! where other local programs can see it) and unpublished right after. The source serves only
//! what is already downloaded (`StreamEngine::stored_media`): reading it downloads nothing and
//! changes no priorities, so FFmpeg never competes with playback, and a part that is missing
//! fails at once. `torrentplayer_media::FfmpegTools` locks the programs down to that URL.

use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError, Weak};
use std::time::Duration;

use base64::Engine as _;
use serde::Serialize;
use tauri::State;
use tokio::sync::watch;
use torrentplayer_media::FfmpegTools;
use torrentplayer_stream::{StreamId, StreamServer};
use torrentplayer_torrent::TorrentId;
use ts_rs::TS;

use crate::audio::Probed;
use crate::core::{AppState, Core};
use crate::error::IpcError;
use crate::latest::Latest;
use crate::settings::{SettingsState, SettingsView};

/// How long one `ffprobe` run may take.
const PROBE_TIMEOUT: Duration = Duration::from_secs(20);
/// `ffprobe` tries per stream: the start and end of the file may still be downloading.
const PROBE_ATTEMPTS: u32 = 6;
/// Pause between `ffprobe` tries.
const PROBE_RETRY: Duration = Duration::from_secs(5);
/// How long one thumbnail may take.
const THUMBNAIL_TIMEOUT: Duration = Duration::from_secs(8);
/// Thumbnail width in pixels (shown at 160 CSS pixels, sharp on high-DPI screens).
const THUMBNAIL_WIDTH: u32 = 320;
/// Longest plausible media duration, in seconds (as `report_playback` accepts).
const MAX_DURATION_SECS: f64 = 1_000_000.0;

/// Managed state: where FFmpeg is, and the thumbnail being made.
#[derive(Debug, Default)]
pub(crate) struct MediaToolsState {
    /// The FFmpeg folder setting the tools were found for, and the tools.
    located: Mutex<Option<(Option<String>, Arc<FfmpegTools>)>>,
    thumbnails: Latest,
}

impl MediaToolsState {
    /// The tools for the current settings, looked for again when the FFmpeg folder changes.
    pub(crate) async fn tools(&self, settings: &SettingsState) -> Arc<FfmpegTools> {
        let dir = settings.current().advanced.ffmpeg_dir;
        if let Some((found_for, tools)) = &*self.lock()
            && *found_for == dir
        {
            return Arc::clone(tools);
        }
        let search = dir.clone();
        let tools = Arc::new(
            tokio::task::spawn_blocking(move || {
                FfmpegTools::locate(search.as_deref().map(Path::new))
            })
            .await
            .unwrap_or_default(),
        );
        *self.lock() = Some((dir, Arc::clone(&tools)));
        tools
    }

    /// Forgets where the tools are, so the next use looks again (after an install).
    fn forget(&self) {
        *self.lock() = None;
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Option<(Option<String>, Arc<FfmpegTools>)>> {
        self.located.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// A stream's duration: from its container header, or else from `ffprobe`. Dropping it (when the
/// stream stops) stops the probe.
pub(crate) struct DurationProbe {
    /// Milliseconds; 0 while unknown.
    duration_ms: Arc<AtomicU64>,
    task: tauri::async_runtime::JoinHandle<()>,
}

impl DurationProbe {
    /// Waits for the header probe (`header`), then asks `ffprobe` if the header gave no duration.
    pub(crate) fn start(
        core: Weak<Core>,
        tools: Arc<FfmpegTools>,
        (stream, torrent, file): (StreamId, TorrentId, usize),
        mut header: watch::Receiver<Option<Probed>>,
    ) -> Self {
        let duration_ms = Arc::new(AtomicU64::new(0));
        let found = Arc::clone(&duration_ms);
        let task = tauri::async_runtime::spawn(async move {
            let from_header = header
                .wait_for(Option::is_some)
                .await
                .ok()
                .and_then(|probed| probed.as_ref().and_then(|p| p.duration_ms));
            let ms = match from_header {
                Some(ms) => Some(ms),
                None if tools.ffprobe.is_some() => {
                    probe_with_ffprobe(&core, &tools, stream, torrent, file).await
                }
                None => None,
            };
            if let Some(ms) = ms {
                found.store(ms, Ordering::Relaxed);
            }
        });
        Self { duration_ms, task }
    }

    /// The duration in seconds, once known.
    pub(crate) fn duration_secs(&self) -> Option<f64> {
        // Durations are at most `torrentplayer_media::MAX_DURATION_MS`, exact in an f64.
        #[allow(clippy::cast_precision_loss)]
        let secs = match self.duration_ms.load(Ordering::Relaxed) {
            0 => None,
            ms => Some(ms as f64 / 1000.0),
        };
        secs
    }
}

impl Drop for DurationProbe {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Runs `ffprobe` until it gives a duration, a few times while the header downloads.
async fn probe_with_ffprobe(
    core: &Weak<Core>,
    tools: &FfmpegTools,
    stream: StreamId,
    torrent: TorrentId,
    file: usize,
) -> Option<u64> {
    for attempt in 1..=PROBE_ATTEMPTS {
        if attempt > 1 {
            tokio::time::sleep(PROBE_RETRY).await;
        }
        let core = core.upgrade()?;
        let outcome = with_stored_url(&core, torrent, file, async |url| {
            tools
                .probe(url, PROBE_TIMEOUT)
                .await
                .map_err(|e| e.to_string())
        })
        .await;
        match outcome {
            Ok(info) => {
                tracing::info!(
                    stream,
                    duration_ms = info.duration_ms,
                    bit_rate = info.bit_rate,
                    "probed with ffprobe"
                );
                return info.duration_ms;
            }
            Err(error) => tracing::debug!(stream, attempt, %error, "ffprobe failed"),
        }
    }
    None
}

/// Unpublishes a stream when dropped, also when the call using it is cancelled.
struct Unpublish<'a>(&'a StreamServer, StreamId);

impl Drop for Unpublish<'_> {
    fn drop(&mut self) {
        self.0.unpublish(self.1);
    }
}

/// Runs `f` with a private URL of what is downloaded of a file (see the module docs).
async fn with_stored_url<T>(
    core: &Core,
    torrent: TorrentId,
    file: usize,
    f: impl AsyncFnOnce(&str) -> Result<T, String>,
) -> Result<T, String> {
    let media = core
        .engine
        .stored_media(torrent, file)
        .await
        .map_err(|e| e.to_string())?;
    let published = core
        .server
        .publish_private(media)
        .map_err(|e| e.to_string())?;
    let _unpublish = Unpublish(&core.server, published.id);
    f(&published.url).await
}

/// Whether the piece at `fraction` of a file is downloaded: a cheap check before starting
/// `ffmpeg`, which then reads only what is stored.
async fn downloaded_at(core: &Core, torrent: TorrentId, file: usize, fraction: f64) -> bool {
    let (Ok((geometry, span)), Ok(have)) = (
        core.engine.file_geometry(torrent, file),
        core.engine.have_pieces(torrent).await,
    ) else {
        return false;
    };
    // The fraction is in 0..=1, so the offset is within the file.
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_precision_loss
    )]
    let offset = ((fraction * span.len as f64) as u64).min(span.len.saturating_sub(1));
    let piece = geometry.piece_at(span.to_abs(offset));
    usize::try_from(piece)
        .ok()
        .and_then(|p| have.get(p))
        .copied()
        .unwrap_or(false)
}

/// A frame of the playing video near `positionSecs` (of `durationSecs`), for the seek bar, as
/// a `data:image/jpeg;base64,` URL. `null` when there is none to show: the
/// `playback.seekThumbnails` setting is off, FFmpeg is not installed, that part is not
/// downloaded yet, FFmpeg failed, or a newer request replaced this one.
#[tauri::command]
pub(crate) async fn seek_thumbnail(
    state: State<'_, MediaToolsState>,
    app: State<'_, AppState>,
    settings: State<'_, SettingsState>,
    stream_id: StreamId,
    position_secs: f64,
    duration_secs: f64,
) -> Result<Option<String>, IpcError> {
    let valid = |s: f64| s.is_finite() && (0.0..=MAX_DURATION_SECS).contains(&s);
    if !valid(position_secs) || !valid(duration_secs) || duration_secs == 0.0 {
        return Err(IpcError::invalid(
            "positionSecs and durationSecs must be valid times.",
        ));
    }
    let stopped = state.thumbnails.begin();
    if !settings.current().playback.seek_thumbnails {
        return Ok(None);
    }
    let tools = state.tools(&settings).await;
    if tools.ffmpeg.is_none() {
        return Ok(None);
    }
    let core = app.core().await?;
    let (torrent, file) = core.with_stream(stream_id, |s| (s.torrent, s.file))?;
    let position_secs = position_secs.min(duration_secs);
    let work = async {
        if !downloaded_at(core, torrent, file, position_secs / duration_secs).await {
            return Ok(None);
        }
        with_stored_url(core, torrent, file, async |url| {
            tools
                .thumbnail(url, position_secs, THUMBNAIL_WIDTH, THUMBNAIL_TIMEOUT)
                .await
                .map_err(|e| e.to_string())
        })
        .await
        .map(Some)
    };
    let jpeg = tokio::select! {
        () = stopped => return Ok(None),
        jpeg = work => jpeg,
    };
    Ok(match jpeg {
        Ok(jpeg) => jpeg.map(|jpeg| {
            format!(
                "data:image/jpeg;base64,{}",
                base64::engine::general_purpose::STANDARD.encode(jpeg)
            )
        }),
        Err(error) => {
            tracing::debug!(stream = stream_id, %error, "no seek thumbnail");
            None
        }
    })
}

/// One FFmpeg program, found.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub(crate) struct MediaToolDto {
    /// Where it is.
    pub path: String,
    /// The version it reports; `null` if it did not answer like FFmpeg.
    pub version: Option<String>,
}

/// Which FFmpeg programs were found, for the Settings page.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub(crate) struct MediaToolsDto {
    /// `ffprobe`: durations of videos whose header has none.
    pub ffprobe: Option<MediaToolDto>,
    /// `ffmpeg`: seek-bar thumbnails.
    pub ffmpeg: Option<MediaToolDto>,
}

/// Looks for FFmpeg again (in the chosen folder, or on `PATH`) and reports what was found.
#[tauri::command]
pub(crate) async fn media_tools(
    state: State<'_, MediaToolsState>,
    settings: State<'_, SettingsState>,
) -> Result<MediaToolsDto, IpcError> {
    state.forget();
    let tools = state.tools(&settings).await;
    let describe = async |path: Option<&Path>| match path {
        Some(path) => Some(MediaToolDto {
            path: path.to_string_lossy().into_owned(),
            version: torrentplayer_media::version(path).await,
        }),
        None => None,
    };
    let (ffprobe, ffmpeg) = tokio::join!(
        describe(tools.ffprobe.as_deref()),
        describe(tools.ffmpeg.as_deref())
    );
    Ok(MediaToolsDto { ffprobe, ffmpeg })
}

/// Opens the system folder picker for the FFmpeg folder and saves the choice as
/// `advanced.ffmpegDir` (checked like any change: a local folder outside the download folder).
/// Returns the saved settings, or `null` if cancelled. This is the only way to choose the
/// folder: the programs in it run, so the webview cannot name one.
#[tauri::command]
pub(crate) async fn choose_ffmpeg_dir(
    app: tauri::AppHandle,
    window: tauri::WebviewWindow,
    state: State<'_, MediaToolsState>,
    settings: State<'_, SettingsState>,
) -> Result<Option<SettingsView>, IpcError> {
    let chosen = rfd::AsyncFileDialog::new()
        .set_title("Choose the folder with ffprobe and ffmpeg")
        .set_parent(&window)
        .pick_folder()
        .await;
    let Some(folder) = chosen else {
        return Ok(None);
    };
    let folder = folder.path().to_string_lossy().into_owned();
    let patch = serde_json::json!({ "advanced": { "ffmpegDir": folder } });
    let view = settings.update(&app, &patch).await?;
    state.forget();
    Ok(Some(view))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tools_are_serialized_as_camel_case() {
        let dto = MediaToolsDto {
            ffprobe: Some(MediaToolDto {
                path: "/usr/bin/ffprobe".into(),
                version: Some("7.1".into()),
            }),
            ffmpeg: None,
        };
        let json = serde_json::to_value(dto).unwrap();
        assert_eq!(json["ffprobe"]["version"], "7.1");
        assert!(json["ffmpeg"].is_null());
    }
}
