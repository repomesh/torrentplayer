// SPDX-License-Identifier: MIT OR Apache-2.0

//! IPC commands exposed to the webview.
//!
//! Each command must also be listed in `build.rs` and granted in a capability file, otherwise
//! Tauri rejects calls to it. Every argument is validated here; the webview is not trusted.
//! The contract is documented in `docs/ipc.md`.

use lumen_prioritize::{Anchor, Purpose};
use lumen_stream::StreamId;
use lumen_torrent::{FileKind, TorrentInfo, TorrentSource, TorrentState, magnet_link};
use serde::{Deserialize, Serialize};
use tauri::State;
use tauri::ipc::{InvokeBody, Request};
use ts_rs::TS;

use crate::audio::TrackProbe;
use crate::core::{ActiveStream, AppState, Core, PlayerMetrics};
use crate::error::IpcError;

/// Longest a torrent may take to load (link resolved, metadata found, added). A dead magnet
/// otherwise waits minutes for peers, and the window stays busy, so nothing else can be
/// queued or played until it gives up.
const ADD_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);
/// Longest starting a stream may take once its torrent has loaded.
const START_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// Runs `work` for at most `limit`, reporting a timeout as "no peers sharing it".
async fn capped<T>(
    limit: std::time::Duration,
    work: impl Future<Output = Result<T, IpcError>>,
) -> Result<T, IpcError> {
    tokio::time::timeout(limit, work)
        .await
        .map_err(|_| IpcError::from(lumen_torrent::EngineError::MetadataTimeout))?
}

/// Number of cells in the piece map sent to the UI.
const PIECE_MAP_CELLS: usize = 200;
/// Longest plausible media duration (about 11 days); anything longer is rejected as bogus.
const MAX_DURATION_SECS: f64 = 1_000_000.0;
/// Longest plausible player timing measurement (10 minutes).
const MAX_METRIC_MS: f64 = 600_000.0;

/// Static information about the running app, shown in the About screen.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct AppInfo {
    /// Product name.
    pub name: String,
    /// Semantic version of this build.
    pub version: String,
    /// SPDX license expression for TorrentStream's own code.
    pub license: String,
}

impl AppInfo {
    /// Returns the information for the current build.
    #[must_use]
    pub fn current() -> Self {
        Self {
            name: "TorrentStream".to_owned(),
            version: env!("CARGO_PKG_VERSION").to_owned(),
            license: env!("CARGO_PKG_LICENSE").to_owned(),
        }
    }
}

/// Returns name, version, and license information for the About screen.
#[tauri::command]
#[must_use]
pub(crate) fn app_info() -> AppInfo {
    AppInfo::current()
}

/// Kind of file, by extension.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub(crate) enum FileKindDto {
    /// A video container.
    Video,
    /// Music, podcasts, audiobooks.
    Audio,
    /// A subtitle file.
    Subtitle,
    /// Notes, slides, and other documents.
    Document,
    /// Anything else.
    Other,
}

impl From<FileKind> for FileKindDto {
    fn from(kind: FileKind) -> Self {
        match kind {
            FileKind::Video => Self::Video,
            FileKind::Audio => Self::Audio,
            FileKind::Subtitle => Self::Subtitle,
            FileKind::Document => Self::Document,
            FileKind::Other => Self::Other,
        }
    }
}

/// One file in a torrent.
#[derive(Debug, Clone, Serialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub(crate) struct FileSummary {
    /// Index used to start a stream.
    pub index: usize,
    /// Path inside the torrent.
    pub path: String,
    /// Size in bytes.
    pub length: u64,
    /// Kind of file.
    pub kind: FileKindDto,
}

/// An added torrent.
#[derive(Debug, Clone, Serialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub(crate) struct TorrentSummary {
    /// Torrent id.
    pub id: usize,
    /// Display name.
    pub name: String,
    /// Hex info hash.
    pub info_hash: String,
    /// Total size in bytes.
    pub total_length: u64,
    /// Files, with videos first.
    pub files: Vec<FileSummary>,
    /// A magnet link that adds this torrent again: info hash, name, and (for torrents opened
    /// from a `.torrent` file) up to 20 of its trackers. The play queue saves it.
    pub magnet: String,
}

impl TorrentSummary {
    /// The summary of an added torrent, with its trackers in `magnet` when the engine has them.
    fn of(core: &Core, info: TorrentInfo) -> Self {
        let magnet = core.engine.magnet_link(info.id).ok();
        let mut summary = Self::from(info);
        if let Some(magnet) = magnet {
            summary.magnet = magnet;
        }
        summary
    }
}

