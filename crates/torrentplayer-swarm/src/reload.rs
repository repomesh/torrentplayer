// SPDX-License-Identifier: MIT OR Apache-2.0

//! Checks the pieces a torrent's cache kept from an earlier session, so they are used again
//! instead of downloaded again.
//!
//! Pieces kept by a session that ended cleanly are trusted (see [`crate::store::Check`]): they
//! count as stored at once and are checked when first read. The others, after a crash, are
//! checked against the torrent's SHA-1 when the torrent is added, in the background, and become
//! available one by one as they pass: the add itself does not wait, even for a large torrent.
//! Until a piece is checked the scheduler does not request it (see
//! [`crate::sched::Scheduler::checking_stored`]) and readers wait for it, as for a piece being
//! downloaded. A piece that fails is forgotten and downloaded again, and no peer is blamed.
//!
//! One [`Rechecker`] per engine does this work for all its torrents on at most [`WORKERS`]
//! blocking threads, so a long queue added at start-up does not take every core and the disk
//! from playback. Pieces the playhead or a reader of any torrent needs come first, then the
//! other pieces of torrents being read, then the torrents in the order they were added; each
//! torrent's pieces in their order in the cache file, so it is read sequentially.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};

use tracing::{debug, info};

use crate::sched::PieceCheck;
use crate::store::Check;
use crate::torrent::Shared;

/// Threads checking pieces at once, across the engine.
const WORKERS: usize = 2;

/// Checks stored pieces for every torrent of an engine.
#[derive(Debug)]
pub(crate) struct Rechecker {
    state: Mutex<State>,
    /// Pieces being checked right now.
    busy: AtomicUsize,
    /// The most pieces checked at once so far.
    peak: AtomicUsize,
}

#[derive(Debug)]
struct State {
    /// In the order the torrents were added.
    jobs: Vec<Job>,
    /// Worker threads running.
    workers: usize,
    /// At most this many.
    limit: usize,
}

/// One torrent's pieces still to check.
#[derive(Debug)]
struct Job {
    shared: Weak<Shared>,
    /// In their order in the cache file.
    order: VecDeque<u32>,
    /// Whether each piece is still to be taken.
    waiting: Vec<bool>,
    /// Pieces not taken yet.
    left: usize,
    /// Pieces whose check has not finished.
    unchecked: usize,
}

impl Job {
    fn is_waiting(&self, piece: u32) -> bool {
        self.waiting.get(piece as usize).copied().unwrap_or(false)
    }

    fn take(&mut self, piece: u32) -> bool {
        let taken = self
            .waiting
            .get_mut(piece as usize)
            .is_some_and(|p| std::mem::replace(p, false));
        if taken {
            self.left -= 1;
        }
        taken
    }

    /// The next piece in cache file order.
    fn take_next(&mut self) -> Option<u32> {
        while let Some(piece) = self.order.pop_front() {
            if self.take(piece) {
                return Some(piece);
            }
        }
        None
    }
}

