// SPDX-License-Identifier: MIT OR Apache-2.0

//! The search commands: search public indexers through the emberglow API, and preload the best
//! result once it has settled (see [`crate::preload`]).
//!
//! `search_torrents_progressive` sends merged results over a channel as indexers answer (first
//! results in about half a second; the service streams them, see `torrentplayer_search`), at most
//! every [`PROGRESS_INTERVAL`]. Starting another search, or `cancel_search`, stops it.
//! `search_torrents` returns everything at once.

use std::time::Duration;

use serde::Serialize;
use tauri::ipc::Channel;
use tauri::{AppHandle, Manager as _, State};
use tokio::sync::{OnceCell, mpsc, watch};
use tokio::time::Instant;
use torrentplayer_search::{
    Codec, Container, ContentKind, EmberglowProvider, Playability, Quality, Query, SearchError,
    SearchProgress, SearchProvider as _, SearchResponse, SearchResult, Settle, SettleAction,
    Source,
};
use ts_rs::TS;

use crate::core::AppState;
use crate::error::{ErrorKind, IpcError};
use crate::latest::Latest;
use crate::prefetch::PrefetchState;
use crate::preload::PreloadState;
use crate::settings::SettingsState;
use crate::thumbnails::ThumbnailState;

/// Progress reaches the webview at most this often (the last report at once), so the burst of
/// indexers answering in the first second re-renders the list a few times, not dozens.
const PROGRESS_INTERVAL: Duration = Duration::from_millis(150);

/// Managed state: the search provider, created on first use. Clones share its result cache,
/// rate limiter, and indexer list.
#[derive(Debug)]
pub(crate) struct SearchState {
    provider: OnceCell<EmberglowProvider>,
    /// Search requests go through the network manager (proxy, WireGuard tunnel, kill switch).
    network: torrentplayer_net::Network,
    /// One progressive search at a time: a new one stops the one running.
    searches: Latest,
}

impl SearchState {
    /// Before following `link`, waits for the rate limiter if it is one of the search
    /// service's download links (the service counts them like searches). Nothing to wait for
    /// before the first search.
    pub(crate) async fn pace(&self, link: &str) {
        if let Some(provider) = self.provider.get() {
            provider.pace(link).await;
        }
    }

    /// The provider, if a search has created it (for pacing download links from tasks).
    pub(crate) fn started_provider(&self) -> Option<EmberglowProvider> {
        self.provider.get().cloned()
    }

    pub(crate) fn new(network: torrentplayer_net::Network) -> Self {
        Self {
            provider: OnceCell::new(),
            network,
            searches: Latest::default(),
        }
    }

    /// The provider, configured with the current settings (read on every search, so a change
    /// applies to the next one). Its cache keys include the adult filter.
    async fn provider(&self, settings: &SettingsState) -> Result<EmberglowProvider, IpcError> {
        let shared = self
            .provider
            .get_or_try_init(|| async {
                let base = url::Url::parse(torrentplayer_search::DEFAULT_BASE_URL)
                    .map_err(|e| IpcError::internal(&e))?;
                let client = self
                    .network
                    .configure_http(EmberglowProvider::client_builder())
                    .map_err(|e| IpcError::internal(&e))?
                    .build()
                    .map_err(|e| IpcError::internal(&e))?;
                // With the bundled FFmpeg, Dolby and DTS audio is converted, so results with it
                // play with sound (see `convert`).
                Ok::<_, IpcError>(
                    EmberglowProvider::with_client(base, client)
                        .converts_audio(crate::convert::sidecar().is_some()),
                )
            })
            .await?;
        let hide_adult = settings.current().search.hide_adult;
        Ok(shared.clone().include_adult(!hide_adult))
    }
}

impl From<SearchError> for IpcError {
    fn from(error: SearchError) -> Self {
        let kind = match error {
            SearchError::InvalidQuery(_) => ErrorKind::InvalidArgument,
            SearchError::RateLimited
            | SearchError::Unavailable(_)
            | SearchError::TooLarge
            | SearchError::BadResponse => ErrorKind::SearchUnavailable,
        };
        // Search error messages are written for users and contain no local data.
        Self::new(kind, error.to_string())
    }
}

