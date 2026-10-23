// SPDX-License-Identifier: MIT OR Apache-2.0

//! The emberglow search API: a JSON API over Jackett that searches many public indexers.
//!
//! `GET /search?q=&limit=&indexer=` returns merged results plus a per-indexer status list;
//! `indexer` may be repeated to search several indexers. `GET /search/stream` takes the same
//! parameters and answers with server-sent events while the search runs: one `indexer` event
//! per indexer as it finishes (its own results, not merged with the others'), then one `done`
//! event with the `/search` body, or an `error` event if the service's backend is down. The
//! first results arrive after about half a second and the last after up to 20 s.
//! `GET /indexers` lists the indexers with their health and last response time.
//!
//! [`EmberglowProvider::search_progressive`](crate::SearchProvider::search_progressive) reads
//! the stream: one request per search, merged and ranked by TorrentPlayer as each indexer answers. If
//! the service does not offer the stream (or sends something TorrentPlayer cannot read) before the
//! first event, it falls back to asking the indexers in small groups (see [`crate::fanout`]).
//! A stream that breaks after some events ends the search with what arrived.
//!
//! The service caches `/search` results for 10 minutes (streams are never cached) and limits
//! each IP to 60 requests per minute (burst 30). TorrentPlayer keeps finished searches for the same 10
//! minutes, paces its requests below the limit, and never adds cache-busting parameters. The
//! API has no license information, so every result's license is unknown.

use std::collections::HashSet;
use std::pin::pin;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use futures_util::StreamExt as _;
use futures_util::future::{BoxFuture, Either, select};
use serde::Deserialize;
use tokio::time::Instant;
use url::Url;

use crate::cache::{CACHE_TTL, QueryCache, RateLimiter, normalize};
use crate::fanout::{IndexerInfo, MAX_CONCURRENT, Merger, SearchProgress, carries, plan};
use crate::quality::kind_of_category_ids;
use crate::rank::rank;
use crate::results::{
    Candidate, IndexerStatus, MAX_RESPONSE_BYTES, SearchResponse, build_response,
};
use crate::sse::{SseEvent, SseParser};
use crate::{Query, SearchError, SearchProvider};

/// The public emberglow endpoint.
pub const DEFAULT_BASE_URL: &str = "https://search.emberglow.tech";
/// Results requested per search (the API ranks by seeders before applying the limit).
const RESULT_LIMIT: u32 = 100;
/// Wait before retrying a request the service rate limited.
const RATE_LIMIT_RETRY: Duration = Duration::from_secs(3);
/// How long a streamed search may run. The service stops waiting for indexers after 20 s.
const STREAM_DEADLINE: Duration = Duration::from_secs(45);
/// Most results accepted from one stream's `indexer` events.
const MAX_STREAM_RESULTS: usize = 5_000;
/// The status of indexers that had not answered when a stream ended early.
const ENDED_EARLY: &str = "No answer: the search stream ended early.";

/// Searches through the emberglow API.
#[derive(Debug, Clone)]
pub struct EmberglowProvider {
    base: Url,
    client: reqwest::Client,
    include_adult: bool,
    converts_audio: bool,
    streaming: bool,
    shared: Arc<Shared>,
}

/// State shared by clones of a provider: the result cache, the rate limiter, the indexer list
/// (fetched once and refreshed after [`CACHE_TTL`]), and a request count.
#[derive(Debug)]
struct Shared {
    cache: QueryCache,
    limiter: RateLimiter,
    indexers: Mutex<Option<(Instant, Arc<Vec<IndexerInfo>>)>>,
    requests: AtomicU32,
}

impl EmberglowProvider {
    /// A provider for the public endpoint, with adult results excluded.
    ///
    /// # Errors
    ///
    /// Returns an error if the HTTP client cannot be created.
    pub fn new() -> Result<Self, SearchError> {
        Self::with_base_url(
            Url::parse(DEFAULT_BASE_URL)
                .map_err(|_| SearchError::Unavailable("invalid search URL".into()))?,
        )
    }

    /// A provider for another endpoint (tests and self-hosted instances).
    ///
    /// # Errors
    ///
    /// Returns an error if the HTTP client cannot be created.
    pub fn with_base_url(base: Url) -> Result<Self, SearchError> {
        let client = Self::client_builder()
            .build()
            .map_err(|e| SearchError::Unavailable(format!("could not start search: {e}")))?;
        Ok(Self::with_client(base, client))
    }

    /// The HTTP client settings the provider needs (timeouts, https-only redirects), to extend
    /// before passing the client to [`EmberglowProvider::with_client`], for example with a
    /// proxy.
    pub fn client_builder() -> reqwest::ClientBuilder {
        reqwest::Client::builder()
            .user_agent(concat!("TorrentPlayer/", env!("CARGO_PKG_VERSION")))
            .connect_timeout(Duration::from_secs(10))
            // Uncached searches take up to a minute; streams are cut off sooner (see
            // `STREAM_DEADLINE`).
            .timeout(Duration::from_secs(90))
            .redirect(reqwest::redirect::Policy::custom(|attempt| {
                if attempt.previous().len() >= 5 {
                    attempt.error("too many redirects")
                } else if attempt.url().scheme() == "https" {
                    attempt.follow()
                } else {
                    attempt.stop()
                }
            }))
    }

    /// A provider using a preconfigured HTTP client (for example, one routed through a proxy).
    #[must_use]
    pub fn with_client(base: Url, client: reqwest::Client) -> Self {
        Self {
            base,
            client,
            include_adult: false,
            converts_audio: false,
            streaming: true,
            shared: Arc::new(Shared {
                cache: QueryCache::default(),
                limiter: RateLimiter::for_emberglow(),
                indexers: Mutex::new(None),
                requests: AtomicU32::new(0),
            }),
        }
    }

    /// Whether adult categories are included (default: excluded).
    #[must_use]
    pub fn include_adult(mut self, include: bool) -> Self {
        self.include_adult = include;
        self
    }

    /// Whether the app converts Dolby and DTS audio for the built-in player (default: no), so
    /// results with such audio are not marked or ranked as playing without sound. See
    /// [`crate::Quality::converts_audio`].
    #[must_use]
    pub fn converts_audio(mut self, converts: bool) -> Self {
        self.converts_audio = converts;
        self
    }

    /// Whether progressive searches read the event stream (default) or always ask the
    /// indexers in groups, as they do when the stream is not available.
    #[must_use]
    pub fn streaming(mut self, enabled: bool) -> Self {
        self.streaming = enabled;
        self
    }

    /// Requests this provider and its clones have sent, retries included (for diagnostics).
    #[must_use]
    pub fn requests_sent(&self) -> u32 {
        self.shared.requests.load(Ordering::Relaxed)
    }

    /// Whether `link` is one of this service's download links (see
    /// [`SearchResult::download`](crate::SearchResult::download)).
    #[must_use]
    pub fn is_download_link(&self, link: &str) -> bool {
        own_download_link(&self.base, link).is_some()
    }

