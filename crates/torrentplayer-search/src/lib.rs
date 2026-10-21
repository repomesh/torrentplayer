// SPDX-License-Identifier: MIT OR Apache-2.0

//! Torrent search for TorrentPlayer.
//!
//! A [`SearchProvider`] turns a validated [`Query`] into a [`SearchResponse`], either in one
//! step or progressively ([`SearchProvider::search_progressive`]), reporting merged results as
//! they arrive. The only provider today is [`EmberglowProvider`], a JSON API over Jackett that
//! searches many public indexers; it reads the service's event stream so the first results
//! show in under a second, with one request per search (falling back to small groups of
//! indexers, see [`fanout`]), keeps finished searches for 10 minutes, and paces its requests
//! below the service's rate limit (see [`cache`]). [`settle`] decides when a streamed search's
//! best pick is stable enough to preload. Internet Archive and Academic Torrents providers can
//! be added behind the same trait.
//!
//! Provider responses are untrusted input. Streams are read by a strict, bounded parser (limits
//! on event size, stream length, event count, results, and time). Every result is validated
//! and bounded before it
//! reaches the UI (see [`results`]): titles are truncated and stripped of control characters,
//! magnet links must have the expected form, details links must be http(s), and results that
//! cannot be added (no magnet and no info hash) are dropped. Adult categories are excluded by
//! default and duplicates are merged by info hash.
//!
//! Results are ranked by relevance first and then by a documented watch score: seeders and
//! swarm health, resolution, source (camera copies rank last), HDR, whether the built-in
//! player can play the container, and a size check for fake 4K (see [`rank`] and
//! [`quality`]). The top result that is safe to recommend is marked as the best pick.

#![forbid(unsafe_code)]

pub mod cache;
mod emberglow;
pub mod fanout;
pub mod quality;
mod query;
pub mod rank;
pub mod results;
pub mod settle;
mod sse;

use futures_util::future::BoxFuture;

pub use emberglow::{DEFAULT_BASE_URL, EmberglowProvider};
pub use fanout::SearchProgress;
pub use quality::{Codec, Container, ContentKind, Playability, Quality, Resolution, Source};
pub use query::{MAX_CATEGORIES, MAX_QUERY_CHARS, Query};
pub use results::{IndexerStatus, SearchResponse, SearchResult};
pub use settle::{Settle, SettleAction};

/// Why a search failed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SearchError {
    /// The query is empty, too long, or contains control characters.
    #[error("{0}")]
    InvalidQuery(String),
    /// The search service is rate limiting this connection.
    #[error("Too many searches in a short time. Wait a few seconds and try again.")]
    RateLimited,
    /// The search service is down, timed out, or returned an unexpected status.
    #[error("{0}")]
    Unavailable(String),
    /// The response was larger than [`results::MAX_RESPONSE_BYTES`].
    #[error("The search service sent a response that was too large.")]
    TooLarge,
    /// The response could not be parsed.
    #[error("The search service sent a response TorrentPlayer could not read.")]
    BadResponse,
}

/// A source of torrent search results.
pub trait SearchProvider: Send + Sync {
    /// Short name shown next to results, for example `emberglow`.
    fn name(&self) -> &str;

    /// Runs a search.
    fn search<'a>(&'a self, query: &'a Query)
    -> BoxFuture<'a, Result<SearchResponse, SearchError>>;

    /// Runs a search, calling `on_progress` with the merged results so far each time part of
    /// it finishes. The returned response equals the last progress report. Providers that
    /// cannot report partial results call `on_progress` once, when done.
    fn search_progressive<'a>(
        &'a self,
        query: &'a Query,
        on_progress: &'a mut (dyn FnMut(SearchProgress) + Send),
    ) -> BoxFuture<'a, Result<SearchResponse, SearchError>> {
        Box::pin(async move {
            let response = self.search(query).await?;
            on_progress(SearchProgress {
                response: response.clone(),
                completed: 1,
                total: 1,
                done: true,
                cached: false,
            });
            Ok(response)
        })
    }
}