impl From<TorrentInfo> for TorrentSummary {
    fn from(info: TorrentInfo) -> Self {
        let mut files: Vec<FileSummary> = info
            .files
            .into_iter()
            .map(|f| FileSummary {
                index: f.index,
                path: f.path,
                length: f.len,
                kind: f.kind.into(),
            })
            .collect();
        files.sort_by_key(|f| (f.kind != FileKindDto::Video, f.path.clone()));
        Self {
            id: info.id,
            magnet: magnet_link(&info.info_hash, &info.name, &[]),
            name: info.name,
            info_hash: info.info_hash,
            total_length: info.total_len,
            files,
        }
    }
}

/// Adds a torrent from a magnet link or an http(s) link to a `.torrent` file.
#[tauri::command]
pub(crate) async fn add_torrent_link(
    state: State<'_, AppState>,
    search: State<'_, crate::search::SearchState>,
    downloads: State<'_, crate::downloads::DownloadsState>,
    settings: State<'_, crate::settings::SettingsState>,
    link: String,
) -> Result<TorrentSummary, IpcError> {
    let _cache_gate = downloads.cache_gate().lock().await;
    let source = TorrentSource::parse_link(&link)?;
    let core = state.core().await?;
    // The search service counts its download links like searches.
    search.pace(&link).await;
    // Download links of search sites often redirect to a magnet link.
    let info = capped(ADD_TIMEOUT, async {
        let source = core.engine.resolve_source(source).await?;
        Ok(core.engine.add(source).await?)
    })
    .await?;
    crate::cache::mark_unfinished(&settings.active_download_dir(), &info.info_hash);
    Ok(TorrentSummary::of(core, info))
}

/// Adds a torrent from the raw bytes of a `.torrent` file (sent as the request body).
#[tauri::command]
pub(crate) async fn add_torrent_file(
    state: State<'_, AppState>,
    downloads: State<'_, crate::downloads::DownloadsState>,
    settings: State<'_, crate::settings::SettingsState>,
    request: Request<'_>,
) -> Result<TorrentSummary, IpcError> {
    let _cache_gate = downloads.cache_gate().lock().await;
    let InvokeBody::Raw(bytes) = request.body() else {
        return Err(IpcError::invalid(
            "Expected the .torrent file as raw bytes.",
        ));
    };
    let source = TorrentSource::from_file_bytes(bytes.clone())?;
    let core = state.core().await?;
    let info = capped(ADD_TIMEOUT, async { Ok(core.engine.add(source).await?) }).await?;
    crate::cache::mark_unfinished(&settings.active_download_dir(), &info.info_hash);
    Ok(TorrentSummary::of(core, info))
}

/// A stream the player can open.
#[derive(Debug, Clone, Serialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub(crate) struct StreamInfo {
    /// Stream id, for reporting playback and stopping.
    pub stream_id: u64,
    /// Loopback URL including the session token. Do not log or persist it.
    pub url: String,
    /// File name, for display.
    pub file_name: String,
    /// Size in bytes.
    pub length: u64,
}

/// Starts streaming one video file: selects it for download, starts prioritization, and
/// publishes it on the loopback server.
#[tauri::command]
pub(crate) async fn start_stream(
    state: State<'_, AppState>,
    downloads: State<'_, crate::downloads::DownloadsState>,
    settings: State<'_, crate::settings::SettingsState>,
    torrent_id: usize,
    file_index: usize,
) -> Result<StreamInfo, IpcError> {
    let _cache_gate = downloads.cache_gate().lock().await;
    let core = state.core().await?;
    let info = core.engine.info(torrent_id)?;
    let entry = info
        .files
        .iter()
        .find(|f| f.index == file_index)
        .ok_or_else(|| IpcError::new(crate::error::ErrorKind::NotFound, "Unknown file."))?;
    if !entry.kind.is_playable() {
        return Err(IpcError::invalid(
            "Only video and audio files can be streamed.",
        ));
    }
    crate::cache::mark_unfinished(&settings.active_download_dir(), &info.info_hash);
    // The queue downloader may have paused or limited this torrent in the background; what is
    // watched always gets the whole link.
    core.engine.set_paused(torrent_id, false).await?;
    core.engine.set_download_limit(torrent_id, None)?;
    // Dropping the half-started session on timeout cancels its playback.
    let (session, media) = capped(START_TIMEOUT, async {
        let session = core
            .engine
            .start_playback(torrent_id, file_index, lumen_prioritize::Config::default())
            .await?;
        let media = core.engine.media_source(torrent_id, file_index).await?;
        Ok((session, media))
    })
    .await?;
    let engine = session.backend();
    let published = core.server.publish(std::sync::Arc::clone(&media));
    // Reads the container header alongside the player, so the player can say if the audio
    // will be silent and list embedded subtitles.
    let tracks = TrackProbe::start(published.id, media);
    core.streams
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(
            published.id,
            ActiveStream {
                torrent: torrent_id,
                file: file_index,
                session,
                metrics: PlayerMetrics::default(),
                tracks,
            },
        );
    tracing::info!(
        stream = published.id,
        torrent = torrent_id,
        file = file_index,
        %engine,
        "stream started"
    );
    Ok(StreamInfo {
        stream_id: published.id,
        url: published.url,
        file_name: entry
            .path
            .rsplit('/')
            .next()
            .unwrap_or(&entry.path)
            .to_owned(),
        length: entry.len,
    })
}

