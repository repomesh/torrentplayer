// SPDX-License-Identifier: MIT OR Apache-2.0

//! The engine session and per-torrent driver.

use std::collections::{HashMap, HashSet, VecDeque};
use std::io;
use std::net::{IpAddr, SocketAddr};
use std::num::NonZeroU32;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};
use std::time::{Duration, Instant};

use futures_util::StreamExt as _;
use futures_util::stream::BoxStream;
use librqbit_core::hash_id::Id20;
use librqbit_tracker_comms::{
    TorrentStatsProvider, TrackerComms, TrackerCommsStats, TrackerCommsStatsState, UdpTrackerClient,
};
use tokio::io::AsyncReadExt as _;
use tokio::sync::{Notify, Semaphore, mpsc, watch};
use tokio_util::sync::CancellationToken;
use torrentplayer_net::{IncomingPeer, IncomingPeers, Network, Policy};
use tracing::{debug, info, trace};

use crate::dht::DhtNode;
use crate::limit::RateLimiter;
use crate::metainfo::{BLOCK_LEN, Metainfo};
use crate::reader::FileReader;
use crate::reload::Rechecker;
use crate::sched::{
    BlockDigest, Interest, InterestId, InterestKind, PeerKey, PieceBudget, PieceCheck, Request,
    Scheduler, SchedulerConfig,
};
use crate::store::{CacheFile, PieceStore};
use crate::wire::{self, HANDSHAKE_LEN, Outgoing};

/// How often the driver re-plans, refills queues, and checks timeouts.
const TICK: Duration = Duration::from_millis(100);
/// How often it does while there is nothing to download: the torrent is paused, or complete,
/// or has no reader and every selected piece (see [`crate::sched::Scheduler::is_idle`]). Seeding
/// torrents stay in the queue until paused, so most of them are idle. Anything that changes
/// this wakes the driver at once.
const IDLE_TICK: Duration = Duration::from_secs(1);
/// Rebuild the rarest-first order every this many ticks.
const BACKGROUND_REFRESH_TICKS: u32 = 10;
/// Disconnect peers after this many timed-out requests (unless the swarm is thin).
const MAX_TIMEOUTS: u32 = 32;
/// With at most this many connected peers the swarm is thin: a slow peer is better than none,
/// so peers are not disconnected for timeouts, and unreachable peers keep being retried.
const THIN_SWARM: usize = 4;
/// Wait before retrying an unreachable peer, doubling with each failure in a row.
const RETRY_BASE: Duration = Duration::from_secs(2);
/// Longest wait between attempts while the swarm is thin...
const THIN_RETRY_MAX: Duration = Duration::from_secs(60);
/// ...and otherwise.
const RETRY_MAX: Duration = Duration::from_secs(300);
/// Reconnect this soon to a peer whose working connection closed.
const RECONNECT_AFTER: Duration = Duration::from_secs(1);
/// A connection counts as working once it has delivered this much data or lasted this long.
const WORKING_BYTES: u64 = 256 * 1024;
const WORKING_FOR: Duration = Duration::from_secs(30);
/// Forget an address after this many failures in a row, unless the swarm is thin.
const MAX_FAILURES: u32 = 8;
/// Most addresses remembered per torrent (trackers and peer exchange can send many).
const MAX_KNOWN: usize = 2000;
/// Promote due retries every this many ticks.
const RETRY_TICKS: u32 = 5;
/// The playhead's interest id; readers use ids above it.
const PLAYHEAD: InterestId = 0;
/// Disk reads queued for upload across the engine. Each holds at most one 16 KiB block.
const MAX_UPLOAD_READS: usize = 256;
/// Incoming connections sending their handshake at once, across the engine; more are closed.
const MAX_INBOUND_HANDSHAKES: usize = 32;

/// The engine's torrents by info hash.
type Registry = Arc<Mutex<HashMap<[u8; 20], Registered>>>;

/// A torrent's entry in the [`Registry`].
struct Registered {
    /// The latest instance, for routing incoming connections.
    shared: Weak<Shared>,
    /// Its piece cache file, which every live instance of the torrent shares: an earlier
    /// instance may outlive a later one.
    cache_file: Weak<CacheFile>,
}

/// Engine settings.
// Independent switches, each read by a different part of the engine.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone)]
pub struct SwarmConfig {
    /// Where piece caches are stored (one sub-folder per torrent).
    pub download_dir: PathBuf,
    /// Maximum connected peers per torrent.
    pub peer_limit: usize,
    /// Maximum simultaneous connection attempts per torrent.
    pub max_connecting: usize,
    /// Give up connecting to a peer after this long.
    pub connect_timeout: Duration,
    /// Give up on the first attempt at an address after this long (capped by
    /// `connect_timeout`). A reachable peer answers within one round trip, and most addresses
    /// in a public swarm are unreachable, so short first attempts find working peers sooner.
    /// Addresses that time out are retried later with `connect_timeout`.
    pub probe_timeout: Duration,
    /// Give up on a peer that does not complete the handshake in time.
    pub handshake_timeout: Duration,
    /// Disconnect peers that send nothing for this long.
    pub idle_timeout: Duration,
    /// Announce to the torrent's trackers.
    pub enable_trackers: bool,
    /// Find peers through the DHT (BEP 5). It uses UDP, so it runs only while the network
    /// policy allows UDP (direct mode). Off by default; turn it on along with trackers for
    /// public swarms.
    pub enable_dht: bool,
    /// Where the DHT's routing tables are kept between runs (direct mode only). `None` starts
    /// from the bootstrap nodes every time.
    pub dht_state_file: Option<PathBuf>,
    /// Port announced to trackers while incoming connections are not accepted (trackers reject
    /// port 0). While they are, the listening port is announced instead.
    pub announce_port: u16,
    /// Serve peers that connect to us, from the network manager's listener
    /// (`torrentplayer_net::Network::incoming_peers`), when incoming connections are turned on
    /// there. Needs `network`. Only one engine per network should accept them.
    pub accept_incoming: bool,
    /// Request scheduling policy.
    pub scheduler: SchedulerConfig,
    /// Maximum download rate for the whole engine, bytes per second.
    pub download_limit_bps: Option<NonZeroU32>,
    /// Maximum upload rate for the whole engine, bytes per second.
    pub upload_limit_bps: Option<NonZeroU32>,
    /// Sends peer and tracker traffic through TorrentPlayer's network manager (proxy, bound
    /// interface, kill switch). `None` connects directly.
    pub network: Option<Network>,
    /// Download from the torrent's web seeds (BEP 19 `url-list`) as well as from peers.
    pub enable_web_seeds: bool,
    /// Allow web seeds on loopback and private addresses. Only for tests and benchmarks: a
    /// torrent could otherwise make TorrentPlayer send requests to the user's own network.
    pub allow_local_web_seeds: bool,
    /// Announce to HTTP trackers on loopback and private addresses (and follow redirects
    /// there). Only for tests and benchmarks with a local tracker: a magnet link could
    /// otherwise make TorrentPlayer send requests to the user's own network.
    pub allow_local_trackers: bool,
}

impl SwarmConfig {
    /// Defaults, storing caches in `download_dir`.
    #[must_use]
    pub fn new(download_dir: PathBuf) -> Self {
        Self {
            download_dir,
            peer_limit: 40,
            max_connecting: 16,
            connect_timeout: Duration::from_secs(3),
            probe_timeout: Duration::from_secs(1),
            handshake_timeout: Duration::from_secs(5),
            idle_timeout: Duration::from_secs(120),
            enable_trackers: true,
            enable_dht: false,
            dht_state_file: None,
            announce_port: 6881,
            accept_incoming: false,
            scheduler: SchedulerConfig::default(),
            download_limit_bps: None,
            upload_limit_bps: None,
            network: None,
            enable_web_seeds: true,
            allow_local_web_seeds: false,
            allow_local_trackers: false,
        }
    }
}

/// Errors from the engine.
#[derive(Debug, thiserror::Error)]
pub enum SwarmError {
    /// The cache directory could not be created.
    #[error("could not create the piece cache: {0}")]
    Storage(#[from] io::Error),
    /// No file with that index.
    #[error("unknown file")]
    UnknownFile,
    /// Setting up tracker communication failed.
    #[error("could not start tracker communication: {0}")]
    Trackers(String),
    /// The HTTP client for web seeds could not be set up.
    #[error("could not set up web seeds: {0}")]
    WebSeeds(String),
}

/// Live statistics for a torrent.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SwarmStats {
    /// Connected peers that completed the handshake.
    pub peers_live: usize,
    /// Connection attempts in progress.
    pub peers_connecting: usize,
    /// Distinct peer addresses learned.
    pub peers_seen: usize,
    /// Peers currently letting us download.
    pub peers_unchoked: usize,
    /// Download rate, bytes per second.
    pub download_bps: f64,
    /// Verified bytes downloaded.
    pub downloaded_bytes: u64,
    /// Upload rate, bytes per second.
    pub upload_bps: f64,
    /// Piece payload bytes uploaded.
    pub uploaded_bytes: u64,
    /// Verified pieces.
    pub have_pieces: u32,
    /// Requests outstanding across all peers.
    pub outstanding_requests: usize,
    /// Requests sent for blocks another peer was already fetching.
    pub duplicate_requests: u64,
    /// Requests cancelled on seeks and timeouts.
    pub cancelled_requests: u64,
    /// Bytes received that were not needed (duplicates or cancelled data).
    pub wasted_bytes: u64,
    /// Web seeds in use (counted in [`SwarmStats::peers_live`] too).
    pub web_seeds: usize,
    /// Bytes received from web seeds.
    pub web_seed_bytes: u64,
}

/// What playback can expect from the download right now, for an honest status line: how much
/// is buffered, whether the download keeps up with playback, and how long to wait before
/// playing to the end would not stall. See [`Torrent::playback_estimate`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PlaybackEstimate {
    /// Contiguous bytes downloaded from the position on.
    pub buffered_bytes: u64,
    /// Download rate averaged over about ten seconds, bytes per second.
    pub download_bps: f64,
    /// Whether the download is faster than playback (with a safety margin).
    pub keeps_up: bool,
    /// Seconds to wait (buffering) before playing to the end of the file would not stall at
    /// the current rate; zero if playback can run now. `None` while nothing downloads.
    pub wait_secs: Option<f64>,
}

/// Only this share of the measured rate is counted on, as rates fluctuate.
const RATE_MARGIN: f64 = 0.9;
/// Time constant of the smoothed download rate behind [`PlaybackEstimate`].
const SMOOTH_TAU_SECS: f64 = 10.0;

/// `(keeps_up, wait_secs)` for `left` bytes still to play from the position, `buffered` of them
/// downloaded, at `download_bps` while playback consumes `byte_rate`.
fn estimate(left: u64, buffered: u64, download_bps: f64, byte_rate: u64) -> (bool, Option<f64>) {
    #[allow(clippy::cast_precision_loss)]
    let (left, buffered, byte_rate) = (left as f64, buffered as f64, byte_rate.max(1) as f64);
    let rate = download_bps.max(0.0) * RATE_MARGIN;
    let keeps_up = rate >= byte_rate;
    if buffered >= left || keeps_up {
        return (keeps_up, Some(0.0));
    }
    if rate <= 0.0 {
        return (false, None);
    }
    // Playing to the end takes `left / byte_rate`; by then `buffered + rate * (wait + that)`
    // must cover `left`. With a slower download the end is the tightest point.
    let play = left / byte_rate;
    let wait = ((left - buffered) / rate - play).max(0.0);
    (false, Some(wait))
}

/// A download rate averaged with a time constant of [`SMOOTH_TAU_SECS`].
#[derive(Debug, Default)]
struct Smoothed {
    last: Option<(Instant, u64)>,
    bps: Option<f64>,
}

impl Smoothed {
    /// Folds in the verified byte count, sampled at least a second apart.
    fn update(&mut self, now: Instant, downloaded: u64) {
        let Some((at, bytes)) = self.last else {
            self.last = Some((now, downloaded));
            return;
        };
        let secs = now.saturating_duration_since(at).as_secs_f64();
        if secs < 1.0 {
            return;
        }
        #[allow(clippy::cast_precision_loss)]
        let sample = downloaded.saturating_sub(bytes) as f64 / secs;
        self.bps = Some(self.bps.map_or(sample, |bps| {
            bps + (1.0 - (-secs / SMOOTH_TAU_SECS).exp()) * (sample - bps)
        }));
        self.last = Some((now, downloaded));
    }
}