    /// Before following `link`, waits for the rate limiter if it is one of this service's
    /// download links: the service counts them like searches (and asks an indexer for each new
    /// one).
    pub async fn pace(&self, link: &str) {
        if self.is_download_link(link) {
            self.shared.limiter.acquire().await;
            self.shared.requests.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn endpoint(&self, path: &str) -> Result<Url, SearchError> {
        self.base
            .join(path)
            .map_err(|_| SearchError::Unavailable("invalid search URL".into()))
    }

    /// The URL of a search (`path` is `search` or `search/stream`).
    fn search_url(
        &self,
        path: &str,
        text: &str,
        categories: &[u32],
        indexers: &[String],
    ) -> Result<Url, SearchError> {
        let mut url = self.endpoint(path)?;
        {
            let mut pairs = url.query_pairs_mut();
            pairs
                .append_pair("q", text)
                .append_pair("limit", &RESULT_LIMIT.to_string());
            for id in categories {
                pairs.append_pair("category", &id.to_string());
            }
            for id in indexers {
                pairs.append_pair("indexer", id);
            }
        }
        Ok(url)
    }

    /// Waits for the rate limiter, then sends a GET.
    async fn send(
        &self,
        url: Url,
        accept: &'static str,
    ) -> Result<reqwest::Response, reqwest::Error> {
        self.shared.limiter.acquire().await;
        self.shared.requests.fetch_add(1, Ordering::Relaxed);
        self.client
            .get(url)
            .header(reqwest::header::ACCEPT, accept)
            .send()
            .await
    }

    /// Sends a GET and reads a bounded body.
    async fn get(&self, url: Url) -> Result<Vec<u8>, SearchError> {
        let mut response = self
            .send(url, "application/json")
            .await
            .map_err(network_error)?;
        let status = response.status().as_u16();
        if status != 200 {
            return Err(status_error(status));
        }
        if response
            .content_length()
            .is_some_and(|len| len > MAX_RESPONSE_BYTES as u64)
        {
            return Err(SearchError::TooLarge);
        }
        let mut body = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(network_error)? {
            if body.len() + chunk.len() > MAX_RESPONSE_BYTES {
                return Err(SearchError::TooLarge);
            }
            body.extend_from_slice(&chunk);
        }
        Ok(body)
    }

    /// One search request; `indexers` empty means all of them. Retries once if rate limited.
    async fn fetch(
        &self,
        text: &str,
        categories: &[u32],
        indexers: &[String],
    ) -> Result<(Vec<Candidate>, Vec<IndexerStatus>), SearchError> {
        let url = self.search_url("search", text, categories, indexers)?;
        let body = match self.get(url.clone()).await {
            Err(SearchError::RateLimited) => {
                tokio::time::sleep(RATE_LIMIT_RETRY).await;
                self.get(url).await?
            }
            other => other?,
        };
        parse_raw(&body, &self.base)
    }

    /// The service's indexer list, fetched once and refreshed after [`CACHE_TTL`]. A stale
    /// list is used if a refresh fails.
    async fn indexer_list(&self) -> Result<Arc<Vec<IndexerInfo>>, SearchError> {
        let cached = self.known_indexers();
        if let Some((at, list)) = &cached
            && at.elapsed() < CACHE_TTL
        {
            return Ok(Arc::clone(list));
        }
        let fetched = match self.endpoint("indexers") {
            Ok(url) => self.get(url).await.and_then(|body| parse_indexers(&body)),
            Err(e) => Err(e),
        };
        match fetched {
            Ok(list) => {
                let list = Arc::new(list);
                *self
                    .shared
                    .indexers
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner) =
                    Some((Instant::now(), Arc::clone(&list)));
                Ok(list)
            }
            Err(e) => cached.map(|(_, list)| list).ok_or(e),
        }
    }

    /// The indexer list fetched last, however old, without fetching it.
    fn known_indexers(&self) -> Option<(Instant, Arc<Vec<IndexerInfo>>)> {
        self.shared
            .indexers
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// One request across every indexer, cached.
    async fn run(&self, query: &Query) -> Result<SearchResponse, SearchError> {
        let text = normalize(query.as_str());
        let key = format!(
            "all|{}|{}|{:?}|{text}",
            self.include_adult,
            self.converts_audio,
            query.categories()
        );
        if let Some(hit) = self.shared.cache.get(&key, Instant::now()) {
            return Ok(hit);
        }
        let (candidates, statuses) = self.fetch(&text, query.categories(), &[]).await?;
        let mut response = build_response(
            candidates,
            statuses,
            self.include_adult,
            self.converts_audio,
        );
        // Indexers rank by seeders only, which puts popular unrelated torrents first.
        rank(&mut response.results, query.as_str());
        self.shared.cache.put(key, response.clone(), Instant::now());
        Ok(response)
    }

    /// The event stream if possible, otherwise groups of indexers; cached when complete.
    async fn run_progressive(
        &self,
        query: &Query,
        on_progress: &mut (dyn FnMut(SearchProgress) + Send),
    ) -> Result<SearchResponse, SearchError> {
        let text = normalize(query.as_str());
        let key = format!(
            "progressive|{}|{}|{:?}|{text}",
            self.include_adult,
            self.converts_audio,
            query.categories()
        );
        if let Some(hit) = self.shared.cache.get(&key, Instant::now()) {
            on_progress(SearchProgress {
                response: hit.clone(),
                completed: 1,
                total: 1,
                done: true,
                cached: true,
            });
            return Ok(hit);
        }
        let mut finished = None;
        if self.streaming {
            finished = self.run_stream(query, &text, on_progress).await?;
            if finished.is_none() {
                tracing::debug!("search stream not available; asking the indexers in groups");
            }
        }
        let (response, complete) = match finished {
            Some(finished) => finished,
            None => self.run_fanout(query, &text, on_progress).await?,
        };
        // Incomplete results are not kept, so the next search tries again.
        if complete {
            self.shared.cache.put(key, response.clone(), Instant::now());
        }
        Ok(response)
    }

    /// Requests per group of indexers, merged as they finish. Also returns whether every
    /// request succeeded.
    async fn run_fanout(
        &self,
        query: &Query,
        text: &str,
        on_progress: &mut (dyn FnMut(SearchProgress) + Send),
    ) -> Result<(SearchResponse, bool), SearchError> {
        let indexers = match self.indexer_list().await {
            Ok(list) => list,
            Err(error) => {
                tracing::debug!(%error, "no indexer list; searching all indexers at once");
                Arc::new(Vec::new())
            }
        };
        let plan = plan(&indexers, self.include_adult, query.categories());
        if plan.batches.is_empty() {
            let response = self.run(query).await?;
            on_progress(SearchProgress {
                response: response.clone(),
                completed: 1,
                total: 1,
                done: true,
                cached: false,
            });
            return Ok((response, true));
        }

        let total = u32::try_from(plan.batches.len()).unwrap_or(u32::MAX);
        let mut merger = Merger::new(&plan, &indexers);
        let mut requests =
            futures_util::stream::iter(plan.batches.clone().into_iter().map(|batch| async move {
                let result = self.fetch(text, query.categories(), &batch).await;
                (batch, result)
            }))
            .buffer_unordered(MAX_CONCURRENT);
        let (mut completed, mut failed) = (0u32, 0u32);
        let mut last_error = None;
        let mut response = SearchResponse::default();
        while let Some((batch, result)) = requests.next().await {
            completed += 1;
            match result {
                Ok((candidates, statuses)) => merger.add(candidates, statuses),
                Err(error) => {
                    tracing::debug!(%error, indexers = batch.len(), "indexer request failed");
                    merger.fail(&batch, &error);
                    failed += 1;
                    last_error = Some(error);
                }
            }
            response = merger.response(self.include_adult, self.converts_audio, query.as_str());
            on_progress(SearchProgress {
                response: response.clone(),
                completed,
                total,
                done: completed == total,
                cached: false,
            });
        }
        if failed == total
            && let Some(error) = last_error
        {
            return Err(error);
        }
        Ok((response, failed == 0))
    }

    /// A search over the event stream. Returns the response and whether it is complete, or
    /// `None` if the stream could not be used before it delivered anything (the caller falls
    /// back to the fan-out then).
    async fn run_stream(
        &self,
        query: &Query,
        text: &str,
        on_progress: &mut (dyn FnMut(SearchProgress) + Send),
    ) -> Result<Option<(SearchResponse, bool)>, SearchError> {
        let deadline = Instant::now() + STREAM_DEADLINE;
        let url = self.search_url("search/stream", text, query.categories(), &[])?;
        let read = pin!(async {
            let Some(response) = self.open_stream(url, deadline).await? else {
                return Ok(None);
            };
            self.read_stream(response, query, deadline, on_progress)
                .await
        });
        // The indexer list (kept for 10 minutes) says how many indexers to expect. It is
        // fetched alongside the stream so that it never delays the first results.
        let list = pin!(async {
            if let Err(error) = self.indexer_list().await {
                tracing::debug!(%error, "no indexer list for the search stream");
            }
        });
        match select(read, list).await {
            Either::Left((outcome, _)) => outcome,
            Either::Right(((), read)) => read.await,
        }
    }

    /// Opens the event stream. `None` means the service does not offer it in a form TorrentPlayer
    /// reads. Retries once if rate limited.
    async fn open_stream(
        &self,
        url: Url,
        deadline: Instant,
    ) -> Result<Option<reqwest::Response>, SearchError> {
        let mut retried = false;
        loop {
            let response = match tokio::time::timeout_at(
                deadline,
                self.send(url.clone(), "text/event-stream"),
            )
            .await
            {
                Err(_) => {
                    tracing::debug!("the search stream did not start in time");
                    return Ok(None);
                }
                Ok(sent) => sent.map_err(network_error)?,
            };
            match response.status().as_u16() {
                200 => {}
                429 if !retried => {
                    retried = true;
                    tokio::time::sleep(RATE_LIMIT_RETRY).await;
                    continue;
                }
                // The same answers as `/search`: trying the fan-out would not help.
                status @ (422 | 429 | 502..=504) => return Err(status_error(status)),
                status => {
                    tracing::debug!(status, "the search stream is not offered");
                    return Ok(None);
                }
            }
            let event_stream = response
                .headers()
                .get(reqwest::header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .is_some_and(|v| {
                    v.trim_start()
                        .to_ascii_lowercase()
                        .starts_with("text/event-stream")
                });
            if !event_stream {
                tracing::debug!("the search stream has an unexpected content type");
                return Ok(None);
            }
            return Ok(Some(response));
        }
    }

    /// Reads the stream until its `done` event, reporting progress after each indexer.
    async fn read_stream(
        &self,
        mut response: reqwest::Response,
        query: &Query,
        deadline: Instant,
        on_progress: &mut (dyn FnMut(SearchProgress) + Send),
    ) -> Result<Option<(SearchResponse, bool)>, SearchError> {
        let mut parser = SseParser::default();
        let mut search = StreamSearch::new(
            query.as_str(),
            &self.base,
            self.include_adult,
            self.converts_audio,
        );
        let mut events = Vec::new();
        let ended = loop {
            let chunk = match tokio::time::timeout_at(deadline, response.chunk()).await {
                Err(_) => break "the search stream took too long",
                Ok(Err(error)) => {
                    tracing::debug!(error = %error.without_url(), "the search stream broke");
                    break "the search stream broke";
                }
                Ok(Ok(None)) => break "the search stream ended without a result",
                Ok(Ok(Some(chunk))) => chunk,
            };
            if let Err(error) = parser.feed(&chunk, &mut events) {
                tracing::debug!(%error, "search stream refused");
                break "the search stream was too large";
            }
            for event in events.drain(..) {
                match search.handle(event) {
                    Handled::Nothing => {}
                    Handled::Indexer | Handled::Resolved => {
                        on_progress(search.progress(self.indexer_count(query.categories()), false));
                    }
                    Handled::Done => {
                        let progress =
                            search.progress(self.indexer_count(query.categories()), true);
                        let response = progress.response.clone();
                        on_progress(progress);
                        return Ok(Some((response, true)));
                    }
                    Handled::Failed if search.started => {
                        tracing::debug!("the search service reported an error mid-stream");
                        return Ok(Some(self.end_early(search, query, on_progress)));
                    }
                    Handled::Failed => {
                        return Err(SearchError::Unavailable(
                            "The search service is temporarily unavailable. Try again in a minute."
                                .into(),
                        ));
                    }
                    Handled::Unreadable => return Ok(None),
                }
            }
        };
        tracing::debug!(reason = ended, "search stream ended early");
        if search.started {
            Ok(Some(self.end_early(search, query, on_progress)))
        } else {
            Ok(None)
        }
    }

    /// Finishes a stream that ended before its `done` event with what arrived: indexers that
    /// had not answered are marked as such, and the response is not complete.
    fn end_early(
        &self,
        mut search: StreamSearch<'_>,
        query: &Query,
        on_progress: &mut (dyn FnMut(SearchProgress) + Send),
    ) -> (SearchResponse, bool) {
        if let Some((_, list)) = self.known_indexers() {
            let asked: Vec<IndexerInfo> = list
                .iter()
                .filter(|i| carries(i, query.categories()))
                .cloned()
                .collect();
            search.mark_unanswered(&asked);
        }
        let progress = search.progress(self.indexer_count(query.categories()), true);
        let response = progress.response.clone();
        on_progress(progress);
        (response, false)
    }

    /// How many indexers a stream reports (those carrying the search's categories), if the
    /// indexer list has been fetched.
    fn indexer_count(&self, categories: &[u32]) -> u32 {
        self.known_indexers().map_or(0, |(_, list)| {
            let count = list.iter().filter(|i| carries(i, categories)).count();
            u32::try_from(count).unwrap_or(u32::MAX)
        })
    }
}

impl SearchProvider for EmberglowProvider {
    // The trait allows borrowed names; this provider's is a constant.
    #[allow(clippy::unnecessary_literal_bound)]
    fn name(&self) -> &str {
        "emberglow"
    }

    fn search<'a>(
        &'a self,
        query: &'a Query,
    ) -> BoxFuture<'a, Result<SearchResponse, SearchError>> {
        Box::pin(self.run(query))
    }

    fn search_progressive<'a>(
        &'a self,
        query: &'a Query,
        on_progress: &'a mut (dyn FnMut(SearchProgress) + Send),
    ) -> BoxFuture<'a, Result<SearchResponse, SearchError>> {
        Box::pin(self.run_progressive(query, on_progress))
    }
}