/// Reports the player's position. Call on every seek and about once a second during playback.
#[tauri::command]
pub(crate) async fn report_playback(
    state: State<'_, AppState>,
    settings: State<'_, crate::settings::SettingsState>,
    stream_id: u64,
    position_secs: f64,
    duration_secs: Option<f64>,
) -> Result<(), IpcError> {
    if !position_secs.is_finite() || !(0.0..=MAX_DURATION_SECS).contains(&position_secs) {
        return Err(IpcError::invalid(
            "positionSecs must be a non-negative number.",
        ));
    }
    let duration_secs =
        duration_secs.filter(|d| d.is_finite() && *d > 0.0 && *d <= MAX_DURATION_SECS);
    let core = state.core().await?;
    let torrent = core.with_stream(stream_id, |s| {
        s.session.update(position_secs, duration_secs);
        s.torrent
    })?;
    if let Some(duration) = duration_secs
        && let (Ok(info), Ok(backend)) = (core.engine.info(torrent), core.engine.backend(torrent))
    {
        let root = settings.active_download_dir();
        if position_secs >= duration {
            if let Err(error) = crate::cache::mark_finished(&root, backend, &info.info_hash) {
                tracing::warn!(%error, "could not mark a download as finished");
            }
        } else {
            crate::cache::mark_unfinished(&root, &info.info_hash);
        }
    }
    Ok(())
}

/// A decoder-level timing measured by the player.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub(crate) enum PlayerMetricKind {
    /// From pressing play to the first decoded frame.
    FirstFrame,
    /// From a seek to the first decoded frame at the new position.
    Seek,
}

/// Records a timing measured by the player, for the debug overlay and the log.
#[tauri::command]
pub(crate) async fn record_player_metric(
    state: State<'_, AppState>,
    stream_id: u64,
    kind: PlayerMetricKind,
    millis: f64,
) -> Result<(), IpcError> {
    if !millis.is_finite() || !(0.0..=MAX_METRIC_MS).contains(&millis) {
        return Err(IpcError::invalid("millis is out of range."));
    }
    let core = state.core().await?;
    core.with_stream(stream_id, |s| match kind {
        PlayerMetricKind::FirstFrame => s.metrics.first_frame_ms = Some(millis),
        PlayerMetricKind::Seek => {
            s.metrics.last_seek_ms = Some(millis);
            s.metrics.seeks += 1;
        }
    })?;
    tracing::info!(stream = stream_id, ?kind, millis, "player metric");
    Ok(())
}

/// Why an anchor exists (mirrors `lumen_prioritize::Purpose`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub(crate) enum AnchorPurpose {
    /// Data the player needs next.
    Playhead,
    /// Container metadata at the start of the file.
    ContainerHead,
    /// Container index at the end of the file.
    ContainerTail,
    /// The rest of the read-ahead window.
    ReadAhead,
    /// Start of the next lecture.
    NextLectureHead,
    /// End of the next lecture.
    NextLectureTail,
}

impl From<Purpose> for AnchorPurpose {
    fn from(p: Purpose) -> Self {
        match p {
            Purpose::Playhead => Self::Playhead,
            Purpose::ContainerHead => Self::ContainerHead,
            Purpose::ContainerTail => Self::ContainerTail,
            Purpose::ReadAhead => Self::ReadAhead,
            Purpose::NextLectureHead => Self::NextLectureHead,
            Purpose::NextLectureTail => Self::NextLectureTail,
        }
    }
}

