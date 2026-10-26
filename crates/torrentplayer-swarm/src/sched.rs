// SPDX-License-Identifier: MIT OR Apache-2.0

//! The request scheduler: decides what to request, from whom, and when.
//!
//! This is where streaming performance comes from, so it is pure logic with no I/O and is
//! unit-tested directly. The policy:
//!
//! - **Deadlines.** Readers and the playhead register *interests*. Every missing piece in an
//!   interest's window gets an urgency: how soon the player will need it at the current byte
//!   rate. Urgent pieces are always requested first, in urgency order.
//! - **Short, adaptive queues.** Each peer gets about [`SchedulerConfig::queue_time`] of requests
//!   at its measured rate (instead of a fixed number), so a newly urgent block never waits behind
//!   seconds of queued data.
//! - **Cancel on seek.** When the playhead jumps, requests that are no longer urgent are cancelled
//!   so peers' queues free up for the new position at once.
//! - **Duplicate urgent blocks.** A block that is due within [`SchedulerConfig::endgame_horizon`]
//!   and has been outstanding for a while is also requested from a second peer; the slower copy
//!   is cancelled when the first arrives.
//! - **Critical phase.** When a reader starts, seeks, or has to wait for data, only the
//!   [`SchedulerConfig::critical_bytes`] ahead of it are requested until half of them are
//!   downloaded, so background downloading cannot compete with the bytes the player is waiting
//!   for.
//! - **Background fill.** Otherwise, selected files are downloaded rarest piece first, finishing
//!   started pieces before starting new ones.
//! - **Bad data.** Blocks are accepted only from a peer that was asked for them. When a piece
//!   fails its hash check, each block's digest is remembered with its sender, and the piece is
//!   downloaded again from one peer where possible; once it verifies, every peer that had sent
//!   a block different from the verified data is returned to be banned ("smart ban", as in
//!   libtorrent), and only those.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use crate::bits::Bits;
use crate::metainfo::{BLOCK_LEN, Metainfo};

/// Identifies a connected peer.
pub(crate) type PeerKey = u64;
/// Identifies an interest (a reader or the playhead).
pub(crate) type InterestId = u64;

/// Scheduler settings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SchedulerConfig {
    /// How much download time each peer's request queue should hold.
    pub queue_time: Duration,
    /// Request queue used when nothing is needed soon (see
    /// [`SchedulerConfig::relax_horizon`]). Data already queued cannot be recalled from peers
    /// that ignore `cancel`, so a short queue here is what makes the *next* seek fast;
    /// background throughput drops slightly (measured about 17% on a link with 200 ms of
    /// queueing delay).
    pub relaxed_queue_time: Duration,
    /// The queue counts as relaxed unless some missing piece is due within this time.
    pub relax_horizon: Duration,
    /// Queue for a peer whose rate has not been measured yet, in blocks. Deep enough that a
    /// fast peer is not throttled while its rate is first measured (about one tick), shallow
    /// enough that a seek right after connecting waits behind little data.
    pub initial_queue: usize,
    /// Smallest per-peer queue once the rate is known, in blocks.
    pub min_queue: usize,
    /// Largest per-peer queue, in blocks.
    pub max_queue: usize,
    /// Blocks due within this time may be requested from a second peer. Zero disables
    /// duplicate requests.
    pub endgame_horizon: Duration,
    /// How long an urgent block must be outstanding before it is duplicated.
    pub duplicate_after: Duration,
    /// Only duplicate blocks held by a snubbed peer: one with requests outstanding that has
    /// delivered nothing for [`SchedulerConfig::snub_after`]. Peers that ignore `cancel`
    /// (such as librqbit) serve every duplicate in full, so time-based duplication wastes
    /// bandwidth; a stalled peer is the case duplication actually helps.
    pub duplicate_only_snubbed: bool,
    /// How long a peer with outstanding requests may deliver nothing before it counts as
    /// snubbed.
    pub snub_after: Duration,
    /// Outstanding requests older than this are given up and re-requested elsewhere.
    pub request_timeout: Duration,
    /// On a seek, outstanding requests due later than this are cancelled.
    pub cancel_beyond: Duration,
    /// Bytes ahead of a reader requested exclusively in its critical phase (see the module
    /// documentation). The phase ends once half of them are downloaded.
    pub critical_bytes: u64,
    /// Also stay critical until this much playback time is buffered (at the media byte
    /// rate), so high-bitrate video gets a proportionally larger start-up buffer. Zero means
    /// [`SchedulerConfig::critical_bytes`] alone decides.
    pub critical_secs: Duration,
    /// Bytes after each reader's position that are urgent.
    pub reader_window: u64,
    /// Playback time to keep downloaded ahead of the playhead.
    pub read_ahead: Duration,
    /// Media byte rate assumed until the real one is known (bytes per second).
    pub fallback_byte_rate: u64,
    /// Largest read-ahead window in bytes.
    pub max_window: u64,
    /// Bytes at the end of a playing file to prefetch (container indexes), once not critical.
    pub tail_bytes: u64,
    /// Upper bound on memory held by partially downloaded background pieces, shared by every
    /// torrent of the engine. Torrents nobody is playing or reading stop starting pieces at
    /// three quarters of it, leaving the rest to the one being played; urgent pieces are never
    /// held back, and every torrent may always work on at least one piece. A paused torrent
    /// gives its partial pieces back, and so does a stalled one while the budget is contended.
    pub max_in_progress_bytes: u64,
}

impl Default for SchedulerConfig {
    fn default() -> Self {
        const MIB: u64 = 1024 * 1024;
        Self {
            queue_time: Duration::from_millis(500),
            relaxed_queue_time: Duration::from_millis(150),
            relax_horizon: Duration::from_secs(4),
            initial_queue: 32,
            min_queue: 4,
            max_queue: 128,
            endgame_horizon: Duration::from_secs(1),
            duplicate_after: Duration::from_millis(300),
            duplicate_only_snubbed: true,
            snub_after: Duration::from_secs(1),
            request_timeout: Duration::from_secs(8),
            cancel_beyond: Duration::from_secs(5),
            critical_bytes: 2 * MIB,
            critical_secs: Duration::from_millis(1_500),
            reader_window: 8 * MIB,
            read_ahead: Duration::from_secs(30),
            fallback_byte_rate: 312_500,
            max_window: 512 * MIB,
            tail_bytes: 2 * MIB,
            max_in_progress_bytes: 256 * MIB,
        }
    }
}

/// A request to send to a peer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct Request {
    pub(crate) piece: u32,
    pub(crate) begin: u32,
    pub(crate) len: u32,
}

/// What kind of interest: a reader has a short urgent window; the playhead has the full
/// read-ahead window plus tail prefetch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum InterestKind {
    Reader,
    Playhead,
}

/// A position in a file the player cares about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Interest {
    pub(crate) kind: InterestKind,
    /// Absolute offset of the file in the torrent.
    pub(crate) file_offset: u64,
    /// File length.
    pub(crate) file_len: u64,
    /// Position within the file.
    pub(crate) position: u64,
    /// Media byte rate, if known.
    pub(crate) byte_rate: Option<u64>,
}

#[derive(Debug, Clone)]
enum BlockState {
    Missing,
    Requested(Vec<(PeerKey, Instant)>),
    Received,
}

#[derive(Debug)]
struct Progress {
    blocks: Vec<BlockState>,
    data: Vec<u8>,
    received: u32,
    /// Who sent each received block.
    senders: Vec<Option<PeerKey>>,
    /// For a piece that failed its hash check before: the one peer downloading it again (see
    /// [`Scheduler::take_blocks`]).
    owner: Option<PeerKey>,
    /// When a block was last requested or received (or the piece was started).
    active: Instant,
}

/// A digest of one block's data, for smart ban (see [`Scheduler::on_verified`]).
pub(crate) type BlockDigest = [u8; 20];

/// The outcome of checking a completed piece.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PieceCheck {
    /// It matched its hash and was stored. Carries the digest of each block when
    /// [`Scheduler::needs_block_digests`] asked for them.
    Valid(Option<Vec<BlockDigest>>),
    /// It failed its hash check: the digest of each block.
    Invalid(Vec<BlockDigest>),
    /// It matched but could not be stored. It is downloaded again and nobody is blamed.
    NotStored,
}

/// What [`Scheduler::on_verified`] found out about the peers.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Verified {
    /// The peers that sent a piece that failed its hash check.
    pub(crate) failed_by: Vec<PeerKey>,
    /// Peers whose data for a piece differed from the data that later verified: to ban.
    pub(crate) banned: Vec<PeerKey>,
}

/// Torrents with no playhead or reader leave this fraction (one in `STREAMING_RESERVE`) of the
/// [`PieceBudget`] to torrents being played.
const STREAMING_RESERVE: u64 = 4;
/// While the [`PieceBudget`] is contended, a partial piece with no block requested or received
/// for this long is dropped and its memory given back, so a stalled torrent cannot hold it
/// forever.
const STALE_PIECE: Duration = Duration::from_secs(60);

/// Memory for partially downloaded pieces, shared by every torrent of an engine (see
/// [`SchedulerConfig::max_in_progress_bytes`]). Each [`Scheduler`] accounts for its own
/// pieces here and gives them back when they complete or it is dropped.
#[derive(Debug)]
pub(crate) struct PieceBudget {
    limit: u64,
    used: AtomicU64,
}

impl PieceBudget {
    pub(crate) fn new(limit: u64) -> Arc<Self> {
        Arc::new(Self {
            limit,
            used: AtomicU64::new(0),
        })
    }

    /// What torrents with no playhead or reader may fill (see [`STREAMING_RESERVE`]).
    fn idle_limit(&self) -> u64 {
        self.limit - self.limit / STREAMING_RESERVE
    }

    /// Takes `bytes` if that keeps the total within `limit`.
    fn try_take(&self, bytes: u64, limit: u64) -> bool {
        self.used
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |used| {
                let after = used.saturating_add(bytes);
                (after <= limit).then_some(after)
            })
            .is_ok()
    }

    fn give_back(&self, bytes: u64) {
        let _ = self
            .used
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |used| {
                Some(used.saturating_sub(bytes))
            });
    }

    pub(crate) fn used(&self) -> u64 {
        self.used.load(Ordering::Relaxed)
    }
}

/// Failed pieces remembered for smart ban at most; the oldest is forgotten first.
const MAX_SUSPECT_PIECES: usize = 64;
/// Distinct (sender, digest) records kept per block of a failed piece.
const MAX_RECORDS_PER_BLOCK: usize = 4;
/// Requests cancelled at a peer whose data is still accepted if it arrives (peers such as
/// librqbit ignore `cancel`). Older ones are forgotten.
const MAX_CANCELLED: usize = 256;

#[derive(Debug, Clone, Copy)]
struct Outstanding {
    piece: u32,
    block: u32,
    at: Instant,
    /// Blocks already queued at this peer when the request was sent (0 = an empty queue).
    ahead: u32,
}

/// The base round-trip estimate is the lowest sample of this window. An older minimum is replaced
/// by the next sample, even a higher one, so the estimate follows a peer whose latency rises.
/// Without samples it stays as it is: a busy peer never has an empty queue to sample, and its
/// round trip cannot be observed while its pipeline is full (latency is then just queue length
/// over rate), so drifting upwards would only inflate its queue.
const RTT_WINDOW: Duration = Duration::from_secs(10);
/// Request queues hold at least this many round trips of data, so a peer's pipeline never
/// runs dry while a request is in flight.
const QUEUE_RTTS: f64 = 2.0;

/// Most requests [`Scheduler::fill_all`] gives one peer per turn: 256 KiB.
const ROUND_BLOCKS: usize = 16;

/// A slow peer leaves an urgent block to a peer that would deliver it this many times sooner,
/// and a block held by a peer this many times slower is requested again from the faster one.
const FASTER_BY: f64 = 2.0;

/// Another peer that could take urgent blocks (see [`Scheduler::leave_to_faster`]).
#[derive(Debug, Clone, Copy)]
struct Rival {
    key: PeerKey,
    /// When it would deliver a block requested now.
    eta: Duration,
    /// Its rate, bytes per second.
    rate: f64,
}

impl Rival {
    /// When it would deliver the block `more` blocks after that one.
    fn finish(&self, more: usize) -> Duration {
        #[allow(clippy::cast_precision_loss)]
        let bytes = more as f64 * f64::from(BLOCK_LEN);
        self.eta + Duration::from_secs_f64(bytes / self.rate)
    }
}

/// What a peer offers for an urgent block another peer holds (see [`Scheduler::take_blocks`]).
#[derive(Debug, Clone, Copy)]
struct Rescue {
    /// When the block is needed.
    needed_in: Duration,
    /// When this peer would deliver it.
    eta: Duration,
    /// This peer's rate, bytes per second.
    rate: f64,
}

/// Rates are sampled over windows of at least this long; shorter windows give wildly noisy
/// samples because blocks arrive in bursts.
const RATE_WINDOW: Duration = Duration::from_millis(100);
/// Time constant of the rate average.
const RATE_TAU: f64 = 0.5;

/// Blocks a peer delivering `rate` bytes per second sends in `time` (rounded up).
fn blocks_for(rate: f64, time: Duration) -> usize {
    let blocks = rate * time.as_secs_f64() / f64::from(BLOCK_LEN);
    // Saturating float-to-int conversion; callers clamp it.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let blocks = blocks.ceil() as usize;
    blocks
}

/// Exponentially weighted download rate, weighted by time rather than by sample count.
#[derive(Debug, Default, Clone, Copy)]
struct Rate {
    bytes_since: u64,
    bytes_per_sec: f64,
    measured: bool,
    /// Recent peak of [`Rate::bytes_per_sec`], decaying slowly. While the queue is relaxed a
    /// peer delivers only what is asked for, so its current rate understates what it can do;
    /// urgent queues are sized from the peak so a seek gets full speed at once.
    peak: f64,
}

/// Time constant of the peak-rate decay.
const PEAK_TAU: f64 = 10.0;

impl Rate {
    fn add(&mut self, bytes: u64) {
        self.bytes_since += bytes;
    }

    /// Folds in the bytes received over `elapsed` (at least [`RATE_WINDOW`]). The first
    /// sample with data is taken whole, so a new peer's queue follows its rate at once.
    fn update(&mut self, elapsed: Duration) {
        let secs = elapsed.as_secs_f64();
        if secs <= 0.0 {
            return;
        }
        #[allow(clippy::cast_precision_loss)]
        let sample = self.bytes_since as f64 / secs;
        if self.measured {
            let alpha = 1.0 - (-secs / RATE_TAU).exp();
            self.bytes_per_sec += alpha * (sample - self.bytes_per_sec);
        } else if self.bytes_since > 0 {
            self.bytes_per_sec = sample;
            self.measured = true;
        }
        self.peak = (self.peak * (-secs / PEAK_TAU).exp()).max(self.bytes_per_sec);
        self.bytes_since = 0;
    }
}