/// Whether the built-in player can be expected to play a result.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub(crate) enum PlayabilityDto {
    /// MP4 or `WebM` with a web codec.
    Plays,
    /// Probably (Matroska, or a web codec in an unnamed container).
    Likely,
    /// HEVC outside MP4: only with a hardware decoder.
    Unlikely,
    /// AVI, MPEG-TS, Xvid, and similar.
    NeedsFullPlayer,
    /// The title does not say.
    Unknown,
}

/// Whether a result is a video, judged by its title and categories.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub(crate) enum ContentKindDto {
    /// Movies, TV, anime, or a title with video markers.
    Video,
    /// Music, audiobooks, podcasts.
    Audio,
    /// Ebooks, software, or games: nothing to play.
    NotVideo,
    /// Nothing to go by: never hide these as "not a video".
    Unknown,
}

/// Quality parsed from a result's title.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, Serialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub(crate) struct QualityDto {
    /// `2160p`, `1080p`, `720p`, or `480p`.
    pub resolution: Option<String>,
    /// HDR10, HDR10+, or Dolby Vision.
    pub hdr: bool,
    /// `x264`, `x265`, `AV1`, `VP9`, or `Xvid`.
    pub codec: Option<String>,
    /// `BluRay`, `WEB-DL`, `WEBRip`, `HDTV`, `DVD`, or `CAM`.
    pub source: Option<String>,
    /// `MP4`, `WebM`, `MKV`, or `AVI/TS`.
    pub container: Option<String>,
    /// A camera recording, telesync, or screener.
    pub camera: bool,
    /// Claims 4K but is too small to be real 4K.
    pub suspect_size: bool,
    /// Whether the built-in player can play it.
    pub playable: PlayabilityDto,
    /// Resolution, HDR, and source, for example `2160p HDR WEB-DL`; empty if unknown.
    pub label: String,
    /// Video or not.
    pub kind: ContentKindDto,
    /// The audio codec the title names (`AAC`, `Dolby Digital`, `DTS`, …), if any.
    pub audio: Option<String>,
    /// Whether the built-in player plays that audio (decoded, or converted with the bundled
    /// FFmpeg); `null` if the title names none.
    pub audio_plays: Option<bool>,
    /// Whether it plays only because TorrentPlayer converts it (Dolby or DTS) to stereo AAC.
    pub audio_converted: bool,
}

impl From<&Quality> for QualityDto {
    fn from(q: &Quality) -> Self {
        Self {
            resolution: q.resolution.map(|r| r.label().to_owned()),
            hdr: q.hdr,
            codec: q.codec.map(|c: Codec| c.label().to_owned()),
            source: q.source.map(|s: Source| s.label().to_owned()),
            container: q.container.map(|c: Container| c.label().to_owned()),
            camera: q.is_cam(),
            suspect_size: q.suspect_size,
            playable: match q.playability() {
                Playability::Plays => PlayabilityDto::Plays,
                Playability::Likely => PlayabilityDto::Likely,
                Playability::Unlikely => PlayabilityDto::Unlikely,
                Playability::NeedsFullPlayer => PlayabilityDto::NeedsFullPlayer,
                Playability::Unknown => PlayabilityDto::Unknown,
            },
            label: q.label(),
            kind: match q.kind {
                ContentKind::Video => ContentKindDto::Video,
                ContentKind::Audio => ContentKindDto::Audio,
                ContentKind::NotVideo => ContentKindDto::NotVideo,
                ContentKind::Unknown => ContentKindDto::Unknown,
            },
            audio: q.audio.map(|a| a.label().to_owned()),
            audio_plays: q.audio.map(|_| q.silent_audio().is_none()),
            // As in the ranking reason: a file that needs the full player plays nothing here.
            audio_converted: q.playability() != Playability::NeedsFullPlayer
                && q.converted_audio().is_some(),
        }
    }
}

