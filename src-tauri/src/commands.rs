// SPDX-License-Identifier: MIT OR Apache-2.0

//! IPC commands exposed to the webview.
//!
//! Each command must also be listed in `build.rs` and granted in a capability file, otherwise
//! Tauri rejects calls to it. Every argument is validated here; the webview is not trusted.
//! The contract is documented in `docs/ipc.md`.

use serde::{Deserialize, Serialize};
use tauri::ipc::{InvokeBody, Request};
use tauri::{AppHandle, Manager as _, State};
use torrentplayer_prioritize::{Anchor, Config as PrioritizeConfig, Purpose};
use torrentplayer_stream::StreamId;
use torrentplayer_torrent::{FileKind, TorrentInfo, TorrentSource, TorrentState, magnet_link};
use ts_rs::TS;

use crate::audio::TrackProbe;
use crate::core::{ActiveStream, AppState, Core, PlayerMetrics, ProgressTracker};
use crate::error::IpcError;
use crate::pieces::{PIECE_MAP_CELLS, piece_map};

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
        .map_err(|_| IpcError::from(torrentplayer_torrent::EngineError::MetadataTimeout))?
}

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
    /// SPDX license expression for TorrentPlayer's own code.
    pub license: String,
}

impl AppInfo {
    /// Returns the information for the current build.
    #[must_use]
    pub fn current() -> Self {
        Self {
            name: "TorrentPlayer".to_owned(),
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
    /// Saved position worth offering to resume, if the preference is enabled.
    pub resume_position_secs: Option<f64>,
}

/// Starts streaming one video file: selects it for download, starts prioritization, and
/// publishes it on the loopback server.
#[tauri::command]
#[allow(
    clippy::too_many_arguments,
    reason = "Tauri injects the application states used to start and account for a stream"
)]
pub(crate) async fn start_stream(
    app: AppHandle,
    state: State<'_, AppState>,
    downloads: State<'_, crate::downloads::DownloadsState>,
    library: State<'_, crate::library::LibraryState>,
    settings: State<'_, crate::settings::SettingsState>,
    media_tools: State<'_, crate::media::MediaToolsState>,
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
    let file_name = entry
        .path
        .rsplit('/')
        .next()
        .unwrap_or(&entry.path)
        .to_owned();
    let library_media = torrentplayer_library::Media {
        info_hash: info.info_hash.to_ascii_lowercase(),
        file_index: u32::try_from(file_index)
            .map_err(|_| IpcError::invalid("fileIndex is out of range."))?,
        source: core
            .engine
            .magnet_link(torrent_id)
            .unwrap_or_else(|_| magnet_link(&info.info_hash, &info.name, &[])),
        title: library_text(&file_name),
        torrent_name: library_text(&info.name),
        size_bytes: i64::try_from(entry.len).ok().map(|_| entry.len),
    };
    // Locate optional tools before publishing the stream. Awaiting after `publish` would leave
    // an orphaned source if this command were cancelled while the lookup was in flight.
    let tools = media_tools.tools(&settings).await;
    crate::cache::mark_unfinished(&settings.active_download_dir(), &info.info_hash);
    // The queue downloader may have paused or limited this torrent in the background; what is
    // watched always gets the whole link.
    core.engine.set_paused(torrent_id, false).await?;
    core.engine.set_download_limit(torrent_id, None)?;
    // Dropping the half-started session on timeout cancels its playback.
    let (session, media) = capped(START_TIMEOUT, async {
        let session = core
            .engine
            .start_playback(torrent_id, file_index, PrioritizeConfig::default())
            .await?;
        let media = core.engine.media_source(torrent_id, file_index).await?;
        Ok((session, media))
    })
    .await?;
    let engine = session.backend();
    let published = core.server.publish(std::sync::Arc::clone(&media));
    core.first_bytes.watch(published.id);
    // Reads the container header alongside the player, so the player can say if the audio
    // will be silent and list embedded subtitles.
    let tracks = TrackProbe::start(published.id, std::sync::Arc::clone(&media));
    let duration = crate::media::DurationProbe::start(
        std::sync::Arc::downgrade(core),
        tools,
        (published.id, torrent_id, file_index),
        tracks.subscribe(),
    );
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
                media: library_media.clone(),
                progress: ProgressTracker::default(),
                tracks,
                duration,
                source: media,
                converter: None,
                conversion_request: crate::latest::Latest::default(),
            },
        );
    tracing::info!(
        stream = published.id,
        torrent = torrent_id,
        file = file_index,
        %engine,
        "stream started"
    );
    let resume_position_secs = match library.opened(&app, library_media).await {
        Ok(item) if settings.current().playback.resume_position => {
            torrentplayer_library::resume_position(&item)
        }
        Ok(_) => None,
        Err(error) => {
            tracing::warn!(%error, "could not add the stream to playback history");
            None
        }
    };
    Ok(StreamInfo {
        stream_id: published.id,
        url: published.url,
        file_name,
        length: entry.len,
        resume_position_secs,
    })
}