/// The engine: shared settings and tracker clients for all torrents.
#[derive(Clone)]
pub struct Swarm {
    cfg: Arc<SwarmConfig>,
    limiter: Option<Arc<RateLimiter>>,
    upload_limiter: Arc<RateLimiter>,
    upload_slots: Arc<Semaphore>,
    /// Memory for partially downloaded pieces, shared by all torrents.
    piece_budget: Arc<PieceBudget>,
    peer_id: [u8; 20],
    http: reqwest::Client,
    /// For web seeds: only public destinations, checked redirects.
    web: reqwest::Client,
    udp: Option<UdpTrackerClient>,
    dht: Option<Arc<DhtNode>>,
    torrents: Registry,
    /// Checks the pieces kept from earlier sessions, for every torrent.
    rechecker: Arc<Rechecker>,
    cancel: CancellationToken,
}

impl std::fmt::Debug for Swarm {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Swarm")
            .field("cfg", &self.cfg)
            .finish_non_exhaustive()
    }
}

impl Swarm {
    /// Starts the engine. Must be called inside a multi-threaded Tokio runtime.
    ///
    /// # Errors
    ///
    /// Returns an error if the download directory cannot be created or the tracker clients
    /// cannot be set up.
    pub async fn start(cfg: SwarmConfig) -> Result<Self, SwarmError> {
        tokio::fs::create_dir_all(&cfg.download_dir).await?;
        let cancel = CancellationToken::new();
        let http = tracker_client(&cfg)?;
        let web = web_client(&cfg)?;
        let dht = cfg.enable_dht.then(|| {
            Arc::new(DhtNode::new(
                cancel.clone(),
                cfg.network.clone(),
                cfg.dht_state_file.clone(),
            ))
        });
        if let (Some(node), Some(network)) = (&dht, &cfg.network) {
            tokio::spawn(crate::dht::follow_policy(
                Arc::clone(node),
                network.subscribe(),
            ));
            tokio::spawn(crate::dht::follow_reachability(
                Arc::clone(node),
                network.clone(),
                cfg.accept_incoming,
            ));
        }
        let torrents = Registry::default();
        if cfg.accept_incoming
            && let Some(network) = &cfg.network
        {
            tokio::spawn(accept_incoming(
                network.incoming_peers(),
                Arc::clone(&torrents),
                cfg.handshake_timeout,
                cancel.clone(),
            ));
        }
        let udp = if cfg.enable_trackers {
            Some(
                UdpTrackerClient::new(cancel.clone(), None)
                    .await
                    .map_err(|e| SwarmError::Trackers(format!("{e:#}")))?,
            )
        } else {
            None
        };
        Ok(Self {
            // Always present, so the limit can be set later (see `Swarm::set_download_limit`).
            limiter: Some(Arc::new(
                cfg.download_limit_bps
                    .map_or_else(RateLimiter::unlimited, RateLimiter::new),
            )),
            upload_limiter: Arc::new(
                cfg.upload_limit_bps
                    .map_or_else(RateLimiter::unlimited, RateLimiter::new),
            ),
            upload_slots: Arc::new(Semaphore::new(MAX_UPLOAD_READS)),
            piece_budget: PieceBudget::new(cfg.scheduler.max_in_progress_bytes),
            cfg: Arc::new(cfg),
            peer_id: generate_peer_id(),
            http,
            web,
            udp,
            dht,
            torrents,
            rechecker: Rechecker::new(),
            cancel,
        })
    }

    /// Adds a torrent and starts connecting to `peers` (and its trackers, if enabled). No file
    /// is downloaded until it is selected or read.
    ///
    /// # Errors
    ///
    /// Returns an error if the piece cache cannot be created.
    pub fn add(&self, meta: Metainfo, peers: Vec<SocketAddr>) -> Result<Torrent, SwarmError> {
        let meta = Arc::new(meta);
        let dir = self.cfg.download_dir.join(meta.info_hash_hex());
        // Held until the new instance is registered, so two adds of one torrent cannot both
        // open the cache file.
        let mut torrents = self.torrents.lock().unwrap_or_else(PoisonError::into_inner);
        // A previous instance still in use (by a reader finishing a request) keeps reading the
        // cache file: share it instead of opening it again, which could truncate it under that
        // reader. Any earlier instance counts, not only the latest, which may be gone while an
        // older one is not.
        let shared_store = torrents
            .get(&meta.info_hash)
            .and_then(|t| PieceStore::share(&t.cache_file));
        let store = match shared_store {
            Some(store) => store,
            None => PieceStore::open(&dir, &meta)?,
        };
        let cache_file = store.cache_file();
        let shared = Arc::new(Shared {
            sched: Mutex::new(Scheduler::with_budget(
                Arc::clone(&meta),
                self.cfg.scheduler.clone(),
                Arc::clone(&self.piece_budget),
            )),
            meta: Arc::clone(&meta),
            store,
            cfg: Arc::clone(&self.cfg),
            limiter: self.limiter.clone(),
            upload_limiter: Arc::clone(&self.upload_limiter),
            upload_slots: Arc::clone(&self.upload_slots),
            own_limit: RateLimiter::unlimited(),
            // Off until the caller enables it (see `Torrent::set_upload_enabled`), so peers that
            // connect right after the add are never unchoked against the user's setting.
            upload_enabled: AtomicBool::new(false),
            paused: AtomicBool::new(false),
            selected: Mutex::new(Vec::new()),
            peer_id: self.peer_id,
            piece_done: Arc::new(Notify::new()),
            wake: Notify::new(),
            seek_pending: AtomicBool::new(false),
            refresh_background: AtomicBool::new(true),
            idle: AtomicBool::new(false),
            peers: Mutex::new(HashMap::new()),
            verdicts: Mutex::new(HashMap::new()),
            blame: Mutex::new(HashMap::new()),
            peer_ips: Mutex::new(PeerIps::default()),
            smoothed: Mutex::new(Smoothed::default()),
            upload_smoothed: Mutex::new(Smoothed::default()),
            candidates: Mutex::new(Candidates::default()),
            next_peer: AtomicU64::new(1),
            next_interest: AtomicU64::new(PLAYHEAD + 1),
            downloaded: AtomicU64::new(0),
            uploaded: AtomicU64::new(0),
            web_seeds: AtomicUsize::new(0),
            downloaded_web: AtomicU64::new(0),
            cancel: self.cancel.child_token(),
        });
        torrents.retain(|_, t| t.cache_file.strong_count() > 0);
        torrents.insert(
            meta.info_hash,
            Registered {
                shared: Arc::downgrade(&shared),
                cache_file,
            },
        );
        drop(torrents);
        // Before any peer connects, so pieces kept from an earlier session are not requested.
        self.rechecker.start(&shared);
        shared.add_candidates(peers);
        if let Some(node) = &self.dht {
            tokio::spawn(crate::dht::discover(
                Arc::clone(node),
                Arc::downgrade(&shared),
                meta.info_hash,
                self.cfg.network.as_ref().map(Network::subscribe),
                shared.cancel.clone(),
            ));
        }
        if self.cfg.enable_web_seeds {
            for (index, url) in meta.web_seeds.iter().enumerate() {
                let key = shared.next_peer.fetch_add(1, Ordering::Relaxed);
                tokio::spawn(crate::webseed::run(
                    Arc::clone(&shared),
                    self.web.clone(),
                    url.clone(),
                    index,
                    key,
                ));
            }
        }

        let trackers = if self.cfg.enable_trackers
            && let Some(udp) = &self.udp
        {
            Some(Trackers {
                info_hash: Id20::new(meta.info_hash),
                peer_id: Id20::new(self.peer_id),
                urls: tracker_urls(&meta, self.cfg.allow_local_trackers),
                shared: Arc::downgrade(&shared),
                port: self.cfg.announce_port,
                http: self.http.clone(),
                udp: udp.clone(),
            })
        } else {
            None
        };
        tokio::spawn(drive(Arc::clone(&shared), trackers));
        info!(torrent = %meta.info_hash_hex(), "torrent added");
        Ok(Torrent { shared })
    }

    /// Changes the download limit shared by every torrent of the engine (bytes per second);
    /// `None` removes it. Takes effect within a fraction of a second.
    pub fn set_download_limit(&self, limit: Option<NonZeroU32>) {
        if let Some(limiter) = &self.limiter {
            limiter.set_rate(limit);
        }
    }

    /// Changes the upload limit shared by every torrent of the engine (bytes per second);
    /// `None` removes it. Takes effect within a fraction of a second.
    pub fn set_upload_limit(&self, limit: Option<NonZeroU32>) {
        self.upload_limiter.set_rate(limit);
    }

    /// Stops every torrent, and marks their piece caches as ended cleanly, so the next session
    /// trusts the pieces in them instead of checking every one before use. Blocks while the
    /// caches that stored pieces since they were opened are flushed to disk. Async callers
    /// should use [`Self::shutdown_async`] so a slow flush does not block their runtime.
    pub fn shutdown(&self) {
        self.cancel.cancel();
        let caches: Vec<Arc<CacheFile>> = self
            .torrents
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .values()
            .filter_map(|t| t.cache_file.upgrade())
            .collect();
        for cache in caches {
            match cache.seal() {
                Ok(true) => {}
                Ok(false) => debug!("piece cache left to be checked next time"),
                Err(error) => tracing::warn!(%error, "could not mark a piece cache as complete"),
            }
        }
    }

    /// Stops every torrent and flushes its piece caches on a blocking worker.
    ///
    /// Unlike [`Self::shutdown`], this yields while storage is busy, so a caller can bound its
    /// wait with a timeout. Dropping the future stops waiting, not a flush already started:
    /// the worker owns the engine until it finishes. An interrupted flush leaves the cache
    /// subject to the usual crash recovery checks on the next start.
    pub async fn shutdown_async(&self) {
        self.cancel.cancel();
        let swarm = self.clone();
        if let Err(error) = tokio::task::spawn_blocking(move || swarm.shutdown()).await {
            tracing::warn!(%error, "piece cache shutdown worker failed");
        }
    }
}

/// A torrent in the engine. Cheap to clone. The torrent stops when [`Torrent::stop`] is called
/// or the engine shuts down.
#[derive(Clone)]
pub struct Torrent {
    shared: Arc<Shared>,
}

impl std::fmt::Debug for Torrent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Torrent")
            .field("info_hash", &self.shared.meta.info_hash_hex())
            .finish_non_exhaustive()
    }
}

impl Torrent {
    /// The torrent's metadata.
    #[must_use]
    pub fn meta(&self) -> &Metainfo {
        &self.shared.meta
    }

    /// Downloads a file in the background (rarest piece first) after urgent data.
    ///
    /// # Errors
    ///
    /// Returns an error if there is no file with that index.
    pub fn select_file(&self, file: usize) -> Result<(), SwarmError> {
        let f = self
            .shared
            .meta
            .files
            .get(file)
            .ok_or(SwarmError::UnknownFile)?;
        self.shared.select(f.offset, f.offset + f.len);
        Ok(())
    }

    /// Stops downloading a file in the background (undoes [`Torrent::select_file`] and
    /// [`Torrent::select_range`] for it). Pieces shared with another selected file, the
    /// playhead, and readers are still downloaded; what is downloaded stays.
    ///
    /// # Errors
    ///
    /// Returns an error if there is no file with that index.
    pub fn deselect_file(&self, file: usize) -> Result<(), SwarmError> {
        let f = self
            .shared
            .meta
            .files
            .get(file)
            .ok_or(SwarmError::UnknownFile)?;
        let (start, end) = (f.offset, f.offset + f.len);
        let mut selected = self
            .shared
            .selected
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let before = selected.len();
        selected.retain(|&(s, e)| e <= start || s >= end);
        if selected.len() != before {
            self.shared.sched().set_wanted(&selected);
            drop(selected);
            self.shared
                .refresh_background
                .store(true, Ordering::Relaxed);
            self.shared.wake.notify_one();
        }
        Ok(())
    }

    /// Pauses or resumes the torrent. While paused nothing is requested (outstanding requests
    /// are cancelled), no new peers are dialed, and trackers are not announced to (resuming
    /// announces again at once); connections stay open, so resuming is immediate. Readers and
    /// the playhead wait until it resumes.
    pub fn set_paused(&self, paused: bool) {
        if self.shared.paused.swap(paused, Ordering::Relaxed) == paused {
            return;
        }
        let cancels = self.shared.sched().set_paused(paused);
        self.shared.send_cancels(&cancels);
        if !paused {
            self.shared.requeue_known();
        }
        self.shared.request_replan(false);
    }