/// One search result.
#[derive(Debug, Clone, Serialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub(crate) struct SearchResultDto {
    /// Torrent name (plain text; never render as HTML).
    pub title: String,
    /// Total size in bytes, if reported.
    pub size_bytes: Option<u64>,
    /// Peers with the complete torrent, if reported.
    pub seeders: Option<u32>,
    /// All peers, if reported.
    pub peers: Option<u32>,
    /// Identifies the result within a search: its info hash, or `download:` and its download
    /// link while the hash is not known. Use it as the list key.
    pub key: String,
    /// What to add with `add_torrent_link`: the magnet link, or else the download link.
    pub link: String,
    /// Magnet link, if known.
    pub magnet: Option<String>,
    /// Lowercase hex info hash (matches `PreloadStatusDto.infoHash`), if known; `null` for
    /// results offered only as a download link.
    pub info_hash: Option<String>,
    /// The search service's download link (https), for results its indexer offers only that
    /// way; it usually leads to a magnet link.
    pub download: Option<String>,
    /// The indexer's page for the torrent (http or https).
    pub details_url: Option<String>,
    /// Every indexer that listed it.
    pub indexers: Vec<String>,
    /// Indexer category.
    pub category: Option<String>,
    /// Publication time (ISO 8601), as reported.
    pub published: Option<String>,
    /// License, if known; `null` means unknown.
    pub license: Option<String>,
    /// The search service's verdict on the swarm: `good`, `low`, `dead`, or `unknown`.
    pub health: Option<String>,
    /// The listing gave a placeholder seed count, a sign of a fake (its seeders are unknown).
    pub suspicious: bool,
    /// Quality parsed from the title.
    pub quality: QualityDto,
    /// Why it ranks where it does, for example `1080p · 412 seeders · plays in built-in player`.
    pub reason: String,
    /// The recommended result: relevant, seeded, not a camera copy, and playable. At most one
    /// per search, always first.
    pub best_pick: bool,
}

impl From<SearchResult> for SearchResultDto {
    fn from(r: SearchResult) -> Self {
        Self {
            quality: QualityDto::from(&r.quality),
            title: r.title,
            size_bytes: r.size_bytes,
            seeders: r.seeders,
            peers: r.peers,
            key: r.key,
            link: r.link,
            magnet: r.magnet,
            info_hash: r.info_hash,
            download: r.download,
            details_url: r.details_url,
            indexers: r.indexers,
            category: r.category,
            published: r.published,
            license: r.license,
            health: r.health,
            suspicious: r.suspicious,
            reason: r.reason,
            best_pick: r.best_pick,
        }
    }
}

/// How one indexer did on this search.
#[derive(Debug, Clone, Serialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub(crate) struct IndexerStatusDto {
    /// Indexer name.
    pub name: String,
    /// Results it returned.
    pub results: u32,
    /// Why it failed this time, if it did.
    pub error: Option<String>,
}

/// Results of a search.
#[derive(Debug, Clone, Serialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub(crate) struct SearchResponseDto {
    /// Results, most relevant first; within equal relevance, the best to watch first.
    pub results: Vec<SearchResultDto>,
    /// Per-indexer outcomes.
    pub indexers: Vec<IndexerStatusDto>,
    /// Results hidden because they are in an adult category.
    pub excluded_adult: u32,
}

impl From<SearchResponse> for SearchResponseDto {
    fn from(response: SearchResponse) -> Self {
        Self {
            results: response.results.into_iter().map(Into::into).collect(),
            indexers: response
                .indexers
                .into_iter()
                .map(|i| IndexerStatusDto {
                    name: i.name,
                    results: i.results,
                    error: i.error,
                })
                .collect(),
            excluded_adult: response.excluded_adult,
        }
    }
}

/// Results so far of a progressive search.
#[derive(Debug, Clone, Serialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub(crate) struct SearchProgressDto {
    /// Everything found so far, merged and ranked.
    pub response: SearchResponseDto,
    /// Indexer requests finished.
    pub completed: u32,
    /// Indexer requests in total.
    pub total: u32,
    /// Whether every request has finished.
    pub done: bool,
    /// Whether the results came from the 10-minute local cache.
    pub cached: bool,
}

impl From<SearchProgress> for SearchProgressDto {
    fn from(p: SearchProgress) -> Self {
        Self {
            response: p.response.into(),
            completed: p.completed,
            total: p.total,
            done: p.done,
            cached: p.cached,
        }
    }
}

fn log_finished(response: &SearchResponse) {
    tracing::info!(
        results = response.results.len(),
        excluded_adult = response.excluded_adult,
        failed_indexers = response
            .indexers
            .iter()
            .filter(|i| i.error.is_some())
            .count(),
        best_pick = response.results.iter().any(|r| r.best_pick),
        "search finished"
    );
}