impl Rechecker {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(State {
                jobs: Vec::new(),
                workers: 0,
                limit: WORKERS,
            }),
            busy: AtomicUsize::new(0),
            peak: AtomicUsize::new(0),
        })
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Makes the pieces in `shared`'s store available: those verified in this session (by a
    /// live instance sharing the cache) or trusted at once, the others once checked. Call
    /// before the torrent's driver starts, inside a Tokio runtime.
    pub(crate) fn start(self: &Arc<Self>, shared: &Arc<Shared>) {
        let stored = shared.store.stored();
        if stored.is_empty() {
            return;
        }
        let mut job = Job {
            shared: Arc::downgrade(shared),
            order: VecDeque::new(),
            waiting: vec![false; shared.meta.piece_count() as usize],
            left: 0,
            unchecked: 0,
        };
        let mut kept = 0_u32;
        let mut sched = shared.sched();
        for (piece, check) in stored {
            match check {
                Check::Verified | Check::Trusted => {
                    sched.on_verified(piece, PieceCheck::Valid(None));
                    kept += 1;
                }
                Check::Unverified => {
                    if let Some(slot) = job.waiting.get_mut(piece as usize) {
                        sched.checking_stored(piece);
                        *slot = true;
                        job.order.push_back(piece);
                        job.left += 1;
                        job.unchecked += 1;
                    }
                }
                Check::Bad => {}
            }
        }
        drop(sched);
        debug!(
            kept,
            to_check = job.left,
            "pieces stored by an earlier session"
        );
        if job.left == 0 {
            return;
        }
        let mut state = self.state();
        state.jobs.push(job);
        self.spawn_workers(&mut state);
    }

    /// Starts workers up to the limit, and no more than there are pieces to check.
    fn spawn_workers(self: &Arc<Self>, state: &mut State) {
        let left: usize = state.jobs.iter().map(|j| j.left).sum();
        while state.workers < state.limit && state.workers < left {
            state.workers += 1;
            let checker = Arc::clone(self);
            tokio::task::spawn_blocking(move || checker.work());
        }
    }

    /// Checks pieces until none is left.
    fn work(&self) {
        while let Some((shared, piece)) = self.next() {
            let busy = self.busy.fetch_add(1, Ordering::Relaxed) + 1;
            self.peak.fetch_max(busy, Ordering::Relaxed);
            let ok = shared.store.verify(piece);
            self.busy.fetch_sub(1, Ordering::Relaxed);
            if ok {
                shared.sched().on_verified(piece, PieceCheck::Valid(None));
                shared.piece_done.notify_waiters();
                shared.announce_have(piece);
            } else {
                shared.sched().on_verified(piece, PieceCheck::NotStored);
                // It can be requested now.
                shared.request_replan(false);
            }
            self.checked(&shared);
        }
    }

    /// The next piece to check, or `None` once there is none, when the calling worker ends.
    fn next(&self) -> Option<(Arc<Shared>, u32)> {
        let mut state = self.state();
        let picked = pick(&mut state.jobs);
        if picked.is_none() {
            state.workers -= 1;
        }
        picked
    }

    /// Counts a piece of `shared` as checked.
    fn checked(&self, shared: &Arc<Shared>) {
        let mut state = self.state();
        let Some(at) = state
            .jobs
            .iter()
            .position(|j| std::ptr::eq(j.shared.as_ptr(), Arc::as_ptr(shared)))
        else {
            return;
        };
        let job = &mut state.jobs[at];
        job.unchecked -= 1;
        if job.unchecked == 0 {
            state.jobs.remove(at);
            drop(state);
            info!(torrent = %shared.meta.info_hash_hex(), "stored pieces checked");
            shared.request_replan(false);
        }
    }

    /// Changes the number of workers, for tests.
    #[cfg(test)]
    pub(crate) fn set_limit(self: &Arc<Self>, limit: usize) {
        let mut state = self.state();
        state.limit = limit;
        self.spawn_workers(&mut state);
    }

    /// Takes the next piece to check as a worker would, without checking it, for tests.
    #[cfg(test)]
    pub(crate) fn take_next(&self) -> Option<(Arc<Shared>, u32)> {
        pick(&mut self.state().jobs)
    }

    /// The most pieces checked at once so far.
    #[cfg(test)]
    pub(crate) fn peak(&self) -> usize {
        self.peak.load(Ordering::Relaxed)
    }
}

/// Takes the next piece to check: one the playhead or a reader of any torrent needs, then one
/// of a torrent being read, then one of the earliest added torrent. Forgets the torrents that
/// stopped.
fn pick(jobs: &mut Vec<Job>) -> Option<(Arc<Shared>, u32)> {
    jobs.retain(|j| j.shared.upgrade().is_some_and(|s| !s.cancel.is_cancelled()));
    let live: Vec<(usize, Arc<Shared>)> = jobs
        .iter()
        .enumerate()
        .filter(|(_, j)| j.left > 0)
        .filter_map(|(i, j)| Some((i, j.shared.upgrade()?)))
        .collect();
    for (i, shared) in &live {
        let job = &jobs[*i];
        let urgent = shared
            .sched()
            .urgent_pieces()
            .iter()
            .copied()
            .find(|&p| job.is_waiting(p));
        if let Some(piece) = urgent {
            jobs[*i].take(piece);
            return Some((Arc::clone(shared), piece));
        }
    }
    let read = live.iter().filter(|(_, s)| s.sched().has_interests());
    for (i, shared) in read.chain(&live) {
        if let Some(piece) = jobs[*i].take_next() {
            return Some((Arc::clone(shared), piece));
        }
    }
    None
}
