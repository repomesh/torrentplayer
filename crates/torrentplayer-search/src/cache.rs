// SPDX-License-Identifier: MIT OR Apache-2.0

//! A short-lived cache of finished searches, and a client-side rate limiter.
//!
//! The emberglow API caches results for 10 minutes, so repeating a search within that time
//! returns the same results; keeping them locally makes the repeat instant and costs the
//! service nothing. The rate limiter keeps TorrentPlayer under the API's limit of 60 requests per
//! minute (burst 30) even when a search fans out to many indexers.

use std::collections::HashMap;
use std::sync::{Mutex, PoisonError};
use std::time::Duration;

use tokio::time::Instant;

use crate::results::SearchResponse;

/// How long a finished search is reused (the API's own cache lifetime).
pub const CACHE_TTL: Duration = Duration::from_secs(10 * 60);
/// Most searches kept.
const MAX_ENTRIES: usize = 32;

/// The cache key form of a query: lowercase, with runs of whitespace collapsed. The same form
/// is sent to the API, so equivalent searches also share its cache.
#[must_use]
pub fn normalize(query: &str) -> String {
    query
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

/// Finished searches by key, expiring after [`CACHE_TTL`].
#[derive(Debug, Default)]
pub(crate) struct QueryCache {
    entries: Mutex<HashMap<String, (Instant, SearchResponse)>>,
}

impl QueryCache {
    pub(crate) fn get(&self, key: &str, now: Instant) -> Option<SearchResponse> {
        let entries = self.entries.lock().unwrap_or_else(PoisonError::into_inner);
        entries
            .get(key)
            .filter(|(at, _)| now.saturating_duration_since(*at) < CACHE_TTL)
            .map(|(_, response)| response.clone())
    }

    pub(crate) fn put(&self, key: String, response: SearchResponse, now: Instant) {
        let mut entries = self.entries.lock().unwrap_or_else(PoisonError::into_inner);
        entries.retain(|_, (at, _)| now.saturating_duration_since(*at) < CACHE_TTL);
        if entries.len() >= MAX_ENTRIES
            && let Some(oldest) = entries
                .iter()
                .min_by_key(|(_, (at, _))| *at)
                .map(|(k, _)| k.clone())
        {
            entries.remove(&oldest);
        }
        entries.insert(key, (now, response));
    }
}

/// A token bucket: `capacity` requests at once, refilled at `per_minute`.
#[derive(Debug)]
pub(crate) struct RateLimiter {
    capacity: f64,
    per_sec: f64,
    bucket: Mutex<(f64, Instant)>,
}

impl RateLimiter {
    /// Below the API's documented 60 per minute and burst of 30, leaving room for other
    /// TorrentPlayer windows or a browser tab on the same connection.
    pub(crate) fn for_emberglow() -> Self {
        Self::new(20, 50)
    }

    pub(crate) fn new(capacity: u32, per_minute: u32) -> Self {
        Self {
            capacity: f64::from(capacity),
            per_sec: f64::from(per_minute.max(1)) / 60.0,
            bucket: Mutex::new((f64::from(capacity), Instant::now())),
        }
    }

    /// Waits until a request may be sent.
    pub(crate) async fn acquire(&self) {
        loop {
            let wait = {
                let mut bucket = self.bucket.lock().unwrap_or_else(PoisonError::into_inner);
                let now = Instant::now();
                let (tokens, at) = *bucket;
                let tokens = (tokens
                    + now.saturating_duration_since(at).as_secs_f64() * self.per_sec)
                    .min(self.capacity);
                if tokens >= 1.0 {
                    *bucket = (tokens - 1.0, now);
                    return;
                }
                *bucket = (tokens, now);
                Duration::from_secs_f64((1.0 - tokens) / self.per_sec)
            };
            tokio::time::sleep(wait).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_case_and_whitespace() {
        assert_eq!(normalize("  MIT   6.006\tLectures "), "mit 6.006 lectures");
        assert_eq!(normalize("Ünïcode"), "ünïcode");
    }

    #[test]
    fn entries_expire_after_ten_minutes() {
        let cache = QueryCache::default();
        let t0 = Instant::now();
        let response = SearchResponse {
            excluded_adult: 7,
            ..SearchResponse::default()
        };
        cache.put("q".into(), response.clone(), t0);
        assert_eq!(
            cache.get("q", t0 + Duration::from_secs(599)),
            Some(response)
        );
        assert_eq!(cache.get("q", t0 + CACHE_TTL), None);
        assert_eq!(cache.get("other", t0), None);
    }

    #[test]
    fn evicts_the_oldest_entry_when_full() {
        let cache = QueryCache::default();
        let t0 = Instant::now();
        for i in 0..MAX_ENTRIES {
            let at = t0 + Duration::from_secs(u64::try_from(i).unwrap());
            cache.put(format!("q{i}"), SearchResponse::default(), at);
        }
        let later = t0 + Duration::from_secs(100);
        cache.put("new".into(), SearchResponse::default(), later);
        assert!(cache.get("q0", later).is_none(), "oldest evicted");
        assert!(cache.get("q1", later).is_some());
        assert!(cache.get("new", later).is_some());
    }

    #[tokio::test(start_paused = true)]
    async fn rate_limiter_allows_a_burst_then_paces() {
        let limiter = RateLimiter::new(3, 60);
        let t0 = Instant::now();
        for _ in 0..3 {
            limiter.acquire().await;
        }
        assert!(
            t0.elapsed() < Duration::from_millis(10),
            "burst is immediate"
        );
        limiter.acquire().await;
        limiter.acquire().await;
        let waited = t0.elapsed();
        assert!(
            waited >= Duration::from_millis(1990) && waited < Duration::from_millis(2100),
            "then one per second: {waited:?}"
        );
    }
}