/// Searches public torrent indexers in one request (about 6 s uncached), in all categories or
/// only `categories` (Torznab ids, at most 4, never adult ones).
#[tauri::command]
pub(crate) async fn search_torrents(
    state: State<'_, SearchState>,
    app: State<'_, AppState>,
    preload: State<'_, PreloadState>,
    settings: State<'_, SettingsState>,
    query: String,
    categories: Option<Vec<u32>>,
) -> Result<SearchResponseDto, IpcError> {
    let query = Query::parse(&query)?.with_categories(&categories.unwrap_or_default())?;
    let provider = state.provider(&settings).await?;
    let response = provider.search(&query).await?;
    log_finished(&response);
    preload.after_search(&app, &*settings, &response).await;
    Ok(response.into())
}

/// Searches public torrent indexers (in all categories, or only `categories`: Torznab ids, at
/// most 4, never adult ones), sending the merged results to `on_progress` as indexers
/// answer (at most every [`PROGRESS_INTERVAL`]), and returns the final results. Starting
/// another search or calling `cancel_search` stops it with a `cancelled` error; a new search
/// also stops the metadata prefetch of the previous results. If the `preload_top_result`
/// setting is on, the best pick is preloaded once it has settled.
#[tauri::command]
pub(crate) async fn search_torrents_progressive(
    handle: AppHandle,
    state: State<'_, SearchState>,
    prefetch: State<'_, PrefetchState>,
    settings: State<'_, SettingsState>,
    query: String,
    categories: Option<Vec<u32>>,
    on_progress: Channel<SearchProgressDto>,
) -> Result<SearchResponseDto, IpcError> {
    let stopped = state.searches.begin();
    prefetch.cancel();
    if let Some(thumbnails) = handle.try_state::<ThumbnailState>() {
        thumbnails.cancel();
    }
    let query = Query::parse(&query)?.with_categories(&categories.unwrap_or_default())?;
    let provider = state.provider(&settings).await?;
    let (updates, received) = watch::channel(None);
    let (preloads, decided) = mpsc::unbounded_channel();
    let search = async move {
        let result = provider
            .search_progressive(&query, &mut |p| {
                updates.send_replace(Some(p));
            })
            .await;
        // Closes the channel: `forward` sends what is left and finishes.
        drop(updates);
        result
    };
    let work = async {
        let (result, (), ()) = tokio::join!(
            search,
            forward(received, &on_progress, preloads),
            apply_preloads(decided, &handle),
        );
        result
    };
    let response = tokio::select! {
        result = work => result?,
        () = stopped => {
            return Err(IpcError::new(ErrorKind::Cancelled, "The search was cancelled."));
        }
    };
    log_finished(&response);
    Ok(response.into())
}

/// Stops the running progressive search, if any. It fails with `cancelled` and preloads
/// nothing more.
#[tauri::command]
#[allow(
    clippy::needless_pass_by_value,
    reason = "Tauri passes managed state by value"
)]
pub(crate) fn cancel_search(state: State<'_, SearchState>) {
    state.searches.cancel();
}

/// Sends progress to the webview at most every [`PROGRESS_INTERVAL`] (the last report at
/// once), and turns it into preload decisions as the best pick settles. Returns when the search
/// has finished.
async fn forward(
    mut updates: watch::Receiver<Option<SearchProgress>>,
    channel: &Channel<SearchProgressDto>,
    preloads: mpsc::UnboundedSender<SettleAction>,
) {
    let mut settle = Settle::default();
    let mut sent_at: Option<Instant> = None;
    let mut unsent = false;
    loop {
        let flush_at = sent_at.filter(|_| unsent).map(|at| at + PROGRESS_INTERVAL);
        let wake = flush_at.into_iter().chain(settle.deadline()).min();
        tokio::select! {
            changed = updates.changed() => {
                if changed.is_err() {
                    break;
                }
                unsent = true;
            }
            () = sleep_until(wake) => {}
        }
        let progress = {
            let current = updates.borrow_and_update();
            unsent |= current.has_changed();
            current.clone()
        };
        let Some(progress) = progress else {
            continue;
        };
        let now = Instant::now();
        if let Some(action) = settle.update(&progress, now) {
            // The receiver outlives this loop.
            let _ = preloads.send(action);
        }
        if unsent && (progress.done || sent_at.is_none_or(|at| now >= at + PROGRESS_INTERVAL)) {
            unsent = false;
            sent_at = Some(now);
            // The window may have moved on to another search; nothing to do then.
            let _ = channel.send(progress.into());
        }
    }
}

