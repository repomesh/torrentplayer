// SPDX-License-Identifier: MIT OR Apache-2.0

//! Fetching the metadata of search results ahead of time, so that playing any of them starts
//! sooner (see `StreamEngine::prefetch_metadata`). Only the best pick also preloads video (see
//! [`crate::preload`]).
//!
//! The webview asks for the results it shows, top first, at most [`MAX_PREFETCH`]. A newer
//! request, or a new search, stops the one running. At most [`CONCURRENCY`] torrents are
//! fetched at once, each for at most [`ITEM_TIMEOUT`]. A result with a download link on the
//! search service is fetched through it first (plain HTTP, which also works without the DHT),
//! checked against the result's info hash when that is known, and through its magnet link if
//! that fails. Download links count against the search rate limit, and since each new one costs
//! the service a request to an indexer (its operators ask clients to follow only the links they
//! will use), only the first [`MAX_DOWNLOADS`] are followed. Failures are only logged: playing a
//! result fetches its metadata itself if it is not there. Nothing is downloaded or uploaded
//! beyond the metadata, and traffic follows the network mode (protected modes have no DHT, so
//! magnet links without working trackers fail there; the kill switch blocks it). Turning off
//! the `search.preloadTopResult` setting turns this off too.

use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tauri::State;
use tauri::ipc::Channel;
use tokio::task::JoinSet;
use torrentplayer_search::EmberglowProvider;
use torrentplayer_torrent::{EngineError, TorrentSource};
use ts_rs::TS;
use url::Url;

use crate::core::AppState;
use crate::error::IpcError;
use crate::latest::Latest;
use crate::preload::PreloadPolicy as _;
use crate::search::SearchState;
use crate::settings::SettingsState;

/// Most results fetched per request; the rest of a longer list is ignored.
pub(crate) const MAX_PREFETCH: usize = 20;
/// Most download links followed per request.
pub(crate) const MAX_DOWNLOADS: usize = 4;
/// Longest list accepted.
const MAX_REQUESTS: usize = 200;
/// Torrents fetched at once.
const CONCURRENCY: usize = 3;
/// How long one torrent may take.
const ITEM_TIMEOUT: Duration = Duration::from_secs(20);

/// Managed state: the running prefetch, so a newer one can stop it.
#[derive(Debug, Default)]
pub(crate) struct PrefetchState {
    latest: Latest,
}

impl PrefetchState {
    /// Stops the running prefetch, if any.
    pub(crate) fn cancel(&self) {
        self.latest.cancel();
    }
}

/// A search result to prefetch.
#[derive(Debug, Clone, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub(crate) struct PrefetchRequest {
    /// Its key (`SearchResultDto.key`), sent back when it is ready.
    pub key: String,
    /// Its magnet link, if it has one.
    pub magnet: Option<String>,
    /// Its download link on the search service, if it has one; tried first.
    pub download: Option<String>,
    /// Its info hash, if known: a download link that gives another torrent counts as failed.
    pub info_hash: Option<String>,
}

/// A result whose metadata is ready.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub(crate) struct PrefetchedDto {
    /// Its key (`SearchResultDto.key`).
    pub key: String,
    /// Its lowercase hex info hash.
    pub info_hash: String,
}

/// How one result is fetched.
#[derive(Debug, Clone)]
enum Fetch {
    /// Through the search service's download link, then the magnet link if that fails.
    Download {
        key: String,
        url: Url,
        expected: Option<String>,
        fallback: Option<TorrentSource>,
    },
    /// Through the magnet link.
    Magnet { key: String, source: TorrentSource },
}

/// What to fetch for `requests`, top first: download links only if they are the search
/// service's own (see `is_download`), and only the first [`MAX_DOWNLOADS`] of those.
fn plan(requests: &[PrefetchRequest], is_download: impl Fn(&str) -> bool) -> Vec<Fetch> {
    let mut downloads = 0;
    let mut plan = Vec::new();
    for request in requests {
        if plan.len() == MAX_PREFETCH {
            break;
        }
        let magnet = request
            .magnet
            .as_deref()
            .and_then(|m| TorrentSource::parse_link(m).ok())
            .filter(|s| matches!(s, TorrentSource::Magnet(_)));
        let download = request
            .download
            .as_deref()
            .filter(|d| is_download(d))
            .and_then(|d| match TorrentSource::parse_link(d) {
                Ok(TorrentSource::Url(url)) => Some(url),
                _ => None,
            });
        let key = request.key.clone();
        match (download, magnet) {
            (Some(url), fallback) if downloads < MAX_DOWNLOADS => {
                downloads += 1;
                let expected = request
                    .info_hash
                    .clone()
                    .filter(|h| h.len() == 40 && h.bytes().all(|b| b.is_ascii_hexdigit()));
                plan.push(Fetch::Download {
                    key,
                    url,
                    expected,
                    fallback,
                });
            }
            (_, Some(source)) => plan.push(Fetch::Magnet { key, source }),
            (_, None) => {}
        }
    }
    plan
}

