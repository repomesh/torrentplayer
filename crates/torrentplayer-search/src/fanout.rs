// SPDX-License-Identifier: MIT OR Apache-2.0

//! Progressive search: one request per group of indexers, merged as they finish.
//!
//! A search across every indexer takes as long as the slowest one (11–12 s measured in
//! October 2026, bounded by a 20 s per-indexer timeout), while most indexers answer in under a
//! second. Asking them in small groups (`indexer=a&indexer=b…`) shows the first results after
//! about half a second. Slow indexers get a request of their own so they cannot hold up the
//! fast ones, and indexers the service currently skips as unhealthy or leaves out of default
//! searches are not asked at all (an all-indexer search skips them too), nor are adult-only
//! indexers unless adult results are wanted.
//!
//! This is the fallback for when the service's event stream is unavailable (see
//! [`crate::EmberglowProvider`]); with 53 indexers (October 2026) it takes about a dozen
//! requests per search where the stream takes one.

use std::collections::{HashMap, HashSet};

use crate::SearchError;
use crate::rank::rank;
use crate::results::{
    Candidate, IndexerStatus, SearchResponse, build_response, clean_statuses, valid_indexer_id,
};

/// Indexers whose last response took longer than this get a request of their own.
pub const SLOW_SECS: f64 = 2.5;
/// Fast indexers asked per request.
pub const BATCH_SIZE: usize = 4;
/// Most requests in flight at once.
pub const MAX_CONCURRENT: usize = 12;
/// Most results kept from a progressive search, after ranking.
pub const MAX_RESULTS: usize = 150;
/// Most indexers accepted from the service's list.
const MAX_INDEXERS: usize = 200;
/// Indexers that list only adult content, left out unless adult results are wanted.
const ADULT_ONLY: &[&str] = &["ehentai"];

/// One step of a progressive search.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchProgress {
    /// Everything found so far: merged, filtered, and ranked.
    pub response: SearchResponse,
    /// Sources that have answered so far: indexers for a streamed search, groups of indexers
    /// otherwise.
    pub completed: u32,
    /// Sources in total, or 0 while unknown (a streamed search learns it from the indexer
    /// list).
    pub total: u32,
    /// Whether the search has finished.
    pub done: bool,
    /// Whether the results came from the local cache.
    pub cached: bool,
}

/// An indexer as the service lists it.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct IndexerInfo {
    pub(crate) id: String,
    pub(crate) name: String,
    /// `false` while the service skips it after repeated failures.
    pub(crate) healthy: bool,
    /// The service leaves it out of default searches (for example 1337x: slow, no magnets).
    pub(crate) excluded: bool,
    /// It lists only adult content (its only category is Torznab 6000, XXX).
    pub(crate) adult_only: bool,
    /// The top-level Torznab categories it carries (1000, 2000, …); empty if not listed.
    pub(crate) categories: Vec<u32>,
    /// How long its last search took, if known.
    pub(crate) last_seconds: Option<f64>,
}

/// The requests for one search.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct Plan {
    /// Indexer ids per request: groups of fast indexers (fastest first, so the first results
    /// come from them), then slow indexers one by one (slowest first).
    pub(crate) batches: Vec<Vec<String>>,
    /// Indexers not asked because the service reports them unhealthy.
    pub(crate) skipped: Vec<IndexerStatus>,
}

/// Whether an indexer carries any of `categories` (Torznab ids; a subcategory counts as its
/// top level), as the service decides which indexers a category search asks. An indexer with
/// no listed categories, or a search of all categories, always counts.
pub(crate) fn carries(info: &IndexerInfo, categories: &[u32]) -> bool {
    categories.is_empty()
        || info.categories.is_empty()
        || categories
            .iter()
            .any(|id| info.categories.contains(&(id / 1000 * 1000)))
}