async fn sleep_until(at: Option<Instant>) {
    match at {
        Some(at) => tokio::time::sleep_until(at).await,
        None => std::future::pending().await,
    }
}

/// Applies preload decisions in order, skipping one that a newer decision replaced before it
/// started. Each runs as a task of its own, so stopping the search never interrupts the engine
/// while it starts.
async fn apply_preloads(mut decided: mpsc::UnboundedReceiver<SettleAction>, handle: &AppHandle) {
    while let Some(mut action) = decided.recv().await {
        while let Ok(newer) = decided.try_recv() {
            action = newer;
        }
        let handle = handle.clone();
        let task = tauri::async_runtime::spawn(async move {
            let (Some(app), Some(preload), Some(settings)) = (
                handle.try_state::<AppState>(),
                handle.try_state::<PreloadState>(),
                handle.try_state::<SettingsState>(),
            ) else {
                return;
            };
            let best = match &action {
                SettleAction::Preload { link, .. } => Some(link.as_str()),
                SettleAction::Evict => None,
            };
            if let (Some(link), Some(search)) = (best, handle.try_state::<SearchState>()) {
                search.pace(link).await;
            }
            preload.apply(&app, &*settings, best).await;
        });
        // A failed preload has logged why.
        let _ = task.await;
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use tauri::ipc::InvokeResponseBody;
    use torrentplayer_search::rank::rank;
    use torrentplayer_search::results::{Candidate, build_response};

    use super::*;

    #[test]
    fn dolby_audio_plays_when_the_app_converts_it() {
        let mut quality = Quality::parse("Film.2024.1080p.WEB-DL.DDP5.1.H.264.mkv", None, None);
        let dto = QualityDto::from(&quality);
        assert_eq!(dto.audio.as_deref(), Some("Dolby Digital Plus"));
        assert_eq!((dto.audio_plays, dto.audio_converted), (Some(false), false));
        assert_eq!(dto.playable, PlayabilityDto::Unlikely);

        quality.converts_audio = true;
        let dto = QualityDto::from(&quality);
        assert_eq!((dto.audio_plays, dto.audio_converted), (Some(true), true));
        assert_eq!(dto.playable, PlayabilityDto::Likely);

        let aac = QualityDto::from(&Quality {
            converts_audio: true,
            ..Quality::parse("Film 2024 AAC mp4", None, None)
        });
        assert_eq!((aac.audio_plays, aac.audio_converted), (Some(true), false));

        // A file the built-in player cannot open is not "converted to stereo" either.
        let avi = QualityDto::from(&Quality {
            converts_audio: true,
            ..Quality::parse("Film 2024 DTS avi", None, None)
        });
        assert_eq!(avi.playable, PlayabilityDto::NeedsFullPlayer);
        assert!(!avi.audio_converted);
    }

    /// Progress of a search for "sintel" that has found `titles` (with seeders).
    fn progress(titles: &[(&str, u32)], done: bool) -> SearchProgress {
        let candidates = titles
            .iter()
            .enumerate()
            .map(|(i, (title, seeders))| Candidate {
                title: (*title).into(),
                seeders: Some(*seeders),
                infohash: Some(format!("{i:02x}").repeat(20)),
                ..Candidate::default()
            })
            .collect();
        let mut response = build_response(candidates, Vec::new(), false, false);
        rank(&mut response.results, "sintel");
        SearchProgress {
            response,
            completed: 1,
            total: 4,
            done,
            cached: false,
        }
    }

    #[tokio::test(start_paused = true)]
    async fn forwards_progress_throttled_and_preloads_once_settled() {
        let t0 = Instant::now();
        let sent = Arc::new(Mutex::new(Vec::new()));
        let log = Arc::clone(&sent);
        let channel = Channel::<SearchProgressDto>::new(move |body| {
            if let InvokeResponseBody::Json(json) = body {
                log.lock()
                    .unwrap()
                    .push((t0.elapsed(), json.contains("\"done\":true")));
            }
            Ok(())
        });
        let (updates, received) = watch::channel(None);
        let (preloads, mut decided) = mpsc::unbounded_channel();
        let forwarding = tokio::spawn(async move { forward(received, &channel, preloads).await });

        // A burst of reports: the first goes at once, the rest as one when the interval ends.
        let first = progress(&[("Sintel 720p mp4", 10)], false);
        for _ in 0..5 {
            updates.send_replace(Some(first.clone()));
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(
            *sent.lock().unwrap(),
            [(Duration::ZERO, false), (PROGRESS_INTERVAL, false)]
        );
        assert!(decided.try_recv().is_err(), "not settled yet");

        tokio::time::sleep(torrentplayer_search::settle::SETTLE_TIME).await;
        let Ok(SettleAction::Preload { key, .. }) = decided.try_recv() else {
            panic!("the settled best pick is preloaded");
        };
        assert_eq!(key, first.response.results[0].key);

        // The end goes at once and finishes the loop; the same pick needs no second preload.
        let mut last = first.clone();
        last.done = true;
        updates.send_replace(Some(last));
        drop(updates);
        forwarding.await.unwrap();
        let sent = sent.lock().unwrap();
        assert_eq!(sent.len(), 3);
        assert!(sent[2].1, "the last report says done");
        assert!(decided.try_recv().is_err(), "one preload per search");
    }

    #[test]
    fn search_errors_map_to_ipc_kinds() {
        let e: IpcError = SearchError::InvalidQuery("Type something.".into()).into();
        assert_eq!(e.kind, ErrorKind::InvalidArgument);
        let e: IpcError = SearchError::RateLimited.into();
        assert_eq!(e.kind, ErrorKind::SearchUnavailable);
        assert!(e.message.contains("Wait"));
    }

    #[test]
    fn dto_serializes_as_camel_case() {
        let dto = SearchResponseDto {
            results: Vec::new(),
            indexers: Vec::new(),
            excluded_adult: 3,
        };
        let json = serde_json::to_value(dto).unwrap();
        assert_eq!(json["excludedAdult"], 3);
    }

    #[test]
    fn quality_and_best_pick_reach_the_dto() {
        let mut result = SearchResult {
            title: "Sintel 2010 1080p WEB-DL x264 mp4".into(),
            size_bytes: Some(1_000_000_000),
            seeders: Some(412),
            peers: Some(430),
            key: "08ada5a7a6183aae1e09d831df6748d566095a10".into(),
            link: "magnet:?xt=urn:btih:08ada5a7a6183aae1e09d831df6748d566095a10".into(),
            magnet: Some("magnet:?xt=urn:btih:08ada5a7a6183aae1e09d831df6748d566095a10".into()),
            info_hash: Some("08ada5a7a6183aae1e09d831df6748d566095a10".into()),
            download: None,
            details_url: None,
            indexers: vec!["T".into()],
            category: None,
            published: None,
            license: None,
            health: None,
            suspicious: false,
            quality: Quality::parse("Sintel 2010 1080p WEB-DL x264 mp4", None, None),
            watch_score: 0,
            reason: "1080p WEB-DL · 412 seeders · plays in built-in player".into(),
            best_pick: true,
        };
        result.quality.hdr = true;
        let json = serde_json::to_value(SearchResultDto::from(result)).unwrap();
        assert_eq!(json["bestPick"], true);
        assert_eq!(json["infoHash"], "08ada5a7a6183aae1e09d831df6748d566095a10");
        assert_eq!(json["quality"]["resolution"], "1080p");
        assert_eq!(json["quality"]["source"], "WEB-DL");
        assert_eq!(json["quality"]["container"], "MP4");
        assert_eq!(json["quality"]["playable"], "plays");
        assert_eq!(json["quality"]["camera"], false);
        assert_eq!(json["quality"]["label"], "1080p HDR WEB-DL");
        assert_eq!(json["quality"]["kind"], "video");
        assert!(json["reason"].as_str().unwrap().contains("412 seeders"));
    }
}