    /// Whether [`Torrent::set_paused`] paused the torrent.
    #[must_use]
    pub fn is_paused(&self) -> bool {
        self.shared.paused.load(Ordering::Relaxed)
    }

    /// Limits this torrent's download rate (bytes per second), on top of the engine's limit;
    /// `None` removes the limit. Takes effect within a fraction of a second.
    pub fn set_download_limit(&self, limit: Option<NonZeroU32>) {
        self.shared.own_limit.set_rate(limit);
    }

    /// Enables or disables serving verified pieces to peers. Existing peers are choked or
    /// unchoked at once. Off when the torrent is added.
    pub fn set_upload_enabled(&self, enabled: bool) {
        // Serialize the state change and choke with the final upload enqueue. This prevents an
        // already-read block from slipping into a peer's queue after its choke.
        let mut peers = self
            .shared
            .peers
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if self.shared.upload_enabled.swap(enabled, Ordering::Relaxed) == enabled {
            return;
        }
        let message = if enabled {
            Outgoing::Unchoke
        } else {
            Outgoing::Choke
        };
        peers.retain(|key, tx| {
            if tx.try_send(message.clone()).is_ok() {
                true
            } else {
                debug!(peer = key, "peer send queue full; disconnecting");
                false
            }
        });
    }

    /// Whether this torrent currently serves pieces to peers.
    #[must_use]
    pub fn upload_enabled(&self) -> bool {
        self.shared.upload_enabled.load(Ordering::Relaxed)
    }

    /// Downloads part of a file (`range`, in bytes within the file, clamped to it) in the
    /// background, like [`Torrent::select_file`]: for example the start and end of the next
    /// lecture, so that playing it starts at once. The playhead and readers keep priority.
    ///
    /// # Errors
    ///
    /// Returns an error if there is no file with that index.
    pub fn select_range(&self, file: usize, range: std::ops::Range<u64>) -> Result<(), SwarmError> {
        let f = self
            .shared
            .meta
            .files
            .get(file)
            .ok_or(SwarmError::UnknownFile)?;
        let (start, end) = (range.start.min(f.len), range.end.min(f.len));
        if start < end {
            self.shared.select(f.offset + start, f.offset + end);
        }
        Ok(())
    }

    /// Opens a seekable reader over a file. Its position is always prioritized.
    ///
    /// # Errors
    ///
    /// Returns an error if there is no file with that index.
    pub fn reader(&self, file: usize) -> Result<FileReader, SwarmError> {
        self.open_reader(file, false)
    }

    /// Opens a seekable reader over a file that reads only pieces already stored: it is never
    /// prioritized, and a read of a missing piece fails at once with
    /// [`std::io::ErrorKind::WouldBlock`]. For tools that must not compete with playback.
    ///
    /// # Errors
    ///
    /// Returns an error if there is no file with that index.
    pub fn stored_reader(&self, file: usize) -> Result<FileReader, SwarmError> {
        self.open_reader(file, true)
    }

    fn open_reader(&self, file: usize, stored_only: bool) -> Result<FileReader, SwarmError> {
        let f = self
            .shared
            .meta
            .files
            .get(file)
            .ok_or(SwarmError::UnknownFile)?;
        let id = self.shared.next_interest.fetch_add(1, Ordering::Relaxed);
        Ok(FileReader::new(
            Arc::clone(&self.shared),
            id,
            f.offset,
            f.len,
            stored_only,
        ))
    }

    /// Reports the player's position, so the read-ahead window (sized from `byte_rate`) and the
    /// file's tail are downloaded ahead of time. A jump counts as a seek.
    ///
    /// # Errors
    ///
    /// Returns an error if there is no file with that index.
    pub fn set_playhead(
        &self,
        file: usize,
        position: u64,
        byte_rate: Option<u64>,
    ) -> Result<(), SwarmError> {
        let f = self
            .shared
            .meta
            .files
            .get(file)
            .ok_or(SwarmError::UnknownFile)?;
        let interest = Interest {
            kind: InterestKind::Playhead,
            file_offset: f.offset,
            file_len: f.len,
            position: position.min(f.len),
            byte_rate,
        };
        let window = self.shared.cfg.scheduler.reader_window;
        let mut sched = self.shared.sched();
        let seek = sched.interest(PLAYHEAD).is_none_or(|old| {
            old.file_offset != interest.file_offset
                || interest.position < old.position
                || interest.position > old.position.saturating_add(window)
        });
        sched.set_interest(PLAYHEAD, interest);
        drop(sched);
        self.shared.request_replan(seek);
        Ok(())
    }

    /// Stops prioritizing a playhead set with [`Torrent::set_playhead`].
    pub fn clear_playhead(&self) {
        self.shared.sched().remove_interest(PLAYHEAD);
        self.shared.request_replan(false);
    }

    /// What playback from `position` in a file whose media plays at `byte_rate` bytes per second
    /// can expect: see [`PlaybackEstimate`]. For a status line such as "Downloading slower than
    /// playback: playing now will pause; ready to play straight through in 2 minutes".
    ///
    /// # Errors
    ///
    /// Returns an error if there is no file with that index.
    pub fn playback_estimate(
        &self,
        file: usize,
        position: u64,
        byte_rate: u64,
    ) -> Result<PlaybackEstimate, SwarmError> {
        let f = self
            .shared
            .meta
            .files
            .get(file)
            .ok_or(SwarmError::UnknownFile)?;
        let start = f.offset + position.min(f.len);
        let end = f.offset + f.len;
        let buffered = self.shared.sched().buffered_from(start, end);
        let download_bps = self
            .shared
            .smoothed
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .bps
            .unwrap_or(0.0);
        let (keeps_up, wait_secs) = estimate(end - start, buffered, download_bps, byte_rate);
        Ok(PlaybackEstimate {
            buffered_bytes: buffered,
            download_bps,
            keeps_up,
            wait_secs,
        })
    }

    /// Which pieces are downloaded and verified.
    #[must_use]
    pub fn have_pieces(&self) -> Vec<bool> {
        self.shared.sched().have_all()
    }

    /// Current statistics.
    #[must_use]
    pub fn stats(&self) -> SwarmStats {
        let s = self.shared.sched().stats();
        let candidates = self.shared.candidates();
        SwarmStats {
            peers_live: s.peers,
            peers_connecting: candidates.connecting + candidates.handshaking,
            peers_seen: candidates.known.len(),
            peers_unchoked: s.unchoked,
            download_bps: s.download_bps,
            downloaded_bytes: self.shared.downloaded.load(Ordering::Relaxed),
            upload_bps: self
                .shared
                .upload_smoothed
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .bps
                .unwrap_or(0.0),
            uploaded_bytes: self.shared.uploaded.load(Ordering::Relaxed),
            have_pieces: s.have_pieces,
            outstanding_requests: s.outstanding,
            duplicate_requests: s.duplicate_requests,
            cancelled_requests: s.cancelled_requests,
            wasted_bytes: s.wasted_bytes,
            web_seeds: self.shared.web_seeds.load(Ordering::Relaxed),
            web_seed_bytes: self.shared.downloaded_web.load(Ordering::Relaxed),
        }
    }

    /// Disconnects all peers and stops the torrent. The piece cache stays on disk.
    pub fn stop(&self) {
        self.shared.cancel.cancel();
    }
}

/// Peer addresses and when to (re)try them.
#[derive(Debug, Default)]
pub(crate) struct Candidates {
    /// Addresses to try next, one queue per [`Tier`], each in order.
    queues: [VecDeque<SocketAddr>; 3],
    /// Every address learned.
    known: HashMap<SocketAddr, Known>,
    /// Peers being connected to or connected.
    active: HashSet<SocketAddr>,
    /// Addresses of peers that sent bad data: never dialed, and refused when they connect.
    pub(crate) banned_ips: HashSet<IpAddr>,
    /// TCP connection attempts in progress (limited by `max_connecting`).
    pub(crate) connecting: usize,
    /// Connected peers still exchanging handshakes. They no longer hold a connection slot but
    /// count toward the peer limit.
    pub(crate) handshaking: usize,
}

/// Which queue an address waits in. Lower tiers are dialed first, so new addresses are not
/// stuck behind retries of ones that already failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tier {
    /// Worked before, or named by peer exchange as a seed or as accepting connections.
    Preferred = 0,
    /// Never tried.
    Fresh = 1,
    /// Failed before.
    Retry = 2,
}

/// What is known about one address.
// Independent facts about the address, each read by a different decision.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, Copy, Default)]
struct Known {
    /// Failed attempts (or connections that did not work) in a row.
    failures: u32,
    /// When to try again; `None` while queued or active, and once given up.
    retry_at: Option<Instant>,
    queued: bool,
    /// Never retried: the wrong torrent, ourselves, or bad data.
    banned: bool,
    /// Attempted before: later attempts get the full connect timeout.
    tried: bool,
    /// A connection to it worked at some point.
    worked: bool,
}

impl Candidates {
    /// Queues `addr` (with the preferred peers if `preferred`) unless it is queued, active,
    /// banned, waiting for a retry, or new while the list is full. Returns whether it was
    /// queued.
    fn offer(&mut self, addr: SocketAddr, preferred: bool) -> bool {
        if self.banned_ips.contains(&addr.ip())
            || !self.known.contains_key(&addr) && self.known.len() >= MAX_KNOWN
        {
            return false;
        }
        let known = self.known.entry(addr).or_default();
        if known.banned || known.queued || known.retry_at.is_some() || self.active.contains(&addr) {
            return false;
        }
        known.queued = true;
        let tier = if preferred || known.worked {
            Tier::Preferred
        } else if known.tried {
            Tier::Retry
        } else {
            Tier::Fresh
        };
        self.queues[tier as usize].push_back(addr);
        true
    }

    /// Queues every address whose retry time has come: peers whose working connection closed
    /// with the preferred ones, failed ones last.
    fn promote_due(&mut self, now: Instant) {
        for (addr, known) in &mut self.known {
            if known.retry_at.is_some_and(|at| at <= now) {
                known.retry_at = None;
                known.queued = true;
                let tier = if known.worked && known.failures == 0 {
                    Tier::Preferred
                } else {
                    Tier::Retry
                };
                self.queues[tier as usize].push_back(*addr);
            }
        }
    }

    /// Takes the next address to dial and counts the attempt. Returns it and whether this is
    /// the first attempt at it (a probe, with a short timeout).
    fn next(&mut self) -> Option<(SocketAddr, bool)> {
        let (addr, known) = loop {
            let addr = self.queues.iter_mut().find_map(VecDeque::pop_front)?;
            let known = self.known.get_mut(&addr);
            if !self.banned_ips.contains(&addr.ip()) {
                break (addr, known);
            }
            // Queued before its address was banned.
            if let Some(known) = known {
                known.queued = false;
                known.banned = true;
            }
        };
        let mut first = false;
        if let Some(known) = known {
            known.queued = false;
            first = !known.tried;
            known.tried = true;
        }
        self.connecting += 1;
        self.active.insert(addr);
        Some((addr, first))
    }

    /// Records how a connection to `addr` ended and schedules the next attempt.
    fn finished(&mut self, addr: SocketAddr, end: PeerEnd, thin: bool, now: Instant) {
        self.active.remove(&addr);
        let Some(known) = self.known.get_mut(&addr) else {
            return;
        };
        match end {
            PeerEnd::Banned => {
                known.banned = true;
                known.retry_at = None;
            }
            PeerEnd::Closed { worked: true } => {
                known.failures = 0;
                known.worked = true;
                known.retry_at = Some(now + RECONNECT_AFTER);
            }
            PeerEnd::Closed { worked: false } | PeerEnd::Unreachable => {
                known.failures = known.failures.saturating_add(1);
                known.retry_at = (thin || known.failures < MAX_FAILURES)
                    .then(|| now + retry_delay(known.failures, thin));
            }
        }
    }
}

/// The wait before attempt `failures + 1`: [`RETRY_BASE`] doubling per failure, capped.
fn retry_delay(failures: u32, thin: bool) -> Duration {
    let cap = if thin { THIN_RETRY_MAX } else { RETRY_MAX };
    RETRY_BASE
        .saturating_mul(1 << failures.saturating_sub(1).min(16))
        .min(cap)
}