/// Groups indexers into requests. Ids that the API would refuse are dropped, and so are
/// indexers that carry none of `categories` (when the search is limited to some).
pub(crate) fn plan(indexers: &[IndexerInfo], include_adult: bool, categories: &[u32]) -> Plan {
    let mut seen = HashSet::new();
    let mut slow: Vec<&IndexerInfo> = Vec::new();
    let mut fast: Vec<&IndexerInfo> = Vec::new();
    let mut skipped = Vec::new();
    for info in indexers.iter().take(MAX_INDEXERS) {
        if !valid_indexer_id(&info.id) || !seen.insert(info.id.as_str()) {
            continue;
        }
        if !include_adult && (info.adult_only || ADULT_ONLY.contains(&info.id.as_str())) {
            continue;
        }
        if !carries(info, categories) {
            continue;
        }
        // Naming an indexer makes the service search it even if it is excluded by default.
        if info.excluded {
            skipped.push(IndexerStatus {
                id: Some(info.id.clone()),
                name: info.name.clone(),
                results: 0,
                error: Some("Not searched by default.".into()),
            });
            continue;
        }
        if !info.healthy {
            skipped.push(IndexerStatus {
                id: Some(info.id.clone()),
                name: info.name.clone(),
                results: 0,
                error: Some("Skipped: not responding lately.".into()),
            });
            continue;
        }
        match info.last_seconds {
            Some(secs) if secs <= SLOW_SECS => fast.push(info),
            _ => slow.push(info),
        }
    }
    // Fastest first, in half-second steps and then by id: a group is as slow as its slowest
    // member, and coarse steps keep the groups (and so the URLs, which the API caches) stable
    // from one search to the next.
    fast.sort_by_key(|i| (speed_step(i.last_seconds), i.id.clone()));
    // Longest first (unknown last) so slow requests overlap with everything else.
    slow.sort_by(|a, b| {
        b.last_seconds
            .unwrap_or(-1.0)
            .total_cmp(&a.last_seconds.unwrap_or(-1.0))
            .then_with(|| a.id.cmp(&b.id))
    });
    let mut batches: Vec<Vec<String>> = fast
        .chunks(BATCH_SIZE)
        .map(|chunk| chunk.iter().map(|i| i.id.clone()).collect())
        .collect();
    batches.extend(slow.iter().map(|i| vec![i.id.clone()]));
    Plan { batches, skipped }
}

/// A response time in half-second steps.
fn speed_step(seconds: Option<f64>) -> u32 {
    // Fast indexers have small, non-negative times (checked when parsed).
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let step = (seconds.unwrap_or(0.0) * 2.0).floor() as u32;
    step
}

/// Accumulates the answers of a progressive search.
#[derive(Debug, Default)]
pub(crate) struct Merger {
    candidates: Vec<Candidate>,
    statuses: Vec<IndexerStatus>,
    names: HashMap<String, String>,
}

impl Merger {
    pub(crate) fn new(plan: &Plan, indexers: &[IndexerInfo]) -> Self {
        Self {
            candidates: Vec::new(),
            statuses: plan.skipped.clone(),
            names: indexers
                .iter()
                .map(|i| (i.id.clone(), i.name.clone()))
                .collect(),
        }
    }

    /// Adds one request's answer.
    pub(crate) fn add(&mut self, candidates: Vec<Candidate>, statuses: Vec<IndexerStatus>) {
        self.candidates.extend(candidates);
        self.statuses.extend(statuses);
    }

    /// Candidates added so far, before merging.
    pub(crate) fn candidates(&self) -> usize {
        self.candidates.len()
    }

    /// Fills in the magnet link and info hash of the candidates offered as `download`, the
    /// provider's download link, that have neither yet. Returns whether any did.
    pub(crate) fn resolve_download(
        &mut self,
        download: &str,
        magnet: Option<&str>,
        info_hash: Option<&str>,
    ) -> bool {
        let mut resolved = false;
        for c in &mut self.candidates {
            if c.download.as_deref() == Some(download) && c.magnet.is_none() && c.infohash.is_none()
            {
                c.magnet = magnet.map(str::to_owned);
                c.infohash = info_hash.map(str::to_owned);
                resolved = true;
            }
        }
        resolved
    }

    /// Replaces the per-indexer outcomes (with a list that covers every indexer).
    pub(crate) fn set_statuses(&mut self, statuses: Vec<IndexerStatus>) {
        self.statuses = statuses;
    }

    /// The validated per-indexer outcomes so far.
    pub(crate) fn statuses(&self) -> Vec<IndexerStatus> {
        clean_statuses(self.statuses.clone())
    }

    /// Records a failed request: every indexer in it failed.
    pub(crate) fn fail(&mut self, batch: &[String], error: &SearchError) {
        for id in batch {
            self.statuses.push(IndexerStatus {
                id: Some(id.clone()),
                name: self.names.get(id).cloned().unwrap_or_else(|| id.clone()),
                results: 0,
                error: Some(error.to_string()),
            });
        }
    }