/// What one stream event did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Handled {
    /// Nothing that changes the results: an unknown, skipped, or unreadable event.
    Nothing,
    /// An indexer answered.
    Indexer,
    /// The service looked up the magnet link of a result offered as a download link.
    Resolved,
    /// The search finished.
    Done,
    /// The service reported that the search failed.
    Failed,
    /// The first event was not in the documented form: the stream is not usable.
    Unreadable,
}

/// The results of a streamed search so far.
#[derive(Debug)]
struct StreamSearch<'q> {
    query: &'q str,
    /// The service, whose download links are kept.
    base: &'q Url,
    include_adult: bool,
    converts_audio: bool,
    merger: Merger,
    /// Indexers that have answered, by id (or by name, without one).
    answered: HashSet<String>,
    response: SearchResponse,
    /// Whether an event in the documented form has arrived.
    started: bool,
}

impl<'q> StreamSearch<'q> {
    fn new(query: &'q str, base: &'q Url, include_adult: bool, converts_audio: bool) -> Self {
        Self {
            query,
            base,
            include_adult,
            converts_audio,
            merger: Merger::default(),
            answered: HashSet::new(),
            response: SearchResponse::default(),
            started: false,
        }
    }

    fn handle(&mut self, event: SseEvent) -> Handled {
        let SseEvent::Event { name, data } = event else {
            tracing::debug!("skipped an oversized search stream event");
            return Handled::Nothing;
        };
        match name.as_str() {
            "indexer" => match serde_json::from_slice::<RawStreamIndexer>(&data) {
                Ok(raw) => {
                    self.started = true;
                    self.add_indexer(raw);
                    Handled::Indexer
                }
                Err(error) => self.unreadable(&error),
            },
            "done" => match serde_json::from_slice::<RawResponse>(&data) {
                Ok(raw) => {
                    self.started = true;
                    self.finish(raw);
                    Handled::Done
                }
                // Without a readable `done`, the stream has ended early.
                Err(error) if self.started => {
                    tracing::debug!(%error, "malformed search stream result");
                    Handled::Failed
                }
                Err(error) => self.unreadable(&error),
            },
            "magnet" if self.started => match serde_json::from_slice::<RawMagnet>(&data) {
                Ok(raw) if self.add_magnet(&raw) => Handled::Resolved,
                Ok(_) => Handled::Nothing,
                Err(error) => {
                    tracing::debug!(%error, "malformed search stream magnet");
                    Handled::Nothing
                }
            },
            "error" => Handled::Failed,
            _ => Handled::Nothing,
        }
    }