/// How a peer connection ended, which decides when it is tried again.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PeerEnd {
    /// The connection or the handshake failed.
    Unreachable,
    /// The connection closed; `worked` if it delivered [`WORKING_BYTES`] or lasted
    /// [`WORKING_FOR`] first, and was not dropped as unresponsive.
    Closed { worked: bool },
    /// Never try again: the wrong torrent, ourselves, or bad data.
    Banned,
}

impl PeerEnd {
    /// The end of a connection that lasted `lasted` and delivered `delivered` bytes.
    pub(crate) fn closed(lasted: Duration, delivered: u64) -> Self {
        Self::Closed {
            worked: delivered >= WORKING_BYTES || lasted >= WORKING_FOR,
        }
    }
}

/// Hash failures counted against a peer.
#[derive(Debug, Clone, Copy, Default)]
struct Blame {
    /// Failed pieces with only this peer's data.
    sole: u32,
    /// Disconnected for bad data.
    banned: bool,
}

/// Counts a failed piece against the peers that sent it, given each one's failed pieces so far
/// (`totals`), and returns those to ban: a peer that alone sent two bad pieces, or that took
/// part in four and in more than any other source of this piece. A piece put together from
/// several sources does not show which one was wrong; blaming them all would let one bad source
/// (such as a web seed with a different file) get honest peers banned, so a tie bans nobody
/// (smart ban settles it once the piece verifies, see [`Scheduler::on_verified`]). A piece with
/// data from a source already banned says nothing about the others, so it is not counted.
fn blame(blames: &mut HashMap<PeerKey, Blame>, totals: &[(PeerKey, u32)]) -> Vec<PeerKey> {
    if totals
        .iter()
        .any(|(k, _)| blames.get(k).is_some_and(|b| b.banned))
    {
        return Vec::new();
    }
    let alone = totals.len() == 1;
    let mut disconnect = Vec::new();
    for &(key, total) in totals {
        let entry = blames.entry(key).or_default();
        if alone {
            entry.sole += 1;
        }
        let worst = totals.iter().all(|&(k, t)| k == key || t < total);
        if entry.sole >= 2 || (total >= 4 && worst) {
            entry.banned = true;
            disconnect.push(key);
        }
    }
    disconnect
}

/// Peers that recently ended, whose addresses are still remembered: smart ban can name a peer
/// only once a piece it spoiled verifies, possibly after it left.
const RECENT_PEER_IPS: usize = 256;

/// The IP address behind each peer key, for bans.
#[derive(Debug, Default)]
struct PeerIps {
    live: HashMap<PeerKey, IpAddr>,
    /// Ended peers, oldest first, at most [`RECENT_PEER_IPS`].
    recent: VecDeque<(PeerKey, IpAddr)>,
}

impl PeerIps {
    fn insert(&mut self, key: PeerKey, ip: IpAddr) {
        self.live.insert(key, ip);
    }

    fn ended(&mut self, key: PeerKey) {
        if let Some(ip) = self.live.remove(&key) {
            if self.recent.len() >= RECENT_PEER_IPS {
                self.recent.pop_front();
            }
            self.recent.push_back((key, ip));
        }
    }

    fn get(&self, key: PeerKey) -> Option<IpAddr> {
        self.live.get(&key).copied().or_else(|| {
            self.recent
                .iter()
                .find(|(k, _)| *k == key)
                .map(|(_, ip)| *ip)
        })
    }
}

/// Why the torrent itself disconnected a peer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Verdict {
    /// It sent data that failed the hash check.
    Banned,
    /// Too many of its requests timed out.
    Unresponsive,
}

/// State shared by the driver, peer tasks, and readers.
pub(crate) struct Shared {
    pub(crate) meta: Arc<Metainfo>,
    pub(crate) sched: Mutex<Scheduler>,
    pub(crate) store: PieceStore,
    pub(crate) cfg: Arc<SwarmConfig>,
    pub(crate) limiter: Option<Arc<RateLimiter>>,
    /// Upload limit shared by every torrent in this engine.
    pub(crate) upload_limiter: Arc<RateLimiter>,
    /// Caps blocking piece reads across every peer and torrent in this engine.
    pub(crate) upload_slots: Arc<Semaphore>,
    /// This torrent's own limit (see [`Torrent::set_download_limit`]), applied after `limiter`.
    pub(crate) own_limit: RateLimiter,
    /// Whether peers may request verified pieces from this torrent.
    pub(crate) upload_enabled: AtomicBool,
    /// See [`Torrent::set_paused`].
    paused: AtomicBool,
    /// Byte ranges selected for background download (absolute), so one file can be deselected
    /// without forgetting the others.
    selected: Mutex<Vec<(u64, u64)>>,
    pub(crate) peer_id: [u8; 20],
    /// Notified (all waiters) whenever a piece is verified.
    pub(crate) piece_done: Arc<Notify>,
    /// Wakes the driver early (a seek, a new reader, a released request).
    pub(crate) wake: Notify,
    pub(crate) seek_pending: AtomicBool,
    refresh_background: AtomicBool,
    /// The driver ticks slowly (see [`IDLE_TICK`]).
    idle: AtomicBool,
    pub(crate) peers: Mutex<HashMap<PeerKey, mpsc::Sender<Outgoing>>>,
    /// Why the torrent disconnected a peer, for its task to report when it ends.
    verdicts: Mutex<HashMap<PeerKey, Verdict>>,
    /// Hash failures counted against each peer (see [`Shared::blame`]).
    blame: Mutex<HashMap<PeerKey, Blame>>,
    /// Where each peer connected from (see [`Shared::ban`]).
    peer_ips: Mutex<PeerIps>,
    /// The download rate behind [`Torrent::playback_estimate`].
    smoothed: Mutex<Smoothed>,
    /// The upload rate behind [`SwarmStats::upload_bps`].
    upload_smoothed: Mutex<Smoothed>,
    candidates: Mutex<Candidates>,
    next_peer: AtomicU64,
    next_interest: AtomicU64,
    pub(crate) downloaded: AtomicU64,
    pub(crate) uploaded: AtomicU64,
    /// Web seeds in use.
    web_seeds: AtomicUsize,
    /// Bytes received from web seeds.
    pub(crate) downloaded_web: AtomicU64,
    pub(crate) cancel: CancellationToken,
}

impl Shared {
    pub(crate) fn sched(&self) -> MutexGuard<'_, Scheduler> {
        self.sched.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub(crate) fn candidates(&self) -> MutexGuard<'_, Candidates> {
        self.candidates
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// The BEP 3 bitfield for every verified piece currently stored.
    pub(crate) fn bitfield(&self) -> bytes::Bytes {
        let have = self.sched().have_all();
        let mut bits = vec![0_u8; have.len().div_ceil(8)];
        for (piece, present) in have.into_iter().enumerate() {
            if present {
                bits[piece / 8] |= 0x80 >> (piece % 8);
            }
        }
        bits.into()
    }

    /// Queues peer addresses from trackers or the caller. An address given up on is tried once
    /// more when a tracker names it again.
    pub(crate) fn add_candidates(&self, addrs: impl IntoIterator<Item = SocketAddr>) {
        let mut c = self.candidates();
        for addr in addrs {
            c.offer(addr, false);
        }
    }

    /// Queues peers learned through peer exchange; seeds and peers said to accept connections
    /// are tried first.
    pub(crate) fn add_exchanged(&self, peers: &[crate::pex::PexPeer]) {
        let mut c = self.candidates();
        let added = peers
            .iter()
            .filter(|p| c.offer(p.addr, p.seed || p.connectable))
            .count();
        if added > 0 {
            trace!(added, "peers from peer exchange");
        }
    }

    /// Queues every known peer that is not connected again, forgetting past failures. Used when
    /// the network comes back after the kill switch, or after a mode change closed the
    /// connections.
    fn requeue_known(&self) {
        let mut guard = self.candidates();
        let c = &mut *guard;
        let mut again = 0;
        for (addr, known) in &mut c.known {
            if known.banned || known.queued || c.active.contains(addr) {
                continue;
            }
            *known = Known {
                queued: true,
                worked: known.worked,
                ..Known::default()
            };
            let tier = if known.worked {
                Tier::Preferred
            } else {
                Tier::Fresh
            };
            c.queues[tier as usize].push_back(*addr);
            again += 1;
        }
        debug!(peers = again, "network changed: retrying known peers");
    }

    /// Counts a failed piece against the peers that sent it (see [`blame`]) and returns those to
    /// ban.
    fn blame(&self, contributors: &[PeerKey]) -> Vec<PeerKey> {
        let sched = self.sched();
        let totals: Vec<(PeerKey, u32)> = contributors
            .iter()
            .map(|&k| (k, sched.bad_pieces(k)))
            .collect();
        drop(sched);
        let mut blames = self.blame.lock().unwrap_or_else(PoisonError::into_inner);
        blame(&mut blames, &totals)
    }

    /// Disconnects peers that sent bad data and refuses their IP addresses from now on, for
    /// outgoing and incoming connections alike (an incoming peer's source port changes with
    /// every connection).
    fn ban(&self, keys: &[PeerKey]) {
        if keys.is_empty() {
            return;
        }
        {
            let mut blames = self.blame.lock().unwrap_or_else(PoisonError::into_inner);
            for &key in keys {
                blames.entry(key).or_default().banned = true;
            }
        }
        let ips: Vec<IpAddr> = {
            let addrs = self.peer_ips.lock().unwrap_or_else(PoisonError::into_inner);
            keys.iter().filter_map(|&key| addrs.get(key)).collect()
        };
        self.candidates().banned_ips.extend(ips);
        let mut peers = self.peers.lock().unwrap_or_else(PoisonError::into_inner);
        for &key in keys {
            // Dropping the sender ends the peer's writer, and so its connection.
            if peers.remove(&key).is_some() {
                self.set_verdict(key, Verdict::Banned);
            }
        }
    }

    /// Records the address of a connected peer, for [`Shared::ban`].
    pub(crate) fn peer_connected(&self, key: PeerKey, addr: SocketAddr) {
        self.peer_ips
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(key, addr.ip());
    }

    /// Called when a peer's connection ends: its address is kept a little longer, for
    /// [`Shared::ban`].
    pub(crate) fn peer_disconnected(&self, key: PeerKey) {
        self.peer_ips
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .ended(key);
    }

    /// Whether connections from `ip` are refused: a peer there sent bad data.
    pub(crate) fn is_banned(&self, ip: IpAddr) -> bool {
        self.candidates().banned_ips.contains(&ip)
    }

    /// Records why the torrent is disconnecting `key`.
    fn set_verdict(&self, key: PeerKey, verdict: Verdict) {
        self.verdicts
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(key, verdict);
    }