    /// The merged, validated, ranked results so far.
    pub(crate) fn response(
        &self,
        include_adult: bool,
        converts_audio: bool,
        query: &str,
    ) -> SearchResponse {
        let mut response = build_response(
            self.candidates.clone(),
            self.statuses.clone(),
            include_adult,
            converts_audio,
        );
        rank(&mut response.results, query);
        response.results.truncate(MAX_RESULTS);
        response
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(id: &str, healthy: bool, last: Option<f64>) -> IndexerInfo {
        IndexerInfo {
            id: id.into(),
            name: id.to_uppercase(),
            healthy,
            excluded: false,
            adult_only: false,
            categories: Vec::new(),
            last_seconds: last,
        }
    }

    #[test]
    fn a_category_search_asks_only_the_indexers_that_carry_it() {
        let tv = IndexerInfo {
            categories: vec![5000],
            ..info("eztv", true, Some(0.5))
        };
        let movies = IndexerInfo {
            categories: vec![2000, 8000],
            ..info("yts", true, Some(0.5))
        };
        let unlisted = info("other", true, Some(0.5));
        let list = vec![tv, movies, unlisted];
        assert_eq!(
            plan(&list, false, &[5070]).batches,
            vec![vec!["eztv".to_owned(), "other".into()]],
            "anime is part of TV"
        );
        assert_eq!(plan(&list, false, &[]).batches[0].len(), 3);
    }

    #[test]
    fn leaves_out_excluded_and_adult_only_indexers() {
        let excluded = IndexerInfo {
            excluded: true,
            ..info("1337x", true, Some(14.1))
        };
        let adult = IndexerInfo {
            adult_only: true,
            ..info("xxxclub", true, Some(0.9))
        };
        let list = vec![excluded, adult, info("yts", true, Some(0.9))];
        let p = plan(&list, false, &[]);
        assert_eq!(p.batches, vec![vec!["yts".to_owned()]]);
        assert_eq!(
            p.skipped.len(),
            1,
            "excluded is reported, adult-only is not"
        );
        assert_eq!(p.skipped[0].id.as_deref(), Some("1337x"));
        let p = plan(&list, true, &[]);
        assert_eq!(p.batches, vec![vec!["xxxclub".to_owned(), "yts".into()]]);
    }

    #[test]
    fn groups_fast_indexers_and_isolates_slow_ones() {
        let list = vec![
            info("yts", true, Some(0.3)),
            info("1337x", true, Some(11.7)),
            info("therarbg", true, Some(0.2)),
            info("kickasstorrents-to", true, Some(3.9)),
            info("magnetz", true, Some(0.4)),
            info("eztv", true, Some(0.7)),
            info("limetorrents", true, Some(0.3)),
            info("fresh", true, None),
            info("torrentbyte", false, None),
            info("ehentai", true, Some(2.8)),
            info("Bad Id", true, Some(0.1)),
            info("yts", true, Some(0.3)),
        ];
        let p = plan(&list, false, &[]);
        assert_eq!(
            p.batches,
            vec![
                // Under 0.5 s, by id; then 0.5–1 s.
                vec![
                    "limetorrents".to_owned(),
                    "magnetz".into(),
                    "therarbg".into(),
                    "yts".into()
                ],
                vec!["eztv".to_owned()],
                // Slow ones alone, slowest first, unknown last.
                vec!["1337x".to_owned()],
                vec!["kickasstorrents-to".to_owned()],
                vec!["fresh".to_owned()],
            ]
        );
        assert_eq!(p.skipped.len(), 1);
        assert_eq!(p.skipped[0].id.as_deref(), Some("torrentbyte"));
        assert!(p.skipped[0].error.is_some());

        let with_adult = plan(&list, true, &[]);
        assert!(with_adult.batches.iter().any(|b| b == &["ehentai"]));
    }

    #[test]
    fn empty_or_all_unhealthy_lists_plan_nothing() {
        assert!(plan(&[], false, &[]).batches.is_empty());
        let p = plan(&[info("a", false, None)], false, &[]);
        assert!(p.batches.is_empty());
        assert_eq!(p.skipped.len(), 1);
    }

    #[test]
    fn merges_answers_and_records_failed_requests() {
        let list = vec![info("a", true, Some(0.1)), info("b", true, Some(0.1))];
        let p = plan(&list, false, &[]);
        let mut m = Merger::new(&p, &list);
        let hash = "3f9aac158c7de8dfcab171ea58a17aabdf7fbc93";
        let c = |seeders| Candidate {
            title: "Sintel 1080p".into(),
            seeders: Some(seeders),
            infohash: Some(hash.into()),
            ..Candidate::default()
        };
        m.add(vec![c(5)], Vec::new());
        m.add(vec![c(50)], Vec::new());
        m.fail(&["b".to_owned()], &SearchError::RateLimited);
        let r = m.response(false, false, "sintel");
        assert_eq!(
            r.results.len(),
            1,
            "same info hash from two requests is merged"
        );
        assert_eq!(r.results[0].seeders, Some(50));
        assert!(r.results[0].best_pick);
        assert_eq!(r.indexers.len(), 1);
        assert_eq!(r.indexers[0].name, "B");
        assert!(r.indexers[0].error.as_deref().unwrap().contains("Too many"));
    }
}