/// A prioritized position, for the debug overlay.
#[derive(Debug, Clone, Serialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub(crate) struct AnchorDto {
    /// Byte offset within the file.
    pub offset: u64,
    /// Why it is prioritized.
    pub purpose: AnchorPurpose,
}

impl From<&Anchor> for AnchorDto {
    fn from(a: &Anchor) -> Self {
        Self {
            offset: a.offset,
            purpose: a.purpose.into(),
        }
    }
}

/// Live statistics for a stream, polled about once a second.
#[derive(Debug, Clone, Serialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub(crate) struct StreamStats {
    /// `initializing`, `live`, `paused`, or `error`.
    pub state: String,
    /// Download rate, bytes per second.
    pub download_bps: u64,
    /// Upload rate, bytes per second.
    pub upload_bps: u64,
    /// Connected peers.
    pub peers_live: u32,
    /// Peers being connected to.
    pub peers_connecting: u32,
    /// Peers learned about.
    pub peers_seen: u32,
    /// File size in bytes.
    pub file_length: u64,
    /// Downloaded and verified bytes of this file.
    pub file_downloaded: u64,
    /// Playhead as a byte offset.
    pub position_bytes: u64,
    /// Contiguous downloaded bytes after the playhead.
    pub buffered_bytes: u64,
    /// The same, in seconds of media, once the bitrate is known.
    pub buffered_secs: Option<f64>,
    /// Prioritized positions, highest priority first.
    pub anchors: Vec<AnchorDto>,
    /// The file's pieces in [`PIECE_MAP_CELLS`] cells, each 0 (missing) to 255 (complete).
    pub piece_map: Vec<u8>,
    /// How long the last request waited for its first bytes on the stream server.
    pub server_first_byte_ms: Option<f64>,
    /// From pressing play to the first decoded frame, as reported by the player.
    pub first_frame_ms: Option<f64>,
    /// From the last seek to the first decoded frame, as reported by the player.
    pub last_seek_ms: Option<f64>,
    /// Number of seeks measured.
    pub seeks: u32,
    /// Engine serving the stream: `lumen-swarm` or `librqbit`.
    pub engine: Option<String>,
}

/// Returns live statistics for a stream.
#[tauri::command]
pub(crate) async fn stream_stats(
    state: State<'_, AppState>,
    stream_id: u64,
) -> Result<StreamStats, IpcError> {
    let core = state.core().await?;
    let (torrent, file) = core.with_stream(stream_id, |s| (s.torrent, s.file))?;
    let torrent_stats = core.engine.stats(torrent).await?;
    let (geometry, span) = core.engine.file_geometry(torrent, file)?;
    // The piece map is only available once the torrent is initialized.
    let have = core.engine.have_pieces(torrent).await.unwrap_or_default();
    let (status, metrics, engine) = core.with_stream(stream_id, |s| {
        (s.session.status(&have), s.metrics, s.session.backend())
    })?;
    let pieces =
        geometry.pieces_overlapping(span.offset_in_torrent..span.offset_in_torrent + span.len);
    let piece_map = piece_map(&have, pieces, PIECE_MAP_CELLS);

    Ok(StreamStats {
        state: match torrent_stats.state {
            TorrentState::Initializing => "initializing",
            TorrentState::Live => "live",
            TorrentState::Paused => "paused",
            TorrentState::Error => "error",
        }
        .to_owned(),
        download_bps: torrent_stats.download_bps,
        upload_bps: torrent_stats.upload_bps,
        peers_live: torrent_stats.peers_live,
        peers_connecting: torrent_stats.peers_connecting,
        peers_seen: torrent_stats.peers_seen,
        file_length: span.len,
        file_downloaded: torrent_stats.file_progress.get(file).copied().unwrap_or(0),
        position_bytes: status.position_bytes,
        buffered_bytes: status.buffered_bytes,
        buffered_secs: status.buffered_secs(),
        anchors: status.anchors.iter().map(AnchorDto::from).collect(),
        piece_map,
        server_first_byte_ms: core.first_bytes.last_ms(stream_id),
        first_frame_ms: metrics.first_frame_ms,
        last_seek_ms: metrics.last_seek_ms,
        seeks: metrics.seeks,
        engine: Some(engine.name().to_owned()),
    })
}