    /// Takes the verdict recorded for `key`, if the torrent disconnected it.
    pub(crate) fn take_verdict(&self, key: PeerKey) -> Option<Verdict> {
        self.verdicts
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&key)
    }

    /// Live peers (including web seeds).
    pub(crate) fn live_peers(&self) -> usize {
        self.peers
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .len()
    }

    /// Whether a swarm of `live` peers (counted with [`Shared::live_peers`]) is thin. Web seeds
    /// do not count: the guards keep BitTorrent peers while there are few of them, whatever web
    /// seeds there are.
    fn is_thin(&self, live: usize) -> bool {
        live.saturating_sub(self.web_seeds.load(Ordering::Relaxed)) <= THIN_SWARM
    }

    /// Asks the driver to re-plan soon; `seek` also cancels requests that are no longer urgent.
    pub(crate) fn request_replan(&self, seek: bool) {
        if seek {
            self.seek_pending.store(true, Ordering::Relaxed);
        }
        self.wake.notify_one();
    }

    /// Waits for the engine-wide upload limit. Returns early when the torrent or request is
    /// cancelled, or when uploading was disabled while it waited.
    pub(crate) async fn throttle_upload(&self, bytes: usize, request: &CancellationToken) -> bool {
        tokio::select! {
            () = self.cancel.cancelled() => false,
            () = request.cancelled() => false,
            () = self.upload_limiter.acquire(bytes) => {
                self.upload_enabled.load(Ordering::Relaxed) && !request.is_cancelled()
            }
        }
    }

    /// Sends messages to a peer. A peer whose queue is full is not keeping up and is dropped
    /// (its writer sees the channel close).
    pub(crate) fn send(&self, key: PeerKey, messages: impl IntoIterator<Item = Outgoing>) {
        let mut peers = self.peers.lock().unwrap_or_else(PoisonError::into_inner);
        let Some(tx) = peers.get(&key) else {
            return;
        };
        for m in messages {
            if tx.try_send(m).is_err() {
                debug!(peer = key, "peer send queue full; disconnecting");
                peers.remove(&key);
                return;
            }
        }
    }

    /// Queues a piece only while uploads remain enabled. This shares the peer-map lock with
    /// [`Torrent::set_upload_enabled`], so an upload is always ordered before the choke or
    /// discarded after it.
    pub(crate) fn send_upload(&self, key: PeerKey, message: Outgoing) {
        let mut peers = self.peers.lock().unwrap_or_else(PoisonError::into_inner);
        if !self.upload_enabled.load(Ordering::Relaxed) {
            return;
        }
        let Some(tx) = peers.get(&key) else {
            return;
        };
        if tx.try_send(message).is_err() {
            debug!(peer = key, "peer send queue full; disconnecting");
            peers.remove(&key);
        }
    }

    /// Sends a batch of requests and cancels to their peers.
    pub(crate) fn send_requests(&self, key: PeerKey, requests: &[Request]) {
        self.send(
            key,
            requests.iter().map(|r| Outgoing::Request {
                piece: r.piece,
                begin: r.begin,
                len: r.len,
            }),
        );
    }

    /// Reads part of a stored piece (see [`PieceStore::read`]). Blocking. A piece trusted from
    /// an earlier session that fails its check on this first read is no longer counted as
    /// stored, and is downloaded again.
    pub(crate) fn read_piece(&self, piece: u32, offset: u32, buf: &mut [u8]) -> io::Result<()> {
        let read = self.store.read(piece, offset, buf);
        if let Err(error) = &read
            && matches!(
                error.kind(),
                io::ErrorKind::InvalidData | io::ErrorKind::NotFound
            )
        {
            let mut sched = self.sched();
            if sched.have(piece) {
                sched.lose(piece);
                drop(sched);
                debug!(piece, "a stored piece is bad; downloading it again");
                self.refresh_background.store(true, Ordering::Relaxed);
                self.request_replan(false);
            }
        }
        read
    }

    /// Folds the bytes downloaded and uploaded so far into the smoothed rates.
    fn update_rates(&self, now: Instant) {
        self.smoothed
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .update(now, self.downloaded.load(Ordering::Relaxed));
        self.upload_smoothed
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .update(now, self.uploaded.load(Ordering::Relaxed));
    }

    /// Tells every peer that `piece` is now verified.
    pub(crate) fn announce_have(&self, piece: u32) {
        let keys: Vec<PeerKey> = self
            .peers
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .keys()
            .copied()
            .collect();
        for key in keys {
            self.send(key, [Outgoing::Have(piece)]);
        }
    }

    pub(crate) fn send_cancels(&self, cancels: &[(PeerKey, Request)]) {
        for (key, r) in cancels {
            self.send(
                *key,
                [Outgoing::Cancel {
                    piece: r.piece,
                    begin: r.begin,
                    len: r.len,
                }],
            );
        }
    }

    /// Verifies a completed piece on a blocking thread, stores it, and updates the scheduler.
    /// Bans the peers the check shows sent bad data (see [`Scheduler::on_verified`]).
    pub(crate) fn complete_piece(self: &Arc<Self>, piece: u32, data: Vec<u8>) {
        let shared = Arc::clone(self);
        let digests = self.sched().needs_block_digests(piece);
        tokio::spawn(async move {
            let check = Arc::clone(&shared);
            let len = data.len() as u64;
            let checked = tokio::task::spawn_blocking(move || {
                use sha1::Digest as _;
                let ok = check
                    .meta
                    .piece_hash(piece)
                    .is_some_and(|expected| sha1::Sha1::digest(&data).as_slice() == expected);
                if !ok {
                    return PieceCheck::Invalid(block_digests(&data));
                }
                match check.store.write_piece(piece, &data) {
                    Ok(()) => PieceCheck::Valid(digests.then(|| block_digests(&data))),
                    Err(error) => {
                        tracing::warn!(%error, piece, "could not store piece");
                        PieceCheck::NotStored
                    }
                }
            })
            .await
            .unwrap_or_else(|error| {
                tracing::warn!(%error, piece, "piece verification task failed");
                PieceCheck::NotStored
            });
            let valid = matches!(checked, PieceCheck::Valid(_));
            let failed = matches!(checked, PieceCheck::Invalid(_));
            let verified = shared.sched().on_verified(piece, checked);
            if valid {
                trace!(piece, "piece verified");
                shared.downloaded.fetch_add(len, Ordering::Relaxed);
                shared.piece_done.notify_waiters();
                shared.announce_have(piece);
                if !verified.banned.is_empty() {
                    debug!(
                        piece,
                        peers = ?verified.banned,
                        "banning peers that sent bad blocks of a piece that now verified"
                    );
                    shared.ban(&verified.banned);
                }
            } else if failed {
                debug!(
                    piece,
                    sources = verified.failed_by.len(),
                    "piece failed its hash check"
                );
                let disconnect = shared.blame(&verified.failed_by);
                shared.ban(&disconnect);
            }
            shared.request_replan(false);
        });
    }
}

/// The digest of each block of a piece, for smart ban.
fn block_digests(data: &[u8]) -> Vec<BlockDigest> {
    use sha1::Digest as _;
    data.chunks(BLOCK_LEN as usize)
        .map(|block| sha1::Sha1::digest(block).into())
        .collect()
}

/// What is needed to (re)start tracker announces for a torrent.
struct Trackers {
    info_hash: Id20,
    peer_id: Id20,
    urls: HashSet<url::Url>,
    shared: Weak<Shared>,
    port: u16,
    http: reqwest::Client,
    udp: UdpTrackerClient,
}

impl Trackers {
    /// Starts announcing under `policy`: not at all while traffic is blocked or trackers are
    /// off. Protected modes use TorrentPlayer's own scoped UDP client through the WireGuard
    /// tunnel or the proxy relay; native unbound UDP is never used (see [`crate::udp_tracker`]).
    fn start(&self, policy: Option<Policy>) -> Option<BoxStream<'static, SocketAddr>> {
        if policy.is_some_and(|p| !p.traffic_allowed || !p.trackers_allowed) {
            return None;
        }
        let udp_allowed = policy.is_none_or(|p| p.udp_allowed);
        let port = self
            .shared
            .upgrade()
            .and_then(|s| s.incoming_port())
            .unwrap_or(self.port);
        let urls = self
            .urls
            .iter()
            .filter(|u| udp_allowed || u.scheme() != "udp")
            .cloned()
            .collect::<HashSet<_>>();
        let comms = TrackerComms::start(
            self.info_hash,
            self.peer_id,
            urls,
            Box::new(TrackerStats(self.shared.clone())),
            None,
            port,
            self.http.clone(),
            self.udp.clone(),
        );
        let bound_udp = !udp_allowed && policy.is_some_and(|p| p.bound_udp_allowed);
        let own: Vec<url::Url> = if bound_udp {
            self.urls
                .iter()
                .filter(|u| u.scheme() == "udp")
                .cloned()
                .collect()
        } else {
            Vec::new()
        };
        let shared = self.shared.upgrade();
        let (Some(shared), false) = (shared, own.is_empty()) else {
            return comms;
        };
        let request = crate::udp_tracker::AnnounceRequest {
            info_hash: self.info_hash.0,
            peer_id: self.peer_id.0,
            left: shared.meta.total_len,
            port,
            started: true,
        };
        let (tx, rx) = mpsc::channel(256);
        for url in own {
            tokio::spawn(announce_udp(
                url,
                shared.cfg.network.clone(),
                request,
                tx.clone(),
            ));
        }
        let own = futures_util::stream::unfold(rx, |mut rx| async move {
            rx.recv().await.map(|addr| (addr, rx))
        })
        .boxed();
        Some(match comms {
            Some(comms) => futures_util::stream::select(comms, own).boxed(),
            None => own,
        })
    }
}

/// Announces to one UDP tracker until the receiver is dropped (the stream is replaced when the
/// network mode changes), at the tracker's interval, backing off after failures.
async fn announce_udp(
    url: url::Url,
    network: Option<Network>,
    mut request: crate::udp_tracker::AnnounceRequest,
    tx: mpsc::Sender<SocketAddr>,
) {
    let mut backoff = Duration::from_secs(30);
    loop {
        let wait = match crate::udp_tracker::announce(network.as_ref(), &url, &request).await {
            Ok(announced) => {
                debug!(peers = announced.peers.len(), "UDP tracker answered");
                for addr in announced.peers {
                    if tx.send(addr).await.is_err() {
                        return;
                    }
                }
                request.started = false;
                backoff = Duration::from_secs(30);
                announced.interval
            }
            Err(error) => {
                debug!(%error, "UDP tracker announce failed");
                let wait = backoff;
                backoff = (backoff * 2).min(crate::udp_tracker::MAX_INTERVAL);
                wait
            }
        };
        tokio::select! {
            () = tx.closed() => return,
            () = tokio::time::sleep(wait) => {}
        }
    }
}

/// Waits for the next policy change. A dropped network counts as blocked.
async fn next_policy(rx: &mut Option<watch::Receiver<Policy>>, last: Option<Policy>) -> Policy {
    let Some(receiver) = rx.as_mut() else {
        return std::future::pending().await;
    };
    if receiver.changed().await.is_ok() {
        *receiver.borrow_and_update()
    } else {
        *rx = None;
        let blocked = Policy {
            epoch: 0,
            traffic_allowed: false,
            protected: true,
            udp_allowed: false,
            bound_udp_allowed: false,
            dht_allowed: false,
            trackers_allowed: false,
        };
        Policy {
            traffic_allowed: false,
            ..last.unwrap_or(blocked)
        }
    }
}

/// Starts announcing to `trackers` under `policy` (see [`Trackers::start`]), unless the torrent
/// is paused. Dropping the stream stops the announces.
fn announce(
    trackers: Option<&Trackers>,
    policy: Option<Policy>,
    paused: bool,
) -> Option<BoxStream<'static, SocketAddr>> {
    trackers.filter(|_| !paused)?.start(policy)
}

/// The per-torrent driver: re-plans, refills queues, handles timeouts, and connects peers.
async fn drive(shared: Arc<Shared>, trackers: Option<Trackers>) {
    // Every TICK, or IDLE_TICK while there is nothing to download.
    let mut next_tick = tokio::time::Instant::now();
    let mut ticks: u32 = 0;
    let mut policy_rx = shared.cfg.network.as_ref().map(Network::subscribe);
    let mut policy = policy_rx.as_mut().map(|rx| *rx.borrow_and_update());
    let mut paused = shared.paused.load(Ordering::Relaxed);
    let mut announces = announce(trackers.as_ref(), policy, paused);
    loop {
        let mut ticked = false;
        tokio::select! {
            () = shared.cancel.cancelled() => break,
            () = tokio::time::sleep_until(next_tick) => {
                ticks = ticks.wrapping_add(1);
                ticked = true;
            }
            () = shared.wake.notified() => {}
            Some(addr) = async {
                match announces.as_mut() {
                    Some(t) => t.next().await,
                    None => std::future::pending().await,
                }
            } => {
                shared.add_candidates([addr]);
            }
            now = next_policy(&mut policy_rx, policy) => {
                if Some(now) != policy {
                    let resumed = now.traffic_allowed
                        && policy.is_some_and(|p| !p.traffic_allowed || p.epoch != now.epoch);
                    // Restart (or stop) announcing so tracker traffic follows the new mode.
                    announces = announce(trackers.as_ref(), Some(now), paused);
                    if resumed {
                        shared.requeue_known();
                    }
                    policy = Some(now);
                }
            }
        }

        // A paused torrent neither downloads nor uploads, so it stops announcing; resuming
        // announces it again (`started`) at once.
        if shared.paused.load(Ordering::Relaxed) != paused {
            paused = !paused;
            announces = announce(trackers.as_ref(), policy, paused);
        }

        let now = Instant::now();
        shared.update_rates(now);
        let seek = shared.seek_pending.swap(false, Ordering::Relaxed);
        let refresh = shared.refresh_background.swap(false, Ordering::Relaxed)
            || ticks.is_multiple_of(BACKGROUND_REFRESH_TICKS);
        let keys: Vec<PeerKey> = shared
            .peers
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .keys()
            .copied()
            .collect();
        let (cancels, fills, unresponsive, idle) = {
            let mut sched = shared.sched();
            let mut cancels = sched.reprioritize(seek);
            if seek {
                debug!(cancels = cancels.len(), "seek: re-prioritized");
            }
            sched.set_rate_limit(shared.rate_limit());
            cancels.extend(sched.tick(now, refresh));
            let unresponsive = sched.unresponsive_peers(MAX_TIMEOUTS);
            let fills = sched.fill_all(&keys, now);
            let idle = shared.paused.load(Ordering::Relaxed) || sched.is_idle();
            (cancels, fills, unresponsive, idle)
        };
        shared.idle.store(idle, Ordering::Relaxed);
        // A wake-up can only bring the next tick forward: a burst of them (peers connecting,
        // pieces verifying) must not hold it off.
        let due = tokio::time::Instant::now() + if idle { IDLE_TICK } else { TICK };
        next_tick = if ticked { due } else { next_tick.min(due) };
        if !unresponsive.is_empty() {
            let mut peers = shared.peers.lock().unwrap_or_else(PoisonError::into_inner);
            for key in unresponsive {
                // In a thin swarm a slow peer is better than none: keep it.
                if shared.is_thin(peers.len()) {
                    break;
                }
                // Dropping the sender ends the peer's writer, and so its connection.
                if peers.remove(&key).is_some() {
                    shared.set_verdict(key, Verdict::Unresponsive);
                }
            }
        }
        shared.send_cancels(&cancels);
        for (key, requests) in fills {
            if !requests.is_empty() {
                shared.send_requests(key, &requests);
            }
        }
        // While the kill switch blocks traffic (or the torrent is paused), candidates wait
        // instead of failing.
        if policy.is_none_or(|p| p.traffic_allowed) && !shared.paused.load(Ordering::Relaxed) {
            if ticks.is_multiple_of(RETRY_TICKS) {
                shared.candidates().promote_due(now);
            }
            connect_more(&shared, keys.len());
        }
    }
    // Peer tasks watch the same token and exit. Readers and stream handles may keep this
    // torrent's state alive a while longer, so its partial pieces give their memory back to
    // the other torrents now (blocks still arriving are counted as wasted).
    let _ = shared.sched().set_paused(true);
    debug!("torrent driver stopped");
}