#[derive(Debug)]
struct Peer {
    has: Bits,
    has_count: u32,
    choked: bool,
    outstanding: Vec<Outstanding>,
    rate: Rate,
    timeouts: u32,
    bad_pieces: u32,
    /// When the peer last delivered a block, or when its queue last became non-empty.
    last_progress: Option<Instant>,
    /// Lowest recent latency of a request sent to an empty queue: an estimate of the network
    /// round trip.
    base_rtt: Option<Duration>,
    /// When [`Peer::base_rtt`] was sampled.
    base_rtt_at: Option<Instant>,
    /// Requests cancelled at this peer that it may still send: peers such as librqbit ignore
    /// `cancel`. While any are pending its queue is not really empty, so no round-trip sample is
    /// taken (it would measure the drain of the cancelled data, not the round trip).
    cancelled_pending: u32,
    /// The blocks `(piece, block)` behind the most recent cancels and releases (at most
    /// [`MAX_CANCELLED`]): their data is still accepted. Data for any other block it was not
    /// asked for is dropped.
    cancelled: VecDeque<(u32, u32)>,
    /// When the peer last delivered a block or had requests cancelled.
    last_activity: Option<Instant>,
}

impl Peer {
    /// Records that the request for `block` of `piece` was cancelled at this peer.
    fn cancelled(&mut self, piece: u32, block: u32, now: Instant) {
        self.cancelled_pending = self.cancelled_pending.saturating_add(1);
        self.last_activity = Some(now);
        self.released(piece, block);
    }

    /// Records that the request for `block` of `piece` was given up without a cancel (the
    /// peer choked us, which discards requests, or left): its data is still accepted if it
    /// comes, but none is expected.
    fn released(&mut self, piece: u32, block: u32) {
        if self.cancelled.len() >= MAX_CANCELLED {
            self.cancelled.pop_front();
        }
        self.cancelled.push_back((piece, block));
    }
}

/// Result of receiving a block.
#[derive(Debug, Default)]
pub(crate) struct BlockOutcome {
    /// Requests for the same block sent to other peers, to cancel.
    pub(crate) cancels: Vec<(PeerKey, Request)>,
    /// The piece is complete and must be verified: `(piece, data)`.
    pub(crate) completed: Option<(u32, Vec<u8>)>,
}

/// Aggregate counters.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub(crate) struct SchedStats {
    /// Requests sent for a block another peer was already fetching.
    pub(crate) duplicate_requests: u64,
    /// Requests cancelled on a seek or timeout.
    pub(crate) cancelled_requests: u64,
    /// Bytes received for blocks that were already received, no longer needed, or never asked
    /// for.
    pub(crate) wasted_bytes: u64,
    pub(crate) peers: usize,
    pub(crate) unchoked: usize,
    pub(crate) download_bps: f64,
    pub(crate) have_pieces: u32,
    pub(crate) outstanding: usize,
}

/// The scheduler for one torrent.
#[derive(Debug)]
pub(crate) struct Scheduler {
    meta: Arc<Metainfo>,
    cfg: SchedulerConfig,
    have: Vec<bool>,
    have_count: u32,
    availability: Vec<u16>,
    wanted: Vec<bool>,
    progress: HashMap<u32, Progress>,
    /// Pieces being hash-checked, with the sender of each block.
    verifying: HashMap<u32, Vec<PeerKey>>,
    /// Pieces that failed their hash check and have not verified since: for each block, who
    /// sent which data. See [`Scheduler::on_verified`].
    suspects: HashMap<u32, Vec<Vec<(PeerKey, BlockDigest)>>>,
    /// [`Scheduler::suspects`] oldest first, to bound them.
    suspect_order: VecDeque<u32>,
    /// The engine's budget for partial pieces, and this torrent's share of it.
    budget: Arc<PieceBudget>,
    in_progress_bytes: u64,
    urgency: Vec<Option<Duration>>,
    urgent_order: Vec<u32>,
    critical: bool,
    /// Pieces within the critical bytes of an interest that is still in the critical phase.
    critical_pieces: Vec<u32>,
    /// The critical pieces of ranges that reach the end of their file (container indexes,
    /// needed whole before playback starts): first among equally urgent pieces. Sorted.
    index_pieces: Vec<u32>,
    background: Vec<u32>,
    /// Every piece of `background` before this index is verified, being downloaded, or being
    /// checked, so a fill starts looking there. Reset whenever a piece can drop out of those.
    background_skip: usize,
    peers: HashMap<PeerKey, Peer>,
    interests: HashMap<InterestId, Interest>,
    /// Interests in their critical phase.
    critical_interests: HashSet<InterestId>,
    last_rate_update: Option<Instant>,
    /// The download limit in force for this torrent (bytes per second), if any; see
    /// [`Scheduler::set_rate_limit`].
    rate_limit: Option<f64>,
    /// The whole torrent's download rate, for [`SchedStats::download_bps`]. Not the sum of the
    /// peers' rates: a peer's first sample is taken whole (see [`Rate::update`]) and a
    /// departing peer takes its average with it, so that sum overstates the rate whenever
    /// peers come and go, as they do while a download limit holds them back.
    total_rate: Rate,
    duplicate_requests: u64,
    cancelled_requests: u64,
    wasted_bytes: u64,
    /// No requests are handed out (see [`Scheduler::set_paused`]).
    paused: bool,
}

impl Scheduler {
    /// A scheduler with a partial-piece budget of its own.
    #[cfg(test)]
    pub(crate) fn new(meta: Arc<Metainfo>, cfg: SchedulerConfig) -> Self {
        let budget = PieceBudget::new(cfg.max_in_progress_bytes);
        Self::with_budget(meta, cfg, budget)
    }

    /// A scheduler sharing the engine's partial-piece `budget` with other torrents.
    pub(crate) fn with_budget(
        meta: Arc<Metainfo>,
        cfg: SchedulerConfig,
        budget: Arc<PieceBudget>,
    ) -> Self {
        let n = meta.piece_count() as usize;
        Self {
            meta,
            cfg,
            have: vec![false; n],
            have_count: 0,
            availability: vec![0; n],
            wanted: vec![false; n],
            progress: HashMap::new(),
            verifying: HashMap::new(),
            suspects: HashMap::new(),
            suspect_order: VecDeque::new(),
            budget,
            in_progress_bytes: 0,
            urgency: vec![None; n],
            urgent_order: Vec::new(),
            critical: false,
            critical_pieces: Vec::new(),
            index_pieces: Vec::new(),
            background: Vec::new(),
            background_skip: 0,
            peers: HashMap::new(),
            interests: HashMap::new(),
            critical_interests: HashSet::new(),
            last_rate_update: None,
            rate_limit: None,
            total_rate: Rate {
                measured: true,
                ..Rate::default()
            },
            duplicate_requests: 0,
            cancelled_requests: 0,
            wasted_bytes: 0,
            paused: false,
        }
    }

    // ----- Queries -----------------------------------------------------------------------

    pub(crate) fn have(&self, piece: u32) -> bool {
        self.have.get(piece as usize).copied().unwrap_or(false)
    }

    pub(crate) fn have_all(&self) -> Vec<bool> {
        self.have.clone()
    }

    /// Whether checking `piece` should also report its block digests: it failed before, so
    /// the peers that sent wrong blocks can be found once it verifies.
    pub(crate) fn needs_block_digests(&self, piece: u32) -> bool {
        self.suspects.contains_key(&piece)
    }

    /// Whether there is nothing to request now: no request, partial piece, or check in flight,
    /// and either every piece is verified or no playhead or reader is open and every wanted
    /// piece is verified. Anything that changes this (a new reader, a seek, a selection)
    /// re-plans at once.
    pub(crate) fn is_idle(&self) -> bool {
        let busy = !self.progress.is_empty()
            || !self.verifying.is_empty()
            || self.peers.values().any(|p| !p.outstanding.is_empty());
        let complete = self.have_count as usize == self.have.len();
        let satisfied = self.interests.is_empty()
            && !self.wanted.iter().zip(&self.have).any(|(&w, &h)| w && !h);
        !busy && (complete || satisfied)
    }

    /// Missing pieces the playhead and readers need, most urgent first, as of the last
    /// [`Scheduler::reprioritize`].
    pub(crate) fn urgent_pieces(&self) -> &[u32] {
        &self.urgent_order
    }

    /// Whether a playhead or reader is open.
    pub(crate) fn has_interests(&self) -> bool {
        !self.interests.is_empty()
    }

    /// Forgets a verified piece whose stored copy turned out to be bad when read (a piece
    /// trusted from an earlier session): it is downloaded again.
    pub(crate) fn lose(&mut self, piece: u32) {
        if let Some(slot) = self.have.get_mut(piece as usize)
            && *slot
        {
            *slot = false;
            self.have_count -= 1;
            self.background_skip = 0;
        }
    }

    /// Holds back `piece` from requests while the copy kept in the piece store from an earlier
    /// session is checked; [`Scheduler::on_verified`] reports the result (`NotStored` if the
    /// copy is bad, so the piece is downloaded again and nobody is blamed).
    pub(crate) fn checking_stored(&mut self, piece: u32) {
        if (piece as usize) < self.have.len() && !self.have(piece) {
            self.verifying.entry(piece).or_default();
        }
    }

    #[cfg(test)]
    pub(crate) fn is_critical(&self) -> bool {
        self.critical
    }

    pub(crate) fn stats(&self) -> SchedStats {
        SchedStats {
            duplicate_requests: self.duplicate_requests,
            cancelled_requests: self.cancelled_requests,
            wasted_bytes: self.wasted_bytes,
            peers: self.peers.len(),
            unchoked: self.peers.values().filter(|p| !p.choked).count(),
            download_bps: self.total_rate.bytes_per_sec,
            have_pieces: self.have_count,
            outstanding: self.peers.values().map(|p| p.outstanding.len()).sum(),
        }
    }

    /// Contiguous downloaded bytes from absolute offset `abs` up to `end`.
    pub(crate) fn buffered_from(&self, abs: u64, end: u64) -> u64 {
        for piece in self.meta.pieces_in(abs, end) {
            if !self.have(piece) {
                return self.meta.piece_offset(piece).max(abs) - abs;
            }
        }
        end.saturating_sub(abs)
    }

    /// Queue length (blocks) to keep outstanding for a peer.
    fn target_queue(&self, peer: &Peer) -> usize {
        let max = self.cfg.max_queue.max(self.cfg.min_queue);
        // Under a download limit a peer gets at most its share of it.
        let share = self.rate_limit.map(|limit| {
            let sharing = self.peers.values().filter(|p| !p.choked).count().max(1);
            #[allow(clippy::cast_precision_loss)]
            let share = limit / sharing as f64;
            share
        });
        if !peer.rate.measured {
            // Many new peers with full initial queues would otherwise hold requests for longer
            // than the limit lets through before they time out.
            let initial = share.map_or(self.cfg.initial_queue, |share| {
                self.cfg
                    .initial_queue
                    .min(blocks_for(share, self.cfg.queue_time))
            });
            return initial.clamp(self.cfg.min_queue, max);
        }
        let relaxed = self.relaxed();
        let configured = if relaxed {
            self.cfg.relaxed_queue_time
        } else {
            self.cfg.queue_time
        };
        let rate = if relaxed {
            peer.rate.bytes_per_sec
        } else {
            peer.rate.bytes_per_sec.max(peer.rate.peak)
        };
        let rate = share.map_or(rate, |share| rate.min(share));
        // Never less than a couple of round trips, or the peer idles between requests.
        let queue_time = peer
            .base_rtt
            .map_or(configured, |rtt| configured.max(rtt.mul_f64(QUEUE_RTTS)));
        blocks_for(rate, queue_time).clamp(self.cfg.min_queue, max)
    }

    /// Whether nothing is needed soon: not in the critical phase, and no missing piece is due
    /// within [`SchedulerConfig::relax_horizon`].
    fn relaxed(&self) -> bool {
        !self.critical
            && self.urgent_order.first().is_none_or(|&p| {
                self.urgency[p as usize].is_none_or(|due| due > self.cfg.relax_horizon)
            })
    }

    // ----- Peers -------------------------------------------------------------------------

    pub(crate) fn add_peer(&mut self, key: PeerKey) {
        self.peers.insert(
            key,
            Peer {
                has: Bits::new(self.have.len()),
                has_count: 0,
                choked: true,
                outstanding: Vec::new(),
                rate: Rate::default(),
                timeouts: 0,
                bad_pieces: 0,
                last_progress: None,
                base_rtt: None,
                base_rtt_at: None,
                cancelled_pending: 0,
                cancelled: VecDeque::new(),
                last_activity: None,
            },
        );
    }

    /// Forgets a peer and releases its outstanding requests.
    pub(crate) fn remove_peer(&mut self, key: PeerKey) {
        self.release_outstanding(key);
        if let Some(peer) = self.peers.remove(&key) {
            for piece in peer.has.ones() {
                self.availability[piece] = self.availability[piece].saturating_sub(1);
            }
        }
    }

    /// Records a peer's bitfield. Returns `false` if its length is wrong (BEP 3).
    pub(crate) fn on_bitfield(&mut self, key: PeerKey, bits: &[u8]) -> bool {
        let n = self.have.len();
        if bits.len() != n.div_ceil(8) {
            return false;
        }
        let Some(peer) = self.peers.get_mut(&key) else {
            return false;
        };
        for piece in 0..n {
            let set = bits[piece / 8] & (0x80 >> (piece % 8)) != 0;
            if set && peer.has.set(piece) {
                peer.has_count += 1;
                self.availability[piece] = self.availability[piece].saturating_add(1);
            }
        }
        true
    }

    /// Records a `have` message. Returns `false` if the index is invalid.
    pub(crate) fn on_have(&mut self, key: PeerKey, piece: u32) -> bool {
        let i = piece as usize;
        let Some(peer) = self.peers.get_mut(&key) else {
            return false;
        };
        if i >= peer.has.len() {
            return false;
        }
        if peer.has.set(i) {
            peer.has_count += 1;
            self.availability[i] = self.availability[i].saturating_add(1);
        }
        true
    }

    /// A choked peer discards our requests (BEP 3), so they are released.
    pub(crate) fn on_choke(&mut self, key: PeerKey) {
        self.release_outstanding(key);
        if let Some(peer) = self.peers.get_mut(&key) {
            peer.choked = true;
        }
    }

    pub(crate) fn on_unchoke(&mut self, key: PeerKey) {
        if let Some(peer) = self.peers.get_mut(&key) {
            peer.choked = false;
        }
    }

    /// Releases a peer's requests to other peers. Their data is still accepted if it arrives
    /// anyway, as from a peer that finishes requests it had already started after choking us.
    fn release_outstanding(&mut self, key: PeerKey) {
        let Some(peer) = self.peers.get_mut(&key) else {
            return;
        };
        for o in std::mem::take(&mut peer.outstanding) {
            peer.released(o.piece, o.block);
            if let Some(progress) = self.progress.get_mut(&o.piece) {
                release_holder(&mut progress.blocks[o.block as usize], key);
            }
        }
    }

    // ----- Files and interests -------------------------------------------------------------

    /// Marks every piece of a file as wanted for background download.
    pub(crate) fn want_range(&mut self, start: u64, end: u64) {
        for piece in self.meta.pieces_in(start, end) {
            self.wanted[piece as usize] = true;
        }
    }

    /// Wants exactly the pieces overlapping `ranges` (absolute byte ranges): pieces no longer
    /// covered stop being downloaded in the background. Requests already sent still complete.
    pub(crate) fn set_wanted(&mut self, ranges: &[(u64, u64)]) {
        self.wanted.fill(false);
        for &(start, end) in ranges {
            self.want_range(start, end);
        }
    }