    /// A malformed event: the stream is unusable if nothing readable came before it, and the
    /// event is skipped otherwise.
    fn unreadable(&self, error: &serde_json::Error) -> Handled {
        tracing::debug!(%error, "malformed search stream event");
        if self.started {
            Handled::Nothing
        } else {
            Handled::Unreadable
        }
    }

    fn add_indexer(&mut self, raw: RawStreamIndexer) {
        let key = raw.id.clone().unwrap_or_else(|| raw.name.clone());
        let room = MAX_STREAM_RESULTS.saturating_sub(self.merger.candidates());
        let mut candidates = candidates(raw.results, self.base);
        let found = u32::try_from(candidates.len()).unwrap_or(u32::MAX);
        candidates.truncate(room);
        let added = !candidates.is_empty();
        self.merger.add(
            candidates,
            vec![IndexerStatus {
                id: raw.id,
                name: raw.name,
                results: found,
                error: raw.error,
            }],
        );
        self.answered.insert(key);
        if added {
            self.response =
                self.merger
                    .response(self.include_adult, self.converts_audio, self.query);
        } else {
            // Only the statuses changed: no need to merge and rank again.
            self.response.indexers = self.merger.statuses();
        }
    }

    /// Fills in the magnet link the service looked up for a result offered as a download
    /// link (matched by that link), so the result keeps its place and key. Returns whether a
    /// result changed.
    fn add_magnet(&mut self, raw: &RawMagnet) -> bool {
        let Some(download) = raw
            .download
            .as_deref()
            .and_then(|d| own_download_link(self.base, d))
        else {
            return false;
        };
        if raw.magnet.is_none() && raw.infohash.is_none() {
            // The lookup failed; the download link still works later.
            return false;
        }
        if !self
            .merger
            .resolve_download(&download, raw.magnet.as_deref(), raw.infohash.as_deref())
        {
            return false;
        }
        self.response = self
            .merger
            .response(self.include_adult, self.converts_audio, self.query);
        true
    }

    /// Adds the `done` event: the service's merged list (which repeats results already seen,
    /// merged again by info hash) and every indexer's outcome.
    fn finish(&mut self, raw: RawResponse) {
        let room = MAX_STREAM_RESULTS.saturating_sub(self.merger.candidates());
        let mut more = candidates(raw.results, self.base);
        more.truncate(room);
        self.merger.add(more, Vec::new());
        if !raw.indexers.is_empty() {
            self.merger.set_statuses(statuses(raw.indexers));
        }
        self.response = self
            .merger
            .response(self.include_adult, self.converts_audio, self.query);
    }

    /// Marks listed indexers that never answered.
    fn mark_unanswered(&mut self, list: &[IndexerInfo]) {
        let missing = list
            .iter()
            .filter(|i| !self.answered.contains(&i.id))
            .map(|i| IndexerStatus {
                id: Some(i.id.clone()),
                name: i.name.clone(),
                results: 0,
                error: Some(ENDED_EARLY.into()),
            })
            .collect();
        self.merger.add(Vec::new(), missing);
        self.response.indexers = self.merger.statuses();
    }

    fn progress(&self, indexers: u32, done: bool) -> SearchProgress {
        let completed = u32::try_from(self.answered.len()).unwrap_or(u32::MAX);
        SearchProgress {
            response: self.response.clone(),
            completed,
            total: if indexers == 0 {
                0
            } else {
                indexers.max(completed)
            },
            done,
            cached: false,
        }
    }
}

/// The user-facing error for an HTTP status other than 200.
fn status_error(status: u16) -> SearchError {
    match status {
        422 => SearchError::InvalidQuery("The search service did not accept that query.".into()),
        429 => SearchError::RateLimited,
        502..=504 => SearchError::Unavailable(
            "The search service is temporarily unavailable. Try again in a minute.".into(),
        ),
        other => SearchError::Unavailable(format!(
            "The search service returned an unexpected response (HTTP {other})."
        )),
    }
}

fn network_error(error: reqwest::Error) -> SearchError {
    let timeout = error.is_timeout();
    // Display hides the underlying DNS, proxy, and TLS failures. Keep the source chain in
    // diagnostics, removing the request URL so search terms and download tokens stay private.
    tracing::debug!(error = ?error.without_url(), "search request failed");
    if timeout {
        SearchError::Unavailable("The search timed out. Try again, or try fewer words.".into())
    } else {
        SearchError::Unavailable(
            "Could not reach the search service. Check your connection.".into(),
        )
    }
}

/// The response body (and the stream's `done` event). Unknown fields are ignored; missing
/// optional fields become `None`.
#[derive(Debug, Deserialize)]
struct RawResponse {
    #[serde(default)]
    results: Vec<RawResult>,
    #[serde(default)]
    indexers: Vec<RawIndexer>,
}

#[derive(Debug, Deserialize)]
struct RawResult {
    #[serde(default)]
    title: String,
    tracker: Option<String>,
    category: Option<String>,
    /// Torznab category ids. Kept as raw values, so an odd one cannot reject the response.
    #[serde(default)]
    categories: Vec<serde_json::Value>,
    size: Option<u64>,
    seeders: Option<u32>,
    peers: Option<u32>,
    /// The seed count is a placeholder (32767) that fake listings use.
    suspicious: Option<bool>,
    /// `good` (10 or more seeders), `low`, `dead`, or `unknown`.
    health: Option<String>,
    published: Option<String>,
    infohash: Option<String>,
    magnet: Option<String>,
    /// A link on the service that resolves the torrent, when there is no magnet link.
    download: Option<String>,
    details: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RawIndexer {
    id: Option<String>,
    #[serde(default)]
    name: String,
    #[serde(default)]
    results: u32,
    error: Option<String>,
}

/// A stream's `magnet` event: the magnet link the service looked up for a result offered as
/// a download link (all `null` but `error` if the lookup failed).
#[derive(Debug, Deserialize)]
struct RawMagnet {
    download: Option<String>,
    magnet: Option<String>,
    infohash: Option<String>,
}

/// A stream's `indexer` event: one indexer's own results, not merged with the others'.
#[derive(Debug, Deserialize)]
struct RawStreamIndexer {
    id: Option<String>,
    #[serde(default)]
    name: String,
    error: Option<String>,
    #[serde(default)]
    results: Vec<RawResult>,
}

/// The `/indexers` body.
#[derive(Debug, Deserialize)]
struct RawIndexerList {
    #[serde(default)]
    indexers: Vec<RawIndexerInfo>,
}

#[derive(Debug, Deserialize)]
struct RawIndexerInfo {
    #[serde(default)]
    id: String,
    #[serde(default)]
    name: String,
    /// Top-level Torznab categories it carries.
    #[serde(default)]
    categories: Vec<serde_json::Value>,
    status: Option<String>,
    last_seconds: Option<f64>,
}

/// Unvalidated candidates from raw results. Placeholder seed counts become unknown, so they
/// never rank as well seeded.
fn candidates(raw: Vec<RawResult>, base: &Url) -> Vec<Candidate> {
    raw.into_iter()
        .map(|r| Candidate {
            seeders: r.seeders.filter(|_| r.suspicious != Some(true)),
            adult: category_ids(&r.categories).any(|id| (6000..=6999).contains(&id)),
            category_kind: kind_of_category_ids(category_ids(&r.categories)),
            title: r.title,
            indexer: r.tracker,
            category: r.category,
            size: r.size,
            peers: r.peers,
            published: r.published,
            infohash: r.infohash,
            magnet: r.magnet,
            download: r
                .download
                .as_deref()
                .and_then(|d| own_download_link(base, d)),
            details: r.details,
            license: None,
            health: r.health,
            suspicious: r.suspicious == Some(true),
        })
        .collect()
}

fn statuses(raw: Vec<RawIndexer>) -> Vec<IndexerStatus> {
    raw.into_iter()
        .map(|i| IndexerStatus {
            id: i.id,
            name: i.name,
            results: i.results,
            error: i.error,
        })
        .collect()
}

/// `link` if it is one of the service's own download links: the same scheme, host, and port
/// as `base`, under `/download/`, with no credentials. Download links are followed without the
/// user asking (to preload the best pick), so links anywhere else are never kept.
fn own_download_link(base: &Url, link: &str) -> Option<String> {
    let url = Url::parse(link.trim()).ok()?;
    let prefix = base.join("download/").ok()?;
    let own = url.scheme() == prefix.scheme()
        && url.host_str() == prefix.host_str()
        && url.port_or_known_default() == prefix.port_or_known_default()
        && url.username().is_empty()
        && url.password().is_none()
        && url.path().len() > prefix.path().len()
        && url.path().starts_with(prefix.path());
    own.then(|| url.into())
}

/// Parses a search response body into unvalidated candidates and statuses.
fn parse_raw(body: &[u8], base: &Url) -> Result<(Vec<Candidate>, Vec<IndexerStatus>), SearchError> {
    let raw: RawResponse = serde_json::from_slice(body).map_err(|error| {
        tracing::debug!(%error, "malformed search response");
        SearchError::BadResponse
    })?;
    Ok((candidates(raw.results, base), statuses(raw.indexers)))
}

/// Parses and validates a search response body (results sorted by seeders, not ranked).
#[cfg(test)]
fn parse(body: &[u8], include_adult: bool) -> Result<SearchResponse, SearchError> {
    let base = Url::parse(DEFAULT_BASE_URL).map_err(|_| SearchError::BadResponse)?;
    let (candidates, indexers) = parse_raw(body, &base)?;
    Ok(build_response(candidates, indexers, include_adult, false))
}

/// Parses the `/indexers` body. Ids are validated when the search is planned.
fn parse_indexers(body: &[u8]) -> Result<Vec<IndexerInfo>, SearchError> {
    let raw: RawIndexerList = serde_json::from_slice(body).map_err(|error| {
        tracing::debug!(%error, "malformed indexer list");
        SearchError::BadResponse
    })?;
    Ok(raw
        .indexers
        .into_iter()
        .map(|i| IndexerInfo {
            healthy: i.status.as_deref() != Some("skipped"),
            excluded: i.status.as_deref() == Some("excluded"),
            adult_only: {
                let mut ids = category_ids(&i.categories).peekable();
                ids.peek().is_some() && ids.all(|id| id == 6000)
            },
            categories: category_ids(&i.categories)
                .filter_map(|id| u32::try_from(id).ok())
                .filter(|id| (1000..=8999).contains(id))
                .take(16)
                .collect(),
            last_seconds: i.last_seconds.filter(|s| s.is_finite() && *s >= 0.0),
            name: i.name,
            id: i.id,
        })
        .collect())
}

/// The integer category ids in a raw list; anything else is ignored.
fn category_ids(raw: &[serde_json::Value]) -> impl Iterator<Item = u64> + '_ {
    raw.iter().filter_map(serde_json::Value::as_u64)
}