/// Reads the handshake of each peer that connects to us and hands the connection to the torrent
/// it names; connections for other torrents, and beyond [`MAX_INBOUND_HANDSHAKES`] waiting
/// ones, are closed.
async fn accept_incoming(
    mut incoming: IncomingPeers,
    torrents: Registry,
    timeout: Duration,
    cancel: CancellationToken,
) {
    let pending = Arc::new(Semaphore::new(MAX_INBOUND_HANDSHAKES));
    loop {
        let peer = tokio::select! {
            () = cancel.cancelled() => return,
            peer = incoming.next() => peer,
        };
        let Some(IncomingPeer {
            mut stream,
            addr,
            mut lease,
        }) = peer
        else {
            return;
        };
        let Ok(permit) = Arc::clone(&pending).try_acquire_owned() else {
            trace!(%addr, "too many incoming handshakes; closing");
            continue;
        };
        let torrents = Arc::clone(&torrents);
        tokio::spawn(async move {
            let mut theirs = [0u8; HANDSHAKE_LEN];
            let read = tokio::select! {
                () = lease.revoked() => return,
                read = tokio::time::timeout(timeout, stream.read_exact(&mut theirs)) => read,
            };
            drop(permit);
            if !matches!(read, Ok(Ok(_))) {
                return;
            }
            let shared = wire::handshake_info_hash(&theirs).and_then(|hash| {
                torrents
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .get(&hash)
                    .and_then(|t| t.shared.upgrade())
            });
            if let Some(shared) = shared {
                crate::peer::accept(shared, stream, addr, lease, theirs).await;
            } else {
                trace!(%addr, "incoming peer for an unknown torrent; closing");
            }
        });
    }
}

fn connect_more(shared: &Arc<Shared>, live: usize) {
    let cfg = &shared.cfg;
    let mut candidates = shared.candidates();
    while live + candidates.connecting + candidates.handshaking < cfg.peer_limit
        && candidates.connecting < cfg.max_connecting
    {
        let Some((addr, first)) = candidates.next() else {
            break;
        };
        let timeout = if first {
            cfg.probe_timeout.min(cfg.connect_timeout)
        } else {
            cfg.connect_timeout
        };
        let key = shared.next_peer.fetch_add(1, Ordering::Relaxed);
        tokio::spawn(crate::peer::run(Arc::clone(shared), addr, key, timeout));
    }
}

impl Shared {
    /// Adds an absolute byte range to the background selection.
    fn select(&self, start: u64, end: u64) {
        self.selected
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push((start, end));
        self.sched().want_range(start, end);
        self.refresh_background.store(true, Ordering::Relaxed);
        self.wake.notify_one();
    }

    /// Most bytes a peer reader may take from its connection at once: unbounded while no
    /// download limit applies, one block under a limit. Readers wait their turn after each
    /// read, so with whole socket buffers (64 KiB and more) each of many peers would deliver in
    /// bursts seconds apart, look snubbed, and have its blocks requested again elsewhere.
    pub(crate) fn read_cap(&self) -> usize {
        if self.rate_limit().is_some() {
            BLOCK_LEN as usize
        } else {
            usize::MAX
        }
    }

    /// The download limit this torrent is held to (bytes per second): the lower of the
    /// engine's and its own.
    pub(crate) fn rate_limit(&self) -> Option<f64> {
        let engine = self.limiter.as_ref().and_then(|l| l.rate());
        match (engine, self.own_limit.rate()) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }

    /// Waits for this torrent's own rate limit (the engine's is applied first).
    pub(crate) async fn throttle(&self, bytes: usize) {
        if let Some(limiter) = &self.limiter {
            limiter.acquire(bytes).await;
        }
        self.own_limit.acquire(bytes).await;
    }