    /// Stops or resumes handing out requests. Pausing cancels every outstanding request, so
    /// data stops arriving within about a round trip; connections stay open for a quick resume.
    /// Returns the cancels to send.
    pub(crate) fn set_paused(&mut self, paused: bool) -> Vec<(PeerKey, Request)> {
        self.paused = paused;
        let mut cancels = Vec::new();
        if !paused {
            return cancels;
        }
        let now = Instant::now();
        for (&key, peer) in &mut self.peers {
            for o in std::mem::take(&mut peer.outstanding) {
                peer.cancelled(o.piece, o.block, now);
                if let Some(progress) = self.progress.get_mut(&o.piece) {
                    release_holder(&mut progress.blocks[o.block as usize], key);
                }
                cancels.push((key, block_request(&self.meta, o.piece, o.block)));
            }
        }
        self.cancelled_requests += cancels.len() as u64;
        // The user paused it: its partial pieces give their memory back to other torrents and
        // are downloaded again later.
        let started: Vec<u32> = self.progress.keys().copied().collect();
        for piece in started {
            self.discard_progress(piece);
        }
        cancels
    }

    /// Adds or moves an interest. Moving forward by at most [`SchedulerConfig::reader_window`]
    /// within the same file is continuous playback; anything else is a jump, which starts a
    /// critical phase.
    pub(crate) fn set_interest(&mut self, id: InterestId, interest: Interest) {
        let continuous = self.interests.get(&id).is_some_and(|old| {
            old.file_offset == interest.file_offset
                && interest.position >= old.position
                && interest.position <= old.position.saturating_add(self.cfg.reader_window)
        });
        if !continuous {
            self.critical_interests.insert(id);
        }
        self.interests.insert(id, interest);
    }

    pub(crate) fn interest(&self, id: InterestId) -> Option<&Interest> {
        self.interests.get(&id)
    }

    pub(crate) fn remove_interest(&mut self, id: InterestId) {
        self.interests.remove(&id);
        self.critical_interests.remove(&id);
    }