/// Fetches the metadata of these results (top first) in the background, sending each one's
/// key and info hash to `on_ready` once it is ready. Stops the previous request; returns when
/// every result is done or a newer request (or search) stops this one. Does nothing while the
/// `search.preloadTopResult` setting is off.
#[tauri::command]
pub(crate) async fn prefetch_torrents(
    state: State<'_, PrefetchState>,
    app: State<'_, AppState>,
    search: State<'_, SearchState>,
    settings: State<'_, SettingsState>,
    results: Vec<PrefetchRequest>,
    on_ready: Channel<PrefetchedDto>,
) -> Result<(), IpcError> {
    let stopped = state.latest.begin();
    if results.len() > MAX_REQUESTS {
        return Err(IpcError::invalid("Too many results to prefetch."));
    }
    let provider: Option<EmberglowProvider> = search.started_provider();
    let fetches = plan(&results, |link| {
        provider.as_ref().is_some_and(|p| p.is_download_link(link))
    });
    if fetches.is_empty() || !settings.preload_top_result() {
        return Ok(());
    }
    let core = Arc::clone(app.core().await?);
    let fetch = move |item: Fetch| {
        let core = Arc::clone(&core);
        let provider = provider.clone();
        async move {
            match item {
                Fetch::Download {
                    key,
                    url,
                    expected,
                    fallback,
                } => {
                    if let Some(provider) = &provider {
                        provider.pace(url.as_str()).await;
                    }
                    let info_hash = core
                        .engine
                        .prefetch_download(url, expected.as_deref(), fallback)
                        .await?;
                    Ok(PrefetchedDto { key, info_hash })
                }
                Fetch::Magnet { key, source } => {
                    let info_hash = core.engine.prefetch_metadata(source).await?;
                    Ok(PrefetchedDto { key, info_hash })
                }
            }
        }
    };
    let ready = |prefetched: PrefetchedDto| {
        // The window may have moved on; nothing to do then.
        let _ = on_ready.send(prefetched);
    };
    tokio::select! {
        () = run_batch(fetches, CONCURRENCY, fetch, ready) => {}
        () = stopped => {}
    }
    Ok(())
}