/// Reports the player's position. Call on every seek and about once a second during playback.
#[tauri::command]
pub(crate) async fn report_playback(
    app: AppHandle,
    state: State<'_, AppState>,
    library: State<'_, crate::library::LibraryState>,
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
    // The player knows the duration of what it plays; for the rest (a Matroska file without
    // one, a format it cannot open), the header or `ffprobe` may.
    let (torrent, duration_secs, progress) = core.with_stream(stream_id, |s| {
        let duration_secs = duration_secs.or_else(|| s.duration.duration_secs());
        s.session.update(position_secs, duration_secs);
        let save = s.progress.update(position_secs, duration_secs);
        (
            s.torrent,
            duration_secs,
            save.then(|| {
                (
                    s.media.clone(),
                    s.progress.duration_secs,
                    library.revision(),
                )
            }),
        )
    })?;
    if let Some((media, known_duration, revision)) = progress
        && let Err(error) = library
            .record(&app, media, position_secs, known_duration, revision)
            .await
    {
        tracing::warn!(%error, "could not save playback progress");
    }
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

/// Why an anchor exists (mirrors `torrentplayer_prioritize::Purpose`).
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
    /// Engine serving the stream: `torrentplayer-swarm` or `librqbit`.
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

/// Stops a stream: unpublishes it and ends prioritization. Downloaded data is kept.
#[tauri::command]
pub(crate) async fn stop_stream(
    app: AppHandle,
    state: State<'_, AppState>,
    library: State<'_, crate::library::LibraryState>,
    stream_id: StreamId,
) -> Result<(), IpcError> {
    let core = state.core().await?;
    let Some(removed) = core.close_stream(stream_id) else {
        return Err(IpcError::new(
            crate::error::ErrorKind::NotFound,
            "Unknown stream.",
        ));
    };
    if let Some(position) = removed.progress.position_secs
        && let Err(error) = library
            .record(
                &app,
                removed.media,
                position,
                removed.progress.duration_secs,
                library.revision(),
            )
            .await
    {
        tracing::warn!(%error, "could not save final playback progress");
    }
    Ok(())
}

/// Stops every stream after the page reloaded or its renderer crashed: the player that read them
/// is gone and nothing would stop them otherwise. Their final progress is saved, as by
/// [`stop_stream`].
pub(crate) fn stop_all_streams(app: &AppHandle) {
    let state = app.state::<AppState>();
    let Some(core) = state.started() else {
        return;
    };
    let removed = core.close_all_streams();
    if removed.is_empty() {
        return;
    }
    tracing::info!(
        streams = removed.len(),
        "stopped the streams of the previous page"
    );
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        let library = app.state::<crate::library::LibraryState>();
        for stream in removed {
            let Some(position) = stream.progress.position_secs else {
                continue;
            };
            let revision = library.revision();
            if let Err(error) = library
                .record(
                    &app,
                    stream.media.clone(),
                    position,
                    stream.progress.duration_secs,
                    revision,
                )
                .await
            {
                tracing::warn!(%error, "could not save final playback progress");
            }
        }
    });
}

/// The connection totals from the downloader's latest snapshot, for the status bar, without
/// sending every queue item (`queue_downloads`). Never starts the engine.
#[tauri::command]
#[allow(
    clippy::needless_pass_by_value,
    reason = "Tauri passes managed state by value"
)]
pub(crate) fn link_use(
    downloads: State<'_, crate::downloads::DownloadsState>,
) -> crate::downloads::LinkUse {
    downloads.link()
}

/// Plain single-line text for the library. Torrent metadata is untrusted and can be much longer
/// than a useful label.
fn library_text(value: &str) -> String {
    let clean: String = value
        .chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .collect();
    let trimmed = clean.trim();
    let mut chars = trimmed.chars();
    let clipped: String = chars
        .by_ref()
        .take(torrentplayer_library::MAX_TITLE_CHARS)
        .collect();
    if clipped.is_empty() {
        "Untitled".to_owned()
    } else {
        clipped
    }
}

/// Development-only automation: a torrent to add and play on start-up, from the
/// `TORRENTPLAYER_DEV_AUTOPLAY` environment variable (`<link or .torrent path>|<file index>|<seeks>|<peers>`,
/// where seeks are comma-separated fractions and the optional peers comma-separated addresses
/// to connect to, such as a local `torrentplayer-bench seed`). Used to measure real decoder timings and
/// to play test media.
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
    let Ok(spec) = std::env::var("TORRENTPLAYER_DEV_AUTOPLAY") else {
        return Ok(None);
    };
    let mut parts = spec.split('|');
    let input = parts.next().unwrap_or_default();
    let file_index = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
    let seeks = parts
        .next()
        .map(|s| s.split(',').filter_map(|f| f.trim().parse().ok()).collect())
        .unwrap_or_default();
    let peers = parts
        .next()
        .map(|s| s.split(',').filter_map(|p| p.trim().parse().ok()).collect())
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
    let info = core.engine.add_with_peers(source, peers).await?;
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
        assert_eq!(info.name, "TorrentPlayer");
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
    fn library_labels_are_plain_bounded_text() {
        assert_eq!(library_text("  Lecture\n2\t "), "Lecture 2");
        assert_eq!(library_text("\0\n"), "Untitled");
        assert_eq!(
            library_text(&"x".repeat(torrentplayer_library::MAX_TITLE_CHARS + 5))
                .chars()
                .count(),
            torrentplayer_library::MAX_TITLE_CHARS
        );
    }
}