    /// Recomputes urgencies. With `seek`, cancels outstanding requests that are no longer
    /// urgent so peers' queues free up for the new position; the cancels are returned.
    pub(crate) fn reprioritize(&mut self, seek: bool) -> Vec<(PeerKey, Request)> {
        for piece in self.urgent_order.drain(..) {
            self.urgency[piece as usize] = None;
        }
        let mut critical = false;
        let mut critical_pieces: Vec<u32> = Vec::new();
        let mut index_pieces: Vec<u32> = Vec::new();
        let mut touched: Vec<u32> = Vec::new();
        let mut phases: Vec<(InterestId, bool)> = Vec::new();
        // Readers do not know the media bitrate; the playhead of the same file does. Using it
        // keeps a reader's window (for example the container index at the end of the file)
        // as urgent as the playhead's, instead of looking far away at the fallback rate.
        let known_rates: Vec<(u64, u64)> = self
            .interests
            .values()
            .filter_map(|i| i.byte_rate.filter(|&r| r > 0).map(|r| (i.file_offset, r)))
            .collect();
        for (id, interest) in &self.interests {
            let rate = interest
                .byte_rate
                .filter(|&r| r > 0)
                .or_else(|| {
                    known_rates
                        .iter()
                        .find(|(offset, _)| *offset == interest.file_offset)
                        .map(|(_, r)| *r)
                })
                .unwrap_or(self.cfg.fallback_byte_rate)
                .max(1);
            let position = interest.position.min(interest.file_len);
            let start = interest.file_offset + position;
            let file_end = interest.file_offset + interest.file_len;
            let window = match interest.kind {
                InterestKind::Reader => self.cfg.reader_window,
                InterestKind::Playhead => {
                    let secs = self.cfg.read_ahead.as_secs_f64();
                    #[allow(
                        clippy::cast_possible_truncation,
                        clippy::cast_sign_loss,
                        clippy::cast_precision_loss
                    )]
                    let bytes = (rate as f64 * secs) as u64;
                    bytes.clamp(
                        self.cfg.reader_window,
                        self.cfg.max_window.max(self.cfg.reader_window),
                    )
                }
            };
            let end = start.saturating_add(window).min(file_end);
            let phase = self.critical_phase(*id, interest, rate);
            phases.push((*id, phase.is_some()));
            if let Some((from, to)) = phase {
                critical = true;
                let missing = self
                    .meta
                    .pieces_in(from, to)
                    .filter(|&p| !self.have[p as usize]);
                if to == file_end {
                    index_pieces.extend(missing.clone());
                }
                critical_pieces.extend(missing);
            }
            for piece in self.meta.pieces_in(start, end) {
                let dist = self.meta.piece_offset(piece).saturating_sub(start);
                #[allow(clippy::cast_precision_loss)]
                let due = Duration::from_secs_f64(dist as f64 / rate as f64);
                set_urgency(&mut self.urgency, &mut touched, &self.have, piece, due);
            }
            if interest.kind == InterestKind::Playhead {
                // Container indexes at the end of the file; due at the end of the read-ahead.
                let tail_start = file_end.saturating_sub(self.cfg.tail_bytes).max(end);
                for piece in self.meta.pieces_in(tail_start, file_end) {
                    set_urgency(
                        &mut self.urgency,
                        &mut touched,
                        &self.have,
                        piece,
                        self.cfg.read_ahead,
                    );
                }
            }
        }
        touched.sort_by_key(|&p| (self.urgency[p as usize], p));
        self.urgent_order = touched;
        for (id, in_phase) in phases {
            if in_phase {
                self.critical_interests.insert(id);
            } else {
                self.critical_interests.remove(&id);
            }
        }
        self.critical = critical;
        critical_pieces.sort_unstable();
        critical_pieces.dedup();
        self.critical_pieces = critical_pieces;
        index_pieces.sort_unstable();
        index_pieces.dedup();
        self.index_pieces = index_pieces;

        if seek {
            self.cancel_stale_requests()
        } else {
            Vec::new()
        }
    }

    /// Whether an interest is in its critical phase, given its state before this update; if so,
    /// the absolute range to request exclusively: the critical bytes ahead of it.
    ///
    /// The phase starts when the interest is added or jumps, and again whenever it has to wait
    /// for data. It ends once half of the range is downloaded ahead of the interest, not all of
    /// it: a player that keeps reading moves the range forward, so its far end is always in
    /// flight, and waiting for all of it would never end the phase.
    fn critical_phase(&self, id: InterestId, interest: &Interest, rate: u64) -> Option<(u64, u64)> {
        let start = interest.file_offset + interest.position.min(interest.file_len);
        #[allow(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            clippy::cast_precision_loss
        )]
        let critical_bytes = self
            .cfg
            .critical_bytes
            .max((rate as f64 * self.cfg.critical_secs.as_secs_f64()) as u64);
        let file_end = interest.file_offset + interest.file_len;
        let end = start.saturating_add(critical_bytes).min(file_end);
        let buffered = self.buffered_from(start, end);
        // A range that reaches the end of the file is a container index being read, which a
        // player needs whole before it can start; streamed data only needs half (see above).
        let needed = if end == file_end {
            end - start
        } else {
            (critical_bytes / 2).min(end - start)
        };
        let critical = if self.critical_interests.contains(&id) {
            buffered < needed
        } else {
            start < end && buffered == 0
        };
        critical.then_some((start, end))
    }

    /// After a seek, cancels outstanding requests that are no longer needed: in the critical
    /// phase everything outside the critical pieces, otherwise read-ahead due later than
    /// [`SchedulerConfig::cancel_beyond`]. Returns the cancels to send.
    fn cancel_stale_requests(&mut self) -> Vec<(PeerKey, Request)> {
        let mut cancels: Vec<(PeerKey, Request)> = Vec::new();
        let urgency = &self.urgency;
        let cancel_beyond = self.cfg.cancel_beyond;
        let critical = self.critical;
        let critical_pieces = &self.critical_pieces;
        let now = Instant::now();
        for (&key, peer) in &mut self.peers {
            let (keep, drop): (Vec<Outstanding>, Vec<Outstanding>) =
                std::mem::take(&mut peer.outstanding)
                    .into_iter()
                    .partition(|o| {
                        if critical {
                            // Only what the player needs right now survives.
                            critical_pieces.binary_search(&o.piece).is_ok()
                        } else {
                            // Background work survives; far-future read-ahead does not.
                            urgency[o.piece as usize].is_none_or(|due| due <= cancel_beyond)
                        }
                    });
            peer.outstanding = keep;
            for o in drop {
                if let Some(progress) = self.progress.get_mut(&o.piece) {
                    release_holder(&mut progress.blocks[o.block as usize], key);
                }
                peer.cancelled(o.piece, o.block, now);
                cancels.push((key, block_request(&self.meta, o.piece, o.block)));
            }
        }
        self.cancelled_requests += cancels.len() as u64;
        cancels
    }

    // ----- Requests ----------------------------------------------------------------------

    /// Tops up the queues of all `keys`, handing out at most [`ROUND_BLOCKS`] per peer in turn.
    /// Peers serve their queues in order, so filling one peer at a time would put all of the
    /// most urgent data behind a single peer while the others send later data; taking turns
    /// spreads it over all of them.
    pub(crate) fn fill_all(
        &mut self,
        keys: &[PeerKey],
        now: Instant,
    ) -> Vec<(PeerKey, Vec<Request>)> {
        if self.paused {
            return Vec::new();
        }
        let mut fills: Vec<(PeerKey, Vec<Request>)> =
            keys.iter().map(|&k| (k, Vec::new())).collect();
        loop {
            let mut progressed = false;
            for (key, requests) in &mut fills {
                let more = self.fill_up_to(*key, now, ROUND_BLOCKS);
                progressed |= !more.is_empty();
                requests.extend(more);
            }
            if !progressed {
                return fills;
            }
        }
    }

    /// Chooses requests to top up a peer's queue.
    pub(crate) fn fill(&mut self, key: PeerKey, now: Instant) -> Vec<Request> {
        if self.paused {
            return Vec::new();
        }
        self.fill_up_to(key, now, usize::MAX)
    }

    /// The critical pieces in the order to request them.
    fn critical_order(&self) -> Vec<u32> {
        let mut critical = self.critical_pieces.clone();
        // A player reading ahead as fast as data arrives keeps its next piece due now; the
        // index it also waits for goes first.
        critical.sort_by_key(|&p| {
            let index = self.index_pieces.binary_search(&p).is_ok();
            (self.urgency[p as usize], !index, p)
        });
        critical
    }

    /// Chooses up to `max` requests to top up a peer's queue.
    fn fill_up_to(&mut self, key: PeerKey, now: Instant, max: usize) -> Vec<Request> {
        let Some(peer) = self.peers.get(&key) else {
            return Vec::new();
        };
        if peer.choked || peer.has_count == 0 {
            return Vec::new();
        }
        let target = self.target_queue(peer);
        let mut want = target.saturating_sub(peer.outstanding.len()).min(max);
        let mut picked: Vec<(u32, u32)> = Vec::new();
        let rivals = self.rivals(key);

        // 1. Urgent pieces, most urgent first. In the critical phase, only critical pieces.
        // Outside the critical phase `urgent_order` is read in place (nothing below changes it),
        // so a fill does not copy it.
        let critical = self.critical.then(|| self.critical_order());
        let urgent_len = critical.as_ref().map_or(self.urgent_order.len(), Vec::len);
        for i in 0..urgent_len {
            if want == 0 {
                break;
            }
            let piece = match &critical {
                Some(critical) => critical[i],
                None => self.urgent_order[i],
            };
            let due = self.urgency[piece as usize].unwrap_or(Duration::MAX);
            if !self.peer_has(key, piece) || self.have(piece) || self.verifying.contains_key(&piece)
            {
                continue;
            }
            // In the critical phase the player is waiting: everything is due now.
            let needed_in = if self.critical { Duration::ZERO } else { due };
            let blocks = (self.meta.blocks_in(piece) as usize).min(want);
            if self.leave_to_faster(key, piece, needed_in, picked.len(), blocks, &rivals) {
                continue;
            }
            let mine = self.eta(key, picked.len());
            self.ensure_progress(piece, true, now);
            let duplicate = !self.cfg.endgame_horizon.is_zero() && due <= self.cfg.endgame_horizon;
            let rescue = mine.zip(self.rate_of(key)).map(|(eta, rate)| Rescue {
                needed_in,
                eta,
                rate,
            });
            want -= self.take_blocks(key, piece, want, duplicate, rescue, now, &mut picked);
        }

        // 2. In the critical phase nothing else competes for bandwidth.
        if !self.critical && want > 0 {
            // Finish started pieces before starting new ones.
            let mut started: Vec<u32> = self.progress.keys().copied().collect();
            started.sort_unstable();
            for piece in started {
                if want == 0 {
                    break;
                }
                // Started urgent pieces follow the same rule as in step 1.
                let blocks = (self.meta.blocks_in(piece) as usize).min(want);
                let leave = self.urgency[piece as usize].is_some_and(|due| {
                    self.leave_to_faster(key, piece, due, picked.len(), blocks, &rivals)
                });
                if self.peer_has(key, piece) && !leave {
                    want -= self.take_blocks(key, piece, want, false, None, now, &mut picked);
                }
            }
            // 3. Rarest wanted pieces first, skipping the run of pieces already taken care of
            // at the front, which grows as pieces are started.
            let mut i = self.background_skip;
            while want > 0 && i < self.background.len() {
                let piece = self.background[i];
                i += 1;
                if self.have(piece)
                    || self.progress.contains_key(&piece)
                    || self.verifying.contains_key(&piece)
                {
                    if self.background_skip == i - 1 {
                        self.background_skip = i;
                    }
                    continue;
                }
                if !self.peer_has(key, piece) {
                    continue;
                }
                if !self.ensure_progress(piece, false, now) {
                    break;
                }
                want -= self.take_blocks(key, piece, want, false, None, now, &mut picked);
            }
        }

        let peer = self
            .peers
            .get_mut(&key)
            .unwrap_or_else(|| unreachable_peer());
        picked
            .into_iter()
            .map(|(piece, block)| {
                if peer.outstanding.is_empty() {
                    peer.last_progress = Some(now);
                }
                let ahead = u32::try_from(peer.outstanding.len())
                    .unwrap_or(u32::MAX)
                    .saturating_add(peer.cancelled_pending);
                peer.outstanding.push(Outstanding {
                    piece,
                    block,
                    at: now,
                    ahead,
                });
                block_request(&self.meta, piece, block)
            })
            .collect()
    }

    fn peer_has(&self, key: PeerKey, piece: u32) -> bool {
        self.peers
            .get(&key)
            .is_some_and(|p| p.has.get(piece as usize))
    }

    /// Creates download state for a piece. Background pieces respect the engine's memory
    /// budget, and leave part of it to torrents being played.
    fn ensure_progress(&mut self, piece: u32, urgent: bool, now: Instant) -> bool {
        if self.progress.contains_key(&piece) {
            return true;
        }
        let size = u64::from(self.meta.piece_size(piece));
        // One piece at a time is always allowed, so a torrent whose partial pieces stall
        // cannot starve the others.
        let limit = if urgent || self.in_progress_bytes == 0 {
            u64::MAX
        } else if self.interests.is_empty() {
            self.budget.idle_limit()
        } else {
            self.budget.limit
        };
        if !self.budget.try_take(size, limit) {
            return false;
        }
        let blocks = self.meta.blocks_in(piece) as usize;
        self.progress.insert(
            piece,
            Progress {
                blocks: vec![BlockState::Missing; blocks],
                data: vec![0; usize::try_from(size).unwrap_or(0)],
                received: 0,
                senders: vec![None; blocks],
                owner: None,
                active: now,
            },
        );
        self.in_progress_bytes += size;
        true
    }

    /// Forgets the partial download of `piece` and gives its memory back. Blocks still on
    /// their way are counted as wasted when they arrive.
    fn discard_progress(&mut self, piece: u32) {
        if let Some(progress) = self.progress.remove(&piece) {
            let size = progress.data.len() as u64;
            self.in_progress_bytes -= size;
            self.budget.give_back(size);
            self.background_skip = 0;
        }
    }

    /// When a block requested from `key` now, after `extra` more, would arrive: what is queued
    /// ahead of it at the peer's rate, plus a round trip. `None` until the rate is measured.
    fn eta(&self, key: PeerKey, extra: usize) -> Option<Duration> {
        let peer = self.peers.get(&key)?;
        let rate = self.rate_of(key)?;
        #[allow(clippy::cast_precision_loss)]
        let bytes = (peer.outstanding.len() + extra + 1) as f64 * f64::from(BLOCK_LEN);
        Some(Duration::from_secs_f64(bytes / rate) + peer.base_rtt.unwrap_or_default())
    }

    /// A peer's rate for urgent work (its recent peak, see [`Rate::peak`]); `None` until
    /// measured.
    fn rate_of(&self, key: PeerKey) -> Option<f64> {
        let peer = self.peers.get(&key)?;
        let rate = peer.rate.bytes_per_sec.max(peer.rate.peak);
        (peer.rate.measured && rate > 0.0).then_some(rate)
    }

    /// The other unchoked peers with a measured rate, for [`Self::leave_to_faster`].
    fn rivals(&self, key: PeerKey) -> Vec<Rival> {
        self.peers
            .iter()
            .filter(|(k, p)| **k != key && !p.choked && p.has_count > 0)
            .filter_map(|(&k, _)| {
                Some(Rival {
                    key: k,
                    eta: self.eta(k, 0)?,
                    rate: self.rate_of(k)?,
                })
            })
            .collect()
    }

    /// Whether `key` should leave `blocks` blocks of `piece` (after `extra` it was just given) to
    /// a rival: only if it would deliver them after they are needed (`needed_in`), and a rival
    /// that has the piece would deliver them [`FASTER_BY`] times sooner. What counts is the last
    /// block, as a piece is complete only then: a peer with a short round trip but a slow link
    /// delivers the first block early and the piece late. A slow peer then fetches data that is
    /// needed later, instead of holding up the next seconds of video.
    fn leave_to_faster(
        &self,
        key: PeerKey,
        piece: u32,
        needed_in: Duration,
        extra: usize,
        blocks: usize,
        rivals: &[Rival],
    ) -> bool {
        let last = blocks.saturating_sub(1);
        let Some(mine) = self.eta(key, extra + last).filter(|&mine| mine > needed_in) else {
            return false;
        };
        rivals
            .iter()
            .any(|r| r.finish(last).mul_f64(FASTER_BY) < mine && self.peer_has(r.key, piece))
    }

    /// Marks up to `want` blocks of `piece` as requested from `key`. Returns how many.
    ///
    /// A block another peer is fetching is requested again if `allow_duplicate` and the holder
    /// has been at it for [`SchedulerConfig::duplicate_after`] (and is snubbed, with
    /// [`SchedulerConfig::duplicate_only_snubbed`]); or, with `rescue`, if the holder is
    /// [`FASTER_BY`] times slower than `key` and would deliver it after it is needed and that
    /// many times later than `key`. A holder that is merely busier is left alone: it delivers
    /// its queue as fast as `key` would, and peers that ignore `cancel` would send both copies.
    // Each argument is a separate decision input; bundling them would only rename them.
    #[allow(clippy::too_many_arguments)]
    fn take_blocks(
        &mut self,
        key: PeerKey,
        piece: u32,
        want: usize,
        allow_duplicate: bool,
        rescue: Option<Rescue>,
        now: Instant,
        picked: &mut Vec<(u32, u32)>,
    ) -> usize {
        let duplicate_after = self.cfg.duplicate_after;
        let only_snubbed = self.cfg.duplicate_only_snubbed;
        let snub_after = self.cfg.snub_after;
        let peers = &self.peers;
        let snubbed = |holder: PeerKey| {
            peers.get(&holder).is_none_or(|p| {
                !p.outstanding.is_empty()
                    && p.last_progress
                        .is_none_or(|t| now.saturating_duration_since(t) >= snub_after)
            })
        };
        // Whether `holder` would deliver block `block` (its queue up to it, at its rate) only
        // after it is needed, and much later than `key` could.
        let late = |holder: PeerKey, block: u32| {
            let Some(rescue) = rescue else {
                return false;
            };
            peers.get(&holder).is_some_and(|p| {
                let rate = p.rate.bytes_per_sec.max(p.rate.peak);
                // The cheap test first: the queue scan below is linear in the holder's
                // outstanding requests, and most holders are not much slower.
                if !(p.rate.measured && rate > 0.0 && rate * FASTER_BY < rescue.rate) {
                    return false;
                }
                let Some(at) = p
                    .outstanding
                    .iter()
                    .position(|o| o.piece == piece && o.block == block)
                else {
                    return false;
                };
                #[allow(clippy::cast_precision_loss)]
                let bytes = (at + 1) as f64 * f64::from(BLOCK_LEN);
                let remaining = Duration::from_secs_f64(bytes / rate);
                remaining > rescue.needed_in && remaining > rescue.eta.mul_f64(FASTER_BY)
            })
        };
        let suspect = self.suspects.contains_key(&piece);
        let Some(progress) = self.progress.get_mut(&piece) else {
            return 0;
        };
        // A piece that failed its hash check is downloaded again from one peer, so that its
        // next check says whose data was wrong. Another peer takes over once that one has
        // stopped (it left, was choked, holds none of the piece's blocks, or is snubbed), or,
        // for an urgent piece, once `key` is much faster and that one would deliver a block
        // late: a peer that keeps sending a little data must not hold up playback. Each block's
        // sender is still recorded, so smart ban names the right peers after a handover.
        let mut takeover = None;
        if suspect {
            if let Some(owner) = progress.owner.filter(|&o| o != key) {
                let mut holds = false;
                let mut slow = false;
                for (i, b) in progress.blocks.iter().enumerate() {
                    if let BlockState::Requested(holders) = b
                        && holders.iter().any(|(k, _)| *k == owner)
                    {
                        holds = true;
                        #[allow(clippy::cast_possible_truncation)]
                        let block = i as u32;
                        slow |= late(owner, block);
                    }
                }
                let working = peers.get(&owner).is_some_and(|p| !p.choked);
                if working && holds && !snubbed(owner) && !slow {
                    return 0;
                }
                takeover = Some(owner);
            }
            progress.owner = Some(key);
        }
        let mut taken = 0;
        for (i, state) in progress.blocks.iter_mut().enumerate() {
            if taken == want {
                break;
            }
            let take = match state {
                BlockState::Missing => true,
                BlockState::Requested(holders) => {
                    #[allow(clippy::cast_possible_truncation)]
                    let block = i as u32;
                    let taken_over = holders.iter().all(|(k, _)| Some(*k) == takeover);
                    let stalled = taken_over
                        || allow_duplicate
                            && holders.iter().all(|(_, at)| {
                                now.saturating_duration_since(*at) >= duplicate_after
                            })
                            && (!only_snubbed || holders.iter().all(|(k, _)| snubbed(*k)));
                    holders.len() < 2
                        && holders.iter().all(|(k, _)| *k != key)
                        && (stalled || holders.iter().all(|(k, _)| late(*k, block)))
                }
                BlockState::Received => false,
            };
            if take {
                match state {
                    BlockState::Requested(holders) => {
                        holders.push((key, now));
                        self.duplicate_requests += 1;
                    }
                    _ => *state = BlockState::Requested(vec![(key, now)]),
                }
                #[allow(clippy::cast_possible_truncation)]
                picked.push((piece, i as u32));
                taken += 1;
            }
        }
        if taken > 0 {
            progress.active = now;
        }
        taken
    }

    // ----- Responses ---------------------------------------------------------------------

    /// Records a received block. Data the peer was not asked for (neither requested from it
    /// nor among its recently cancelled requests), malformed data, and duplicates are ignored.
    pub(crate) fn on_block(
        &mut self,
        key: PeerKey,
        piece: u32,
        begin: u32,
        data: &[u8],
    ) -> BlockOutcome {
        self.on_block_at(key, piece, begin, data, Instant::now())
    }

    /// [`Scheduler::on_block`] with an explicit time, for tests.
    pub(crate) fn on_block_at(
        &mut self,
        key: PeerKey,
        piece: u32,
        begin: u32,
        data: &[u8],
        now: Instant,
    ) -> BlockOutcome {
        let mut outcome = BlockOutcome::default();
        let Some(peer) = self.peers.get_mut(&key) else {
            return outcome;
        };
        if !begin.is_multiple_of(BLOCK_LEN) || piece >= self.meta.piece_count() {
            return outcome;
        }
        let block = begin / BLOCK_LEN;
        // Checked first: a block past the end of the piece has no request.
        if block >= self.meta.blocks_in(piece) {
            return outcome;
        }
        let expected = block_request(&self.meta, piece, block);
        if data.len() != expected.len as usize {
            return outcome;
        }
        let requested = peer
            .outstanding
            .iter()
            .any(|o| o.piece == piece && o.block == block);
        if !requested {
            // Only a block cancelled at this peer that it sent anyway is accepted. Anything
            // else could slip bad data into pieces other peers are fetching.
            let Some(at) = peer
                .cancelled
                .iter()
                .position(|&(p, b)| p == piece && b == block)
            else {
                self.wasted_bytes += data.len() as u64;
                return outcome;
            };
            peer.cancelled.remove(at);
            peer.cancelled_pending = peer.cancelled_pending.saturating_sub(1);
        }
        // Only data the peer was asked for counts towards its rate: flooding us with blocks
        // nobody asked for must not make a peer look fast, which earns deeper queues and
        // urgent blocks taken from other peers.
        peer.rate.add(data.len() as u64);
        self.total_rate.add(data.len() as u64);
        peer.last_progress = Some(now);
        peer.last_activity = Some(now);
        // A request sent to an empty queue waits behind nothing, so its latency is the round
        // trip plus one block transfer: an unbiased round-trip sample.
        if let Some(o) = peer
            .outstanding
            .iter()
            .find(|o| o.piece == piece && o.block == block && o.ahead == 0)
        {
            let sample = now.saturating_duration_since(o.at);
            let expired = peer
                .base_rtt_at
                .is_none_or(|at| now.saturating_duration_since(at) >= RTT_WINDOW);
            if expired || peer.base_rtt.is_none_or(|rtt| sample <= rtt) {
                peer.base_rtt = Some(sample);
                peer.base_rtt_at = Some(now);
            }
        }
        peer.outstanding
            .retain(|o| !(o.piece == piece && o.block == block));
        let Some(progress) = self.progress.get_mut(&piece) else {
            self.wasted_bytes += data.len() as u64;
            return outcome;
        };
        let state = std::mem::replace(&mut progress.blocks[block as usize], BlockState::Received);
        match state {
            BlockState::Received => {
                self.wasted_bytes += data.len() as u64;
                return outcome;
            }
            BlockState::Requested(holders) => {
                for (other, _) in holders.into_iter().filter(|(k, _)| *k != key) {
                    if let Some(peer) = self.peers.get_mut(&other) {
                        peer.outstanding
                            .retain(|o| !(o.piece == piece && o.block == block));
                        peer.cancelled(piece, block, now);
                    }
                    outcome.cancels.push((other, expected));
                }
            }
            BlockState::Missing => {}
        }
        let at = begin as usize;
        progress.data[at..at + data.len()].copy_from_slice(data);
        progress.received += 1;
        progress.active = now;
        progress.senders[block as usize] = Some(key);
        if progress.received as usize == progress.blocks.len()
            && let Some(done) = self.progress.remove(&piece)
        {
            self.in_progress_bytes -= done.data.len() as u64;
            self.budget.give_back(done.data.len() as u64);
            // Every block was received, so every block has a sender.
            let senders = done.senders.into_iter().flatten().collect();
            self.verifying.insert(piece, senders);
            outcome.completed = Some((piece, done.data));
        }
        outcome
    }

    /// Records the check of a completed piece and returns what it says about the peers.
    ///
    /// A piece that failed is downloaded again; its senders are returned in
    /// [`Verified::failed_by`], and each block's digest is remembered with its sender. When such
    /// a piece later verifies (with its block digests, see
    /// [`Scheduler::needs_block_digests`]), every peer that had sent a block different from the
    /// verified one is returned in [`Verified::banned`]: those sent bad data, and no other peer
    /// is blamed for it, however the failed piece was put together.
    pub(crate) fn on_verified(&mut self, piece: u32, check: PieceCheck) -> Verified {
        let senders = self.verifying.remove(&piece).unwrap_or_default();
        let mut verified = Verified::default();
        match check {
            PieceCheck::Valid(digests) => {
                if let Some(slot) = self.have.get_mut(piece as usize)
                    && !*slot
                {
                    *slot = true;
                    self.have_count += 1;
                }
                let records = self.suspects.remove(&piece);
                if records.is_some() {
                    self.suspect_order.retain(|&p| p != piece);
                }
                if let Some((records, digests)) = records.zip(digests) {
                    for (records, good) in records.iter().zip(&digests) {
                        for &(key, digest) in records {
                            if digest != *good && !verified.banned.contains(&key) {
                                verified.banned.push(key);
                            }
                        }
                    }
                }
            }
            PieceCheck::Invalid(digests) => {
                for &key in &senders {
                    if !verified.failed_by.contains(&key) {
                        verified.failed_by.push(key);
                        if let Some(peer) = self.peers.get_mut(&key) {
                            peer.bad_pieces += 1;
                        }
                    }
                }
                self.remember_suspect(piece, &senders, &digests);
                self.background_skip = 0;
            }
            PieceCheck::NotStored => self.background_skip = 0,
        }
        verified
    }

    /// Remembers who sent which data for a piece that failed its hash check, within
    /// [`MAX_SUSPECT_PIECES`] and [`MAX_RECORDS_PER_BLOCK`].
    fn remember_suspect(&mut self, piece: u32, senders: &[PeerKey], digests: &[BlockDigest]) {
        if !self.suspects.contains_key(&piece) {
            if self.suspect_order.len() >= MAX_SUSPECT_PIECES
                && let Some(oldest) = self.suspect_order.pop_front()
            {
                self.suspects.remove(&oldest);
            }
            self.suspect_order.push_back(piece);
        }
        let blocks = self.meta.blocks_in(piece) as usize;
        let records = self
            .suspects
            .entry(piece)
            .or_insert_with(|| vec![Vec::new(); blocks]);
        for ((records, &key), &digest) in records.iter_mut().zip(senders).zip(digests) {
            if records.len() < MAX_RECORDS_PER_BLOCK && !records.contains(&(key, digest)) {
                records.push((key, digest));
            }
        }
    }

    pub(crate) fn bad_pieces(&self, key: PeerKey) -> u32 {
        self.peers.get(&key).map_or(0, |p| p.bad_pieces)
    }

    #[cfg(test)]
    pub(crate) fn timeouts(&self, key: PeerKey) -> u32 {
        self.peers.get(&key).map_or(0, |p| p.timeouts)
    }

    /// Peers that have timed out this often are not worth keeping.
    pub(crate) fn unresponsive_peers(&self, max_timeouts: u32) -> Vec<PeerKey> {
        self.peers
            .iter()
            .filter(|(_, p)| p.timeouts >= max_timeouts)
            .map(|(k, _)| *k)
            .collect()
    }

    /// Sets the download limit in force (bytes per second), so request queues fit it: under a
    /// limit the peers share it, and queues sized from each peer's own (bursty) rate would
    /// together hold requests for many seconds, which then time out and are fetched twice.
    pub(crate) fn set_rate_limit(&mut self, bytes_per_sec: Option<f64>) {
        self.rate_limit = bytes_per_sec;
    }

    // ----- Time --------------------------------------------------------------------------

    /// Updates rates, gives up on timed-out requests (returned as cancels), and periodically
    /// refreshes the rarest-first order.
    pub(crate) fn tick(
        &mut self,
        now: Instant,
        refresh_background: bool,
    ) -> Vec<(PeerKey, Request)> {
        match self.last_rate_update {
            Some(last) if now.saturating_duration_since(last) >= RATE_WINDOW => {
                let elapsed = now.saturating_duration_since(last);
                for peer in self.peers.values_mut() {
                    peer.rate.update(elapsed);
                }
                self.total_rate.update(elapsed);
                if tracing::enabled!(tracing::Level::TRACE) {
                    for (key, peer) in &self.peers {
                        tracing::trace!(
                            peer = key,
                            rate_kib = peer.rate.bytes_per_sec / 1024.0,
                            rtt_ms = peer.base_rtt.map(|r| r.as_secs_f64() * 1000.0),
                            target = self.target_queue(peer),
                            outstanding = peer.outstanding.len(),
                            choked = peer.choked,
                            relaxed = self.relaxed(),
                            critical = self.critical,
                            "peer queue"
                        );
                    }
                }
                self.last_rate_update = Some(now);
            }
            Some(_) => {}
            None => self.last_rate_update = Some(now),
        }

        let timeout = self.cfg.request_timeout;
        let mut cancels = Vec::new();
        for (&key, peer) in &mut self.peers {
            let (expired, waiting): (Vec<Outstanding>, Vec<Outstanding>) =
                std::mem::take(&mut peer.outstanding)
                    .into_iter()
                    .partition(|o| now.saturating_duration_since(o.at) >= timeout);
            peer.outstanding = waiting;
            peer.timeouts += u32::try_from(expired.len()).unwrap_or(u32::MAX);
            for o in expired {
                if let Some(progress) = self.progress.get_mut(&o.piece) {
                    release_holder(&mut progress.blocks[o.block as usize], key);
                }
                peer.cancelled(o.piece, o.block, now);
                cancels.push((key, block_request(&self.meta, o.piece, o.block)));
            }
            // A peer quiet for a while after cancels has honoured them: nothing more is coming.
            let quiet = peer.base_rtt.unwrap_or_default() * 2 + Duration::from_secs(1);
            if peer.cancelled_pending > 0
                && peer
                    .last_activity
                    .is_none_or(|t| now.saturating_duration_since(t) >= quiet)
            {
                peer.cancelled_pending = 0;
            }
        }

        self.cancelled_requests += cancels.len() as u64;
        if self.budget.used() >= self.budget.idle_limit() {
            let stale: Vec<u32> = self
                .progress
                .iter()
                .filter(|(_, p)| {
                    now.saturating_duration_since(p.active) >= STALE_PIECE
                        && !p
                            .blocks
                            .iter()
                            .any(|b| matches!(b, BlockState::Requested(_)))
                })
                .map(|(&piece, _)| piece)
                .collect();
            for piece in stale {
                self.discard_progress(piece);
            }
        }
        if refresh_background {
            let mut order: Vec<u32> = (0..self.meta.piece_count())
                .filter(|&p| self.wanted[p as usize] && !self.have[p as usize])
                .collect();
            order.sort_by_key(|&p| (self.availability[p as usize], p));
            self.background = order;
            self.background_skip = 0;
        }
        cancels
    }
}

