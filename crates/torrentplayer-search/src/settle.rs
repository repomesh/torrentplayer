// SPDX-License-Identifier: MIT OR Apache-2.0

//! Deciding when a progressive search's best pick is settled enough to preload.
//!
//! A streamed search keeps receiving results for up to 20 s, and a late indexer can change the
//! best pick. Preloading every new best pick would start and cancel several downloads per
//! search; waiting for the last indexer would delay the preload by 10–20 s. [`Settle`] commits
//! to the best pick once it has stayed best for [`SETTLE_TIME`], or when the search finishes,
//! and changes its mind at most once: when the finished search's best pick is a different
//! torrent. So a search starts at most two preloads, and usually one.

use std::time::Duration;

use tokio::time::Instant;

use crate::SearchProgress;

/// How long a best pick must stay best before it is preloaded.
pub const SETTLE_TIME: Duration = Duration::from_secs(2);

/// What to do with the preload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SettleAction {
    /// Preload this result.
    Preload {
        /// Its key ([`SearchResult::key`](crate::SearchResult::key)).
        key: String,
        /// What to add: its magnet link, or its download link.
        link: String,
    },
    /// The finished search has no best pick: drop the previous search's preload.
    Evict,
}

/// The best pick of one search, as it settles. Feed it every progress report with
/// [`Settle::update`], and call it again at [`Settle::deadline`] if no report came first.
#[derive(Debug, Default)]
pub struct Settle {
    /// The current best pick (key, link) and when it became best.
    leader: Option<(String, String, Instant)>,
    /// The key of the result preloaded so far, if any.
    committed: Option<String>,
    finished: bool,
}

impl Settle {
    /// Takes the latest progress (or the same one again, at the deadline) and returns what to
    /// do now, if anything.
    pub fn update(&mut self, progress: &SearchProgress, now: Instant) -> Option<SettleAction> {
        if self.finished {
            return None;
        }
        let best = progress.response.results.iter().find(|r| r.best_pick);
        let same = match (&self.leader, best) {
            (Some((key, _, _)), Some(best)) => *key == best.key,
            (None, None) => true,
            _ => false,
        };
        if !same {
            self.leader = best.map(|b| (b.key.clone(), b.link.clone(), now));
        }
        if progress.done {
            self.finished = true;
            return match &self.leader {
                Some((key, _, _)) if self.committed.as_ref() == Some(key) => None,
                Some((key, link, _)) => Some(SettleAction::Preload {
                    key: key.clone(),
                    link: link.clone(),
                }),
                None => Some(SettleAction::Evict),
            };
        }
        match &self.leader {
            Some((key, link, since))
                if self.committed.is_none()
                    && now.saturating_duration_since(*since) >= SETTLE_TIME =>
            {
                self.committed = Some(key.clone());
                Some(SettleAction::Preload {
                    key: key.clone(),
                    link: link.clone(),
                })
            }
            _ => None,
        }
    }

    /// When the current best pick will have settled, if it is still waiting to.
    #[must_use]
    pub fn deadline(&self) -> Option<Instant> {
        match &self.leader {
            Some((_, _, since)) if !self.finished && self.committed.is_none() => {
                Some(*since + SETTLE_TIME)
            }
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rank::rank;
    use crate::results::{Candidate, build_response};

    /// Progress whose results are `titles` (with seeders), ranked for the query "sintel".
    fn progress(titles: &[(&str, u32)], done: bool) -> SearchProgress {
        let candidates = titles
            .iter()
            .map(|(title, seeders)| Candidate {
                title: (*title).into(),
                seeders: Some(*seeders),
                infohash: Some(format!("{:02x}", title.len()).repeat(20)),
                ..Candidate::default()
            })
            .collect();
        let mut response = build_response(candidates, Vec::new(), false, false);
        rank(&mut response.results, "sintel");
        SearchProgress {
            response,
            completed: 1,
            total: 2,
            done,
            cached: false,
        }
    }

    fn hash(action: Option<SettleAction>) -> Option<String> {
        match action {
            Some(SettleAction::Preload { key, .. }) => Some(key),
            Some(SettleAction::Evict) => Some("evict".into()),
            None => None,
        }
    }

    #[test]
    fn preloads_once_the_best_pick_has_held() {
        let t0 = Instant::now();
        let mut s = Settle::default();
        let early = progress(&[("Sintel 720p mp4", 10)], false);
        assert_eq!(s.update(&early, t0), None);
        assert_eq!(s.deadline(), Some(t0 + SETTLE_TIME));
        // A new leader restarts the wait.
        let better = progress(
            &[("Sintel 720p mp4", 10), ("Sintel 1080p WEB-DL mp4", 300)],
            false,
        );
        let t1 = t0 + Duration::from_secs(1);
        assert_eq!(s.update(&better, t1), None);
        assert_eq!(s.deadline(), Some(t1 + SETTLE_TIME));
        assert_eq!(s.update(&better, t1 + Duration::from_millis(1999)), None);
        let committed = hash(s.update(&better, t1 + SETTLE_TIME)).unwrap();
        assert_eq!(committed, better.response.results[0].key);
        assert_eq!(s.deadline(), None);
        // Later changes wait for the end; the same pick at the end needs nothing.
        assert_eq!(s.update(&early, t1 + Duration::from_secs(5)), None);
        let mut done = better.clone();
        done.done = true;
        assert_eq!(s.update(&done, t1 + Duration::from_secs(9)), None);
        assert_eq!(
            s.update(&done, t1 + Duration::from_secs(10)),
            None,
            "finished"
        );
    }

    #[test]
    fn switches_at_most_once_when_the_final_pick_differs() {
        let t0 = Instant::now();
        let mut s = Settle::default();
        let first = progress(&[("Sintel 720p mp4", 10)], false);
        s.update(&first, t0);
        assert!(s.update(&first, t0 + SETTLE_TIME).is_some());
        let last = progress(
            &[
                ("Sintel 720p mp4", 10),
                ("Sintel 2160p WEB-DL mp4 HDR", 900),
            ],
            true,
        );
        let switched = hash(s.update(&last, t0 + Duration::from_secs(12))).unwrap();
        assert_eq!(switched, last.response.results[0].key);
        assert_ne!(switched, first.response.results[0].key);
    }

    #[test]
    fn a_search_that_finishes_first_acts_at_once() {
        let t0 = Instant::now();
        let mut s = Settle::default();
        let cached = progress(&[("Sintel 720p mp4", 10)], true);
        assert!(matches!(
            s.update(&cached, t0),
            Some(SettleAction::Preload { .. })
        ));
        let mut s = Settle::default();
        let nothing = progress(&[("Other film", 900)], true);
        assert_eq!(s.update(&nothing, t0), Some(SettleAction::Evict));
    }

    #[test]
    fn waits_while_there_is_no_best_pick() {
        let t0 = Instant::now();
        let mut s = Settle::default();
        let none = progress(&[("Other film", 900)], false);
        assert_eq!(s.update(&none, t0), None);
        assert_eq!(s.deadline(), None);
        assert_eq!(s.update(&none, t0 + Duration::from_secs(30)), None);
    }
}