/// Fetches `items`, at most `limit` at a time and each for at most [`ITEM_TIMEOUT`], calling
/// `ready` with each result as it arrives. Dropping the future stops the fetches still running.
pub(crate) async fn run_batch<T, R, F, Fut>(
    items: Vec<T>,
    limit: usize,
    fetch: F,
    mut ready: impl FnMut(R),
) where
    F: Fn(T) -> Fut,
    Fut: Future<Output = Result<R, EngineError>> + Send + 'static,
    R: Send + 'static,
{
    let mut pending = items.into_iter();
    let mut running = JoinSet::new();
    loop {
        while running.len() < limit.max(1)
            && let Some(item) = pending.next()
        {
            running.spawn(tokio::time::timeout(ITEM_TIMEOUT, fetch(item)));
        }
        let Some(finished) = running.join_next().await else {
            break;
        };
        match finished {
            Ok(Ok(Ok(result))) => ready(result),
            Ok(Ok(Err(error))) => tracing::debug!(%error, "metadata prefetch failed"),
            Ok(Err(_)) => tracing::debug!("metadata prefetch timed out"),
            Err(error) => tracing::debug!(%error, "metadata prefetch stopped"),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    /// Counts fetches in flight and the most at once; a fetch dropped early counts as stopped.
    #[derive(Default)]
    struct Probe {
        running: AtomicUsize,
        most: AtomicUsize,
        stopped: AtomicUsize,
    }

    struct Running(Arc<Probe>, bool);

    impl Drop for Running {
        fn drop(&mut self) {
            self.0.running.fetch_sub(1, Ordering::SeqCst);
            if !self.1 {
                self.0.stopped.fetch_add(1, Ordering::SeqCst);
            }
        }
    }

    /// A fetch that takes `secs` seconds (forever for `u64::MAX`) and fails for odd items.
    fn fetcher(probe: Arc<Probe>) -> impl Fn((u64, u64)) -> BoxedFetch {
        move |(item, secs)| {
            let probe = Arc::clone(&probe);
            Box::pin(async move {
                let now = probe.running.fetch_add(1, Ordering::SeqCst) + 1;
                probe.most.fetch_max(now, Ordering::SeqCst);
                let mut guard = Running(probe, false);
                if secs == u64::MAX {
                    std::future::pending::<()>().await;
                }
                tokio::time::sleep(Duration::from_secs(secs)).await;
                guard.1 = true;
                if item % 2 == 1 {
                    Err(EngineError::MetadataTimeout)
                } else {
                    Ok(format!("{item:040x}"))
                }
            })
        }
    }

    type BoxedFetch =
        std::pin::Pin<Box<dyn Future<Output = Result<String, EngineError>> + Send + 'static>>;

    #[tokio::test(start_paused = true)]
    async fn fetches_a_few_at_a_time_and_skips_failures_and_slow_ones() {
        let probe = Arc::new(Probe::default());
        let ready = Mutex::new(Vec::new());
        // Item 6 never answers and is given up after the timeout.
        let items: Vec<(u64, u64)> = (0..10)
            .map(|i| (i, if i == 6 { u64::MAX } else { 1 }))
            .collect();
        let started = tokio::time::Instant::now();
        run_batch(items, CONCURRENCY, fetcher(Arc::clone(&probe)), |hash| {
            ready.lock().unwrap().push(hash);
        })
        .await;
        assert_eq!(probe.most.load(Ordering::SeqCst), CONCURRENCY);
        assert_eq!(probe.running.load(Ordering::SeqCst), 0);
        let mut ready = ready.into_inner().unwrap();
        ready.sort();
        let expected: Vec<String> = [0u64, 2, 4, 8]
            .iter()
            .map(|i| format!("{i:040x}"))
            .collect();
        assert_eq!(ready, expected, "even items succeed; 6 timed out");
        assert!(started.elapsed() >= ITEM_TIMEOUT);
    }

    #[tokio::test(start_paused = true)]
    async fn a_newer_request_stops_the_fetches_still_running() {
        let probe = Arc::new(Probe::default());
        let latest = Latest::default();
        let stopped = latest.begin();
        let items: Vec<(u64, u64)> = (0..6).map(|i| (i * 2, 5)).collect();
        let batch = {
            let probe = Arc::clone(&probe);
            tokio::spawn(async move {
                tokio::select! {
                    () = run_batch(items, CONCURRENCY, fetcher(probe), |_| {}) => false,
                    () = stopped => true,
                }
            })
        };
        tokio::time::sleep(Duration::from_secs(1)).await;
        assert_eq!(probe.running.load(Ordering::SeqCst), CONCURRENCY);
        let _newer = latest.begin();
        assert!(batch.await.unwrap(), "stopped by the newer request");
        // The aborted fetches are dropped the next time the runtime runs them.
        tokio::time::sleep(Duration::from_millis(1)).await;
        assert_eq!(
            probe.running.load(Ordering::SeqCst),
            0,
            "nothing left running"
        );
        assert_eq!(probe.stopped.load(Ordering::SeqCst), CONCURRENCY);
    }

    fn request(n: usize, magnet: bool, download: Option<&str>) -> PrefetchRequest {
        let hash = format!("{n:040x}");
        PrefetchRequest {
            key: hash.clone(),
            magnet: magnet.then(|| format!("magnet:?xt=urn:btih:{hash}")),
            download: download.map(Into::into),
            info_hash: Some(hash),
        }
    }

    #[test]
    fn plans_download_links_first_but_only_the_services_and_only_a_few() {
        let own = |link: &str| link.starts_with("https://search.example/download/");
        let mut requests = vec![
            request(1, true, Some("https://search.example/download/a")),
            request(2, false, Some("https://evil.example/download/b")),
            request(3, false, None),
            request(4, true, Some("javascript:alert(1)")),
        ];
        requests[0].info_hash = Some("not a hash".into());
        let planned = plan(&requests, own);
        assert_eq!(planned.len(), 2, "{planned:?}");
        let Fetch::Download {
            key,
            expected,
            fallback,
            ..
        } = &planned[0]
        else {
            panic!("the download link goes first: {planned:?}");
        };
        assert_eq!(key, &format!("{:040x}", 1));
        assert_eq!(expected, &None, "a malformed hash is not checked against");
        assert!(matches!(fallback, Some(TorrentSource::Magnet(_))));
        assert!(matches!(&planned[1], Fetch::Magnet { key, .. } if *key == format!("{:040x}", 4)));

        let many: Vec<PrefetchRequest> = (0..30)
            .map(|n| request(n, true, Some("https://search.example/download/x")))
            .collect();
        let capped = plan(&many, own);
        assert_eq!(capped.len(), MAX_PREFETCH);
        let downloads = capped
            .iter()
            .filter(|f| matches!(f, Fetch::Download { .. }))
            .count();
        assert_eq!(
            downloads, MAX_DOWNLOADS,
            "the rest go through their magnet links"
        );
    }
}