impl Drop for Scheduler {
    /// Gives this torrent's partial pieces back to the engine's budget.
    fn drop(&mut self) {
        self.budget.give_back(self.in_progress_bytes);
    }
}

fn set_urgency(
    urgency: &mut [Option<Duration>],
    touched: &mut Vec<u32>,
    have: &[bool],
    piece: u32,
    due: Duration,
) {
    let i = piece as usize;
    if have[i] {
        return;
    }
    match urgency[i] {
        None => {
            urgency[i] = Some(due);
            touched.push(piece);
        }
        Some(existing) if due < existing => urgency[i] = Some(due),
        Some(_) => {}
    }
}

fn release_holder(state: &mut BlockState, key: PeerKey) {
    if let BlockState::Requested(holders) = state {
        holders.retain(|(k, _)| *k != key);
        if holders.is_empty() {
            *state = BlockState::Missing;
        }
    }
}

fn block_request(meta: &Metainfo, piece: u32, block: u32) -> Request {
    let begin = block * BLOCK_LEN;
    Request {
        piece,
        begin,
        len: (meta.piece_size(piece) - begin).min(BLOCK_LEN),
    }
}

/// Peers are only removed through `remove_peer`, never while `fill` runs.
#[cold]
fn unreachable_peer() -> &'static mut Peer {
    // `fill` returns early when the peer is missing, so this cannot happen.
    #[allow(clippy::panic)]
    {
        panic!("peer disappeared during fill")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PIECE: u32 = 4 * BLOCK_LEN; // 64 KiB pieces, 4 blocks each
    const MIB: u64 = 1024 * 1024;

    /// A single-file torrent of `pieces` pieces.
    fn meta(pieces: u32) -> Arc<Metainfo> {
        let len = u64::from(pieces) * u64::from(PIECE);
        let mut t = format!(
            "d4:infod6:lengthi{len}e4:name5:a.mp412:piece lengthi{PIECE}e6:pieces{}:",
            pieces * 20
        )
        .into_bytes();
        t.extend(std::iter::repeat_n(0u8, (pieces * 20) as usize));
        t.extend_from_slice(b"ee");
        Arc::new(Metainfo::from_bytes(&t).unwrap())
    }

    fn sched(pieces: u32, cfg: SchedulerConfig) -> Scheduler {
        Scheduler::new(meta(pieces), cfg)
    }

    /// Defaults with a 4-block initial queue, which keeps the expected fills small.
    fn small_queues() -> SchedulerConfig {
        SchedulerConfig {
            initial_queue: 4,
            ..SchedulerConfig::default()
        }
    }

    /// Adds an unchoked seed.
    fn seed(s: &mut Scheduler, key: PeerKey) {
        s.add_peer(key);
        let bits = vec![0xffu8; s.have.len().div_ceil(8)];
        assert!(s.on_bitfield(key, &bits));
        s.on_unchoke(key);
    }

    fn reader(position: u64, s: &Scheduler) -> Interest {
        Interest {
            kind: InterestKind::Reader,
            file_offset: 0,
            file_len: s.meta.total_len,
            position,
            byte_rate: Some(MIB),
        }
    }

    fn deliver(s: &mut Scheduler, key: PeerKey, r: Request) -> BlockOutcome {
        s.on_block(key, r.piece, r.begin, &vec![0u8; r.len as usize])
    }

    #[test]
    fn the_partial_piece_budget_is_shared_and_favours_the_torrent_being_played() {
        let piece = u64::from(PIECE);
        let budget = PieceBudget::new(8 * piece);
        let shared =
            |pieces| Scheduler::with_budget(meta(pieces), small_queues(), Arc::clone(&budget));
        let mut background = shared(100);
        let mut playing = shared(100);
        playing.set_interest(1, reader(0, &playing));

        // A torrent nobody is playing stops at three quarters of the budget...
        let started = (0..100)
            .take_while(|&p| background.ensure_progress(p, false, Instant::now()))
            .count();
        assert_eq!(started, 6);
        // ...and the torrent being played gets the rest.
        let started = (0..100)
            .take_while(|&p| playing.ensure_progress(p, false, Instant::now()))
            .count();
        assert_eq!(started, 2);
        assert_eq!(budget.used(), 8 * piece);

        // Another torrent can still work on one piece, and urgent pieces are never held back.
        let mut third = shared(100);
        assert!(third.ensure_progress(0, false, Instant::now()));
        assert!(!third.ensure_progress(1, false, Instant::now()));
        assert!(playing.ensure_progress(50, true, Instant::now()));
        assert_eq!(budget.used(), 10 * piece);

        // A torrent that goes away gives back its pieces: alone, the one being played gets the
        // whole budget, as a torrent did before the budget was shared.
        drop((background, third));
        assert_eq!(budget.used(), 3 * piece);
        let started = (3..100)
            .take_while(|&p| playing.ensure_progress(p, false, Instant::now()))
            .count();
        assert_eq!(started, 5);
        assert_eq!(budget.used(), 8 * piece);
    }

    #[test]
    fn a_torrent_finishes_its_pieces_while_others_hold_the_whole_budget() {
        let piece = u64::from(PIECE);
        let budget = PieceBudget::new(4 * piece);
        // A torrent whose partial pieces stall (its peers left) holds every byte it may.
        let mut stalled = Scheduler::with_budget(meta(100), small_queues(), Arc::clone(&budget));
        let started = (0..100)
            .take_while(|&p| stalled.ensure_progress(p, false, Instant::now()))
            .count();
        assert_eq!(started, 3);
        assert_eq!(budget.used(), 3 * piece);

        let mut s = Scheduler::with_budget(meta(20), small_queues(), Arc::clone(&budget));
        s.want_range(0, s.meta.total_len);
        s.tick(Instant::now(), true);
        seed(&mut s, 1);
        for _ in 0..100 {
            for r in s.fill(1, Instant::now()) {
                if let Some((piece, _)) = deliver(&mut s, 1, r).completed {
                    s.on_verified(piece, PieceCheck::Valid(None));
                }
            }
            assert!(budget.used() <= 4 * piece);
        }
        assert_eq!(s.stats().have_pieces, 20, "every piece downloaded");
        assert_eq!(budget.used(), 3 * piece, "only the stalled pieces are held");
    }

    #[test]
    fn pausing_gives_back_partial_pieces_and_ignores_their_late_blocks() {
        let budget = PieceBudget::new(64 * MIB);
        let mut s = Scheduler::with_budget(meta(10), small_queues(), Arc::clone(&budget));
        s.want_range(0, s.meta.total_len);
        s.tick(Instant::now(), true);
        seed(&mut s, 1);
        let requests = s.fill(1, Instant::now());
        assert!(budget.used() > 0);

        s.set_paused(true);
        assert_eq!(budget.used(), 0, "a paused torrent holds no partial pieces");
        assert!(s.progress.is_empty());
        // A peer that ignores the cancels still sends them: dropped, not stored.
        let wasted = s.stats().wasted_bytes;
        for r in requests {
            assert!(deliver(&mut s, 1, r).completed.is_none());
        }
        assert_eq!(s.stats().wasted_bytes, wasted + u64::from(PIECE));
        assert_eq!(budget.used(), 0);

        s.set_paused(false);
        for _ in 0..20 {
            for r in s.fill(1, Instant::now()) {
                if let Some((piece, _)) = deliver(&mut s, 1, r).completed {
                    s.on_verified(piece, PieceCheck::Valid(None));
                }
            }
        }
        assert_eq!(s.stats().have_pieces, 10, "downloaded again after resuming");
        assert_eq!(budget.used(), 0);
    }

    #[test]
    fn stalled_partial_pieces_are_given_back_while_the_budget_is_contended() {
        let piece = u64::from(PIECE);
        let cfg = SchedulerConfig {
            request_timeout: Duration::from_secs(600),
            ..small_queues()
        };
        let t0 = Instant::now();
        let stall = |budget: &Arc<PieceBudget>| {
            let mut s = Scheduler::with_budget(meta(100), cfg.clone(), Arc::clone(budget));
            s.want_range(0, s.meta.total_len);
            s.tick(t0, true);
            seed(&mut s, 1);
            // Piece 0 has requests outstanding at a peer that never answers; two more pieces
            // were started and have none.
            assert!(s.fill(1, t0).iter().all(|r| r.piece == 0));
            assert!(s.ensure_progress(10, false, t0) && s.ensure_progress(11, false, t0));
            s
        };

        // Uncontended, nothing is dropped however long it stalls.
        let budget = PieceBudget::new(100 * piece);
        let mut s = stall(&budget);
        s.tick(t0 + 10 * STALE_PIECE, false);
        assert_eq!(budget.used(), 3 * piece);

        // Contended (it holds all that a torrent nobody plays may), stale pieces without
        // outstanding requests are given back.
        let budget = PieceBudget::new(4 * piece);
        let mut s = stall(&budget);
        s.tick(t0 + Duration::from_secs(59), false);
        assert_eq!(budget.used(), 3 * piece, "not stale yet");
        s.tick(t0 + STALE_PIECE, false);
        assert_eq!(
            budget.used(),
            piece,
            "only the piece with requests outstanding is kept"
        );
        assert_eq!(s.progress.keys().copied().collect::<Vec<_>>(), [0]);
    }

    #[test]
    fn the_download_rate_counts_what_arrives_while_peers_come_and_go() {
        // As under a download limit: peers keep joining, each delivers a burst, then leaves.
        let mut s = sched(1000, SchedulerConfig::default());
        let t0 = Instant::now();
        let step = Duration::from_millis(100);
        s.tick(t0, true);
        let (mut delivered, mut reported) = (0u64, 0.0);
        for i in 1..=300u32 {
            let key = PeerKey::from(i / 10);
            if i % 10 == 1 && i < 200 {
                s.add_peer(key);
                for block in 0..4 {
                    // Requested, or it would not count.
                    s.peers
                        .get_mut(&key)
                        .unwrap()
                        .outstanding
                        .push(Outstanding {
                            piece: i,
                            block,
                            at: t0,
                            ahead: 1,
                        });
                    s.on_block_at(key, i, block * BLOCK_LEN, &[0; BLOCK_LEN as usize], t0);
                    delivered += u64::from(BLOCK_LEN);
                }
            }
            if i % 10 == 5 {
                s.remove_peer(key);
            }
            s.tick(t0 + step * i, false);
            reported += s.stats().download_bps * step.as_secs_f64();
        }
        #[allow(clippy::cast_precision_loss)]
        let ratio = reported / delivered as f64;
        assert!(
            (0.95..=1.0).contains(&ratio),
            "reported {ratio:.2}× what arrived"
        );
    }

    #[test]
    fn queues_start_at_the_initial_size_then_follow_the_measured_rate() {
        let mut s = sched(
            1000,
            SchedulerConfig {
                relaxed_queue_time: Duration::from_millis(500),
                ..SchedulerConfig::default()
            },
        );
        s.want_range(0, s.meta.total_len);
        s.tick(Instant::now(), true);
        seed(&mut s, 1);
        let now = Instant::now();
        assert_eq!(
            s.fill(1, now).len(),
            32,
            "initial_queue before the rate is known"
        );

        // Deliver 4 MiB/s worth for one second: 0.5 s queue at 4 MiB/s = 128 blocks (the max).
        s.peers.get_mut(&1).unwrap().rate.add(4 * MIB);
        s.tick(now + Duration::from_secs(1), false);
        assert_eq!(s.target_queue(&s.peers[&1]), 128);
        s.peers.get_mut(&1).unwrap().rate = Rate {
            bytes_since: 0,
            bytes_per_sec: 320.0 * 1024.0,
            measured: true,
            peak: 0.0,
        };
        assert_eq!(
            s.target_queue(&s.peers[&1]),
            10,
            "320 KiB/s × 0.5 s = 10 blocks"
        );
        s.peers.get_mut(&1).unwrap().rate.bytes_per_sec = 1.0;
        assert_eq!(
            s.target_queue(&s.peers[&1]),
            4,
            "never below min_queue once measured"
        );
    }

    #[test]
    fn under_a_download_limit_queues_fit_each_peers_share() {
        let mut s = sched(
            1000,
            SchedulerConfig {
                relaxed_queue_time: Duration::from_millis(500),
                ..SchedulerConfig::default()
            },
        );
        s.want_range(0, s.meta.total_len);
        s.tick(Instant::now(), true);
        for key in 1..=40 {
            seed(&mut s, key);
        }
        // 40 peers sharing 2 MiB/s: about 51 KiB/s each, 2 blocks in the 0.5 s queue time, so
        // the minimum queue of 4 applies from the start instead of the initial 32. With 32
        // each, 20 MiB would be outstanding: ten seconds at the limit, past the request
        // timeout, and every late block would be fetched twice.
        assert_eq!(s.target_queue(&s.peers[&1]), 32, "no limit");
        s.set_rate_limit(Some(2.0 * 1024.0 * 1024.0));
        assert_eq!(
            s.target_queue(&s.peers[&1]),
            4,
            "initial queue under a limit"
        );
        // A measured peer bursting at 4 MiB/s still gets only its share.
        s.peers.get_mut(&1).unwrap().rate.add(4 * MIB);
        s.tick(Instant::now() + Duration::from_secs(1), false);
        assert_eq!(
            s.target_queue(&s.peers[&1]),
            4,
            "measured queue under a limit"
        );
        s.set_rate_limit(None);
        assert_eq!(s.target_queue(&s.peers[&1]), 128, "the limit lifted");
    }

    #[test]
    fn readers_inherit_the_playhead_bitrate_of_their_file() {
        let mut s = sched(1000, small_queues());
        let len = s.meta.total_len;
        // A reader near the end (a container index) with no bitrate of its own.
        let mut tail = reader(len - 8 * u64::from(PIECE), &s);
        tail.byte_rate = None;
        s.set_interest(1, tail);
        s.reprioritize(false);
        let at_fallback = s.urgency[998].unwrap();
        // Once the playhead reports 4 MiB/s, the reader's pieces become correspondingly urgent.
        s.set_interest(
            2,
            Interest {
                kind: InterestKind::Playhead,
                file_offset: 0,
                file_len: len,
                position: 0,
                byte_rate: Some(4 * MIB),
            },
        );
        s.reprioritize(false);
        let inherited = s.urgency[998].unwrap();
        assert!(
            inherited < at_fallback / 10,
            "{inherited:?} vs {at_fallback:?}"
        );
    }

    #[test]
    fn urgent_queues_use_the_recent_peak_rate() {
        let mut s = sched(1000, small_queues());
        seed(&mut s, 1);
        let t0 = Instant::now();
        s.tick(t0, false);
        // One second at 4 MiB/s, then two quiet seconds (relaxed, demand-limited).
        s.peers.get_mut(&1).unwrap().rate.add(4 * MIB);
        s.tick(t0 + Duration::from_secs(1), false);
        s.tick(t0 + Duration::from_secs(3), false);
        let rate = s.peers[&1].rate;
        assert!(rate.bytes_per_sec < 0.1 * rate.peak, "{rate:?}");
        // With urgent work, the queue is sized from the peak (≈ 3.3 MiB/s after 2 s of decay):
        // 0.5 s of that is about 105 blocks.
        s.set_interest(1, reader(0, &s));
        s.reprioritize(false);
        let target = s.target_queue(&s.peers[&1]);
        assert!((95..=115).contains(&target), "{target}");
    }

    #[test]
    fn queues_cover_at_least_two_round_trips() {
        let mut s = sched(1000, small_queues());
        seed(&mut s, 1);
        s.peers.get_mut(&1).unwrap().rate = Rate {
            bytes_since: 0,
            bytes_per_sec: 1024.0 * 1024.0,
            measured: true,
            peak: 0.0,
        };
        s.set_interest(1, reader(0, &s));
        s.reprioritize(false);
        // 0.5 s at 1 MiB/s = 32 blocks.
        assert_eq!(s.target_queue(&s.peers[&1]), 32);

        // The first block (nothing queued ahead of it) arrives after 400 ms: RTT ≈ 400 ms, so
        // the queue must hold 0.8 s = 52 blocks (rounded up).
        let t0 = Instant::now();
        let r = s.fill(1, t0)[0];
        s.on_block_at(
            1,
            r.piece,
            r.begin,
            &vec![0; r.len as usize],
            t0 + Duration::from_millis(400),
        );
        assert_eq!(s.peers[&1].base_rtt, Some(Duration::from_millis(400)));
        assert_eq!(s.target_queue(&s.peers[&1]), 52);
    }

    #[test]
    fn round_trip_estimate_is_a_windowed_minimum() {
        let cfg = SchedulerConfig {
            min_queue: 1,
            initial_queue: 1,
            ..SchedulerConfig::default()
        };
        let mut s = sched(1000, cfg);
        seed(&mut s, 1);
        s.set_interest(1, reader(0, &s));
        s.reprioritize(false);
        let t0 = Instant::now();
        // One request at a time, so each is sent to an empty queue and is a sample.
        let sample = |s: &mut Scheduler, sent: Instant, rtt_ms: u64| {
            let r = s.fill(1, sent)[0];
            s.on_block_at(
                1,
                r.piece,
                r.begin,
                &vec![0; r.len as usize],
                sent + Duration::from_millis(rtt_ms),
            );
            s.peers[&1].base_rtt.unwrap()
        };
        assert_eq!(sample(&mut s, t0, 100), Duration::from_millis(100));
        // A higher sample within the window keeps the minimum, and time alone changes nothing.
        let t1 = t0 + Duration::from_secs(5);
        s.tick(t1, false);
        assert_eq!(sample(&mut s, t1, 300), Duration::from_millis(100));
        s.tick(t0 + Duration::from_secs(9), false);
        assert_eq!(s.peers[&1].base_rtt, Some(Duration::from_millis(100)));
        // Once the minimum is older than the window, the next sample replaces it.
        let t2 = t0 + Duration::from_secs(11);
        assert_eq!(sample(&mut s, t2, 250), Duration::from_millis(250));
    }

    #[test]
    fn queues_shrink_once_only_background_work_remains() {
        let mut s = sched(1000, SchedulerConfig::default());
        s.want_range(0, s.meta.total_len);
        seed(&mut s, 1);
        s.peers.get_mut(&1).unwrap().rate = Rate {
            bytes_since: 0,
            bytes_per_sec: 2.0 * 1024.0 * 1024.0,
            measured: true,
            peak: 0.0,
        };
        // A reader with missing data ahead: 0.5 s at 2 MiB/s = 64 blocks.
        s.set_interest(1, reader(0, &s));
        s.reprioritize(false);
        assert_eq!(s.target_queue(&s.peers[&1]), 64);
        // No interests: background only, 0.15 s at 2 MiB/s = 20 blocks (rounded up).
        s.remove_interest(1);
        s.reprioritize(false);
        assert_eq!(s.target_queue(&s.peers[&1]), 20);
    }

    #[test]
    fn rates_are_time_weighted_and_ignore_tiny_windows() {
        let mut s = sched(10, SchedulerConfig::default());
        seed(&mut s, 1);
        let t0 = Instant::now();
        s.tick(t0, false);
        // A burst within 1 ms must not register as a huge rate.
        s.peers.get_mut(&1).unwrap().rate.add(1024 * 1024);
        s.tick(t0 + Duration::from_millis(1), false);
        assert!(
            !s.peers[&1].rate.measured,
            "windows shorter than 100 ms are not sampled"
        );
        s.tick(t0 + Duration::from_millis(100), false);
        let first = s.peers[&1].rate.bytes_per_sec;
        assert!(
            (first - 10.0 * 1024.0 * 1024.0).abs() < 1.0,
            "1 MiB over 100 ms = 10 MiB/s, got {first}"
        );
        // A quiet 100 ms then decays the estimate by a factor of e^(-0.1 / 0.5).
        s.tick(t0 + Duration::from_millis(200), false);
        let decayed = s.peers[&1].rate.bytes_per_sec / first;
        assert!((decayed - (-0.2f64).exp()).abs() < 0.01, "{decayed}");
    }

    #[test]
    fn critical_phase_requests_only_the_critical_bytes() {
        let cfg = SchedulerConfig {
            min_queue: 128,
            initial_queue: 128,
            ..SchedulerConfig::default()
        };
        let mut s = sched(1000, cfg);
        seed(&mut s, 1);
        // 2 MiB critical = 32 pieces of 64 KiB; the 8 MiB reader window is 128 pieces.
        s.set_interest(1, reader(100 * u64::from(PIECE), &s));
        s.reprioritize(false);
        let requests = s.fill(1, Instant::now());
        assert_eq!(requests.len(), 128, "32 critical pieces × 4 blocks");
        assert!(
            requests.iter().all(|r| (100..132).contains(&r.piece)),
            "{:?}",
            requests.last()
        );
    }

    #[test]
    fn urgent_pieces_come_first_in_deadline_order() {
        let mut s = sched(1000, small_queues());
        s.want_range(0, s.meta.total_len);
        s.tick(Instant::now(), true);
        seed(&mut s, 1);
        s.set_interest(1, reader(500 * u64::from(PIECE), &s));
        s.reprioritize(false);
        let requests = s.fill(1, Instant::now());
        assert_eq!(requests.len(), 4);
        assert!(requests.iter().all(|r| r.piece == 500), "{requests:?}");
        assert_eq!(requests[0].begin, 0);
    }

    #[test]
    fn critical_phase_suppresses_background_requests() {
        let cfg = SchedulerConfig {
            min_queue: 64,
            ..SchedulerConfig::default()
        };
        let mut s = sched(1000, cfg);
        s.want_range(0, s.meta.total_len);
        s.tick(Instant::now(), true);
        seed(&mut s, 1);
        // A reader near the end of the file: only a few pieces are urgent.
        let position = s.meta.total_len - 2 * u64::from(PIECE);
        s.set_interest(1, reader(position, &s));
        s.reprioritize(false);
        assert!(s.is_critical());
        let requests = s.fill(1, Instant::now());
        assert_eq!(
            requests.len(),
            8,
            "only the 2 urgent pieces (8 blocks), no background"
        );
        assert!(requests.iter().all(|r| r.piece >= 998));

        // Once the reader is buffered, background fills the rest of the queue.
        for r in requests {
            if let Some((piece, _)) = deliver(&mut s, 1, r).completed {
                s.on_verified(piece, PieceCheck::Valid(None));
            }
        }
        s.reprioritize(false);
        assert!(!s.is_critical());
        let requests = s.fill(1, Instant::now());
        assert_eq!(requests.len(), 64);
        assert!(requests.iter().any(|r| r.piece < 998));
    }

    #[test]
    fn critical_phase_ends_while_the_reader_keeps_reading() {
        let cfg = SchedulerConfig {
            min_queue: 128,
            initial_queue: 128,
            ..SchedulerConfig::default()
        };
        let mut s = sched(1000, cfg);
        seed(&mut s, 1);
        s.set_interest(1, reader(0, &s));
        s.reprioritize(false);
        for r in s.fill(1, Instant::now()) {
            if let Some((piece, _)) = deliver(&mut s, 1, r).completed {
                s.on_verified(piece, PieceCheck::Valid(None));
            }
        }
        // The player has read 1 MiB. Only 1 MiB of the 2 MiB critical bytes ahead of it is
        // downloaded (the rest would be in flight while it keeps reading), which is enough.
        s.set_interest(1, reader(16 * u64::from(PIECE), &s));
        s.reprioritize(false);
        assert!(!s.is_critical());

        // A seek starts a new critical phase at the target.
        s.set_interest(1, reader(500 * u64::from(PIECE), &s));
        s.reprioritize(true);
        assert!(s.is_critical());
    }

    #[test]
    fn fill_all_spreads_the_most_urgent_data_over_all_peers() {
        let cfg = SchedulerConfig {
            min_queue: 64,
            initial_queue: 64,
            ..SchedulerConfig::default()
        };
        let mut s = sched(1000, cfg);
        seed(&mut s, 1);
        seed(&mut s, 2);
        s.set_interest(1, reader(100 * u64::from(PIECE), &s));
        s.reprioritize(false);
        let fills = s.fill_all(&[1, 2], Instant::now());
        let pieces = |key: PeerKey| -> Vec<u32> {
            let mut p: Vec<u32> = fills
                .iter()
                .filter(|(k, _)| *k == key)
                .flat_map(|(_, r)| r.iter().map(|r| r.piece))
                .collect();
            p.dedup();
            p
        };
        // Turns of 16 blocks (4 pieces): each peer starts with its share of the most urgent.
        assert_eq!(pieces(1)[..4], [100, 101, 102, 103]);
        assert_eq!(pieces(2)[..4], [104, 105, 106, 107]);
        assert_eq!(pieces(1)[4], 108);
        assert_eq!(fills.iter().map(|(_, r)| r.len()).sum::<usize>(), 128);
    }

    #[test]
    fn critical_phase_covers_a_reader_that_is_waiting_for_data() {
        let cfg = SchedulerConfig {
            min_queue: 256,
            initial_queue: 256,
            ..SchedulerConfig::default()
        };
        let mut s = sched(1000, cfg);
        seed(&mut s, 1);
        s.set_interest(1, reader(0, &s));
        s.reprioritize(false);
        for r in s.fill(1, Instant::now()) {
            if let Some((piece, _)) = deliver(&mut s, 1, r).completed {
                s.on_verified(piece, PieceCheck::Valid(None));
            }
        }
        // The player read all 2 MiB and still wants more (it needs more than the critical
        // bytes to start): the phase continues ahead of it.
        s.set_interest(1, reader(32 * u64::from(PIECE), &s));
        s.reprioritize(false);
        assert!(s.is_critical());
        let requests = s.fill(1, Instant::now());
        assert!(!requests.is_empty());
        assert!(
            requests.iter().all(|r| (32..64).contains(&r.piece)),
            "{requests:?}"
        );
        for r in requests {
            if let Some((piece, _)) = deliver(&mut s, 1, r).completed {
                s.on_verified(piece, PieceCheck::Valid(None));
            }
        }
        s.reprioritize(false);
        assert!(!s.is_critical());

        // Playing on into data that is not there yet starts the phase again.
        s.set_interest(1, reader(64 * u64::from(PIECE), &s));
        s.reprioritize(false);
        assert!(s.is_critical());
    }

    #[test]
    fn seeking_cancels_requests_that_are_no_longer_urgent() {
        let mut s = sched(1000, small_queues());
        seed(&mut s, 1);
        s.set_interest(1, reader(0, &s));
        s.reprioritize(false);
        let first = s.fill(1, Instant::now());
        assert!(first.iter().all(|r| r.piece == 0));

        // Jump far ahead: the old requests are cancelled and the queue refills at the target.
        s.set_interest(1, reader(700 * u64::from(PIECE), &s));
        let cancels = s.reprioritize(true);
        assert_eq!(cancels.len(), first.len());
        assert!(cancels.iter().all(|(k, r)| *k == 1 && r.piece == 0));
        let next = s.fill(1, Instant::now());
        assert_eq!(next.len(), 4);
        assert!(next.iter().all(|r| r.piece == 700));
    }

    #[test]
    fn urgent_blocks_are_duplicated_to_a_second_peer_and_the_loser_cancelled() {
        let cfg = SchedulerConfig {
            duplicate_only_snubbed: false,
            ..small_queues()
        };
        let mut s = sched(100, cfg);
        seed(&mut s, 1);
        seed(&mut s, 2);
        s.set_interest(1, reader(0, &s));
        s.reprioritize(false);
        let t0 = Instant::now();
        let slow = s.fill(1, t0);
        assert_eq!(slow.len(), 4);

        // Too soon to duplicate: peer 2 gets the next piece instead.
        let early = s.fill(2, t0 + Duration::from_millis(10));
        assert!(early.iter().all(|r| r.piece == 1));
        s.on_choke(2);
        s.on_unchoke(2);

        // After `duplicate_after`, the still-missing urgent blocks go to peer 2 as well.
        let late = s.fill(2, t0 + Duration::from_millis(400));
        assert!(late.iter().any(|r| r.piece == 0), "{late:?}");
        let dup = *late.iter().find(|r| r.piece == 0).unwrap();
        let outcome = deliver(&mut s, 2, dup);
        assert_eq!(
            outcome.cancels,
            vec![(1, dup)],
            "peer 1's copy is cancelled"
        );
    }

    #[test]
    fn snubbed_policy_duplicates_only_from_stalled_peers() {
        let mut s = sched(100, small_queues());
        seed(&mut s, 1);
        seed(&mut s, 2);
        s.set_interest(1, reader(0, &s));
        s.reprioritize(false);
        let t0 = Instant::now();
        let first = s.fill(1, t0);
        assert_eq!(first.len(), 4);

        // Peer 1 keeps delivering (just one of its four blocks), so it is not snubbed: after
        // 400 ms its remaining blocks are still not duplicated.
        s.on_block_at(
            1,
            first[0].piece,
            first[0].begin,
            &vec![0; first[0].len as usize],
            t0 + Duration::from_millis(350),
        );
        let busy = s.fill(2, t0 + Duration::from_millis(400));
        assert!(busy.iter().all(|r| r.piece != 0), "{busy:?}");
        s.on_choke(2);
        s.on_unchoke(2);

        // Peer 1 then stalls for more than `snub_after`: its blocks go to peer 2 as well.
        let stalled = s.fill(2, t0 + Duration::from_millis(1_500));
        assert!(stalled.iter().any(|r| r.piece == 0), "{stalled:?}");
    }

    #[test]
    fn choke_releases_requests_for_other_peers() {
        let mut s = sched(10, small_queues());
        seed(&mut s, 1);
        seed(&mut s, 2);
        s.set_interest(1, reader(0, &s));
        s.reprioritize(false);
        let now = Instant::now();
        let taken = s.fill(1, now);
        s.on_choke(1);
        assert!(s.fill(1, now).is_empty(), "choked peers get nothing");
        let moved = s.fill(2, now);
        assert_eq!(moved, taken, "the same blocks go to the other peer");
    }

    #[test]
    fn pausing_cancels_outstanding_requests_and_hands_out_none() {
        let mut s = sched(10, small_queues());
        seed(&mut s, 1);
        s.set_interest(1, reader(0, &s));
        s.reprioritize(false);
        let now = Instant::now();
        let taken = s.fill(1, now);
        assert!(!taken.is_empty());
        let cancels = s.set_paused(true);
        assert_eq!(
            cancels.len(),
            taken.len(),
            "everything outstanding is cancelled"
        );
        assert!(s.fill(1, now).is_empty());
        assert!(s.fill_all(&[1], now).is_empty());
        assert!(s.set_paused(false).is_empty());
        assert!(!s.fill(1, now).is_empty(), "requests resume");
    }

    #[test]
    fn set_wanted_replaces_the_background_selection() {
        let mut s = sched(10, small_queues());
        let piece = u64::from(PIECE);
        let wanted = |s: &Scheduler| (0..10).filter(|&p| s.wanted[p]).collect::<Vec<usize>>();
        s.set_wanted(&[(0, 4 * piece), (6 * piece, 8 * piece)]);
        assert_eq!(wanted(&s), [0, 1, 2, 3, 6, 7]);
        s.set_wanted(&[(6 * piece, 8 * piece)]);
        assert_eq!(wanted(&s), [6, 7]);
        s.set_wanted(&[]);
        assert!(wanted(&s).is_empty());
    }

    #[test]
    fn completed_pieces_are_verified_once_and_failures_are_retried() {
        let mut s = sched(10, small_queues());
        seed(&mut s, 1);
        s.set_interest(1, reader(0, &s));
        s.reprioritize(false);
        let now = Instant::now();
        let mut completed = None;
        for r in s.fill(1, now) {
            completed = deliver(&mut s, 1, r).completed.or(completed);
        }
        let (piece, data) = completed.expect("all four blocks received");
        assert_eq!((piece, data.len()), (0, PIECE as usize));
        // While verifying, the piece is not requested again.
        s.reprioritize(false);
        assert!(s.fill(1, now).iter().all(|r| r.piece != 0));

        // Free the queue (it now holds piece 1) so there is room to re-request piece 0.
        s.on_choke(1);
        s.on_unchoke(1);
        let failed = s.on_verified(0, PieceCheck::Invalid(digests(&data)));
        assert_eq!(failed.failed_by, vec![1]);
        assert_eq!(s.bad_pieces(1), 1);
        s.reprioritize(false);
        assert!(
            s.fill(1, now).iter().any(|r| r.piece == 0),
            "re-requested after a bad hash"
        );
    }

    #[test]
    fn ignores_unrequested_malformed_and_duplicate_blocks() {
        let mut s = sched(10, small_queues());
        seed(&mut s, 1);
        assert!(
            s.on_block(1, 0, 0, &[0; 16]).completed.is_none(),
            "wrong length"
        );
        assert!(
            s.on_block(1, 0, 5, &[0; 16_384]).completed.is_none(),
            "unaligned"
        );
        assert!(
            s.on_block(1, 99, 0, &[0; 16_384]).completed.is_none(),
            "bad index"
        );
        assert!(
            !s.on_bitfield(1, &[0xff; 1]),
            "bitfield too short for 10 pieces"
        );
        assert!(
            s.on_block(1, 0, PIECE + BLOCK_LEN, &[0; 16_384])
                .completed
                .is_none(),
            "past the end of the piece"
        );
        assert!(
            s.on_block(1, 0, u32::MAX / BLOCK_LEN * BLOCK_LEN, &[0; 16_384])
                .completed
                .is_none(),
            "far past the end of the piece"
        );
        assert!(
            !s.on_bitfield(1, &[0xff; 5]),
            "bitfield too long for 10 pieces"
        );
        assert!(!s.on_have(1, 10));
    }

    #[test]
    fn timed_out_requests_are_released() {
        let mut s = sched(10, small_queues());
        seed(&mut s, 1);
        seed(&mut s, 2);
        s.set_interest(1, reader(0, &s));
        s.reprioritize(false);
        let t0 = Instant::now();
        let taken = s.fill(1, t0);
        let cancels = s.tick(t0 + Duration::from_secs(9), false);
        assert_eq!(cancels.len(), taken.len());
        assert_eq!(s.timeouts(1), 4);
        assert_eq!(s.fill(2, t0 + Duration::from_secs(9)), taken);
    }

    #[test]
    fn background_is_rarest_first_and_respects_selection() {
        let mut s = sched(
            8,
            SchedulerConfig {
                critical_bytes: 0,
                ..SchedulerConfig::default()
            },
        );
        // Only the second half of the torrent is wanted.
        s.want_range(4 * u64::from(PIECE), 8 * u64::from(PIECE));
        seed(&mut s, 1);
        s.add_peer(2);
        assert!(s.on_have(2, 6));
        s.tick(Instant::now(), true);
        let requests = s.fill(1, Instant::now());
        assert!(requests.iter().all(|r| r.piece >= 4), "{requests:?}");
        assert_ne!(requests[0].piece, 6, "piece 6 is less rare than 4, 5, 7");
    }

    #[test]
    fn background_fills_skip_started_pieces_until_one_is_dropped() {
        let mut s = sched(8, small_queues());
        s.want_range(0, 8 * u64::from(PIECE));
        seed(&mut s, 1);
        seed(&mut s, 2);
        s.tick(Instant::now(), true);
        // Peer 1 takes every block of the rarest piece, so peer 2 starts the next one.
        let first = s.fill(1, Instant::now());
        assert_eq!(first.len(), 4);
        assert!(first.iter().all(|r| r.piece == first[0].piece));
        let second = s.fill(2, Instant::now());
        assert!(!second.is_empty());
        assert!(second.iter().all(|r| r.piece != first[0].piece));
        assert_eq!(
            s.background_skip, 1,
            "the started piece is skipped from now on"
        );
        // Pausing drops both partial pieces: the first is the first one started again.
        s.set_paused(true);
        s.set_paused(false);
        assert_eq!(s.background_skip, 0);
        assert_eq!(s.fill(1, Instant::now())[0].piece, first[0].piece);
    }

    fn rate(bytes_per_sec: f64) -> Rate {
        Rate {
            bytes_since: 0,
            bytes_per_sec,
            measured: true,
            peak: 0.0,
        }
    }

    #[test]
    fn a_container_index_stays_critical_until_it_is_complete() {
        let cfg = SchedulerConfig {
            min_queue: 128,
            initial_queue: 128,
            ..SchedulerConfig::default()
        };
        let mut s = sched(1000, cfg);
        seed(&mut s, 1);
        // A reader of the last 1 MiB (16 pieces): its critical range reaches the end of the file.
        let first = 1000 - 16;
        s.set_interest(1, reader(u64::from(first) * u64::from(PIECE), &s));
        s.reprioritize(false);
        assert!(s.is_critical());
        let requests = s.fill(1, Instant::now());
        // Twelve of the sixteen pieces arrive: more than half, but not all.
        for r in requests.iter().filter(|r| r.piece < first + 12) {
            if let Some((piece, _)) = deliver(&mut s, 1, *r).completed {
                s.on_verified(piece, PieceCheck::Valid(None));
            }
        }
        s.reprioritize(false);
        assert!(s.is_critical(), "the rest of the index stays exclusive");
        for r in requests.iter().filter(|r| r.piece >= first + 12) {
            if let Some((piece, _)) = deliver(&mut s, 1, *r).completed {
                s.on_verified(piece, PieceCheck::Valid(None));
            }
        }
        s.reprioritize(false);
        assert!(!s.is_critical());
    }

    #[test]
    fn the_container_index_goes_first_among_equally_urgent_pieces() {
        let mut s = sched(1000, small_queues());
        seed(&mut s, 1);
        // A player reading the head and one reading the index at the end both wait for the
        // piece at their position (due now).
        s.set_interest(1, reader(0, &s));
        s.set_interest(2, reader(996 * u64::from(PIECE), &s));
        s.reprioritize(false);
        let requests = s.fill(1, Instant::now());
        assert_eq!(requests.len(), 4);
        assert!(requests.iter().all(|r| r.piece == 996), "{requests:?}");
    }

    #[test]
    fn slow_peers_leave_urgent_blocks_to_much_faster_ones() {
        // 4 MiB critical: more than the fast peer queues at once.
        let cfg = SchedulerConfig {
            critical_bytes: 4 * MIB,
            ..small_queues()
        };
        let mut s = sched(1000, cfg);
        seed(&mut s, 1);
        seed(&mut s, 2);
        s.peers.get_mut(&1).unwrap().rate = rate(4.0 * 1024.0 * 1024.0);
        s.peers.get_mut(&2).unwrap().rate = rate(64.0 * 1024.0);
        s.set_interest(1, reader(0, &s));
        s.reprioritize(false);
        assert!(s.is_critical());
        let now = Instant::now();
        // The slow peer would need 0.25 s per block; the idle fast peer 4 ms.
        assert!(s.fill(2, now).is_empty());
        // 0.5 s at 4 MiB/s: the fast peer's queue holds 128 blocks, about 0.5 s of work.
        assert_eq!(s.fill(1, now).len(), 128);
        // Now the slow peer is sooner for the next block, and takes some.
        let slow = s.fill(2, now);
        assert!(!slow.is_empty() && slow.len() <= 4, "{slow:?}");
    }

    #[test]
    fn blocks_held_by_a_slow_peer_are_requested_from_a_much_faster_one() {
        let mut s = sched(1000, small_queues());
        seed(&mut s, 1);
        seed(&mut s, 2);
        s.set_interest(1, reader(0, &s));
        s.reprioritize(false);
        let t0 = Instant::now();
        // Before any rate is known the slow peer gets its initial queue of urgent blocks.
        let held = s.fill(2, t0);
        assert_eq!(held.len(), 4);
        // Then the rates are measured: peer 2 needs 0.25 s per block, peer 1 4 ms.
        s.peers.get_mut(&1).unwrap().rate = rate(4.0 * 1024.0 * 1024.0);
        s.peers.get_mut(&2).unwrap().rate = rate(64.0 * 1024.0);
        let fast = s.fill(1, t0 + Duration::from_millis(10));
        assert!(held.iter().all(|r| fast.contains(r)), "{held:?} {fast:?}");
        assert_eq!(s.stats().duplicate_requests, 4);
        // The first copy to arrive wins; the slow peer's request is cancelled.
        let outcome = deliver(&mut s, 1, held[0]);
        assert_eq!(outcome.cancels, [(2, held[0])]);
    }

    #[test]
    fn a_busier_peer_of_the_same_speed_keeps_its_blocks() {
        let mut s = sched(1000, small_queues());
        seed(&mut s, 1);
        seed(&mut s, 2);
        for key in [1, 2] {
            s.peers.get_mut(&key).unwrap().rate = rate(1024.0 * 1024.0);
        }
        s.set_interest(1, reader(0, &s));
        s.reprioritize(false);
        let t0 = Instant::now();
        // Peer 2 is idle while peer 1 has queued blocks: as fast, it gets other blocks, not
        // copies of peer 1's (which a peer ignoring cancels would send anyway).
        let busy = s.fill(1, t0);
        let idle = s.fill(2, t0);
        assert!(!busy.is_empty() && !idle.is_empty());
        assert!(idle.iter().all(|r| !busy.contains(r)), "{busy:?} {idle:?}");
        assert_eq!(s.stats().duplicate_requests, 0);
    }

    #[test]
    fn cancelled_data_still_arriving_is_no_round_trip_sample() {
        let mut s = sched(1000, small_queues());
        seed(&mut s, 1);
        s.set_interest(1, reader(0, &s));
        s.reprioritize(false);
        let t0 = Instant::now();
        let old = s.fill(1, t0);
        assert_eq!(old.len(), 4);
        // A seek cancels them, but the peer (like librqbit) sends them anyway, taking 2 s.
        s.set_interest(1, reader(500 * u64::from(PIECE), &s));
        assert_eq!(s.reprioritize(true).len(), 4);
        let new = s.fill(1, t0);
        let at = |ms| t0 + Duration::from_millis(ms);
        for (i, r) in old.iter().enumerate() {
            s.on_block_at(
                1,
                r.piece,
                r.begin,
                &vec![0; r.len as usize],
                at(500 * (i as u64 + 1)),
            );
        }
        // The new requests waited behind that drain: no sample from them.
        for r in &new {
            s.on_block_at(1, r.piece, r.begin, &vec![0; r.len as usize], at(2100));
        }
        assert_eq!(s.peers[&1].base_rtt, None);
        // A request to a queue that is really empty is a sample again.
        let r = s.fill(1, at(3000))[0];
        s.on_block_at(1, r.piece, r.begin, &vec![0; r.len as usize], at(3100));
        assert_eq!(s.peers[&1].base_rtt, Some(Duration::from_millis(100)));
    }

    fn digests(data: &[u8]) -> Vec<BlockDigest> {
        use sha1::Digest as _;
        data.chunks(BLOCK_LEN as usize)
            .map(|block| sha1::Sha1::digest(block).into())
            .collect()
    }

    /// Delivers `requests` from `key`, every byte `byte`. Returns the completed piece, if any.
    fn send_all(
        s: &mut Scheduler,
        key: PeerKey,
        requests: &[Request],
        byte: u8,
    ) -> Option<(u32, Vec<u8>)> {
        let mut completed = None;
        for r in requests {
            let outcome = s.on_block(key, r.piece, r.begin, &vec![byte; r.len as usize]);
            completed = outcome.completed.or(completed);
        }
        completed
    }

    /// Two-block queues, so that two peers share a four-block piece.
    fn two_block_queues() -> SchedulerConfig {
        SchedulerConfig {
            initial_queue: 2,
            min_queue: 2,
            ..SchedulerConfig::default()
        }
    }

    #[test]
    fn blocks_nobody_asked_the_sender_for_are_dropped() {
        let mut s = sched(10, small_queues());
        seed(&mut s, 1);
        seed(&mut s, 2);
        s.set_interest(1, reader(0, &s));
        s.reprioritize(false);
        let now = Instant::now();
        let asked = s.fill(1, now);
        assert!(asked.iter().all(|r| r.piece == 0) && asked.len() == 4);

        // Peer 2 sends a block of a piece only peer 1 was asked for: dropped, and peer 1's
        // request stands.
        let outcome = deliver(&mut s, 2, asked[0]);
        assert!(outcome.cancels.is_empty() && outcome.completed.is_none());
        assert_eq!(s.peers[&1].outstanding.len(), 4);
        assert_eq!(s.stats().wasted_bytes, u64::from(BLOCK_LEN));
        // A block of a piece in progress that nobody was asked for, and one of a piece not
        // started: dropped too.
        let unasked = block_request(&s.meta, 5, 0);
        deliver(&mut s, 2, unasked);
        assert!(!s.progress.contains_key(&5));
        assert_eq!(s.stats().wasted_bytes, 2 * u64::from(BLOCK_LEN));
        // Nor does it count as progress from peer 2.
        assert_eq!(s.peers[&2].last_progress, None);
        // An unknown peer's data is ignored.
        assert!(deliver(&mut s, 99, asked[0]).completed.is_none());

        // Peer 1's own blocks complete the piece, with peer 1 as the only sender.
        assert!(send_all(&mut s, 1, &asked, 0).is_some());
        assert_eq!(s.verifying[&0], vec![1; 4]);
    }

    #[test]
    fn data_for_requests_cancelled_at_the_sender_is_still_accepted() {
        let mut s = sched(1000, small_queues());
        seed(&mut s, 1);
        s.set_interest(1, reader(0, &s));
        s.reprioritize(false);
        let t0 = Instant::now();
        let old = s.fill(1, t0);
        // A seek cancels them; the peer sends them anyway.
        s.set_interest(1, reader(500 * u64::from(PIECE), &s));
        assert_eq!(s.reprioritize(true).len(), 4);
        assert!(send_all(&mut s, 1, &old, 0).is_some(), "piece 0 completes");
        assert_eq!(s.stats().wasted_bytes, 0);
        // Once each, though: a second copy is dropped.
        assert!(deliver(&mut s, 1, old[0]).completed.is_none());
        assert_eq!(s.stats().wasted_bytes, u64::from(BLOCK_LEN));
    }

    #[test]
    fn smart_ban_names_the_peer_whose_blocks_differed_and_only_it() {
        let honest: PeerKey = 1;
        let attacker: PeerKey = 2;
        let mut s = sched(10, two_block_queues());
        seed(&mut s, honest);
        seed(&mut s, attacker);
        s.set_interest(1, reader(0, &s));
        s.reprioritize(false);
        let now = Instant::now();
        let fills = s.fill_all(&[honest, attacker], now);
        let (h, a) = (fills[0].1.clone(), fills[1].1.clone());
        assert!(h.iter().chain(&a).all(|r| r.piece == 0), "{fills:?}");

        // The honest peer sends good blocks (0x11), the attacker garbage (0xEE): the piece is
        // made of both and fails. Neither can be told from the other yet.
        let good = vec![0x11u8; PIECE as usize];
        assert!(send_all(&mut s, honest, &h, 0x11).is_none());
        let (piece, data) = send_all(&mut s, attacker, &a, 0xEE).expect("complete");
        let failed = s.on_verified(piece, PieceCheck::Invalid(digests(&data)));
        assert_eq!(failed.failed_by.len(), 2);
        assert!(failed.banned.is_empty());
        assert!(s.needs_block_digests(0));

        // The retry goes to one peer: the attacker asks first and gets the piece to itself.
        s.reprioritize(false);
        let fills = s.fill_all(&[attacker, honest], now);
        assert!(fills[0].1.iter().all(|r| r.piece == 0) && !fills[0].1.is_empty());
        assert!(
            fills[1].1.iter().all(|r| r.piece != 0),
            "only the owner fetches a piece that failed: {fills:?}"
        );
        let mut asked = fills[0].1.clone();
        let mut completed = None;
        while completed.is_none() {
            completed = send_all(&mut s, attacker, &asked, 0xEE);
            asked = s.fill(attacker, now);
        }
        let (piece, data) = completed.unwrap();
        let failed = s.on_verified(piece, PieceCheck::Invalid(digests(&data)));
        assert_eq!(failed.failed_by, vec![attacker], "a sole source this time");

        // The attacker stops serving (choked us); the honest peer takes over and the piece
        // verifies: the attacker, and only the attacker, is named.
        s.on_choke(attacker);
        s.on_choke(honest);
        s.on_unchoke(honest);
        s.reprioritize(false);
        let mut asked = s.fill(honest, now);
        assert!(asked.iter().any(|r| r.piece == 0));
        let mut completed = None;
        while completed.is_none() {
            let zero: Vec<Request> = asked.iter().copied().filter(|r| r.piece == 0).collect();
            completed = send_all(&mut s, honest, &zero, 0x11);
            asked = s.fill(honest, now);
        }
        let (piece, data) = completed.unwrap();
        assert_eq!(data, good);
        let verified = s.on_verified(piece, PieceCheck::Valid(Some(digests(&data))));
        assert_eq!(verified.banned, vec![attacker]);
        assert!(s.have(0), "playback can go on");
        assert!(
            !s.needs_block_digests(0),
            "the records are dropped once verified"
        );
    }

    #[test]
    fn smart_ban_spares_peers_whose_blocks_were_right() {
        // Peer 1 sent blocks 0-1 right and peer 2 blocks 2-3 right; the piece failed for
        // another reason (a third peer's block, here simulated by the digests). Neither is
        // banned when the piece verifies with the same blocks.
        let mut s = sched(10, two_block_queues());
        seed(&mut s, 1);
        seed(&mut s, 2);
        s.set_interest(1, reader(0, &s));
        s.reprioritize(false);
        let now = Instant::now();
        let fills = s.fill_all(&[1, 2], now);
        send_all(&mut s, 1, &fills[0].1, 0x11);
        let (piece, data) = send_all(&mut s, 2, &fills[1].1, 0x11).unwrap();
        let failed = s.on_verified(piece, PieceCheck::Invalid(digests(&data)));
        assert_eq!(failed.failed_by.len(), 2);
        // Verified later with exactly that data: nobody sent anything different.
        s.verifying.insert(0, vec![1, 1, 2, 2]);
        let verified = s.on_verified(0, PieceCheck::Valid(Some(digests(&data))));
        assert!(verified.banned.is_empty());
    }

    #[test]
    fn pieces_that_could_not_be_stored_blame_nobody() {
        let mut s = sched(10, small_queues());
        seed(&mut s, 1);
        s.set_interest(1, reader(0, &s));
        s.reprioritize(false);
        let asked = s.fill(1, Instant::now());
        let (piece, _) = send_all(&mut s, 1, &asked, 0).unwrap();
        assert_eq!(
            s.on_verified(piece, PieceCheck::NotStored),
            Verified::default()
        );
        assert_eq!(s.bad_pieces(1), 0);
        assert!(!s.have(0) && !s.needs_block_digests(0));
    }

    #[test]
    fn smart_ban_memory_is_bounded() {
        let mut s = sched(200, small_queues());
        let bad = [[0xAAu8; 20]; 4];
        for piece in 0..100 {
            s.remember_suspect(piece, &[1, 2, 3, 4], &bad);
            // Repeats add nothing.
            s.remember_suspect(piece, &[1, 2, 3, 4], &bad);
        }
        assert_eq!(s.suspects.len(), MAX_SUSPECT_PIECES);
        assert_eq!(s.suspect_order.len(), MAX_SUSPECT_PIECES);
        assert!(!s.needs_block_digests(0), "the oldest are forgotten");
        assert!(s.needs_block_digests(99));
        assert!(s.suspects[&99].iter().all(|records| records.len() == 1));
        for n in 0..10u8 {
            s.remember_suspect(99, &[PeerKey::from(n); 4], &[[n; 20]; 4]);
        }
        assert!(
            s.suspects[&99]
                .iter()
                .all(|records| records.len() == MAX_RECORDS_PER_BLOCK)
        );
    }

    #[test]
    fn remembered_cancels_are_bounded() {
        let mut s = sched(1000, small_queues());
        seed(&mut s, 1);
        let now = Instant::now();
        let peer = s.peers.get_mut(&1).unwrap();
        for piece in 0..1000 {
            peer.cancelled(piece, 0, now);
        }
        assert_eq!(peer.cancelled.len(), MAX_CANCELLED);
        assert_eq!(
            peer.cancelled.front(),
            Some(&(1000 - u32::try_from(MAX_CANCELLED).unwrap(), 0))
        );
    }

    #[test]
    fn a_much_faster_peer_takes_an_urgent_failed_piece_from_a_slow_owner() {
        let (fast, slow): (PeerKey, PeerKey) = (1, 2);
        let mut s = sched(1000, small_queues());
        seed(&mut s, fast);
        seed(&mut s, slow);
        s.set_interest(1, reader(0, &s));
        s.reprioritize(false);
        assert!(s.is_critical());
        // Piece 0, which the player is waiting for, failed before.
        s.remember_suspect(0, &[slow; 4], &[[0xAA; 20]; 4]);
        let t0 = Instant::now();
        let owned = s.fill(slow, t0);
        assert!(owned.iter().all(|r| r.piece == 0) && owned.len() == 4);

        // The owner sends correct data just often enough not to count as snubbed, at 16 KiB/s:
        // four seconds for this 64 KiB piece (minutes for a 4 MiB one).
        deliver(&mut s, slow, owned[0]);
        s.peers.get_mut(&slow).unwrap().last_progress = Some(t0 + Duration::from_millis(900));
        s.peers.get_mut(&slow).unwrap().rate = rate(16.0 * 1024.0);
        let at = t0 + Duration::from_secs(1);

        // A peer not much faster leaves it alone.
        s.peers.get_mut(&fast).unwrap().rate = rate(24.0 * 1024.0);
        let similar = s.fill(fast, at);
        assert!(!similar.is_empty());
        assert!(similar.iter().all(|r| r.piece != 0), "{similar:?}");

        // A much faster one takes over the blocks the owner still holds.
        s.peers.get_mut(&fast).unwrap().rate = rate(4.0 * 1024.0 * 1024.0);
        let rescue = s.fill(fast, at);
        let zero: Vec<Request> = rescue.iter().copied().filter(|r| r.piece == 0).collect();
        assert_eq!(zero, owned[1..], "{rescue:?}");
        // And keeps the piece: the slow peer gets no more of it.
        assert!(s.fill(slow, at).iter().all(|r| r.piece != 0));
        let (piece, _) = send_all(&mut s, fast, &zero, 0).expect("complete");
        assert_eq!(piece, 0);
        // Every block's sender is known for smart ban.
        assert_eq!(s.verifying[&0], vec![slow, fast, fast, fast]);
    }

    #[test]
    fn data_for_requests_released_by_a_choke_is_still_accepted() {
        let mut s = sched(10, small_queues());
        seed(&mut s, 1);
        s.set_interest(1, reader(0, &s));
        s.reprioritize(false);
        let asked = s.fill(1, Instant::now());
        s.on_choke(1);
        // The peer finishes what it had started anyway.
        assert!(
            send_all(&mut s, 1, &asked, 0).is_some(),
            "piece 0 completes"
        );
        assert_eq!(s.stats().wasted_bytes, 0);
        assert_eq!(s.peers[&1].cancelled_pending, 0, "none was expected");
    }

    #[test]
    fn data_nobody_asked_for_does_not_count_towards_the_rate() {
        let mut s = sched(10, small_queues());
        seed(&mut s, 1);
        let t0 = Instant::now();
        s.tick(t0, false);
        for piece in 0..10 {
            let r = block_request(&s.meta, piece, 0);
            deliver(&mut s, 1, r);
        }
        s.tick(t0 + Duration::from_secs(1), false);
        assert!(!s.peers[&1].rate.measured);
        assert!(s.stats().download_bps < 1.0);
        assert_eq!(s.stats().wasted_bytes, 10 * u64::from(BLOCK_LEN));
    }
}