/// Downsamples the pieces in `range` into `cells` values of 0–255 (fraction downloaded).
fn piece_map(have: &[bool], range: std::ops::Range<u64>, cells: usize) -> Vec<u8> {
    let start = usize::try_from(range.start).unwrap_or(usize::MAX);
    let end = usize::try_from(range.end).unwrap_or(usize::MAX);
    let pieces = have.get(start..end.min(have.len())).unwrap_or_default();
    if pieces.is_empty() {
        return vec![0; cells.min(1)];
    }
    let cells = cells.min(pieces.len()).max(1);
    (0..cells)
        .map(|c| {
            let lo = c * pieces.len() / cells;
            let hi = ((c + 1) * pieces.len() / cells).max(lo + 1);
            let done = pieces[lo..hi].iter().filter(|h| **h).count();
            u8::try_from(done * 255 / (hi - lo)).unwrap_or(u8::MAX)
        })
        .collect()
}

/// Stops a stream: unpublishes it and ends prioritization. Downloaded data is kept.
#[tauri::command]
pub(crate) async fn stop_stream(
    state: State<'_, AppState>,
    stream_id: StreamId,
) -> Result<(), IpcError> {
    let core = state.core().await?;
    core.server.unpublish(stream_id);
    core.first_bytes.forget(stream_id);
    let removed = core
        .streams
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .remove(&stream_id);
    if removed.is_none() {
        return Err(IpcError::new(
            crate::error::ErrorKind::NotFound,
            "Unknown stream.",
        ));
    }
    Ok(())
}

/// Development-only automation: a torrent to add and play on start-up, from the
/// `LUMEN_DEV_AUTOPLAY` environment variable (`<link or .torrent path>|<file index>|<seeks>`,
/// where seeks are comma-separated fractions). Used to measure real decoder timings.
#[derive(Debug, Clone, Serialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub(crate) struct DevAutoplay {
    /// The added torrent.
    pub torrent: TorrentSummary,
    /// File to play.
    pub file_index: usize,
    /// Seek positions, as fractions of the duration.
    pub seeks: Vec<f64>,
}

/// Returns the development autoplay request, if any. Always `None` in release builds.
#[tauri::command]
pub(crate) async fn dev_autoplay(
    state: State<'_, AppState>,
) -> Result<Option<DevAutoplay>, IpcError> {
    if !cfg!(debug_assertions) {
        return Ok(None);
    }
    let Ok(spec) = std::env::var("LUMEN_DEV_AUTOPLAY") else {
        return Ok(None);
    };
    let mut parts = spec.split('|');
    let input = parts.next().unwrap_or_default();
    let file_index = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
    let seeks = parts
        .next()
        .map(|s| s.split(',').filter_map(|f| f.trim().parse().ok()).collect())
        .unwrap_or_default();
    let source = if std::path::Path::new(input).is_file() {
        let bytes = tokio::fs::read(input)
            .await
            .map_err(|e| IpcError::internal(&e))?;
        TorrentSource::from_file_bytes(bytes)?
    } else {
        TorrentSource::parse_link(input)?
    };
    let core = state.core().await?;
    let info = core.engine.add(source).await?;
    let torrent = TorrentSummary::of(core, info);
    Ok(Some(DevAutoplay {
        torrent,
        file_index,
        seeks,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn app_info_matches_package_metadata() {
        let info = AppInfo::current();
        assert_eq!(info.name, "TorrentStream");
        assert_eq!(info.version, env!("CARGO_PKG_VERSION"));
        assert_eq!(info.license, "MIT OR Apache-2.0");
    }

    #[test]
    fn app_info_serializes_as_camel_case_json() {
        let json = serde_json::to_value(AppInfo::current()).unwrap();
        let mut keys: Vec<_> = json.as_object().unwrap().keys().cloned().collect();
        keys.sort();
        assert_eq!(keys, ["license", "name", "version"]);
    }

    #[test]
    fn piece_map_downsamples_and_scales() {
        let have = [true, true, false, false, true, false, true, true];
        assert_eq!(piece_map(&have, 0..8, 4), vec![255, 0, 127, 255]);
        assert_eq!(
            piece_map(&have, 0..8, 100),
            vec![255, 255, 0, 0, 255, 0, 255, 255]
        );
        assert_eq!(piece_map(&have, 2..4, 10), vec![0, 0]);
        assert_eq!(piece_map(&have, 20..30, 10), vec![0]);
        assert_eq!(piece_map(&[], 0..0, 10), vec![0]);
    }
}