    pub(crate) fn web_seed_started(&self) {
        self.web_seeds.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn web_seed_finished(&self) {
        self.web_seeds.fetch_sub(1, Ordering::Relaxed);
    }

    /// Called by a peer task when its TCP connection attempt ends; `connected` if the handshake
    /// follows, which no longer holds a connection slot.
    pub(crate) fn tcp_finished(&self, connected: bool) {
        let mut c = self.candidates();
        c.connecting = c.connecting.saturating_sub(1);
        if connected {
            c.handshaking += 1;
        }
        drop(c);
        // A slot is free: dial the next candidate without waiting for the next tick.
        self.wake.notify_one();
    }

    /// Makes room for a peer that connected to us: it counts as handshaking until
    /// [`Shared::handshake_finished`]. Refused at the peer limit (counting connections being
    /// made), while paused, once the torrent stopped, and from a banned address.
    pub(crate) fn reserve_inbound(&self, addr: SocketAddr) -> bool {
        if self.paused.load(Ordering::Relaxed) || self.cancel.is_cancelled() {
            return false;
        }
        let live = self.live_peers();
        let mut c = self.candidates();
        if c.banned_ips.contains(&addr.ip())
            || live + c.connecting + c.handshaking >= self.cfg.peer_limit
        {
            return false;
        }
        c.handshaking += 1;
        true
    }

    /// A key for a new peer connection.
    pub(crate) fn next_peer_key(&self) -> PeerKey {
        self.next_peer.fetch_add(1, Ordering::Relaxed)
    }

    /// The listening port to announce, while this engine accepts incoming peers.
    fn incoming_port(&self) -> Option<u16> {
        if !self.cfg.accept_incoming {
            return None;
        }
        self.cfg.network.as_ref()?.incoming_port()
    }

    /// What DHT peer discovery needs to pace its lookups.
    pub(crate) fn dht_demand(&self) -> crate::dht::Demand {
        crate::dht::Demand {
            paused: self.paused.load(Ordering::Relaxed),
            idle: self.idle.load(Ordering::Relaxed),
            live_peers: self.live_peers(),
            peer_limit: self.cfg.peer_limit,
            listen_port: self.incoming_port(),
        }
    }

    /// Called by a peer task when the handshake after [`Shared::tcp_finished`] ends.
    pub(crate) fn handshake_finished(&self) {
        let mut c = self.candidates();
        c.handshaking = c.handshaking.saturating_sub(1);
    }

    /// Called by a peer task when it ends, to schedule the next attempt.
    pub(crate) fn peer_finished(&self, addr: SocketAddr, end: PeerEnd) {
        let thin = self.is_thin(self.live_peers());
        self.candidates().finished(addr, end, thin, Instant::now());
    }
}

struct TrackerStats(Weak<Shared>);

impl TorrentStatsProvider for TrackerStats {
    fn get(&self) -> TrackerCommsStats {
        let Some(shared) = self.0.upgrade() else {
            return TrackerCommsStats::default();
        };
        TrackerCommsStats {
            uploaded_bytes: shared.uploaded.load(Ordering::Relaxed),
            downloaded_bytes: shared.downloaded.load(Ordering::Relaxed),
            total_bytes: shared.meta.total_len,
            torrent_state: TrackerCommsStatsState::Live,
        }
    }
}

/// The trackers of `meta` to announce to: HTTP ones on the user's own network (a literal
/// non-public address or a `localhost` name) only if `allow_local`. Names are checked when
/// they are resolved (see [`tracker_client`]). UDP trackers are not checked here.
fn tracker_urls(meta: &Metainfo, allow_local: bool) -> HashSet<url::Url> {
    meta.trackers
        .iter()
        .filter(|u| {
            !matches!(u.scheme(), "http" | "https") || allow_local || crate::webseed::url_allowed(u)
        })
        .cloned()
        .collect()
}

/// The HTTP client for trackers. Their URLs come from magnet links and torrent files, so unless
/// [`SwarmConfig::allow_local_trackers`], as for web seeds: only public destinations, through
/// the network manager's public-only bridge (or, without one, a resolver that drops non-public
/// addresses), with checked redirects. Literal addresses are checked by [`tracker_urls`].
fn tracker_client(cfg: &SwarmConfig) -> Result<reqwest::Client, SwarmError> {
    let allow_local = cfg.allow_local_trackers;
    let http = reqwest::Client::builder()
        .user_agent(concat!("TorrentPlayer/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(30))
        .redirect(crate::webseed::redirect_policy(allow_local));
    match (&cfg.network, allow_local) {
        (Some(network), false) => network.configure_http_public(http),
        (Some(network), true) => network.configure_http(http),
        (None, false) => Ok(http.dns_resolver(crate::webseed::PublicResolver)),
        (None, true) => Ok(http),
    }
    .map_err(|e| SwarmError::Trackers(e.to_string()))?
    .build()
    .map_err(|e| SwarmError::Trackers(e.to_string()))
}

/// The HTTP client for web seeds: through the network manager's public-only bridge (or, without
/// one, a resolver that drops non-public addresses), with checked redirects.
fn web_client(cfg: &SwarmConfig) -> Result<reqwest::Client, SwarmError> {
    let mut web = reqwest::Client::builder()
        .user_agent(concat!("TorrentPlayer/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(Duration::from_secs(10))
        .read_timeout(crate::webseed::READ_TIMEOUT)
        .redirect(crate::webseed::redirect_policy(cfg.allow_local_web_seeds));
    web = match (&cfg.network, cfg.allow_local_web_seeds) {
        (Some(network), false) => network.configure_http_public(web),
        (Some(network), true) => network.configure_http(web),
        (None, false) => Ok(web.dns_resolver(crate::webseed::PublicResolver)),
        (None, true) => Ok(web),
    }
    .map_err(|e| SwarmError::WebSeeds(e.to_string()))?;
    web.build().map_err(|e| SwarmError::WebSeeds(e.to_string()))
}

/// An Azureus-style peer id: `-TP0100-` followed by 12 random alphanumerics.
pub(crate) fn generate_peer_id() -> [u8; 20] {
    const ALPHABET: &[u8; 62] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";
    let mut id = *b"-TP0100-000000000000";
    let mut random = [0u8; 12];
    // Falling back to a fixed suffix only weakens peer-id uniqueness, not security.
    let _ = getrandom::fill(&mut random);
    for (slot, r) in id[8..].iter_mut().zip(random) {
        *slot = ALPHABET[usize::from(r) % ALPHABET.len()];
    }
    id
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addr(port: u16) -> SocketAddr {
        SocketAddr::from(([127, 0, 0, 1], port))
    }

    /// Takes the next address for a connection attempt, as `connect_more` does.
    fn take(c: &mut Candidates) -> SocketAddr {
        let (a, _) = c.next().unwrap();
        c.connecting -= 1;
        a
    }

    /// Queued addresses in dialing order.
    fn queued(c: &Candidates) -> Vec<SocketAddr> {
        c.queues.iter().flatten().copied().collect()
    }

    #[test]
    #[allow(clippy::cast_precision_loss)]
    fn estimates_when_playback_can_run_without_stalling() {
        const MIB: u64 = 1024 * 1024;
        // Downloading faster than playback: play now.
        assert_eq!(
            estimate(100 * MIB, 0, 2.0 * MIB as f64, MIB),
            (true, Some(0.0))
        );
        // Everything left is downloaded.
        assert_eq!(estimate(10 * MIB, 10 * MIB, 0.0, MIB), (false, Some(0.0)));
        // Nothing downloading and data missing: no estimate.
        assert_eq!(estimate(10 * MIB, 0, 0.0, MIB), (false, None));
        // Half the playback rate (after the margin): 100 s of media needs 100 s of waiting.
        let (keeps_up, wait) = estimate(100 * MIB, 0, MIB as f64 / 2.0 / RATE_MARGIN, MIB);
        assert!(!keeps_up);
        assert!((wait.unwrap() - 100.0).abs() < 1e-6, "{wait:?}");
        // A buffer shortens the wait: 25 MiB at 0.5 MiB/s is 50 s less.
        let (_, wait) = estimate(100 * MIB, 25 * MIB, MIB as f64 / 2.0 / RATE_MARGIN, MIB);
        assert!((wait.unwrap() - 50.0).abs() < 1e-6, "{wait:?}");
    }

    #[test]
    fn smooths_the_download_rate() {
        let t0 = Instant::now();
        let mut s = Smoothed::default();
        s.update(t0, 0);
        assert_eq!(s.bps, None);
        s.update(t0 + Duration::from_millis(500), 1000);
        assert_eq!(s.bps, None, "samples are at least a second apart");
        s.update(t0 + Duration::from_secs(1), 1000);
        assert_eq!(s.bps, Some(1000.0));
        // A stall pulls the average down gradually.
        s.update(t0 + Duration::from_secs(2), 1000);
        let bps = s.bps.unwrap();
        assert!(bps > 800.0 && bps < 1000.0, "{bps}");
    }

    #[test]
    fn retry_delays_double_up_to_a_cap() {
        assert_eq!(retry_delay(1, false), RETRY_BASE);
        assert_eq!(retry_delay(2, false), RETRY_BASE * 2);
        assert_eq!(retry_delay(3, true), RETRY_BASE * 4);
        assert_eq!(retry_delay(40, true), THIN_RETRY_MAX);
        assert_eq!(retry_delay(40, false), RETRY_MAX);
    }

    #[test]
    fn retries_unreachable_peers_and_reconnects_working_ones() {
        let mut c = Candidates::default();
        let now = Instant::now();
        assert!(c.offer(addr(1), false));
        assert!(!c.offer(addr(1), false), "already queued");
        let a = take(&mut c);
        assert!(!c.offer(a, false), "already connecting");

        c.finished(a, PeerEnd::Unreachable, false, now);
        assert!(!c.offer(a, false), "waits for its retry");
        c.promote_due(now + RETRY_BASE.saturating_sub(Duration::from_millis(1)));
        assert!(queued(&c).is_empty());
        c.promote_due(now + RETRY_BASE);
        assert_eq!(queued(&c), [a]);

        // A second failure waits twice as long.
        let a = take(&mut c);
        c.finished(a, PeerEnd::Unreachable, false, now);
        assert_eq!(c.known[&a].retry_at, Some(now + RETRY_BASE * 2));

        // A connection that worked is retried soon, and its failures are forgotten.
        c.promote_due(now + RETRY_MAX);
        let a = take(&mut c);
        c.finished(a, PeerEnd::closed(WORKING_FOR, 0), false, now);
        assert_eq!(c.known[&a].failures, 0);
        assert_eq!(c.known[&a].retry_at, Some(now + RECONNECT_AFTER));
        // One that closed quickly without delivering much counts as a failure.
        c.promote_due(now + RECONNECT_AFTER);
        let a = take(&mut c);
        c.finished(a, PeerEnd::closed(Duration::from_secs(1), 1000), false, now);
        assert_eq!(c.known[&a].failures, 1);
    }

    #[test]
    fn gives_up_unless_the_swarm_is_thin() {
        let mut c = Candidates::default();
        let now = Instant::now();
        let (a, b) = (addr(1), addr(2));
        c.offer(a, false);
        c.offer(b, false);
        for _ in 0..MAX_FAILURES {
            for addr in [a, b] {
                c.known.get_mut(&addr).unwrap().queued = false;
                for q in &mut c.queues {
                    q.retain(|q| *q != addr);
                }
                c.active.insert(addr);
                // `a` fails in a healthy swarm, `b` in a thin one.
                c.finished(addr, PeerEnd::Unreachable, addr == b, now);
                c.known.get_mut(&addr).unwrap().retry_at = None;
            }
        }
        // Re-run the last failure to see the decision.
        c.active.insert(a);
        c.finished(a, PeerEnd::Unreachable, false, now);
        c.active.insert(b);
        c.finished(b, PeerEnd::Unreachable, true, now);
        assert_eq!(c.known[&a].retry_at, None, "given up");
        assert!(c.known[&b].retry_at.is_some(), "a thin swarm keeps trying");
        // A tracker naming a given-up address again earns it one more try.
        assert!(c.offer(a, false));
    }

    #[test]
    fn never_retries_banned_peers() {
        let mut c = Candidates::default();
        c.offer(addr(1), false);
        let a = take(&mut c);
        c.finished(a, PeerEnd::Banned, true, Instant::now());
        assert!(!c.offer(a, true));
        c.promote_due(Instant::now() + RETRY_MAX);
        assert!(queued(&c).is_empty());
    }

    #[test]
    fn exchanged_seeds_are_tried_first_and_the_list_is_bounded() {
        let mut c = Candidates::default();
        c.offer(addr(1), false);
        c.offer(addr(2), true);
        assert_eq!(queued(&c), [addr(2), addr(1)]);
        for port in 3..=u16::try_from(MAX_KNOWN).unwrap() {
            c.offer(addr(port), false);
        }
        assert_eq!(c.known.len(), MAX_KNOWN);
        assert!(!c.offer(addr(u16::MAX), false), "full");
        assert!(!c.offer(addr(1), false), "still known and queued");
    }

    #[test]
    fn a_banned_address_is_never_dialed_on_any_port() {
        let mut c = Candidates::default();
        let other = SocketAddr::from(([127, 0, 0, 2], 7));
        c.offer(addr(1), false);
        c.offer(other, false);
        c.offer(addr(2), false);
        c.banned_ips.insert(addr(1).ip());
        assert!(!c.offer(addr(3), false), "another port of a banned address");
        // Queued before the ban: skipped.
        assert_eq!(take(&mut c), other);
        assert!(c.next().is_none());
        assert!(c.known[&addr(2)].banned);
    }

    #[test]
    fn peer_addresses_outlive_their_connections_for_a_while() {
        let mut ips = PeerIps::default();
        ips.insert(1, addr(1).ip());
        ips.ended(1);
        assert_eq!(ips.get(1), Some(addr(1).ip()));
        for key in 2..2 + RECENT_PEER_IPS as u64 {
            ips.insert(key, addr(2).ip());
            ips.ended(key);
        }
        assert_eq!(ips.get(1), None, "forgotten after enough others ended");
        assert_eq!(ips.recent.len(), RECENT_PEER_IPS);
    }

    #[test]
    fn a_peer_alone_with_two_bad_pieces_is_banned() {
        let mut blames = HashMap::new();
        assert!(blame(&mut blames, &[(1, 1)]).is_empty());
        assert_eq!(blame(&mut blames, &[(1, 2)]), vec![1]);
        // Pieces with its data say nothing about the others any more.
        assert!(blame(&mut blames, &[(1, 3), (2, 9)]).is_empty());
    }

    #[test]
    fn a_tie_never_bans_an_honest_peer() {
        // An attacker spoils every piece an honest peer also sent blocks of: both are counted
        // the same, and neither is banned however long it goes on (smart ban settles it).
        let mut blames = HashMap::new();
        for failures in 1..100 {
            assert!(blame(&mut blames, &[(1, failures), (2, failures)]).is_empty());
            assert!(blame(&mut blames, &[(2, failures), (1, failures), (3, 1)]).is_empty());
        }
    }

    #[test]
    fn the_worst_of_several_sources_is_banned_after_four_bad_pieces() {
        let mut blames = HashMap::new();
        assert!(blame(&mut blames, &[(1, 3), (2, 1)]).is_empty());
        assert_eq!(blame(&mut blames, &[(1, 4), (2, 1)]), vec![1]);
    }

    #[tokio::test]
    async fn a_re_add_keeps_the_cache_of_any_live_earlier_instance() {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = SwarmConfig::new(dir.path().to_path_buf());
        cfg.enable_trackers = false;
        cfg.enable_web_seeds = false;
        let swarm = Swarm::start(cfg).await.unwrap();
        let mut bytes =
            b"d4:infod6:lengthi16384e4:name5:a.mp412:piece lengthi16384e6:pieces20:".to_vec();
        bytes.extend_from_slice(&[0; 20]);
        bytes.extend_from_slice(b"ee");
        let meta = Metainfo::from_bytes(&bytes).unwrap();

        // A is still read from (a stream request finishing) while B is added, stopped and gone.
        let a = swarm.add(meta.clone(), Vec::new()).unwrap();
        a.shared.store.write_piece(0, b"piece of a").unwrap();
        let b = swarm.add(meta.clone(), Vec::new()).unwrap();
        b.stop();
        let gone = Arc::downgrade(&b.shared);
        drop(b);
        tokio::time::timeout(Duration::from_secs(5), async {
            while gone.strong_count() > 0 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("the stopped instance is dropped");

        // Adding C must not truncate the cache file under A, and C has A's verified piece.
        let c = swarm.add(meta, Vec::new()).unwrap();
        let mut buf = [0u8; 10];
        a.shared.store.read(0, 0, &mut buf).unwrap();
        assert_eq!(&buf, b"piece of a");
        assert_eq!(c.have_pieces(), vec![true]);
        swarm.shutdown();
    }

    #[tokio::test]
    async fn stopping_wakes_a_waiting_reader_and_gives_back_partial_pieces() {
        let dir = tempfile::tempdir().unwrap();
        let swarm = Swarm::start(config(dir.path())).await.unwrap();
        let mut bytes =
            b"d4:infod6:lengthi65536e4:name5:a.mp412:piece lengthi16384e6:pieces80:".to_vec();
        bytes.extend_from_slice(&[0; 80]);
        bytes.extend_from_slice(b"ee");
        let torrent = swarm
            .add(Metainfo::from_bytes(&bytes).unwrap(), Vec::new())
            .unwrap();
        let mut reader = torrent.reader(0).unwrap();
        // A peer (never connected) that has every piece starts the reader's pieces.
        {
            let mut sched = torrent.shared.sched();
            sched.add_peer(1_000);
            sched.on_bitfield(1_000, &[0xf0]);
            sched.on_unchoke(1_000);
            sched.reprioritize(false);
            assert!(!sched.fill(1_000, Instant::now()).is_empty());
        }
        assert!(swarm.piece_budget.used() > 0);

        let read = tokio::spawn(async move {
            let mut buf = [0u8; 1024];
            let result = reader.read_exact(&mut buf).await.map(|_| ());
            (result, reader)
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        torrent.stop();
        let (result, reader) = tokio::time::timeout(Duration::from_secs(5), read)
            .await
            .expect("the read ends when the torrent stops")
            .unwrap();
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::ConnectionAborted);
        // The reader still holds the torrent, but no longer its share of the budget.
        wait_for(
            "the partial pieces are given back",
            Duration::from_secs(5),
            || swarm.piece_budget.used() == 0,
        )
        .await;

        let gone = Arc::downgrade(&torrent.shared);
        drop((reader, torrent));
        wait_for("the torrent closes", Duration::from_secs(5), || {
            gone.strong_count() == 0
        })
        .await;
        swarm.shutdown();
    }

    const PIECES: [&[u8]; 3] = [b"zeroth..", b"first...", b"sec"];

    #[tokio::test]
    async fn shutdown_can_time_out_while_a_cache_is_locked_and_the_flush_can_finish() {
        let dir = tempfile::tempdir().unwrap();
        let meta = crate::store::tests::meta(&PIECES, 8);
        let swarm = Swarm::start(config(dir.path())).await.unwrap();
        let torrent = swarm.add(meta.clone(), Vec::new()).unwrap();
        for (piece, data) in (0..).zip(PIECES) {
            torrent.shared.store.write_piece(piece, data).unwrap();
        }
        let cache = torrent.shared.store.cache_file().upgrade().unwrap();
        let shared = Arc::clone(&torrent.shared);
        let (locked, ready) = tokio::sync::oneshot::channel();
        let (release, released) = std::sync::mpsc::channel();
        let holder = std::thread::spawn(move || {
            shared.store.while_index_locked(|| {
                locked.send(()).unwrap();
                // Also releases the lock if shutdown regresses to blocking the test runtime.
                let _ = released.recv_timeout(Duration::from_secs(2));
            });
        });
        ready.await.unwrap();

        // A single-thread runtime cannot service this deadline if shutdown flushes inline.
        // Drop the actual shutdown future on timeout, just as the app does on exit.
        let result = tokio::time::timeout(Duration::from_millis(100), swarm.shutdown_async()).await;
        if result.is_err() {
            wait_for("the flush owns its cache", Duration::from_secs(5), || {
                Arc::strong_count(&cache) > 2
            })
            .await;
        }
        let _ = release.send(());
        holder.join().unwrap();
        assert!(
            result.is_err(),
            "the blocked flush must not block the deadline"
        );
        assert!(
            swarm.cancel.is_cancelled(),
            "torrent work stops before flushing"
        );
        wait_for("the flush finishes", Duration::from_secs(5), || {
            Arc::strong_count(&cache) == 2
        })
        .await;

        let reopened = PieceStore::open(&dir.path().join(meta.info_hash_hex()), &meta).unwrap();
        assert_eq!(
            reopened.stored(),
            vec![
                (0, crate::store::Check::Trusted),
                (1, crate::store::Check::Trusted),
                (2, crate::store::Check::Trusted)
            ],
            "the detached flush still leaves a usable clean marker"
        );
    }

    fn config(dir: &std::path::Path) -> SwarmConfig {
        let mut cfg = SwarmConfig::new(dir.to_path_buf());
        cfg.enable_trackers = false;
        cfg.enable_web_seeds = false;
        cfg
    }

    async fn wait_for(what: &str, within: Duration, mut done: impl FnMut() -> bool) {
        tokio::time::timeout(within, async {
            while !done() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("timed out: {what}"));
    }

    /// A session that stored `pieces` of `meta` in `dir`, then ended: cleanly (the app quit) or
    /// not (a crash: the engine was not shut down).
    async fn earlier_session(
        dir: &std::path::Path,
        meta: &Metainfo,
        pieces: &[&[u8]],
        clean: bool,
    ) {
        let swarm = Swarm::start(config(dir)).await.unwrap();
        let torrent = swarm.add(meta.clone(), Vec::new()).unwrap();
        for (piece, data) in (0..).zip(pieces) {
            torrent.shared.store.write_piece(piece, data).unwrap();
        }
        if clean {
            swarm.shutdown();
        } else {
            torrent.stop();
        }
        let gone = Arc::downgrade(&torrent.shared);
        drop(torrent);
        wait_for("the torrent closes", Duration::from_secs(5), || {
            gone.strong_count() == 0
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn pieces_stored_before_a_clean_restart_are_used_at_once() {
        let dir = tempfile::tempdir().unwrap();
        let meta = crate::store::tests::meta(&PIECES, 8);
        earlier_session(dir.path(), &meta, &PIECES, true).await;

        let swarm = Swarm::start(config(dir.path())).await.unwrap();
        let torrent = swarm.add(meta, Vec::new()).unwrap();
        assert_eq!(
            torrent.have_pieces(),
            [true; 3],
            "trusted, not checked first"
        );
        let mut all = Vec::new();
        tokio::io::AsyncReadExt::read_to_end(&mut torrent.reader(0).unwrap(), &mut all)
            .await
            .unwrap();
        assert_eq!(all, PIECES.concat());
        assert_eq!(torrent.stats().downloaded_bytes, 0, "nothing downloaded");
        assert_eq!(
            swarm.rechecker.peak(),
            0,
            "nothing checked in the background"
        );
        swarm.shutdown();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn pieces_stored_before_a_crash_are_checked_then_used() {
        let dir = tempfile::tempdir().unwrap();
        let meta = crate::store::tests::meta(&PIECES, 8);
        earlier_session(dir.path(), &meta, &PIECES, false).await;

        let swarm = Swarm::start(config(dir.path())).await.unwrap();
        let torrent = swarm.add(meta, Vec::new()).unwrap();
        wait_for("the stored pieces verify", Duration::from_secs(10), || {
            torrent.have_pieces().iter().all(|h| *h)
        })
        .await;
        let mut all = Vec::new();
        tokio::io::AsyncReadExt::read_to_end(&mut torrent.reader(0).unwrap(), &mut all)
            .await
            .unwrap();
        assert_eq!(all, PIECES.concat());
        assert_eq!(torrent.stats().downloaded_bytes, 0, "nothing downloaded");
        swarm.shutdown();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_trusted_piece_that_fails_when_read_is_downloaded_again() {
        let dir = tempfile::tempdir().unwrap();
        let meta = crate::store::tests::meta(&PIECES, 8);
        earlier_session(dir.path(), &meta, &PIECES, true).await;
        // Damaged in place, keeping the cache file's length and modification time.
        let data = dir.path().join(meta.info_hash_hex()).join("pieces.dat");
        let mut bytes = std::fs::read(&data).unwrap();
        let modified = std::fs::metadata(&data).unwrap().modified().unwrap();
        bytes[9] ^= 0xff;
        std::fs::write(&data, &bytes).unwrap();
        std::fs::File::options()
            .write(true)
            .open(&data)
            .unwrap()
            .set_modified(modified)
            .unwrap();

        let swarm = Swarm::start(config(dir.path())).await.unwrap();
        let torrent = swarm.add(meta, Vec::new()).unwrap();
        assert_eq!(torrent.have_pieces(), [true; 3]);
        let mut buf = [0u8; 8];
        let error = torrent.shared.read_piece(1, 0, &mut buf).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert_eq!(
            torrent.have_pieces(),
            [true, false, true],
            "to download again"
        );
        torrent.shared.read_piece(0, 0, &mut buf).unwrap();
        assert_eq!(&buf, PIECES[0]);
        swarm.shutdown();
    }

    /// `n` pieces of 8 bytes, different for each `seed`.
    fn numbered(seed: u8, n: u8) -> Vec<Vec<u8>> {
        (0..n).map(|i| vec![seed, i, 1, 2, 3, 4, 5, 6]).collect()
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn checks_after_a_crash_share_two_threads_across_the_engine() {
        let dir = tempfile::tempdir().unwrap();
        let mut metas = Vec::new();
        for seed in 0..8 {
            let pieces = numbered(seed, 16);
            let pieces: Vec<&[u8]> = pieces.iter().map(Vec::as_slice).collect();
            let meta = crate::store::tests::meta(&pieces, 8);
            earlier_session(dir.path(), &meta, &pieces, false).await;
            metas.push(meta);
        }

        let swarm = Swarm::start(config(dir.path())).await.unwrap();
        let torrents: Vec<Torrent> = metas
            .into_iter()
            .map(|meta| swarm.add(meta, Vec::new()).unwrap())
            .collect();
        wait_for("every piece verifies", Duration::from_secs(10), || {
            torrents.iter().all(|t| t.have_pieces().iter().all(|h| *h))
        })
        .await;
        let peak = swarm.rechecker.peak();
        assert!((1..=2).contains(&peak), "{peak} pieces checked at once");
        swarm.shutdown();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_playhead_and_the_torrent_being_read_are_checked_first() {
        let dir = tempfile::tempdir().unwrap();
        let mut metas = Vec::new();
        for seed in 0..2 {
            let pieces = numbered(seed, 8);
            let pieces: Vec<&[u8]> = pieces.iter().map(Vec::as_slice).collect();
            let meta = crate::store::tests::meta(&pieces, 8);
            earlier_session(dir.path(), &meta, &pieces, false).await;
            metas.push(meta);
        }

        let swarm = Swarm::start(config(dir.path())).await.unwrap();
        swarm.rechecker.set_limit(0);
        let mut added = metas
            .into_iter()
            .map(|meta| swarm.add(meta, Vec::new()).unwrap());
        let (first, second) = (added.next().unwrap(), added.next().unwrap());
        // The torrent added second is played from piece 5.
        second.set_playhead(0, 5 * 8, None).unwrap();
        wait_for(
            "the driver plans the playhead",
            Duration::from_secs(5),
            || second.shared.sched().urgent_pieces().first() == Some(&5),
        )
        .await;

        let mut order = Vec::new();
        while let Some((shared, piece)) = swarm.rechecker.take_next() {
            let which = if Arc::ptr_eq(&shared, &second.shared) {
                2
            } else {
                1
            };
            order.push((which, piece));
        }
        assert_eq!(order.first(), Some(&(2, 5)), "{order:?}");
        let seconds = order.iter().take_while(|(which, _)| *which == 2).count();
        assert_eq!(
            seconds, 8,
            "the torrent being read before the other: {order:?}"
        );
        assert_eq!(order.len(), 16);
        assert!(!first.have_pieces()[0]);
        swarm.shutdown();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_corrupted_stored_piece_is_not_trusted_and_is_requested_again() {
        let dir = tempfile::tempdir().unwrap();
        let meta = crate::store::tests::meta(&PIECES, 8);
        earlier_session(dir.path(), &meta, &PIECES, false).await;
        let data = dir.path().join(meta.info_hash_hex()).join("pieces.dat");
        let mut bytes = std::fs::read(&data).unwrap();
        bytes[9] ^= 0xff;
        std::fs::write(&data, &bytes).unwrap();

        let swarm = Swarm::start(config(dir.path())).await.unwrap();
        let torrent = swarm.add(meta, Vec::new()).unwrap();
        wait_for(
            "the stored pieces are checked",
            Duration::from_secs(10),
            || torrent.have_pieces() == [true, false, true] && torrent.shared.sched().is_idle(),
        )
        .await;
        let mut buf = [0u8; 8];
        assert!(torrent.shared.store.read(1, 0, &mut buf).is_err());

        // A peer with every piece is asked for piece 1 only.
        torrent.select_file(0).unwrap();
        let mut sched = torrent.shared.sched();
        sched.add_peer(99);
        assert!(sched.on_bitfield(99, &[0xe0]));
        sched.on_unchoke(99);
        sched.tick(Instant::now(), true);
        let requests = sched.fill(99, Instant::now());
        drop(sched);
        assert!(!requests.is_empty());
        assert!(requests.iter().all(|r| r.piece == 1), "{requests:?}");
        swarm.shutdown();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_driver_ticks_slowly_while_there_is_nothing_to_download() {
        let dir = tempfile::tempdir().unwrap();
        let meta = crate::store::tests::meta(&PIECES, 8);
        let swarm = Swarm::start(config(dir.path())).await.unwrap();
        let torrent = swarm.add(meta, Vec::new()).unwrap();
        let idle = || torrent.shared.idle.load(Ordering::Relaxed);
        // Less than IDLE_TICK: these changes take effect at once, not at the next slow tick.
        let soon = IDLE_TICK / 2;

        wait_for("idle with nothing selected", soon, idle).await;
        torrent.select_file(0).unwrap();
        wait_for("busy once a file is selected", soon, || !idle()).await;
        torrent.set_paused(true);
        wait_for("idle while paused", soon, idle).await;
        torrent.set_paused(false);
        wait_for("busy once resumed", soon, || !idle()).await;
        torrent.deselect_file(0).unwrap();
        wait_for("idle once deselected", soon, idle).await;
        torrent.set_playhead(0, 0, None).unwrap();
        wait_for("busy with a playhead", soon, || !idle()).await;

        // Complete: idle even while playing.
        for (piece, data) in (0..).zip(PIECES) {
            torrent.shared.store.write_piece(piece, data).unwrap();
            torrent
                .shared
                .sched()
                .on_verified(piece, PieceCheck::Valid(None));
        }
        torrent.shared.request_replan(false);
        wait_for("idle once complete", soon, idle).await;
        torrent.set_playhead(0, 20, None).unwrap();
        tokio::time::sleep(TICK * 3).await;
        assert!(idle(), "nothing to download for the playhead");
        swarm.shutdown();
    }
}