#[cfg(test)]
mod tests {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    use super::*;

    /// Shaped like real emberglow responses (October 2026).
    const FIXTURE: &str = r#"{
      "query": "ubuntu 24.04",
      "count": 6,
      "results": [
        {"title": "ubuntu-24.04.1-desktop-amd64.iso", "tracker": "ExampleTracker", "category": "PC/ISO",
         "size": 6114656256, "seeders": 412, "peers": 430, "published": "2026-08-29T00:00:00Z",
         "infohash": "3f9aac158c7de8dfcab171ea58a17aabdf7fbc93",
         "magnet": "magnet:?xt=urn:btih:3f9aac158c7de8dfcab171ea58a17aabdf7fbc93&dn=ubuntu",
         "details": "https://tracker.example/torrent/12345"},
        {"title": "ubuntu-24.04.1-desktop-amd64.iso", "tracker": "OtherTracker", "category": "PC/ISO",
         "size": 6114656256, "seeders": 120, "peers": 140, "published": "2026-08-29T00:00:00Z",
         "infohash": "3F9AAC158C7DE8DFCAB171EA58A17AABDF7FBC93", "magnet": null,
         "details": "javascript:alert(1)"},
        {"title": "Some adult video", "tracker": "TheRARBG", "category": "XXX", "size": 1,
         "seeders": 900, "peers": 1000, "published": "2026-09-28T01:34:03+00:00",
         "infohash": "1111111111111111111111111111111111111111", "magnet": null,
         "details": "https://example.org/x"},
        {"title": "Big Buck Bunny", "tracker": "LimeTorrents", "category": "Movies",
         "size": 276134947, "seeders": 35, "peers": 40, "published": "2026-01-01T00:00:00Z",
         "infohash": "dd8255ecdc7ca55fb0bbf81323d87062db1f6d1c", "magnet": null,
         "details": "https://www.limetorrents.example/bbb"},
        {"title": "No way to add this", "tracker": "TorrentGalaxyClone", "category": "Other",
         "size": 10, "seeders": 5, "peers": 5, "published": "2026-01-01T00:00:00Z",
         "infohash": null, "magnet": null, "details": "https://example.org/y"},
        {"title": "LONGTITLE", "tracker": "Nyaa.si", "category": "Anime",
         "size": 10, "seeders": null, "peers": null, "published": null,
         "infohash": "2222222222222222222222222222222222222222", "magnet": null, "details": null,
         "unexpected": {"nested": true}}
      ],
      "indexers": [
        {"id": "exampletracker", "name": "ExampleTracker", "results": 1, "error": null},
        {"id": "eztv", "name": "EZTV", "results": 0, "error": "timed out after 20s"}
      ]
    }"#;

    fn fixture() -> String {
        FIXTURE.replace("LONGTITLE", &"L".repeat(1000))
    }

    #[test]
    fn parses_filters_and_merges_a_realistic_response() {
        let r = parse(fixture().as_bytes(), false).unwrap();
        assert_eq!(r.indexers[0].id.as_deref(), Some("exampletracker"));
        assert_eq!(r.excluded_adult, 1);
        assert_eq!(r.dropped, 1, "the result with no magnet and no info hash");
        let titles: Vec<&str> = r.results.iter().map(|x| x.title.as_str()).collect();
        assert_eq!(titles.len(), 3, "{titles:?}");
        assert_eq!(r.results[0].indexers, ["ExampleTracker", "OtherTracker"]);
        assert_eq!(
            r.results[0].details_url.as_deref(),
            Some("https://tracker.example/torrent/12345")
        );
        assert_eq!(r.results[1].title, "Big Buck Bunny");
        assert!(r.results[1].link.starts_with(
            "magnet:?xt=urn:btih:dd8255ecdc7ca55fb0bbf81323d87062db1f6d1c&dn=Big+Buck+Bunny"
        ));
        assert_eq!(
            r.results[2].title.chars().count(),
            300,
            "over-long title truncated"
        );
        assert_eq!(r.results[2].seeders, None);
        assert!(r.results.iter().all(|x| x.license.is_none()));
        assert_eq!(r.indexers[1].error.as_deref(), Some("timed out after 20s"));
    }

    #[test]
    fn adult_results_can_be_included() {
        let r = parse(fixture().as_bytes(), true).unwrap();
        assert_eq!(r.excluded_adult, 0);
        assert_eq!(r.results[0].title, "Some adult video");
    }

    #[test]
    fn rejects_malformed_bodies() {
        assert_eq!(parse(b"not json", false), Err(SearchError::BadResponse));
        assert_eq!(
            parse(br#"{"results": [{"title": 5}]}"#, false),
            Err(SearchError::BadResponse)
        );
        assert_eq!(parse(b"{}", false).map(|r| r.results.len()), Ok(0));
    }

    #[test]
    fn adult_category_ids_hide_results_whatever_their_label() {
        let body = format!(
            r#"{{"results": [{{"title": "Clip", "category": "Other", "categories": [8000, 6010],
                "seeders": 3, "infohash": "{}"}}, {{"title": "Film", "category": "Movies",
                "categories": [2000, "?"], "seeders": 3, "infohash": "{}"}}]}}"#,
            "a".repeat(40),
            "b".repeat(40)
        );
        let r = parse(body.as_bytes(), false).unwrap();
        assert_eq!(r.excluded_adult, 1);
        assert_eq!(r.results.len(), 1);
        assert_eq!(r.results[0].title, "Film");
    }

    #[test]
    fn placeholder_seed_counts_count_as_unknown() {
        let body = format!(
            r#"{{"results": [{{"title": "Fake", "seeders": 32767, "suspicious": true,
                "infohash": "{}"}}]}}"#,
            "a".repeat(40)
        );
        let r = parse(body.as_bytes(), false).unwrap();
        assert_eq!(r.results[0].seeders, None);
    }

    #[test]
    fn builds_the_search_url() {
        let p = EmberglowProvider::new().unwrap();
        let url = p
            .search_url("search", "mit 6.006 & more", &[], &[])
            .unwrap();
        assert_eq!(
            url.as_str(),
            "https://search.emberglow.tech/search?q=mit+6.006+%26+more&limit=100"
        );
        let url = p
            .search_url("search", "sintel", &[], &["therarbg".into(), "yts".into()])
            .unwrap();
        assert_eq!(
            url.as_str(),
            "https://search.emberglow.tech/search?q=sintel&limit=100&indexer=therarbg&indexer=yts"
        );
        let url = p.search_url("search/stream", "sintel", &[], &[]).unwrap();
        assert_eq!(
            url.as_str(),
            "https://search.emberglow.tech/search/stream?q=sintel&limit=100"
        );
        let url = p
            .search_url("search/stream", "sintel", &[2000, 5070], &[])
            .unwrap();
        assert_eq!(
            url.as_str(),
            "https://search.emberglow.tech/search/stream?q=sintel&limit=100&category=2000&category=5070"
        );
    }

    #[test]
    fn parses_the_indexer_list() {
        let body = br#"{"count": 3, "indexers": [
            {"id": "therarbg", "name": "TheRARBG", "language": "en-US", "categories": [2000],
             "status": "ok", "failures": 0, "last_seconds": 0.2},
            {"id": "torrentbyte", "name": "TorrentByte", "status": "skipped", "failures": 3,
             "last_seconds": null},
            {"id": "weird", "name": "W", "status": "ok", "last_seconds": -4,
             "categories": [6000, "x"]},
            {"id": "1337x", "name": "1337x", "status": "excluded", "categories": [2000, 6000]}
        ]}"#;
        let list = parse_indexers(body).unwrap();
        assert_eq!(list.len(), 4);
        assert!(
            !list[0].adult_only && list[2].adult_only,
            "only category 6000"
        );
        assert!(list[3].excluded && !list[3].adult_only && !list[0].excluded);
        assert_eq!(list[0].id, "therarbg");
        assert!(list[0].healthy);
        assert_eq!(list[0].last_seconds, Some(0.2));
        assert!(!list[1].healthy);
        assert_eq!(list[2].last_seconds, None, "nonsense timings are ignored");
        assert_eq!(
            parse_indexers(br#"{"indexers": 5}"#),
            Err(SearchError::BadResponse)
        );
    }

    // --- The event stream ---------------------------------------------------------------

    const SINTEL: &str = "08ada5a7a6183aae1e09d831df6748d566095a10";
    const SINTEL_720: &str = "dd8255ecdc7ca55fb0bbf81323d87062db1f6d1c";

    /// One result in the service's format.
    fn result_json(title: &str, hash: &str, seeders: u32, tracker: &str) -> String {
        format!(
            r#"{{"title": "{title}", "tracker": "{tracker}", "trackers": ["{tracker}"],
                "category": "Movies", "categories": [2000], "size": 1000000000,
                "seeders": {seeders}, "peers": 3, "health": "good", "suspicious": false,
                "published": "2026-01-01T00:00:00Z", "infohash": "{hash}", "magnet": null,
                "download": null, "details": null, "release": {{"resolution": null}}}}"#
        )
        .replace('\n', " ")
    }

    fn low() -> String {
        result_json("Sintel 2010 720p WEB-DL x264 mp4", SINTEL_720, 20, "A")
    }

    fn high() -> String {
        result_json("Sintel 2010 1080p WEB-DL x264 mp4", SINTEL, 400, "B")
    }

    /// The data of an `indexer` event.
    fn indexer_data(id: &str, results: &[String]) -> String {
        format!(
            r#"{{"id": "{id}", "name": "{}", "error": null, "seconds": 0.4, "count": {}, "results": [{}]}}"#,
            id.to_uppercase(),
            results.len(),
            results.join(",")
        )
    }

    fn indexer_event(id: &str, results: &[String]) -> String {
        format!("event: indexer\ndata: {}\n\n", indexer_data(id, results))
    }

    fn done_event(results: &[String]) -> String {
        let data = format!(
            r#"{{"query": "sintel", "count": {}, "results": [{}], "indexers": [{}, {}]}}"#,
            results.len(),
            results.join(","),
            r#"{"id": "a", "name": "A", "results": 1, "error": null}"#,
            r#"{"id": "b", "name": "B", "results": 1, "error": null}"#,
        );
        format!("event: done\ndata: {data}\n\n")
    }

    /// A complete stream: indexer `a` finds a 720p copy, then `b` a better 1080p one.
    fn full_stream() -> String {
        format!(
            ": comment\n\n{}{}{}",
            indexer_event("a", &[low()]),
            indexer_event("b", &[high()]),
            done_event(&[high(), low()])
        )
    }

    const INDEXERS: &str = r#"{"indexers": [
        {"id": "a", "name": "A", "status": "ok", "last_seconds": 0.1},
        {"id": "b", "name": "B", "status": "ok", "last_seconds": 0.2}]}"#;

    /// The `/search` body the fan-out gets.
    fn search_body() -> String {
        format!(
            r#"{{"results": [{}], "indexers": [{{"id": "a", "name": "A", "results": 1, "error": null}}]}}"#,
            high()
        )
    }

    type Reply = (u16, &'static str, String);
    type Respond = fn(&str) -> Reply;

    /// Answers `/indexers` and `/search` like the service, and the stream with `stream`.
    fn answer(path: &str, stream: Reply) -> Reply {
        if path.starts_with("/search/stream") {
            stream
        } else if path.starts_with("/indexers") {
            (200, "application/json", INDEXERS.into())
        } else {
            (200, "application/json", search_body())
        }
    }

    /// A minimal HTTP server: answers each request with `respond(path)` and records the
    /// paths asked for.
    struct Server {
        base: Url,
        paths: Arc<Mutex<Vec<String>>>,
    }

    impl Server {
        async fn start(respond: Respond) -> Self {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let base = Url::parse(&format!("http://{}/", listener.local_addr().unwrap())).unwrap();
            let paths = Arc::new(Mutex::new(Vec::new()));
            let seen = Arc::clone(&paths);
            let base_text = base.to_string();
            tokio::spawn(async move {
                while let Ok((mut socket, _)) = listener.accept().await {
                    let seen = Arc::clone(&seen);
                    let base_text = base_text.clone();
                    tokio::spawn(async move {
                        let mut head = Vec::new();
                        let mut buf = [0u8; 1024];
                        while !head.windows(4).any(|w| w == b"\r\n\r\n") {
                            match socket.read(&mut buf).await {
                                Ok(0) | Err(_) => return,
                                Ok(n) => head.extend_from_slice(&buf[..n]),
                            }
                        }
                        let path = String::from_utf8_lossy(&head)
                            .split_whitespace()
                            .nth(1)
                            .unwrap_or_default()
                            .to_owned();
                        seen.lock().unwrap().push(path.clone());
                        let (status, content_type, body) = respond(&path);
                        let body = body.replace("BASE/", base_text.as_str());
                        let reply = format!(
                            "HTTP/1.1 {status} X\r\nContent-Type: {content_type}\r\n\
                             Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                            body.len()
                        );
                        let _ = socket.write_all(reply.as_bytes()).await;
                        let _ = socket.shutdown().await;
                    });
                }
            });
            Self { base, paths }
        }

        fn provider(&self) -> EmberglowProvider {
            EmberglowProvider::with_base_url(self.base.clone()).unwrap()
        }

        /// How many requests went to paths starting with `prefix`.
        fn asked(&self, prefix: &str) -> usize {
            let paths = self.paths.lock().unwrap();
            paths.iter().filter(|p| p.starts_with(prefix)).count()
        }
    }

    async fn search(
        provider: &EmberglowProvider,
    ) -> (Result<SearchResponse, SearchError>, Vec<SearchProgress>) {
        let query = Query::parse("Sintel").unwrap();
        let mut seen = Vec::new();
        let result = provider
            .search_progressive(&query, &mut |p| seen.push(p))
            .await;
        (result, seen)
    }

    #[tokio::test]
    async fn streams_results_as_indexers_answer() {
        let server = Server::start(|path| {
            answer(
                path,
                (200, "text/event-stream; charset=utf-8", full_stream()),
            )
        })
        .await;
        let provider = server.provider();
        let (result, seen) = search(&provider).await;
        let response = result.unwrap();
        assert_eq!(seen.len(), 3, "one report per indexer, then the end");
        assert_eq!(seen[0].response.results.len(), 1);
        assert_eq!(seen[0].response.results[0].key, SINTEL_720);
        assert!(seen[0].response.results[0].best_pick, "best so far");
        assert!(!seen[0].done && !seen[1].done && seen[2].done);
        assert_eq!(seen[1].completed, 2);
        assert_eq!(seen[2].response, response);
        assert_eq!(response.results[0].key, SINTEL);
        assert!(response.results[0].best_pick);
        assert_eq!(
            response.results.len(),
            2,
            "the done list merges with the rest"
        );
        assert_eq!(response.indexers.len(), 2);
        assert_eq!(server.asked("/search?"), 0, "no fan-out");
        assert_eq!(server.asked("/search/stream"), 1);

        // The finished search is kept: repeating it sends nothing.
        let before = provider.requests_sent();
        let (again, seen) = search(&provider).await;
        assert_eq!(again.unwrap(), response);
        assert!(seen[0].cached && seen[0].done);
        assert_eq!(provider.requests_sent(), before);
    }

    #[tokio::test]
    async fn falls_back_to_groups_when_the_stream_is_missing() {
        let server =
            Server::start(|path| answer(path, (404, "application/json", "{}".into()))).await;
        let (result, seen) = search(&server.provider()).await;
        let response = result.unwrap();
        assert_eq!(response.results[0].key, SINTEL);
        assert!(seen.last().unwrap().done);
        assert_eq!(server.asked("/search/stream"), 1);
        assert_eq!(
            server.asked("/search?q=sintel&limit=100&indexer=a&indexer=b"),
            1
        );
    }

    #[tokio::test]
    async fn falls_back_when_the_stream_is_not_readable() {
        let cases: [Respond; 4] = [
            |path| answer(path, (200, "application/json", search_body())),
            |path| answer(path, (200, "text/event-stream", "data: <html>\n\n".into())),
            |path| {
                let body = "event: indexer\ndata: {\"results\": 5}\n\n";
                answer(path, (200, "text/event-stream", body.into()))
            },
            |path| answer(path, (200, "text/event-stream", String::new())),
        ];
        for (case, respond) in cases.into_iter().enumerate() {
            let server = Server::start(respond).await;
            let (result, _) = search(&server.provider()).await;
            assert_eq!(result.unwrap().results[0].key, SINTEL, "case {case}");
            assert_eq!(server.asked("/search?"), 1, "case {case}");
        }
    }

    #[tokio::test]
    async fn query_and_service_errors_do_not_fall_back() {
        let server =
            Server::start(|path| answer(path, (422, "application/json", "{}".into()))).await;
        let (result, _) = search(&server.provider()).await;
        assert!(matches!(result, Err(SearchError::InvalidQuery(_))));
        assert_eq!(server.asked("/search?"), 0);

        let server = Server::start(|path| {
            let body = "event: error\ndata: {\"detail\": \"Jackett request failed\"}\n\n";
            answer(path, (200, "text/event-stream", body.into()))
        })
        .await;
        let (result, seen) = search(&server.provider()).await;
        assert!(matches!(result, Err(SearchError::Unavailable(_))));
        assert!(seen.is_empty());
        assert_eq!(server.asked("/search?"), 0);
    }

    #[tokio::test]
    async fn a_stream_that_breaks_keeps_what_arrived() {
        let server = Server::start(|path| {
            // Indexer `a` answers, then the connection closes in the middle of an event.
            let body = indexer_event("a", &[low()]) + "event: indexer\ndata: {\"id\": \"b\"";
            answer(path, (200, "text/event-stream", body))
        })
        .await;
        let provider = server.provider();
        provider.indexer_list().await.unwrap();
        let (result, seen) = search(&provider).await;
        let response = result.unwrap();
        assert_eq!(response.results.len(), 1);
        let last = seen.last().unwrap();
        assert!(last.done);
        assert_eq!((last.completed, last.total), (1, 2));
        let b = response.indexers.iter().find(|i| i.name == "B").unwrap();
        assert_eq!(b.error.as_deref(), Some(ENDED_EARLY));
        assert_eq!(
            server.asked("/search?"),
            0,
            "no fan-out after results arrived"
        );
        // Not kept: the next search asks again.
        search(&provider).await.0.unwrap();
        assert_eq!(server.asked("/search/stream"), 2);
    }

    #[tokio::test]
    async fn the_fan_out_can_be_chosen() {
        let server =
            Server::start(|path| answer(path, (200, "text/event-stream", full_stream()))).await;
        let (result, _) = search(&server.provider().streaming(false)).await;
        assert_eq!(result.unwrap().results[0].key, SINTEL);
        assert_eq!(server.asked("/search/stream"), 0);
        assert_eq!(server.asked("/search?"), 1);
    }

    fn base() -> Url {
        Url::parse(DEFAULT_BASE_URL).unwrap()
    }

    #[test]
    fn keeps_only_the_services_own_download_links() {
        let base = base();
        let own = "https://search.emberglow.tech/download/WyJ0b2tlbiJd";
        assert_eq!(own_download_link(&base, own).as_deref(), Some(own));
        for other in [
            "http://search.emberglow.tech/download/WyJ0b2tlbiJd",
            "https://search.emberglow.tech.evil.example/download/x",
            "https://evil.example/download/x",
            "https://user:pw@search.emberglow.tech/download/x",
            "https://search.emberglow.tech:8443/download/x",
            "https://search.emberglow.tech/search?q=x",
            "https://search.emberglow.tech/download/",
            "http://192.168.1.1/download/x",
            "file:///C:/download/x",
            "not a link",
        ] {
            assert_eq!(own_download_link(&base, other), None, "{other}");
        }
        let p = EmberglowProvider::new().unwrap();
        assert!(p.is_download_link(own));
        assert!(!p.is_download_link("magnet:?xt=urn:btih:00"));
    }

    #[test]
    fn results_offered_only_as_a_download_link_are_kept() {
        let body = r#"{"results": [
            {"title": "Lecture 1 720p mp4", "tracker": "Magnetz", "seeders": 4, "infohash": null,
             "magnet": null, "download": "https://search.emberglow.tech/download/WyJtYWduZXR6Il0"},
            {"title": "Elsewhere", "seeders": 4, "infohash": null, "magnet": null,
             "download": "https://evil.example/download/x"}]}"#;
        let r = parse(body.as_bytes(), false).unwrap();
        assert_eq!(r.dropped, 1, "a download link elsewhere is not kept");
        assert_eq!(r.results.len(), 1);
        assert_eq!(
            r.results[0].link,
            "https://search.emberglow.tech/download/WyJtYWduZXR6Il0"
        );
        assert_eq!(r.results[0].info_hash, None);
    }

    fn event(name: &str, data: &str) -> SseEvent {
        SseEvent::Event {
            name: name.into(),
            data: data.as_bytes().to_vec(),
        }
    }

    #[test]
    fn skips_bad_events_once_the_stream_has_started() {
        let base = base();
        let mut s = StreamSearch::new("sintel", &base, false, false);
        assert_eq!(
            s.handle(event("ping", "{}")),
            Handled::Nothing,
            "unknown type"
        );
        assert_eq!(s.handle(SseEvent::Oversized), Handled::Nothing);
        assert_eq!(
            s.handle(event("indexer", "nope")),
            Handled::Unreadable,
            "a malformed first event"
        );
        let first = indexer_data("a", &[low()]);
        assert_eq!(s.handle(event("indexer", &first)), Handled::Indexer);
        assert_eq!(
            s.handle(event("indexer", "nope")),
            Handled::Nothing,
            "later"
        );
        assert_eq!(
            s.handle(event("indexer", r#"{"results": [1]}"#)),
            Handled::Nothing
        );
        assert_eq!(s.handle(SseEvent::Oversized), Handled::Nothing);
        assert_eq!(
            s.handle(event("done", "{")),
            Handled::Failed,
            "unreadable end"
        );
        assert_eq!(s.response.results.len(), 1);
        assert_eq!(
            s.progress(0, false).total,
            0,
            "unknown without an indexer list"
        );
        assert_eq!(s.progress(5, false).total, 5);
    }

    #[test]
    fn caps_the_results_a_stream_can_add() {
        let many: Vec<String> = (0..MAX_STREAM_RESULTS + 10)
            .map(|i| format!(r#"{{"title": "Sintel {i}", "seeders": 1, "infohash": "{i:040x}"}}"#))
            .collect();
        let data = format!(
            r#"{{"id": "a", "name": "A", "results": [{}]}}"#,
            many.join(",")
        );
        let base = base();
        let mut s = StreamSearch::new("sintel", &base, false, false);
        assert_eq!(s.handle(event("indexer", &data)), Handled::Indexer);
        assert_eq!(s.merger.candidates(), MAX_STREAM_RESULTS);
        assert_eq!(s.response.results.len(), crate::fanout::MAX_RESULTS);
        let again = data.replacen("\"a\"", "\"b\"", 1);
        assert_eq!(s.handle(event("indexer", &again)), Handled::Indexer);
        assert_eq!(s.merger.candidates(), MAX_STREAM_RESULTS, "nothing more");
        assert_eq!(s.response.indexers.len(), 2, "statuses are still recorded");
        assert_eq!(
            s.response.indexers[0].results, 5_010,
            "what the indexer found"
        );
    }

    // --- Magnet links the stream looks up later --------------------------------------------

    const LINK: &str = "https://search.emberglow.tech/download/WyJ0b3JyZW50Il0";

    /// A result offered only as a download link, in the service's format.
    fn download_json(link: &str) -> String {
        format!(
            r#"{{"title": "Sintel 2010 720p WEB-DL x264 mp4", "tracker": "TGx", "category": "Movies",
                "size": 1000000000, "seeders": 20, "peers": 3, "infohash": null, "magnet": null,
                "download": "{link}"}}"#
        )
        .replace('\n', " ")
    }

    /// A `magnet` event: the lookup's result, or a failed lookup.
    fn magnet_data(link: &str, hash: Option<&str>) -> String {
        match hash {
            Some(h) => format!(
                r#"{{"download": "{link}", "magnet": "magnet:?xt=urn:btih:{h}&dn=Sintel", "infohash": "{h}", "error": null}}"#
            ),
            None => format!(
                r#"{{"download": "{link}", "magnet": null, "infohash": null, "error": "timed out"}}"#
            ),
        }
    }

    #[test]
    fn a_late_magnet_upgrades_a_result_in_place() {
        let base = base();
        let mut s = StreamSearch::new("sintel", &base, false, false);
        assert_eq!(
            s.handle(event("magnet", &magnet_data(LINK, Some(SINTEL_720)))),
            Handled::Nothing,
            "nothing to upgrade before any results"
        );
        let first = indexer_data("tgx", &[download_json(LINK)]);
        assert_eq!(s.handle(event("indexer", &first)), Handled::Indexer);
        let key = format!("download:{LINK}");
        assert_eq!(s.response.results[0].key, key);
        assert_eq!(s.response.results[0].link, LINK);

        // Failed lookups, links elsewhere, other links, and junk change nothing.
        for data in [
            magnet_data(LINK, None),
            magnet_data("https://evil.example/download/x", Some(SINTEL_720)),
            magnet_data(
                "https://search.emberglow.tech/download/other",
                Some(SINTEL_720),
            ),
            "{".to_owned(),
        ] {
            assert_eq!(s.handle(event("magnet", &data)), Handled::Nothing, "{data}");
        }
        assert_eq!(
            s.handle(event("magnet", &magnet_data(LINK, Some(SINTEL_720)))),
            Handled::Resolved
        );
        let r = &s.response.results[0];
        assert_eq!(r.key, key, "the same row");
        assert_eq!(r.info_hash.as_deref(), Some(SINTEL_720));
        assert!(r.link.starts_with("magnet:?xt=urn:btih:"), "{}", r.link);
        assert_eq!(r.download.as_deref(), Some(LINK));

        // Another indexer's copy of the same torrent joins that row.
        assert_eq!(
            s.handle(event("indexer", &indexer_data("a", &[low()]))),
            Handled::Indexer
        );
        assert_eq!(s.response.results.len(), 1);
        assert_eq!(s.response.results[0].key, key);
        assert_eq!(s.response.results[0].indexers, ["TGx", "A"]);
        assert_eq!(
            s.handle(event("magnet", &magnet_data(LINK, Some(SINTEL_720)))),
            Handled::Nothing,
            "already known"
        );
    }

    #[test]
    fn a_late_magnet_merges_the_result_into_an_earlier_copy() {
        let base = base();
        let mut s = StreamSearch::new("sintel", &base, false, false);
        s.handle(event("indexer", &indexer_data("a", &[low()])));
        s.handle(event(
            "indexer",
            &indexer_data("tgx", &[download_json(LINK)]),
        ));
        assert_eq!(s.response.results.len(), 2);
        assert_eq!(
            s.handle(event("magnet", &magnet_data(LINK, Some(SINTEL_720)))),
            Handled::Resolved
        );
        assert_eq!(s.response.results.len(), 1);
        assert_eq!(
            s.response.results[0].key, SINTEL_720,
            "the earlier row stays"
        );
        assert_eq!(s.response.results[0].indexers, ["A", "TGx"]);
    }

    #[tokio::test]
    async fn the_stream_fills_in_late_magnets_and_keeps_the_row() {
        let server = Server::start(|path| {
            let link = "BASE/download/WyJ0Z3giXQ";
            // As the service sends it: the result, the lookup, then the merged list with the
            // magnet filled in.
            let resolved = result_json("Sintel 2010 720p WEB-DL x264 mp4", SINTEL_720, 20, "TGx");
            let body = format!(
                "{}event: magnet\ndata: {}\n\n{}",
                indexer_event("tgx", &[download_json(link)]),
                magnet_data(link, Some(SINTEL_720)),
                done_event(&[resolved]),
            );
            answer(path, (200, "text/event-stream", body))
        })
        .await;
        let (result, seen) = search(&server.provider()).await;
        let response = result.unwrap();
        let link = format!("{}download/WyJ0Z3giXQ", server.base);
        let key = format!("download:{link}");
        assert_eq!(seen.len(), 3, "the result, its magnet link, the end");
        assert_eq!(seen[0].response.results[0].link, link);
        assert_eq!(seen[1].response.results[0].key, key);
        assert!(seen[1].response.results[0].link.starts_with("magnet:"));
        assert_eq!(
            response.results.len(),
            1,
            "the done list's copy is merged in"
        );
        assert_eq!(response.results[0].key, key);
        assert_eq!(response.results[0].info_hash.as_deref(), Some(SINTEL_720));
    }
}
