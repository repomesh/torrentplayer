// SPDX-License-Identifier: MIT OR Apache-2.0

//! Checks the pieces a torrent's cache kept from an earlier session, so they are used again
//! instead of downloaded again.
//!
//! The pieces are checked against the torrent's SHA-1 when the torrent is added, on a few
//! blocking threads, and become available one by one as they pass: the add itself does not
//! wait, even for a large torrent. Until a piece is checked the scheduler does not request it
//! (see [`crate::sched::Scheduler::checking_stored`]) and readers wait for it, as for a piece
//! being downloaded; pieces the playhead or a reader needs are checked first. A piece that
//! fails is forgotten and downloaded again, and no peer is blamed for it.
//!
//! Checking everything at once, rather than each piece when it is first read or uploaded, keeps
//! one meaning for "have": what the scheduler, the bitfield sent to peers, and the progress
//! shown to the user count is verified data, and a finished torrent shows as finished again
//! within seconds of a restart (reading at disk speed, which is far faster than downloading).

use std::collections::VecDeque;
use std::num::NonZeroUsize;
use std::sync::{Arc, Mutex, PoisonError};

use tracing::{debug, info};

use crate::sched::PieceCheck;
use crate::torrent::Shared;

/// Most threads checking one torrent's pieces.
const MAX_THREADS: usize = 4;

/// The pieces still to check.
#[derive(Debug)]
struct Pending {
    /// In their order in the cache file, so it is read sequentially.
    order: VecDeque<u32>,
    /// Whether each piece is still to be taken.
    waiting: Vec<bool>,
    /// Pieces not taken yet.
    left: usize,
    /// Pieces whose check has not finished.
    unchecked: usize,
}

impl Pending {
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
}

/// Makes the pieces in `shared`'s store available: those already verified in this session (by
/// a live instance sharing the cache) at once, the others once checked in the background.
/// Call before the torrent's driver starts. Must be called inside a Tokio runtime.
pub(crate) fn start(shared: &Arc<Shared>) {
    let stored = shared.store.stored();
    if stored.is_empty() {
        return;
    }
    let mut pending = Pending {
        order: VecDeque::new(),
        waiting: vec![false; shared.meta.piece_count() as usize],
        left: 0,
        unchecked: 0,
    };
    let mut sched = shared.sched();
    for (piece, verified) in stored {
        if verified {
            sched.on_verified(piece, PieceCheck::Valid(None));
        } else if let Some(slot) = pending.waiting.get_mut(piece as usize) {
            sched.checking_stored(piece);
            *slot = true;
            pending.order.push_back(piece);
            pending.left += 1;
            pending.unchecked += 1;
        }
    }
    drop(sched);
    let count = pending.left;
    if count == 0 {
        return;
    }
    debug!(
        pieces = count,
        "checking pieces stored by an earlier session"
    );
    let threads = std::thread::available_parallelism()
        .map_or(1, NonZeroUsize::get)
        .min(MAX_THREADS)
        .min(count);
    let pending = Arc::new(Mutex::new(pending));
    for _ in 0..threads {
        let shared = Arc::clone(shared);
        let pending = Arc::clone(&pending);
        tokio::task::spawn_blocking(move || check(&shared, &pending));
    }
}

/// Checks pieces until none is left or the torrent stops.
fn check(shared: &Arc<Shared>, pending: &Mutex<Pending>) {
    let (mut kept, mut lost) = (0_u32, 0_u32);
    while !shared.cancel.is_cancelled() {
        let Some(piece) = next(shared, pending) else {
            break;
        };
        let ok = shared
            .meta
            .piece_hash(piece)
            .is_some_and(|expected| shared.store.verify(piece, expected));
        if ok {
            shared.sched().on_verified(piece, PieceCheck::Valid(None));
            shared.piece_done.notify_waiters();
            shared.announce_have(piece);
            kept += 1;
        } else {
            shared.sched().on_verified(piece, PieceCheck::NotStored);
            // It can be requested now.
            shared.request_replan(false);
            lost += 1;
        }
        let mut state = pending.lock().unwrap_or_else(PoisonError::into_inner);
        state.unchecked -= 1;
        if state.unchecked == 0 {
            drop(state);
            info!(torrent = %shared.meta.info_hash_hex(), "stored pieces checked");
            shared.request_replan(false);
        }
    }
    if kept > 0 || lost > 0 {
        debug!(kept, lost, "checked stored pieces");
    }
}

/// The next piece to check: one the playhead or a reader needs, if any, otherwise the next in
/// the cache file.
fn next(shared: &Shared, pending: &Mutex<Pending>) -> Option<u32> {
    let mut pending = pending.lock().unwrap_or_else(PoisonError::into_inner);
    if pending.left == 0 {
        return None;
    }
    let urgent = {
        let sched = shared.sched();
        sched
            .urgent_pieces()
            .iter()
            .copied()
            .find(|&p| pending.is_waiting(p))
    };
    let piece = match urgent {
        Some(piece) => piece,
        None => loop {
            let piece = pending.order.pop_front()?;
            if pending.is_waiting(piece) {
                break piece;
            }
        },
    };
    pending.take(piece);
    Some(piece)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pending_pieces_are_taken_once() {
        let mut pending = Pending {
            order: VecDeque::from([2, 0]),
            waiting: vec![true, false, true],
            left: 2,
            unchecked: 2,
        };
        assert!(pending.take(2));
        assert!(!pending.take(2));
        assert!(!pending.take(1));
        assert!(!pending.take(9));
        assert_eq!(pending.left, 1);
    }
}
