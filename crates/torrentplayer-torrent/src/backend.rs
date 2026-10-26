// SPDX-License-Identifier: MIT OR Apache-2.0

//! One streaming API over both torrent engines.
//!
//! [`StreamEngine`] routes each torrent to an engine:
//!
//! - **torrentplayer-swarm**, TorrentPlayer's own streaming-first engine (deadline scheduling, short adaptive
//!   queues, cancel on seek), for every source by default. For a magnet link, librqbit is used
//!   only as a metadata resolver and hands the metadata and peers it found to torrentplayer-swarm.
//! - **librqbit** as an explicit compatibility choice and the magnet metadata fallback.
//!
//! [`StreamEngine::preload`] fetches a torrent's metadata (through librqbit for magnet links,
//! metadata only) and downloads the start and end of its best video file into torrentplayer-swarm, so
//! a later add of the same torrent is served from what is already there (see [`crate::preload`]).
//! [`StreamEngine::prefetch_metadata`] fetches only the metadata, for search results the user
//! may play; an add or preload of the same torrent then skips asking peers for it (see
//! [`crate::metadata`]).
//!
//! `TORRENTPLAYER_ENGINE` (`auto`, `swarm`, or `rqbit`) overrides the choice; `auto` is the default.
//! Both paths apply the same safety checks: scheme allow-list, `.torrent` size limit, file
//! path validation, and a file-count limit.

use std::collections::HashMap;
use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::num::NonZeroU32;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError, Weak};
use std::time::Duration;

use bytes::Bytes;
use futures_util::future::BoxFuture;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;
use torrentplayer_net::{ExternalListener, IncomingConfig};
use torrentplayer_prioritize::{FileSpan, PieceGeometry, buffered_bytes_from};
use torrentplayer_stream::{MediaReader, MediaSource};
use torrentplayer_swarm::{Metainfo, Swarm, SwarmConfig, SwarmError, Torrent};
use tracing::{info, warn};

use crate::engine::http_for;
use crate::engine::{
    Engine, EngineConfig, EngineError, FileEntry, MAX_FILES, TorrentId, TorrentInfo, TorrentState,
    TorrentStats, WeakEngine,
};
use crate::media::{FileKind, mime_type};
use crate::metadata::{LinkTarget, MetadataCache};
use crate::paths::validate_components;
use crate::playback::{PlaybackSession, PlaybackStatus, PlayheadPosition};
use crate::preload::{
    FileReadiness, PRELOAD_TIMEOUT, PreloadPhase, PreloadStatus, byte_cap, file_readiness,
    magnet_info_hash, preload_target, preloaded_bytes, targets,
};
use crate::source::{SourceError, TorrentSource, fetch_torrent_file, magnet_link};
use crate::thumbnail::{ImageType, choose_image, image_type};

mod small_file;

/// Name of the environment variable that selects the engine.
pub const ENGINE_ENV_VAR: &str = "TORRENTPLAYER_ENGINE";

/// How long librqbit may go unused (no torrents, nothing in flight) before its session, with
/// its DHT node, trackers, and listener, is stopped. The next torrent that needs it starts a
/// new one.
const RQBIT_IDLE_STOP: Duration = Duration::from_secs(60);

/// Which engine to use for new torrents.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum EngineChoice {
    /// torrentplayer-swarm for every source. For magnets, librqbit resolves metadata only.
    #[default]
    Auto,
    /// Always torrentplayer-swarm. For magnet links, librqbit fetches the metadata only.
    Swarm,
    /// Always librqbit.
    Rqbit,
}

impl EngineChoice {
    /// Parses `auto`, `swarm` (or `torrentplayer`), and `rqbit` (or `librqbit`), case-insensitively.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "" | "auto" => Some(Self::Auto),
            "swarm" | "torrentplayer" | "torrentplayer-swarm" => Some(Self::Swarm),
            "rqbit" | "librqbit" => Some(Self::Rqbit),
            _ => None,
        }
    }

    /// Reads [`ENGINE_ENV_VAR`]. Unset or unrecognized values mean [`EngineChoice::Auto`].
    #[must_use]
    pub fn from_env() -> Self {
        match std::env::var(ENGINE_ENV_VAR) {
            Ok(value) => Self::parse(&value).unwrap_or_else(|| {
                tracing::warn!(%value, "unrecognized {ENGINE_ENV_VAR}; using auto");
                Self::Auto
            }),
            Err(_) => Self::Auto,
        }
    }

    /// The engine a source goes to.
    #[must_use]
    pub fn backend_for(self, _source: &TorrentSource) -> BackendKind {
        match self {
            Self::Rqbit => BackendKind::Rqbit,
            Self::Auto | Self::Swarm => BackendKind::Swarm,
        }
    }
}

/// The engine serving a torrent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BackendKind {
    /// TorrentPlayer's own engine.
    Swarm,
    /// librqbit.
    Rqbit,
}

impl BackendKind {
    /// Name used in logs and the debug overlay.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Swarm => "torrentplayer-swarm",
            Self::Rqbit => "librqbit",
        }
    }
}

impl std::fmt::Display for BackendKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name())
    }
}

/// Settings for [`StreamEngine`].
#[derive(Debug, Clone)]
pub struct StreamEngineConfig {
    /// Root folder for downloads; each engine gets a sub-folder.
    pub download_dir: PathBuf,
    /// Which engine new torrents go to.
    pub choice: EngineChoice,
    /// Find peers through trackers (both engines) and the DHT (librqbit). Off in tests that
    /// use a local seeder.
    pub public_discovery: bool,
    /// Download limit for everything together (both engines, bytes per second); `None` is
    /// unlimited. Change it later with [`StreamEngine::set_rate_limits`].
    pub download_limit_bps: Option<NonZeroU32>,
    /// Upload limit for everything together (both engines, bytes per second); `None` is
    /// unlimited. Change it later with [`StreamEngine::set_rate_limits`].
    pub upload_limit_bps: Option<NonZeroU32>,
    /// Maximum connected peers per torrent.
    pub peer_limit: usize,
    /// Folder where torrentplayer-swarm keeps its DHT routing tables between runs (direct mode
    /// only), so magnet links find peers sooner after a restart. `None` keeps nothing.
    /// librqbit keeps none: restored from a saved table, it fetched magnet metadata more
    /// slowly than from a fresh start (see docs/performance.md).
    pub dht_state_dir: Option<PathBuf>,
    /// Download `.torrent` links (and follow their redirects) to the local network too, and
    /// announce to HTTP trackers there. Off in the app: links and magnet links come from
    /// untrusted search results and sites, and must not reach the user's router, other devices,
    /// or local services. For tests with a local server or tracker.
    pub allow_local_links: bool,
}

impl StreamEngineConfig {
    /// Defaults for the desktop app: engine from [`ENGINE_ENV_VAR`], public peer discovery on.
    #[must_use]
    pub fn new(download_dir: PathBuf) -> Self {
        Self {
            download_dir,
            choice: EngineChoice::from_env(),
            public_discovery: true,
            download_limit_bps: None,
            upload_limit_bps: None,
            peer_limit: 40,
            dht_state_dir: None,
            allow_local_links: false,
        }
    }
}

/// What following a link gave.
enum Opened {
    /// The `.torrent` file it serves.
    File(Bytes),
    /// The magnet link it redirects to.
    Magnet(String),
}

enum Backed {
    Rqbit { inner: TorrentId, info: TorrentInfo },
    Swarm { torrent: Torrent, info: TorrentInfo },
}

impl Backed {
    fn info(&self) -> &TorrentInfo {
        match self {
            Self::Rqbit { info, .. } | Self::Swarm { info, .. } => info,
        }
    }
}

/// Both engines behind one API. Torrent ids are this facade's own and never collide between
/// engines.
pub struct StreamEngine {
    config: StreamEngineConfig,
    swarm: Option<Swarm>,
    /// librqbit, once started. Replaced when the network mode turns protection on or off, and
    /// stopped once unused for a while (see `stop_rqbit_when_idle`).
    rqbit: Arc<tokio::sync::Mutex<Option<Engine>>>,
    /// How long librqbit may go unused before it stops: [`RQBIT_IDLE_STOP`], shorter in tests.
    rqbit_idle_stop: Duration,
    /// The download and upload limits now (see [`StreamEngine::set_rate_limits`]).
    limits: Mutex<(Option<NonZeroU32>, Option<NonZeroU32>)>,
    /// TorrentPlayer's network manager, shared by both engines; `None` connects directly.
    network: Option<torrentplayer_net::Network>,
    http: reqwest::Client,
    torrents: Arc<Mutex<HashMap<TorrentId, Backed>>>,
    next_id: AtomicUsize,
    /// The current preload. Lock order: this before `torrents`.
    preload: Mutex<Option<PreloadSlot>>,
    /// torrentplayer-swarm's read-ahead time and tail prefetch, which size the preload window.
    swarm_window: (Duration, u64),
    /// Metadata fetched ahead of an add (see [`StreamEngine::prefetch_metadata`]).
    metadata: MetadataCache,
    /// A second torrentplayer-swarm, started when first needed, that downloads single pictures for
    /// thumbnails (see [`StreamEngine::thumbnail`]), apart from the torrents being played.
    thumbnails: tokio::sync::OnceCell<Swarm>,
}

/// A picture from inside a torrent, checked to be JPEG, PNG, or WebP.
#[derive(Clone)]
pub struct Thumbnail {
    /// The torrent's info hash.
    pub info_hash: String,
    /// The picture's format, from its bytes.
    pub kind: ImageType,
    /// The picture.
    pub bytes: Bytes,
}

impl std::fmt::Debug for Thumbnail {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Thumbnail")
            .field("info_hash", &self.info_hash)
            .field("kind", &self.kind)
            .field("bytes", &self.bytes.len())
            .finish()
    }
}

/// Download rate of the thumbnail engine, so pictures never compete with a video.
const THUMBNAIL_RATE: u32 = 2 * 1024 * 1024;

/// The thumbnail swarm's limit: [`THUMBNAIL_RATE`], or the overall limit if lower.
fn thumbnail_rate(overall: Option<NonZeroU32>) -> Option<NonZeroU32> {
    let own = NonZeroU32::new(THUMBNAIL_RATE);
    match (own, overall) {
        (Some(own), Some(overall)) => Some(own.min(overall)),
        (own, None) => own,
        (None, overall) => overall,
    }
}

/// A thumbnail's torrent in the thumbnail engine: stopped, and its cache deleted, when the
/// download ends or is dropped.
struct ThumbnailTorrent {
    torrent: Torrent,
    dir: PathBuf,
}

impl Drop for ThumbnailTorrent {
    fn drop(&mut self) {
        self.torrent.stop();
        let dir = std::mem::take(&mut self.dir);
        // The torrent's tasks close its cache files as they stop; delete them a moment later.
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                tokio::time::sleep(Duration::from_secs(2)).await;
                let _ = tokio::fs::remove_dir_all(&dir).await;
            });
        }
    }
}

/// The current preload: its status (watched by adds waiting for its metadata), a token that
/// stops it, and its torrent once added to torrentplayer-swarm.
struct PreloadSlot {
    status: Arc<watch::Sender<PreloadStatus>>,
    cancel: CancellationToken,
    torrent: Option<TorrentId>,
}

/// How often a preload checks its progress.
const PRELOAD_POLL: Duration = Duration::from_millis(250);
/// How long an add waits for a preload that is still fetching the same torrent's metadata.
const PRELOAD_METADATA_WAIT: Duration = Duration::from_secs(130);
/// The thumbnail engine's folder inside torrentplayer-swarm's (see `StreamEngine::thumbnail_dir`).
const THUMBNAIL_DIR: &str = "thumbnails";
/// Suffix of the marker file next to an unplayed preload's cache.
const PRELOAD_MARKER: &str = ".preload";

impl std::fmt::Debug for StreamEngine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StreamEngine")
            .field("config", &self.config)
            .field(
                "rqbit_started",
                &self.rqbit.try_lock().map_or(true, |e| e.is_some()),
            )
            .finish_non_exhaustive()
    }
}

/// Reads a picture file of a torrent in torrentplayer-swarm and checks that it is one.
async fn read_thumbnail(
    torrent: &Torrent,
    index: usize,
    info_hash: String,
) -> Result<Option<Thumbnail>, EngineError> {
    use tokio::io::AsyncReadExt as _;
    let len = torrent
        .meta()
        .files
        .get(index)
        .map(|f| f.len)
        .ok_or(EngineError::UnknownFile)?;
    let len = usize::try_from(len).map_err(|_| EngineError::UnknownFile)?;
    let mut reader = torrent.reader(index).map_err(swarm_err)?;
    let mut bytes = vec![0; len];
    reader
        .read_exact(&mut bytes)
        .await
        .map_err(|e| EngineError::Engine(format!("reading a thumbnail: {e}")))?;
    let Some(kind) = image_type(&bytes) else {
        tracing::debug!("a torrent's picture is not a JPEG, PNG, or WebP image");
        return Ok(None);
    };
    Ok(Some(Thumbnail {
        info_hash,
        kind,
        bytes: bytes.into(),
    }))
}

fn swarm_err(e: SwarmError) -> EngineError {
    match e {
        SwarmError::UnknownFile => EngineError::UnknownFile,
        // May contain local paths: callers log `Engine` errors instead of showing them.
        other => EngineError::Engine(other.to_string()),
    }
}

impl StreamEngine {
    /// Starts the facade. torrentplayer-swarm starts now (unless librqbit is forced); librqbit starts
    /// when the first torrent needs it.
    ///
    /// # Errors
    ///
    /// Returns an error if torrentplayer-swarm cannot start (for example, the download folder cannot
    /// be created).
    pub async fn start(config: StreamEngineConfig) -> Result<Self, EngineError> {
        Self::start_with_network(config, None).await
    }

    /// Like [`StreamEngine::start`], with all traffic (peers, trackers, `.torrent` downloads,
    /// both engines) going through `network`: its mode, kill switch, and UDP rules.
    ///
    /// # Errors
    ///
    /// As for [`StreamEngine::start`].
    pub async fn start_with_network(
        config: StreamEngineConfig,
        network: Option<torrentplayer_net::Network>,
    ) -> Result<Self, EngineError> {
        let mut swarm_config = SwarmConfig::new(config.download_dir.join("torrentplayer-swarm"));
        swarm_config.enable_trackers = config.public_discovery;
        // Magnet links come here too, and their trackers are often dead: the DHT finds their
        // peers (and runs only while the network policy allows UDP, as for librqbit).
        swarm_config.enable_dht = config.public_discovery;
        swarm_config.dht_state_file = config.dht_state_dir.as_ref().map(|d| d.join("swarm.json"));
        swarm_config.network.clone_from(&network);
        swarm_config.allow_local_trackers = config.allow_local_links;
        // The main engine serves peers that connect to us, when that is turned on (see
        // `StreamEngine::set_incoming`); the thumbnail engine never does.
        swarm_config.accept_incoming = network.is_some();
        swarm_config.download_limit_bps = config.download_limit_bps;
        swarm_config.upload_limit_bps = config.upload_limit_bps;
        swarm_config.peer_limit = config.peer_limit;
        let swarm_window = (
            swarm_config.scheduler.read_ahead,
            swarm_config.scheduler.tail_bytes,
        );
        let swarm = if config.choice == EngineChoice::Rqbit {
            None
        } else {
            // Preloads left over from a previous run were never played; their caches are
            // useless now (piece indexes are not persisted).
            remove_stale_preloads(&swarm_config.download_dir);
            Some(Swarm::start(swarm_config).await.map_err(swarm_err)?)
        };
        let http = http_for(
            concat!("TorrentPlayer/", env!("CARGO_PKG_VERSION")),
            network.as_ref(),
            config.allow_local_links,
        )?;
        info!(choice = ?config.choice, "stream engine started");
        let engine = Self {
            limits: Mutex::new((config.download_limit_bps, config.upload_limit_bps)),
            config,
            swarm,
            rqbit: Arc::new(tokio::sync::Mutex::new(None)),
            rqbit_idle_stop: RQBIT_IDLE_STOP,
            network,
            http,
            torrents: Arc::new(Mutex::new(HashMap::new())),
            next_id: AtomicUsize::new(1),
            preload: Mutex::new(None),
            swarm_window,
            metadata: MetadataCache::default(),
            thumbnails: tokio::sync::OnceCell::new(),
        };
        engine.report_rqbit_listener(None);
        Ok(engine)
    }

    /// The download and upload limits in force.
    fn limits(&self) -> (Option<NonZeroU32>, Option<NonZeroU32>) {
        *self.limits.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Changes the download and upload limits for everything together (both engines, playback
    /// and background alike), bytes per second; `None` removes one.
    /// Takes effect at once, and carries over when librqbit restarts for a network change.
    pub async fn set_rate_limits(&self, download: Option<NonZeroU32>, upload: Option<NonZeroU32>) {
        {
            let mut limits = self.limits.lock().unwrap_or_else(PoisonError::into_inner);
            if *limits == (download, upload) {
                return;
            }
            *limits = (download, upload);
        }
        if let Some(swarm) = &self.swarm {
            swarm.set_download_limit(download);
            swarm.set_upload_limit(upload);
        }
        if let Some(swarm) = self.thumbnails.get() {
            swarm.set_download_limit(thumbnail_rate(download));
        }
        if let Some(engine) = self.rqbit.lock().await.as_ref() {
            engine.set_rate_limits(download, upload);
        }
        info!(
            download = download.map(NonZeroU32::get),
            upload = upload.map(NonZeroU32::get),
            "rate limits changed"
        );
    }

    /// The configured engine choice.
    #[must_use]
    pub fn choice(&self) -> EngineChoice {
        self.config.choice
    }

    async fn rqbit(&self) -> Result<Engine, EngineError> {
        let mut slot = self.rqbit.lock().await;
        let protected = self.network.as_ref().is_some_and(|n| n.policy().protected);
        if let Some(engine) = slot.as_ref() {
            if engine.protected() == protected {
                return Ok(engine.clone());
            }
            // Started for the other kind of network mode: never reuse it.
            self.stop_rqbit(&mut slot).await;
        }
        let mut c = EngineConfig::new(self.config.download_dir.join("librqbit"));
        c.enable_dht = self.config.public_discovery;
        // No saved DHT state (`dht_state_file`): librqbit starts each lookup from the 8 closest
        // contacts it knows and, with 8 of them, adds no others for a minute. Contacts restored
        // from a saved table have not answered yet, so magnet metadata took two to three times
        // as long as from an empty table, which waits for the bootstrap servers' responsive nodes.
        c.enable_trackers = self.config.public_discovery;
        c.network.clone_from(&self.network);
        c.allow_local_links = self.config.allow_local_links;
        c.peer_limit = Some(self.config.peer_limit);
        (c.download_limit_bps, c.upload_limit_bps) = self.limits();
        let listen = self.rqbit_listen(protected);
        c.listen_addr = listen.map(|(addr, _)| addr);
        c.enable_upnp = listen.is_some_and(|(_, upnp)| upnp);
        let engine = match Engine::start(c.clone()).await {
            Ok(engine) => engine,
            // Most likely the port is taken: run without listening rather than not at all.
            Err(error) if c.listen_addr.is_some() => {
                warn!(%error, "librqbit could not listen; continuing without incoming peers");
                c.listen_addr = None;
                c.enable_upnp = false;
                Engine::start(c).await?
            }
            Err(error) => return Err(error),
        };
        *slot = Some(engine.clone());
        self.report_rqbit_listener(Some(&engine));
        tokio::spawn(stop_rqbit_when_idle(
            Arc::downgrade(&self.rqbit),
            Arc::downgrade(&self.torrents),
            engine.downgrade(),
            self.rqbit_idle_stop,
            // Only reported when librqbit is the engine for every torrent.
            self.network.clone().filter(|_| self.swarm.is_none()),
        ));
        Ok(engine)
    }

    /// Where librqbit should listen for incoming peers, and whether with UPnP. Only when
    /// librqbit is the engine for every torrent (otherwise torrentplayer-swarm takes incoming
    /// peers from the network manager), in direct mode (librqbit's listener cannot be moved into
    /// a tunnel), and with incoming connections turned on.
    fn rqbit_listen(&self, protected: bool) -> Option<(SocketAddr, bool)> {
        if self.swarm.is_some() || protected {
            return None;
        }
        let network = self.network.as_ref()?;
        let port = network.incoming_port()?;
        Some((
            SocketAddr::new(rqbit_listen_ip(network.loopback_only()), port),
            network.incoming_config().upnp,
        ))
    }

    /// Tells the network manager about librqbit's own listener, for the status, when librqbit
    /// is the engine for every torrent.
    fn report_rqbit_listener(&self, engine: Option<&Engine>) {
        let Some(network) = self.network.as_ref().filter(|_| self.swarm.is_none()) else {
            return;
        };
        let report = match engine.map(Engine::listen_addr) {
            None => stopped_rqbit_listener(network),
            Some(_) if network.policy().protected => stopped_rqbit_listener(network),
            Some(Some(addr)) => ExternalListener::Listening(addr),
            Some(None) if self.rqbit_listen(false).is_some() => {
                ExternalListener::Failed("librqbit could not open the port".into())
            }
            Some(None) => ExternalListener::Idle("librqbit is not listening".into()),
        };
        network.set_external_listener(Some(report));
    }

    /// Turns incoming peer connections on or off, or changes their port or UPnP mapping (see
    /// `torrentplayer_net::Network::set_incoming`). torrentplayer-swarm follows by itself;
    /// librqbit, when it is the engine for every torrent, fixes its listener when its session
    /// starts, so a running session whose listener no longer matches is stopped (its torrents
    /// are dropped) and the next torrent starts a new one.
    pub async fn set_incoming(&self, config: IncomingConfig) {
        let Some(network) = &self.network else {
            return;
        };
        let mut slot = self.rqbit.lock().await;
        network.set_incoming(config);
        if let Some(engine) = slot.as_ref()
            && engine.requested_listen() != self.rqbit_listen(engine.protected())
        {
            self.stop_rqbit(&mut slot).await;
        }
        self.report_rqbit_listener(slot.as_ref());
    }

    /// Stops librqbit and forgets its torrents, whose ids would otherwise point into the next
    /// session. Returns once its operations in flight have ended (see `Engine::shutdown`).
    async fn stop_rqbit(&self, slot: &mut Option<Engine>) {
        let Some(engine) = slot.take() else {
            return;
        };
        let dropped = {
            let mut torrents = self.torrents();
            let before = torrents.len();
            torrents.retain(|_, b| !matches!(b, Backed::Rqbit { .. }));
            before - torrents.len()
        };
        info!(dropped, "network settings changed: stopping librqbit");
        engine.shutdown().await;
        self.report_rqbit_listener(None);
    }

    /// Switches the network mode (see [`torrentplayer_net::Network::apply`]). librqbit's DHT, listening
    /// socket, and proxy are fixed when its session starts, so when protection turns on or off,
    /// librqbit is stopped first (its torrents are dropped, and what it was doing for a caller,
    /// such as fetching a magnet link's metadata, ends with an error before the mode changes)
    /// and the next magnet link starts a new session; no librqbit traffic is sent under the
    /// wrong mode. torrentplayer-swarm follows the change by itself.
    ///
    /// # Errors
    ///
    /// Returns an error if the engine has no network manager or the network was shut down.
    pub async fn apply_network(
        &self,
        config: torrentplayer_net::NetworkConfig,
    ) -> Result<torrentplayer_net::NetworkStatus, torrentplayer_net::NetError> {
        let network = self.network.as_ref().ok_or_else(|| {
            torrentplayer_net::NetError::Unsupported("the engine has no network manager".into())
        })?;
        // Held across the switch so no librqbit session can start under the old mode.
        let mut slot = self.rqbit.lock().await;
        if slot
            .as_ref()
            .is_some_and(|e| e.protected() != config.is_protected())
        {
            self.stop_rqbit(&mut slot).await;
        }
        let status = network.apply(config);
        self.report_rqbit_listener(slot.as_ref());
        status
    }

    fn torrents(&self) -> std::sync::MutexGuard<'_, HashMap<TorrentId, Backed>> {
        self.torrents.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Adds a torrent with no files selected. See [`StreamEngine::add_with_peers`].
    ///
    /// # Errors
    ///
    /// As for [`StreamEngine::add_with_peers`].
    pub async fn add(&self, source: TorrentSource) -> Result<TorrentInfo, EngineError> {
        self.add_with_peers(source, Vec::new()).await
    }

    /// Adds a torrent, also connecting to `peers` directly (used with local seeders). Adding a
    /// torrent that is already present returns the existing one. A preloaded torrent is handed
    /// over as it is (in torrentplayer-swarm, with its data), even for a magnet link; if its metadata
    /// is still being fetched, this waits for it instead of fetching it a second time.
    ///
    /// # Errors
    ///
    /// Returns an error if the source is refused, the metadata is invalid or unsafe, or the
    /// engine fails.
    pub async fn add_with_peers(
        &self,
        source: TorrentSource,
        peers: Vec<SocketAddr>,
    ) -> Result<TorrentInfo, EngineError> {
        // A link is followed first, to the `.torrent` file it serves or the magnet link it
        // redirects to (as search sites' download links do), so the checks below see which
        // torrent it is.
        let source = self.resolve_source(source).await?;
        if let Some(info) = self.take_preloaded(&source).await {
            info!(torrent = info.id, "using the preloaded torrent");
            return Ok(info);
        }
        // A magnet link for a torrent already added (perhaps with the other engine before the
        // setting changed) returns it, without fetching the metadata again.
        if let TorrentSource::Magnet(link) = &source
            && let Some(hash) = magnet_info_hash(link)
            && let Some(existing) = self
                .torrents()
                .values()
                .map(Backed::info)
                .find(|i| i.info_hash == hash)
        {
            return Ok(existing.clone());
        }
        let info = match self.config.choice.backend_for(&source) {
            BackendKind::Rqbit => self.add_rqbit(source, peers).await,
            BackendKind::Swarm => self.add_swarm(source, peers).await,
        }?;
        // A `.torrent` file for the preloaded torrent returns the same entry.
        self.claim_preload(&info.info_hash);
        Ok(info)
    }

    async fn add_rqbit(
        &self,
        source: TorrentSource,
        mut peers: Vec<SocketAddr>,
    ) -> Result<TorrentInfo, EngineError> {
        let engine = self.rqbit().await?;
        if let TorrentSource::Magnet(link) = &source {
            peers.extend(self.protected_udp_peers(link).await);
        }
        // A magnet link whose metadata was prefetched (or is being fetched) is added from that
        // metadata, so librqbit does not ask peers for it again; it still finds peers through
        // the DHT and trackers. Protected modes accept magnet links only.
        let hash = match &source {
            TorrentSource::Magnet(link) if !engine.protected() => magnet_info_hash(link),
            _ => None,
        };
        let _fetching = match &hash {
            Some(hash) => Some(self.metadata.lock(hash).await),
            None => None,
        };
        let source = match hash.as_deref().and_then(|h| self.metadata.get(h)) {
            Some(cached) => {
                peers.extend(cached.peers);
                TorrentSource::File(cached.bytes)
            }
            None => source,
        };
        let mut info = engine.add_with_peers(source, peers).await?;
        let inner = info.id;
        let mut torrents = self.torrents();
        if let Some(existing) = torrents.values().find_map(|b| match b {
            Backed::Rqbit { inner: i, info } if *i == inner => Some(info.clone()),
            _ => None,
        }) {
            return Ok(existing);
        }
        info.id = self.next_id.fetch_add(1, Ordering::Relaxed);
        torrents.insert(
            info.id,
            Backed::Rqbit {
                inner,
                info: info.clone(),
            },
        );
        Ok(info)
    }

    async fn add_swarm(
        &self,
        source: TorrentSource,
        peers: Vec<SocketAddr>,
    ) -> Result<TorrentInfo, EngineError> {
        let swarm = self.swarm.as_ref().ok_or(EngineError::Unsupported(
            "TorrentPlayer's engine is disabled by TORRENTPLAYER_ENGINE.",
        ))?;
        let (bytes, peers) = self.torrent_bytes(source, peers).await?;
        let meta =
            Metainfo::from_bytes(&bytes).map_err(|e| EngineError::InvalidTorrent(e.to_string()))?;
        validate_swarm_metadata(&meta)?;
        let info_hash = meta.info_hash_hex();

        // Re-adding would reset the piece cache, so return the existing torrent instead.
        let mut torrents = self.torrents();
        if let Some(existing) = torrents
            .values()
            .map(Backed::info)
            .find(|i| i.info_hash == info_hash)
        {
            return Ok(existing.clone());
        }
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let info = swarm_info(id, &meta);
        let torrent = swarm.add(meta, peers).map_err(swarm_err)?;
        // The facade only serves pieces while the torrent is actively being watched,
        // downloaded, or retained by the user's post-playback seeding policy.
        torrent.set_upload_enabled(false);
        torrents.insert(
            id,
            Backed::Swarm {
                torrent,
                info: info.clone(),
            },
        );
        Ok(info)
    }

    /// The `.torrent` file of a source, and the peers to connect to. For a magnet link, the
    /// prefetched metadata if there is some; otherwise librqbit fetches it from peers (metadata
    /// only, nothing is downloaded or shared), and it is kept for next time. The peers seen are
    /// added to `peers`.
    async fn torrent_bytes(
        &self,
        source: TorrentSource,
        mut peers: Vec<SocketAddr>,
    ) -> Result<(Bytes, Vec<SocketAddr>), EngineError> {
        let link = match source {
            TorrentSource::File(bytes) => return Ok((bytes, peers)),
            TorrentSource::Url(url) => match self.open_link(&url).await? {
                Opened::File(bytes) => return Ok((bytes, peers)),
                Opened::Magnet(link) => link,
            },
            TorrentSource::Magnet(link) => link,
        };
        let hash = magnet_info_hash(&link);
        // Waits for a prefetch of the same torrent rather than fetching it twice.
        let _fetching = match &hash {
            Some(hash) => Some(self.metadata.lock(hash).await),
            None => None,
        };
        if let Some(cached) = hash.as_deref().and_then(|h| self.metadata.get(h)) {
            peers.extend(cached.peers);
            return Ok((cached.bytes, peers));
        }
        peers.extend(self.protected_udp_peers(&link).await);
        let resolved = self
            .rqbit()
            .await?
            .resolve(TorrentSource::Magnet(link), peers.clone())
            .await?;
        let resolved = resolved.ok_or(EngineError::Unsupported(
            "This torrent has already been added.",
        ))?;
        peers.extend(resolved.seen_peers.iter().copied());
        if let Some(hash) = hash {
            self.metadata
                .put(hash, resolved.torrent_bytes.clone(), resolved.seen_peers);
        }
        Ok((resolved.torrent_bytes, peers))
    }

    /// Peers from scoped DHT and UDP trackers through an interface, WireGuard, or SOCKS5 relay
    /// (see [`protected_magnet_peers`]).
    async fn protected_udp_peers(&self, link: &str) -> Vec<SocketAddr> {
        let Some(network) = self.network.as_ref() else {
            return Vec::new();
        };
        protected_magnet_peers(
            network,
            link,
            self.config.public_discovery,
            self.config.allow_local_links,
        )
        .await
    }

    /// Follows a link: downloads the `.torrent` file it serves, or stops at the magnet link it
    /// redirects to. What each link gave is remembered (see `crate::metadata`), so following
    /// it again sends no request; concurrent calls for one link wait for each other. Links are
    /// never logged: search sites put tokens in them.
    async fn open_link(&self, url: &url::Url) -> Result<Opened, EngineError> {
        let key = url.as_str();
        let _fetching = self.metadata.lock(&format!("link {key}")).await;
        match self.metadata.link(key) {
            Some(LinkTarget::Magnet(link)) => return Ok(Opened::Magnet(link)),
            Some(LinkTarget::Torrent(hash)) => {
                if let Some(cached) = self.metadata.get(&hash) {
                    return Ok(Opened::File(cached.bytes));
                }
            }
            None => {}
        }
        match fetch_torrent_file(&self.http, url, self.config.allow_local_links).await {
            Ok(bytes) => {
                // Checked when it is used; the hash is only needed to remember it.
                if let Ok(meta) = Metainfo::from_bytes(&bytes) {
                    let hash = meta.info_hash_hex();
                    self.metadata.put(hash.clone(), bytes.clone(), Vec::new());
                    self.metadata
                        .put_link(key.to_owned(), LinkTarget::Torrent(hash));
                }
                Ok(Opened::File(bytes))
            }
            Err(SourceError::MagnetRedirect(link)) => {
                self.metadata
                    .put_link(key.to_owned(), LinkTarget::Magnet(link.clone()));
                Ok(Opened::Magnet(link))
            }
            Err(error) => Err(error.into()),
        }
    }

    /// Fetches a torrent's metadata ahead of an add: see
    /// [`StreamEngine::prefetch_metadata_with_peers`].
    ///
    /// # Errors
    ///
    /// As for [`StreamEngine::prefetch_metadata_with_peers`].
    pub async fn prefetch_metadata(&self, source: TorrentSource) -> Result<String, EngineError> {
        self.prefetch_metadata_with_peers(source, Vec::new()).await
    }

    /// Fetches a torrent's metadata ahead of an add, so that adding or preloading it later
    /// starts without asking peers for it. For a magnet link librqbit fetches the metadata only
    /// (no files are created, nothing is downloaded or uploaded); for a link the `.torrent`
    /// file is downloaded. The metadata gets the same checks as an add (file paths, file count)
    /// and is kept, with the peers seen, in a small cache (64 torrents, least recently used
    /// evicted first). A torrent
    /// that is already added or cached returns at once. Traffic follows the network mode, like
    /// an add. Returns the info hash.
    ///
    /// # Errors
    ///
    /// Returns an error if the source is refused, the metadata cannot be fetched (dropping
    /// the future stops waiting for it), or it is invalid or unsafe.
    pub async fn prefetch_metadata_with_peers(
        &self,
        source: TorrentSource,
        peers: Vec<SocketAddr>,
    ) -> Result<String, EngineError> {
        if let TorrentSource::Magnet(link) = &source
            && let Some(hash) = magnet_info_hash(link)
            && (self.metadata.contains(&hash)
                || self.torrents().values().any(|b| b.info().info_hash == hash))
        {
            return Ok(hash);
        }
        let (bytes, peers) = self.torrent_bytes(source, peers).await?;
        let meta =
            Metainfo::from_bytes(&bytes).map_err(|e| EngineError::InvalidTorrent(e.to_string()))?;
        validate_swarm_metadata(&meta)?;
        let hash = meta.info_hash_hex();
        self.metadata.put(hash.clone(), bytes, peers);
        Ok(hash)
    }

    /// A picture from inside a torrent to show as its thumbnail, if it has a suitable one (see
    /// `crate::thumbnail`: a poster, cover, or screenshot; JPEG, PNG, or WebP only; at most
    /// 2 MiB, and 8 MiB of pieces). Its metadata must already be known (prefetched with
    /// [`StreamEngine::prefetch_metadata`], or kept from an add); otherwise this returns
    /// `None` without any traffic.
    ///
    /// Only that file's pieces are downloaded, checked against the torrent's hashes as always,
    /// and the bytes must really be one of those formats. A torrent already in torrentplayer-swarm (a
    /// preload, or one being played) gives the picture from there; any other is fetched by a
    /// separate, rate-limited torrentplayer-swarm from the peers seen while fetching its metadata and
    /// its trackers and web seeds, nothing is uploaded, and its cache is deleted afterwards.
    /// Traffic follows the network mode. Dropping the future stops the download.
    ///
    /// # Errors
    ///
    /// Returns an error if the metadata is invalid or unsafe, or the download fails.
    pub async fn thumbnail(&self, info_hash: &str) -> Result<Option<Thumbnail>, EngineError> {
        let info_hash = info_hash.to_ascii_lowercase();
        // It names a cache folder below; anything but an info hash is no torrent of ours.
        if !is_hex_hash(&info_hash) {
            return Ok(None);
        }
        let running = self.torrents().values().find_map(|b| match b {
            Backed::Swarm { torrent, info } if info.info_hash == info_hash => Some(torrent.clone()),
            _ => None,
        });
        if let Some(torrent) = running {
            let Some(index) = choose_image(torrent.meta()) else {
                return Ok(None);
            };
            return read_thumbnail(&torrent, index, info_hash).await;
        }
        let Some(cached) = self.metadata.get(&info_hash) else {
            return Ok(None);
        };
        let meta = Metainfo::from_bytes(&cached.bytes)
            .map_err(|e| EngineError::InvalidTorrent(e.to_string()))?;
        validate_swarm_metadata(&meta)?;
        let Some(index) = choose_image(&meta) else {
            return Ok(None);
        };
        let Some(swarm) = self.thumbnail_swarm().await? else {
            return Ok(None);
        };
        // One download per torrent at a time: they would share a cache folder.
        let _fetching = self.metadata.lock(&format!("thumbnail {info_hash}")).await;
        let dir = self.thumbnail_dir().join(&info_hash);
        // An interrupted earlier download of this torrent may have left its cache behind.
        let _ = tokio::fs::remove_dir_all(&dir).await;
        let torrent = swarm.add(meta, cached.peers).map_err(swarm_err)?;
        torrent.set_upload_enabled(false);
        let fetching = ThumbnailTorrent {
            torrent: torrent.clone(),
            dir,
        };
        let thumbnail = read_thumbnail(&torrent, index, info_hash).await;
        drop(fetching);
        thumbnail
    }

    /// What a torrent's metadata says about it (files, piece size, trackers, and the optional
    /// fields of its `.torrent` file), if its metadata is known: prefetched, kept from an add,
    /// or in torrentplayer-swarm. Sends nothing.
    #[must_use]
    pub fn torrent_details(&self, info_hash: &str) -> Option<crate::details::TorrentDetails> {
        let info_hash = info_hash.to_ascii_lowercase();
        let (meta, extras) = if let Some(cached) = self.metadata.get(&info_hash) {
            let meta = Metainfo::from_bytes(&cached.bytes).ok()?;
            (meta, crate::details::extras(&cached.bytes))
        } else {
            let meta = self.torrents().values().find_map(|b| match b {
                Backed::Swarm { torrent, info } if info.info_hash == info_hash => {
                    Some(torrent.meta().clone())
                }
                _ => None,
            })?;
            (meta, crate::details::Extras::default())
        };
        let udp_trackers = meta.trackers.iter().filter(|t| t.scheme() == "udp").count();
        Some(crate::details::TorrentDetails {
            files: swarm_info(0, &meta).files,
            name: meta.name.clone(),
            piece_len: meta.piece_len,
            total_len: meta.total_len,
            udp_trackers,
            http_trackers: meta.trackers.len() - udp_trackers,
            web_seeds: meta.web_seeds.len(),
            extras,
            info_hash,
        })
    }

    /// The thumbnail engine, started on first use; `None` if torrentplayer-swarm is disabled.
    async fn thumbnail_swarm(&self) -> Result<Option<&Swarm>, EngineError> {
        if self.swarm.is_none() {
            return Ok(None);
        }
        let swarm = self
            .thumbnails
            .get_or_try_init(|| async {
                let dir = self.thumbnail_dir();
                // Pictures left over from a previous run.
                remove_thumbnail_caches(&dir);
                let mut c = SwarmConfig::new(dir);
                c.enable_trackers = self.config.public_discovery;
                c.network.clone_from(&self.network);
                c.allow_local_trackers = self.config.allow_local_links;
                c.peer_limit = 8;
                c.max_connecting = 4;
                c.download_limit_bps = thumbnail_rate(self.limits().0);
                Swarm::start(c).await.map_err(swarm_err)
            })
            .await?;
        Ok(Some(swarm))
    }

    /// Fetches the metadata of a search result through its download link (plain HTTP, usually
    /// faster than asking peers, and it works without the DHT), as
    /// [`StreamEngine::prefetch_metadata`] does. The link may serve the `.torrent` file or
    /// redirect to a magnet link. If `expected` names the result's info hash, a link that
    /// gives another torrent counts as failed. When the link fails, `fallback` (the result's
    /// magnet link) is tried instead, if there is one. Returns the info hash.
    ///
    /// # Errors
    ///
    /// Returns the link's error (or a mismatch) if there is no fallback, and the fallback's
    /// error otherwise.
    pub async fn prefetch_download(
        &self,
        download: url::Url,
        expected: Option<&str>,
        fallback: Option<TorrentSource>,
    ) -> Result<String, EngineError> {
        let followed = match self.open_link(&download).await {
            Ok(Opened::Magnet(link)) => match magnet_info_hash(&link) {
                Some(hash) if expected.is_none_or(|e| e.eq_ignore_ascii_case(&hash)) => {
                    Ok(TorrentSource::Magnet(link))
                }
                _ => Err(EngineError::InvalidTorrent(
                    "the download link leads to another torrent".into(),
                )),
            },
            Ok(Opened::File(bytes)) => match Metainfo::from_bytes(&bytes) {
                Ok(meta)
                    if expected.is_none_or(|e| e.eq_ignore_ascii_case(&meta.info_hash_hex())) =>
                {
                    Ok(TorrentSource::File(bytes))
                }
                Ok(_) => Err(EngineError::InvalidTorrent(
                    "the download link leads to another torrent".into(),
                )),
                Err(e) => Err(EngineError::InvalidTorrent(e.to_string())),
            },
            Err(error) => Err(error),
        };
        let outcome = match followed {
            Ok(source) => self.prefetch_metadata(source).await,
            Err(error) => Err(error),
        };
        match (outcome, fallback) {
            (Err(error), Some(fallback)) => {
                tracing::debug!(%error, "download link failed; trying the magnet link");
                self.prefetch_metadata(fallback).await
            }
            (outcome, _) => outcome,
        }
    }

    /// Metadata of a torrent.
    ///
    /// # Errors
    ///
    /// Returns an error if the torrent is unknown.
    pub fn info(&self, id: TorrentId) -> Result<TorrentInfo, EngineError> {
        self.torrents()
            .get(&id)
            .map(|b| b.info().clone())
            .ok_or(EngineError::UnknownTorrent)
    }

    /// Which engine serves a torrent.
    ///
    /// # Errors
    ///
    /// Returns an error if the torrent is unknown.
    pub fn backend(&self, id: TorrentId) -> Result<BackendKind, EngineError> {
        self.torrents()
            .get(&id)
            .map(|b| match b {
                Backed::Rqbit { .. } => BackendKind::Rqbit,
                Backed::Swarm { .. } => BackendKind::Swarm,
            })
            .ok_or(EngineError::UnknownTorrent)
    }

    /// Piece layout of a torrent and the position of one of its files.
    ///
    /// # Errors
    ///
    /// Returns an error if the torrent or file is unknown.
    pub fn file_geometry(
        &self,
        id: TorrentId,
        file: usize,
    ) -> Result<(PieceGeometry, FileSpan), EngineError> {
        let info = self.info(id)?;
        geometry_of(&info, file)
    }

    /// Selects a file for download and starts prioritizing it for playback.
    ///
    /// # Errors
    ///
    /// Returns an error if the torrent or file is unknown or the engine fails.
    pub async fn start_playback(
        &self,
        id: TorrentId,
        file: usize,
        config: torrentplayer_prioritize::Config,
    ) -> Result<Playback, EngineError> {
        let (geometry, span) = self.file_geometry(id, file)?;
        match self.handle(id)? {
            Handle::Rqbit(inner) => {
                let session = self
                    .rqbit()
                    .await?
                    .start_playback(inner, file, config)
                    .await?;
                Ok(Playback::Rqbit(session))
            }
            Handle::Swarm(torrent) => {
                torrent.set_upload_enabled(true);
                torrent.select_file(file).map_err(swarm_err)?;
                torrent.set_playhead(file, 0, None).map_err(swarm_err)?;
                Ok(Playback::Swarm(SwarmPlayback {
                    torrent,
                    file,
                    geometry,
                    span,
                    position: Mutex::new((0, None)),
                }))
            }
        }
    }

    /// A media source serving one file over the stream server.
    ///
    /// # Errors
    ///
    /// Returns an error if the torrent or file is unknown.
    pub async fn media_source(
        &self,
        id: TorrentId,
        file: usize,
    ) -> Result<Arc<dyn MediaSource>, EngineError> {
        let entry = file_entry(&self.info(id)?, file)?;
        Ok(match self.handle(id)? {
            Handle::Rqbit(inner) => Arc::new(self.rqbit().await?.media_source(inner, file)?),
            Handle::Swarm(torrent) => Arc::new(SwarmMedia {
                torrent,
                file,
                len: entry.len,
                content_type: mime_type(&entry.path),
                stored_only: false,
            }),
        })
    }

    /// A media source that serves only what is already downloaded of one file, for tools
    /// (`ffprobe`, `ffmpeg`) that must not change what is downloaded: reading it prioritizes
    /// nothing, and a read of a missing piece fails at once. librqbit cannot read without
    /// prioritizing, so its files are served only once complete.
    ///
    /// # Errors
    ///
    /// Returns an error if the torrent or file is unknown, or a librqbit file is incomplete.
    pub async fn stored_media(
        &self,
        id: TorrentId,
        file: usize,
    ) -> Result<Arc<dyn MediaSource>, EngineError> {
        let entry = file_entry(&self.info(id)?, file)?;
        match self.handle(id)? {
            Handle::Rqbit(_) => {
                let have = self.have_pieces(id).await?;
                let readiness = self.file_readiness(id, Some(file), &have)?;
                if readiness.file != file || !readiness.complete() {
                    return Err(EngineError::Unsupported("the file is not downloaded yet"));
                }
                self.media_source(id, file).await
            }
            Handle::Swarm(torrent) => Ok(Arc::new(SwarmMedia {
                torrent,
                file,
                len: entry.len,
                content_type: mime_type(&entry.path),
                stored_only: true,
            })),
        }
    }

    /// Downloads a whole file in the background, after whatever is being played.
    ///
    /// # Errors
    ///
    /// Returns an error if the torrent or file is unknown or the engine fails.
    pub async fn download_file(&self, id: TorrentId, file: usize) -> Result<(), EngineError> {
        file_entry(&self.info(id)?, file)?;
        match self.handle(id)? {
            Handle::Rqbit(inner) => self.rqbit().await?.select_file(inner, file).await,
            Handle::Swarm(torrent) => torrent.select_file(file).map_err(swarm_err),
        }
    }

    /// Stops downloading a file in the background (see [`StreamEngine::download_file`] and
    /// [`StreamEngine::prefetch_file`]). Playback of it still downloads what it needs.
    ///
    /// # Errors
    ///
    /// Returns an error if the torrent or file is unknown or the engine fails.
    pub async fn stop_downloading_file(
        &self,
        id: TorrentId,
        file: usize,
    ) -> Result<(), EngineError> {
        file_entry(&self.info(id)?, file)?;
        match self.handle(id)? {
            Handle::Rqbit(inner) => self.rqbit().await?.deselect_file(inner, file).await,
            Handle::Swarm(torrent) => torrent.deselect_file(file).map_err(swarm_err),
        }
    }

    /// Pauses or resumes a torrent. A paused torrent downloads nothing, playback included.
    ///
    /// # Errors
    ///
    /// Returns an error if the torrent is unknown or the engine fails.
    pub async fn set_paused(&self, id: TorrentId, paused: bool) -> Result<(), EngineError> {
        match self.handle(id)? {
            Handle::Rqbit(inner) => self.rqbit().await?.set_paused(inner, paused).await,
            Handle::Swarm(torrent) => {
                torrent.set_paused(paused);
                Ok(())
            }
        }
    }

    /// Limits one torrent's download rate (bytes per second); `None` removes the limit.
    /// Returns `false` if its engine cannot limit a single torrent (librqbit), in which case
    /// nothing changed.
    ///
    /// # Errors
    ///
    /// Returns an error if the torrent is unknown.
    pub fn set_download_limit(
        &self,
        id: TorrentId,
        limit: Option<NonZeroU32>,
    ) -> Result<bool, EngineError> {
        match self.handle(id)? {
            Handle::Rqbit(_) => Ok(false),
            Handle::Swarm(torrent) => {
                torrent.set_download_limit(limit);
                Ok(true)
            }
        }
    }

    /// Enables or disables serving a torrentplayer-swarm torrent to peers. librqbit applies the same
    /// policy by being paused or resumed by the queue manager, so this returns `false` for it.
    ///
    /// # Errors
    ///
    /// Returns an error if the torrent is unknown.
    pub fn set_upload_enabled(&self, id: TorrentId, enabled: bool) -> Result<bool, EngineError> {
        match self.handle(id)? {
            Handle::Rqbit(_) => Ok(false),
            Handle::Swarm(torrent) => {
                torrent.set_upload_enabled(enabled);
                Ok(true)
            }
        }
    }

    /// Which pieces are downloaded and verified.
    ///
    /// # Errors
    ///
    /// Returns an error if the torrent is unknown or not initialized yet.
    pub async fn have_pieces(&self, id: TorrentId) -> Result<Vec<bool>, EngineError> {
        match self.handle(id)? {
            Handle::Rqbit(inner) => self.rqbit().await?.have_pieces(inner),
            Handle::Swarm(torrent) => Ok(torrent.have_pieces()),
        }
    }

    /// Current statistics.
    ///
    /// # Errors
    ///
    /// Returns an error if the torrent is unknown.
    pub async fn stats(&self, id: TorrentId) -> Result<TorrentStats, EngineError> {
        match self.handle(id)? {
            Handle::Rqbit(inner) => self.rqbit().await?.stats(inner),
            Handle::Swarm(torrent) => self.swarm_stats(id, &torrent, &torrent.have_pieces()),
        }
    }

    /// [`Self::stats`] and [`Self::have_pieces`] together, reading the pieces present once.
    ///
    /// # Errors
    ///
    /// Returns an error if the torrent is unknown.
    pub async fn stats_and_have(
        &self,
        id: TorrentId,
    ) -> Result<(TorrentStats, Vec<bool>), EngineError> {
        match self.handle(id)? {
            Handle::Rqbit(inner) => {
                let rqbit = self.rqbit().await?;
                Ok((rqbit.stats(inner)?, rqbit.have_pieces(inner)?))
            }
            Handle::Swarm(torrent) => {
                let have = torrent.have_pieces();
                Ok((self.swarm_stats(id, &torrent, &have)?, have))
            }
        }
    }

    /// Whether a torrent with this id is added. Cheaper than [`Self::info`] as a check.
    #[must_use]
    pub fn contains(&self, id: TorrentId) -> bool {
        self.torrents().contains_key(&id)
    }

    fn swarm_stats(
        &self,
        id: TorrentId,
        torrent: &Torrent,
        have: &[bool],
    ) -> Result<TorrentStats, EngineError> {
        let s = torrent.stats();
        let info = self.info(id)?;
        let to_u32 = |n: usize| u32::try_from(n).unwrap_or(u32::MAX);
        // Rates are non-negative and far below u64::MAX.
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let (download_bps, upload_bps) =
            (s.download_bps.max(0.0) as u64, s.upload_bps.max(0.0) as u64);
        Ok(TorrentStats {
            state: if torrent.is_paused() {
                TorrentState::Paused
            } else {
                TorrentState::Live
            },
            download_bps,
            upload_bps,
            uploaded_bytes: s.uploaded_bytes,
            peers_live: to_u32(s.peers_live),
            peers_connecting: to_u32(s.peers_connecting),
            peers_seen: to_u32(s.peers_seen),
            file_progress: file_progress(&info, have),
            progress_bytes: s.downloaded_bytes,
            error: None,
        })
    }

    /// Stops every torrent. An unplayed preload's cache is deleted. Cache flushes run on
    /// blocking workers, so callers may time out waiting for shutdown on slow storage.
    pub async fn shutdown(&self) {
        self.cancel_preload().await;
        let main = async {
            if let Some(swarm) = &self.swarm {
                swarm.shutdown_async().await;
            }
        };
        let thumbnails = async {
            if let Some(swarm) = self.thumbnails.get() {
                swarm.shutdown_async().await;
            }
        };
        let rqbit = async {
            let engine = self.rqbit.lock().await.take();
            if let Some(engine) = engine {
                engine.shutdown().await;
            }
        };
        // Stop every engine promptly, even when another one's cache is slow to flush.
        tokio::join!(main, thumbnails, rqbit);
    }

    /// Preloads a torrent: see [`StreamEngine::preload_with_peers`].
    pub async fn preload(&self, source: TorrentSource) -> Option<PreloadStatus> {
        self.preload_with_peers(source, Vec::new()).await
    }

    /// Preloads a torrent like [`StreamEngine::preload`], but the start and end of `file` (the
    /// play queue's next lecture) rather than the file the engine would choose, if `file` is a
    /// video of the torrent. A link to a `.torrent` file is fetched first, and one that leads
    /// to a magnet link preloads that (see [`StreamEngine::resolve_source`]).
    pub async fn preload_file(
        &self,
        source: TorrentSource,
        file: Option<usize>,
    ) -> Option<PreloadStatus> {
        let source = match self.resolve_source(source).await {
            Ok(source) => source,
            Err(error) => {
                tracing::debug!(%error, "cannot preload this link");
                return None;
            }
        };
        self.preload_file_with_peers(source, Vec::new(), file).await
    }

    /// What a link to a `.torrent` file stands for: the file it downloads, or the magnet link
    /// it redirects to (as download links of search sites often do). Magnet links and file
    /// contents are returned as they are. What each link gave is remembered, so resolving it
    /// again sends no request.
    ///
    /// # Errors
    ///
    /// Returns an error if the download fails or is refused.
    pub async fn resolve_source(
        &self,
        source: TorrentSource,
    ) -> Result<TorrentSource, EngineError> {
        let TorrentSource::Url(url) = source else {
            return Ok(source);
        };
        Ok(match self.open_link(&url).await? {
            Opened::File(bytes) => TorrentSource::File(bytes),
            Opened::Magnet(link) => TorrentSource::Magnet(link),
        })
    }

    /// Downloads the start (and the last 2 MiB) of a torrent's best video file into
    /// torrentplayer-swarm, so that playing it later starts at once. Runs until that is done, the
    /// preload stops at its time or byte limit, or a newer preload replaces it, and returns
    /// the final status; progress is available from [`StreamEngine::preload_status`]
    /// meanwhile. Nothing is uploaded.
    ///
    /// One preload exists at a time: starting one evicts the previous one (stopping it and
    /// deleting its cache) unless the user has added that torrent. Preloading the torrent that
    /// is already being preloaded changes nothing. Returns `None` if there is nothing to do: the
    /// torrent was already added, or torrentplayer-swarm is disabled.
    pub async fn preload_with_peers(
        &self,
        source: TorrentSource,
        peers: Vec<SocketAddr>,
    ) -> Option<PreloadStatus> {
        self.preload_file_with_peers(source, peers, None).await
    }

    /// [`StreamEngine::preload_with_peers`] of `file`, or of the engine's choice if that is
    /// not a video of the torrent. `source` is used as it is (see
    /// [`StreamEngine::preload_file`] for links to `.torrent` files).
    pub async fn preload_file_with_peers(
        &self,
        source: TorrentSource,
        peers: Vec<SocketAddr>,
        file: Option<usize>,
    ) -> Option<PreloadStatus> {
        let swarm = self.swarm.as_ref()?;
        let known_hash = match &source {
            TorrentSource::Magnet(link) => magnet_info_hash(link),
            TorrentSource::File(bytes) => {
                Metainfo::from_bytes(bytes).ok().map(|m| m.info_hash_hex())
            }
            TorrentSource::Url(_) => None,
        };
        if let Some(hash) = &known_hash {
            if let Some(current) = self.preload_status()
                && current.info_hash == *hash
                && !matches!(
                    current.phase,
                    PreloadPhase::Failed | PreloadPhase::Cancelled
                )
            {
                return Some(current);
            }
            if self
                .torrents()
                .values()
                .any(|b| b.info().info_hash == *hash)
            {
                return None;
            }
        }

        let status = Arc::new(watch::Sender::new(PreloadStatus::new(
            known_hash.unwrap_or_default(),
        )));
        let cancel = CancellationToken::new();
        let previous = self.preload_slot().replace(PreloadSlot {
            status: Arc::clone(&status),
            cancel: cancel.clone(),
            torrent: None,
        });
        if let Some(previous) = previous {
            self.evict(previous).await;
        }

        let outcome = tokio::select! {
            outcome = self.run_preload(swarm, source, peers, file, &status, &cancel) => outcome,
            () = cancel.cancelled() => Ok(()),
        };
        if let Err(error) = outcome {
            tracing::debug!(%error, "preload failed");
            status.send_modify(|s| s.fail(&preload_error(&error)));
        }
        let last = status.borrow().clone();
        info!(phase = ?last.phase, bytes = last.buffered_bytes, "preload finished");
        if last.phase == PreloadPhase::Failed {
            // Nothing worth keeping; a later add goes to the engine best suited to it.
            let current = {
                let mut slot = self.preload_slot();
                if slot
                    .as_ref()
                    .is_some_and(|s| Arc::ptr_eq(&s.status, &status))
                {
                    slot.take()
                } else {
                    None
                }
            };
            if let Some(current) = current {
                self.evict(current).await;
            }
        }
        Some(last)
    }

    /// The steps of [`StreamEngine::preload_with_peers`]; errors are recorded by the caller.
    async fn run_preload(
        &self,
        swarm: &Swarm,
        source: TorrentSource,
        peers: Vec<SocketAddr>,
        wanted: Option<usize>,
        status: &watch::Sender<PreloadStatus>,
        cancel: &CancellationToken,
    ) -> Result<(), EngineError> {
        // 1. Metadata. For magnet links librqbit fetches it from peers (metadata only).
        let (bytes, seen_peers) = self.torrent_bytes(source, peers).await?;
        let meta =
            Metainfo::from_bytes(&bytes).map_err(|e| EngineError::InvalidTorrent(e.to_string()))?;
        validate_swarm_metadata(&meta)?;
        let hash = meta.info_hash_hex();
        status.send_modify(|s| s.info_hash.clone_from(&hash));
        let file = preload_target(&swarm_info(0, &meta).files, wanted)
            .cloned()
            .ok_or(EngineError::Unsupported(
                "This torrent has no video the built-in player can play.",
            ))?;
        let (head, tail) = targets(file.len, self.swarm_window.1);

        // 2. Add it to torrentplayer-swarm, unless this preload was replaced or the user added the
        //    torrent in the meantime.
        let (info, torrent) = {
            let mut slot = self.preload_slot();
            if cancel.is_cancelled() {
                return Ok(());
            }
            let mut torrents = self.torrents();
            if torrents.values().any(|b| b.info().info_hash == hash) {
                return Err(EngineError::Unsupported(
                    "This torrent has already been added.",
                ));
            }
            let id = self.next_id.fetch_add(1, Ordering::Relaxed);
            let info = swarm_info(id, &meta);
            let torrent = swarm.add(meta, seen_peers).map_err(swarm_err)?;
            torrent.set_upload_enabled(false);
            let marker = self.swarm_dir().join(format!("{hash}{PRELOAD_MARKER}"));
            if let Err(error) = std::fs::write(&marker, b"") {
                tracing::debug!(%error, "could not mark the preload cache");
            }
            torrents.insert(
                id,
                Backed::Swarm {
                    torrent: torrent.clone(),
                    info: info.clone(),
                },
            );
            if let Some(slot) = slot.as_mut() {
                slot.torrent = Some(id);
            }
            (info, torrent)
        };
        status.send_modify(|s| {
            s.resolved(file.index, file.path.clone(), head + tail);
        });

        self.buffer_preload(&torrent, &info, file.index, (head, tail), status, cancel)
            .await
    }

    /// Step 3 of a preload: downloads the head and tail of `file`, then stops prioritizing it.
    async fn buffer_preload(
        &self,
        torrent: &Torrent,
        info: &TorrentInfo,
        file: usize,
        (head, tail): (u64, u64),
        status: &watch::Sender<PreloadStatus>,
        cancel: &CancellationToken,
    ) -> Result<(), EngineError> {
        // A read-ahead window of `head` bytes from the start; the scheduler adds the tail.
        let read_ahead = self.swarm_window.0.as_secs().max(1);
        torrent
            .set_playhead(file, 0, Some((head / read_ahead).max(1)))
            .map_err(swarm_err)?;
        let (geometry, span) = geometry_of(info, file)?;
        let cap = byte_cap(head + tail);
        let deadline = tokio::time::Instant::now() + PRELOAD_TIMEOUT;
        loop {
            tokio::select! {
                () = cancel.cancelled() => return Ok(()),
                () = tokio::time::sleep(PRELOAD_POLL) => {}
            }
            let have = torrent.have_pieces();
            let done = preloaded_bytes(&geometry, span, head, tail, |p| {
                usize::try_from(p)
                    .ok()
                    .and_then(|p| have.get(p))
                    .copied()
                    .unwrap_or(false)
            });
            let (mut finished, mut claimed) = (false, false);
            status.send_modify(|s| {
                finished = s.progress(done);
                claimed = s.claimed;
            });
            if claimed {
                // The user's torrent now: its player takes over the playhead.
                return Ok(());
            }
            let received = torrent.stats();
            let over_limit = received.downloaded_bytes + received.wasted_bytes > cap
                || tokio::time::Instant::now() >= deadline;
            if finished || over_limit {
                // Claims happen under this lock, so a new player's playhead is never cleared.
                let _slot = self.preload_slot();
                if !status.borrow().claimed {
                    torrent.clear_playhead();
                }
                if over_limit {
                    status.send_modify(PreloadStatus::stopped);
                }
                return Ok(());
            }
        }
    }

    /// The current preload, if any.
    #[must_use]
    pub fn preload_status(&self) -> Option<PreloadStatus> {
        self.preload_slot()
            .as_ref()
            .map(|s| s.status.borrow().clone())
    }

    /// The finished preload's file, to show a frame from its start (already downloaded) as a
    /// thumbnail, with its info hash. Does not claim the preload. `None` unless a preload is
    /// ready.
    pub async fn preloaded_media(&self) -> Option<(String, Arc<dyn MediaSource>)> {
        let (info_hash, torrent, file) = {
            let slot = self.preload_slot();
            let slot = slot.as_ref()?;
            let status = slot.status.borrow();
            if status.phase != PreloadPhase::Ready {
                return None;
            }
            (status.info_hash.clone(), slot.torrent?, status.file?)
        };
        let media = self.media_source(torrent, file).await.ok()?;
        Some((info_hash, media))
    }

    /// Prefetches the start and end of a file of a torrent that is already added (for example
    /// the next lecture of the course playing), at background priority: the playhead's
    /// read-ahead keeps priority, so the video playing is not slowed down. `file` `None`, or a
    /// file that is not a video, picks the file a preload would. Returns the file chosen.
    ///
    /// torrentplayer-swarm downloads as much as a preload would ([`targets`]). librqbit cannot
    /// download part of a file, so it selects the whole file; an open [`Playback`] of the same
    /// torrent also prioritizes its head and tail near the end once told with
    /// [`Playback::set_next_file`].
    ///
    /// # Errors
    ///
    /// Returns an error if the torrent is unknown, has no video, or the engine fails.
    pub async fn prefetch_file(
        &self,
        id: TorrentId,
        file: Option<usize>,
    ) -> Result<usize, EngineError> {
        let info = self.info(id)?;
        let entry = preload_target(&info.files, file)
            .cloned()
            .ok_or(EngineError::Unsupported(
                "This torrent has no video the built-in player can play.",
            ))?;
        match self.handle(id)? {
            Handle::Swarm(torrent) => {
                let (head, tail) = targets(entry.len, self.swarm_window.1);
                torrent
                    .select_range(entry.index, 0..head)
                    .and_then(|()| {
                        torrent.select_range(entry.index, entry.len.saturating_sub(tail)..entry.len)
                    })
                    .map_err(swarm_err)?;
            }
            Handle::Rqbit(inner) => {
                self.rqbit().await?.select_file(inner, entry.index).await?;
            }
        }
        info!(
            torrent = id,
            file = entry.index,
            "prefetching the next file"
        );
        Ok(entry.index)
    }

    /// Every added torrent's id (preloads not yet played excluded).
    #[must_use]
    pub fn torrent_ids(&self) -> Vec<TorrentId> {
        self.torrents().keys().copied().collect()
    }

    /// The added torrent with this info hash (a preload's included), if any.
    #[must_use]
    pub fn torrent_with_hash(&self, info_hash: &str) -> Option<TorrentId> {
        self.torrents()
            .iter()
            .find(|(_, b)| b.info().info_hash.eq_ignore_ascii_case(info_hash))
            .map(|(id, _)| *id)
    }

    /// Stops and forgets an added torrent without deleting its downloaded files.
    ///
    /// Cache housekeeping uses this only after the app has established that the torrent is not
    /// queued and has no open stream. Once its engine has released the files, the app can remove
    /// the validated cache directory itself.
    ///
    /// # Errors
    ///
    /// Returns an error if the torrent is unknown or librqbit cannot remove it.
    pub async fn remove(&self, id: TorrentId) -> Result<(), EngineError> {
        match self.handle(id)? {
            Handle::Rqbit(inner) => {
                let engine = self
                    .rqbit
                    .lock()
                    .await
                    .as_ref()
                    .cloned()
                    .ok_or(EngineError::UnknownTorrent)?;
                engine.remove(inner, false).await?;
                self.torrents().remove(&id);
            }
            Handle::Swarm(torrent) => {
                self.torrents().remove(&id);
                torrent.stop();
            }
        }
        Ok(())
    }

    /// How ready a file of an added torrent is to play, from `have` (its
    /// [`StreamEngine::have_pieces`]): downloaded bytes, and whether its start and end are
    /// present as a preload would leave them. `file` `None`, or a file that is not a video,
    /// picks the file a preload would. Reads nothing else and downloads nothing.
    ///
    /// # Errors
    ///
    /// Returns an error if the torrent is unknown or has no video.
    pub fn file_readiness(
        &self,
        id: TorrentId,
        file: Option<usize>,
        have: &[bool],
    ) -> Result<FileReadiness, EngineError> {
        let info = self.info(id)?;
        let entry = preload_target(&info.files, file).ok_or(EngineError::UnknownFile)?;
        let (geometry, span) = geometry_of(&info, entry.index)?;
        Ok(file_readiness(
            &geometry,
            span,
            entry.index,
            targets(entry.len, self.swarm_window.1),
            |p| {
                usize::try_from(p)
                    .ok()
                    .and_then(|p| have.get(p))
                    .copied()
                    .unwrap_or(false)
            },
        ))
    }

    /// A magnet link that adds this torrent again, with its name and (for torrents in
    /// torrentplayer-swarm) up to [`crate::MAGNET_TRACKERS`] of its trackers, so that a torrent opened
    /// from a `.torrent` file can be saved (in the play queue) without keeping the file.
    ///
    /// # Errors
    ///
    /// Returns an error if the torrent is unknown.
    pub fn magnet_link(&self, id: TorrentId) -> Result<String, EngineError> {
        let info = self.info(id)?;
        let trackers = match self.handle(id)? {
            Handle::Swarm(torrent) => torrent.meta().trackers.clone(),
            Handle::Rqbit(_) => Vec::new(),
        };
        Ok(magnet_link(&info.info_hash, &info.name, &trackers))
    }

    /// Stops the current preload and deletes its cache, unless the user has added it.
    pub async fn cancel_preload(&self) {
        let slot = self.preload_slot().take();
        if let Some(slot) = slot {
            self.evict(slot).await;
        }
    }

    /// Hands a preloaded torrent to an add of the same magnet link.
    async fn take_preloaded(&self, source: &TorrentSource) -> Option<TorrentInfo> {
        let TorrentSource::Magnet(link) = source else {
            return None;
        };
        let hash = magnet_info_hash(link)?;
        let mut watching = {
            let slot = self.preload_slot();
            let current = slot.as_ref()?;
            if current.status.borrow().info_hash != hash {
                return None;
            }
            current.status.subscribe()
        };
        // Still fetching the metadata: wait for it rather than fetching it twice.
        let _ = tokio::time::timeout(
            PRELOAD_METADATA_WAIT,
            watching.wait_for(|s| s.phase != PreloadPhase::Resolving),
        )
        .await;
        let slot = self.preload_slot();
        let current = slot.as_ref()?;
        if current.status.borrow().info_hash != hash {
            return None;
        }
        let id = current.torrent?;
        let mut claimed = false;
        current.status.send_modify(|s| claimed = s.claim());
        if !claimed {
            return None;
        }
        self.remove_preload_marker(&hash);
        self.info(id).ok()
    }

    /// Marks the preload as the user's if it is this torrent.
    fn claim_preload(&self, info_hash: &str) {
        let slot = self.preload_slot();
        let Some(current) = slot.as_ref() else {
            return;
        };
        if current.status.borrow().info_hash != info_hash {
            return;
        }
        let mut claimed = false;
        current.status.send_modify(|s| claimed = s.claim());
        if claimed {
            self.remove_preload_marker(info_hash);
        }
    }

    /// Stops a replaced preload and deletes its cache, unless the user added it.
    async fn evict(&self, slot: PreloadSlot) {
        let mut evictable = false;
        slot.status.send_modify(|s| evictable = s.cancel());
        if !evictable {
            return;
        }
        slot.cancel.cancel();
        let hash = slot.status.borrow().info_hash.clone();
        let removed = slot.torrent.and_then(|id| self.torrents().remove(&id));
        let Some(Backed::Swarm { torrent, .. }) = removed else {
            return;
        };
        torrent.stop();
        drop(torrent);
        if !is_hex_hash(&hash) {
            return;
        }
        // The cache file closes once the torrent's tasks have exited.
        let cache = self.swarm_dir().join(&hash);
        for _ in 0..20 {
            match tokio::fs::remove_dir_all(&cache).await {
                Ok(()) => break,
                Err(e) if e.kind() == io::ErrorKind::NotFound => break,
                Err(_) => tokio::time::sleep(Duration::from_millis(100)).await,
            }
        }
        if !cache.exists() {
            self.remove_preload_marker(&hash);
        }
        info!("evicted an unplayed preload");
    }

    fn remove_preload_marker(&self, info_hash: &str) {
        if is_hex_hash(info_hash) {
            let _ = std::fs::remove_file(
                self.swarm_dir()
                    .join(format!("{info_hash}{PRELOAD_MARKER}")),
            );
        }
    }

    fn swarm_dir(&self) -> PathBuf {
        self.config.download_dir.join("torrentplayer-swarm")
    }

    /// The thumbnail engine's cache. It lives in torrentplayer-swarm's own folder: the download
    /// folder is the user's, and so may be a `thumbnails` folder in it.
    fn thumbnail_dir(&self) -> PathBuf {
        self.swarm_dir().join(THUMBNAIL_DIR)
    }

    fn preload_slot(&self) -> std::sync::MutexGuard<'_, Option<PreloadSlot>> {
        self.preload.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn handle(&self, id: TorrentId) -> Result<Handle, EngineError> {
        match self.torrents().get(&id) {
            Some(Backed::Rqbit { inner, .. }) => Ok(Handle::Rqbit(*inner)),
            Some(Backed::Swarm { torrent, .. }) => Ok(Handle::Swarm(torrent.clone())),
            None => Err(EngineError::UnknownTorrent),
        }
    }
}

/// What librqbit's listener is, for the status, while no session runs (or in a protected
/// mode, where it never listens).
fn stopped_rqbit_listener(network: &torrentplayer_net::Network) -> ExternalListener {
    if network.policy().protected {
        ExternalListener::Idle(
            "librqbit accepts incoming peers only in direct mode; choose the automatic \n                 engine to accept them through WireGuard"
                .into(),
        )
    } else {
        ExternalListener::Idle("librqbit opens the port when its first torrent starts".into())
    }
}

/// Stops `engine` once it has been idle (see `Engine::idle_for`) for `after`, unless it was
/// replaced or stopped first; the next torrent that needs librqbit starts a new session. The
/// check and the stop happen under the slot's lock, which every caller takes to get the
/// engine (see `StreamEngine::rqbit`): a caller either gets it first, and so keeps it in use,
/// or waits for the stop and starts a new one. `network`, if given, is told the listener
/// closed.
async fn stop_rqbit_when_idle(
    slot: Weak<tokio::sync::Mutex<Option<Engine>>>,
    torrents: Weak<Mutex<HashMap<TorrentId, Backed>>>,
    engine: WeakEngine,
    after: Duration,
    network: Option<torrentplayer_net::Network>,
) {
    let mut wait = after;
    loop {
        tokio::time::sleep(wait).await;
        let (Some(slot), Some(torrents)) = (slot.upgrade(), torrents.upgrade()) else {
            return;
        };
        let mut slot = slot.lock().await;
        let Some(current) = slot.as_ref().filter(|current| engine.is(current)) else {
            return;
        };
        let has_torrents = torrents
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .values()
            .any(|b| matches!(b, Backed::Rqbit { .. }));
        match current.idle_for().filter(|_| !has_torrents) {
            Some(idle) if idle >= after => {}
            Some(idle) => {
                wait = after.saturating_sub(idle);
                continue;
            }
            None => {
                wait = after;
                continue;
            }
        }
        let Some(current) = slot.take() else {
            return;
        };
        info!(
            idle_secs = after.as_secs(),
            "librqbit unused: stopping its session"
        );
        current.shutdown().await;
        if let Some(network) = &network {
            network.set_external_listener(Some(stopped_rqbit_listener(network)));
        }
        return;
    }
}

/// A copy of what is needed to talk to a torrent's engine, taken out of the lock.
enum Handle {
    Rqbit(TorrentId),
    Swarm(Torrent),
}

/// Deletes the thumbnail caches (`<info hash>` folders) a previous run left in `dir`. Nothing
/// else there is touched, and nothing at all if `dir` is not a real folder.
fn remove_thumbnail_caches(dir: &Path) {
    if !std::fs::symlink_metadata(dir).is_ok_and(|m| m.file_type().is_dir()) {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        if entry.file_name().to_str().is_some_and(is_hex_hash)
            && std::fs::symlink_metadata(entry.path()).is_ok_and(|m| m.file_type().is_dir())
        {
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }
}

/// Deletes the caches of preloads that were never played (marked by `<hash>.preload` files).
fn remove_stale_preloads(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(hash) = name.to_str().and_then(|n| n.strip_suffix(PRELOAD_MARKER)) else {
            continue;
        };
        if is_hex_hash(hash) {
            let cache = dir.join(hash);
            if cache.exists() && std::fs::remove_dir_all(&cache).is_err() {
                continue;
            }
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// Peers for a magnet link from scoped DHT, UDP trackers, and HTTP trackers through a protected
/// network mode (WireGuard or a SOCKS5 relay). librqbit's unbound UDP stays disabled there,
/// and it contacts no trackers (its HTTP client would resolve their names with the system
/// resolver), so TorrentPlayer finds peers itself, through the mode, and hands them to
/// whichever engine fetches the metadata. HTTP trackers are announced to through the network's
/// bridge, which passes their host names unresolved to the tunnel or proxy. Returns the first
/// source's peers, or waits for the others while one finds none. Empty in direct mode
/// (librqbit uses its own DHT and trackers), while the mode blocks traffic, or with
/// `public_discovery` off. HTTP trackers on loopback or private addresses are skipped unless
/// `allow_local_trackers` (for tests with a local tracker).
pub async fn protected_magnet_peers(
    network: &torrentplayer_net::Network,
    link: &str,
    public_discovery: bool,
    allow_local_trackers: bool,
) -> Vec<SocketAddr> {
    use futures_util::StreamExt as _;
    use futures_util::stream::FuturesUnordered;

    let policy = network.policy();
    if policy.udp_allowed || !policy.traffic_allowed || !public_discovery {
        return Vec::new();
    }
    let Some(info_hash) = magnet_info_hash(link).and_then(|h| hex_hash(&h)) else {
        return Vec::new();
    };
    let Ok(url) = url::Url::parse(link) else {
        return Vec::new();
    };
    let trackers: Vec<url::Url> = url
        .query_pairs()
        .filter(|(key, _)| key == "tr")
        .filter_map(|(_, value)| url::Url::parse(&value).ok())
        .collect();
    let deadline = Duration::from_secs(12);
    let mut sources: FuturesUnordered<BoxFuture<'_, Vec<SocketAddr>>> = FuturesUnordered::new();
    if policy.trackers_allowed {
        sources.push(Box::pin(torrentplayer_swarm::http_tracker::announce_all(
            network,
            &trackers,
            info_hash,
            deadline,
            allow_local_trackers,
        )));
        if policy.bound_udp_allowed {
            sources.push(Box::pin(torrentplayer_swarm::udp_tracker::announce_all(
                Some(network),
                &trackers,
                info_hash,
                deadline,
            )));
        }
    }
    if policy.dht_allowed && policy.bound_udp_allowed {
        sources.push(Box::pin(torrentplayer_swarm::dht_bootstrap_peers(
            network.clone(),
            info_hash,
            deadline,
        )));
    }
    let mut peers = Vec::new();
    while let Some(found) = sources.next().await {
        if !found.is_empty() {
            peers = found;
            break;
        }
    }
    info!(
        peers = peers.len(),
        "Peer discovery bootstrap completed through the protected network"
    );
    peers
}

/// The 20 bytes of a 40-character hex info hash.
fn hex_hash(hash: &str) -> Option<[u8; 20]> {
    let mut out = [0u8; 20];
    for (i, slot) in out.iter_mut().enumerate() {
        *slot = u8::from_str_radix(hash.get(i * 2..i * 2 + 2)?, 16).ok()?;
    }
    Some(out)
}

fn is_hex_hash(hash: &str) -> bool {
    hash.len() == 40 && hash.bytes().all(|b| b.is_ascii_hexdigit())
}

/// A message for a failed preload that is safe to show (engine errors may contain paths).
fn preload_error(error: &EngineError) -> String {
    match error {
        EngineError::MetadataTimeout => "Could not find the torrent's details in time.".into(),
        EngineError::Unsupported(message) => (*message).into(),
        EngineError::UnsafePath(_) | EngineError::TooManyFiles | EngineError::InvalidTorrent(_) => {
            "The torrent was refused.".into()
        }
        _ => "Preloading failed.".into(),
    }
}

/// Refuses torrentplayer-swarm torrents with unsafe names, the same rules librqbit torrents get.
fn validate_swarm_metadata(meta: &Metainfo) -> Result<(), EngineError> {
    if meta.files.len() > MAX_FILES {
        return Err(EngineError::TooManyFiles);
    }
    if !meta.name.is_empty() {
        validate_components(&[meta.name.as_str()])?;
    }
    for file in &meta.files {
        validate_components(&file.path)?;
    }
    Ok(())
}

fn swarm_info(id: TorrentId, meta: &Metainfo) -> TorrentInfo {
    let files = meta
        .files
        .iter()
        .enumerate()
        .filter(|(_, f)| !f.padding)
        .map(|(index, f)| {
            let path = f.path.join("/");
            FileEntry {
                index,
                kind: FileKind::from_name(&path),
                path,
                len: f.len,
                offset_in_torrent: f.offset,
            }
        })
        .collect();
    TorrentInfo {
        id,
        info_hash: meta.info_hash_hex(),
        name: meta.name.clone(),
        piece_len: meta.piece_len,
        total_len: meta.total_len,
        files,
    }
}

fn file_entry(info: &TorrentInfo, file: usize) -> Result<FileEntry, EngineError> {
    info.files
        .iter()
        .find(|f| f.index == file)
        .cloned()
        .ok_or(EngineError::UnknownFile)
}

fn geometry_of(info: &TorrentInfo, file: usize) -> Result<(PieceGeometry, FileSpan), EngineError> {
    let entry = file_entry(info, file)?;
    let geometry = PieceGeometry::new(info.piece_len, info.total_len)
        .ok_or_else(|| EngineError::Engine("torrent has an empty layout".into()))?;
    Ok((
        geometry,
        FileSpan {
            offset_in_torrent: entry.offset_in_torrent,
            len: entry.len,
        },
    ))
}

/// Verified bytes per file, from the piece bitfield. Indexed like [`FileEntry::index`].
fn file_progress(info: &TorrentInfo, have: &[bool]) -> Vec<u64> {
    let piece_len = u64::from(info.piece_len.max(1));
    let slots = info.files.iter().map(|f| f.index + 1).max().unwrap_or(0);
    let mut progress = vec![0; slots];
    for f in &info.files {
        let (start, end) = (f.offset_in_torrent, f.offset_in_torrent + f.len);
        if start >= end {
            continue;
        }
        let mut done = 0;
        for piece in start / piece_len..=(end - 1) / piece_len {
            if have
                .get(usize::try_from(piece).unwrap_or(usize::MAX))
                .copied()
                .unwrap_or(false)
            {
                let piece_start = piece * piece_len;
                let piece_end = (piece_start + piece_len).min(info.total_len);
                done += piece_end.min(end) - piece_start.max(start);
            }
        }
        progress[f.index] = done;
    }
    progress
}

/// Prioritization for a file being played, on either engine.
pub enum Playback {
    /// librqbit, steered by `torrentplayer-prioritize` anchors.
    Rqbit(PlaybackSession),
    /// torrentplayer-swarm, steered by its playhead deadline.
    Swarm(SwarmPlayback),
}

impl std::fmt::Debug for Playback {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Rqbit(_) => f.write_str("Playback::Rqbit"),
            Self::Swarm(_) => f.write_str("Playback::Swarm"),
        }
    }
}

impl Playback {
    /// The engine behind this playback.
    #[must_use]
    pub fn backend(&self) -> BackendKind {
        match self {
            Self::Rqbit(_) => BackendKind::Rqbit,
            Self::Swarm(_) => BackendKind::Swarm,
        }
    }

    /// Reports the player's position. Seeks take effect immediately.
    pub fn update(&self, position_secs: f64, duration_secs: Option<f64>) {
        match self {
            Self::Rqbit(session) => session.update_position(PlayheadPosition::Time {
                position_secs,
                duration_secs,
            }),
            Self::Swarm(playback) => playback.update(position_secs, duration_secs),
        }
    }

    /// The next lecture, a file of the same torrent (the play queue's next item), or `None`.
    /// librqbit selects it and prioritizes its head and tail in the last minute of playback.
    /// torrentplayer-swarm needs nothing here: [`StreamEngine::prefetch_file`] already asked for them.
    pub fn set_next_file(&self, next_file: Option<usize>) {
        match self {
            Self::Rqbit(session) => session.set_next_file(next_file),
            Self::Swarm(_) => {}
        }
    }

    /// What prioritization is doing. `have` is the torrent's piece bitfield, used by
    /// torrentplayer-swarm to measure the buffer at the playhead.
    #[must_use]
    pub fn status(&self, have: &[bool]) -> PlaybackStatus {
        match self {
            Self::Rqbit(session) => session.status(),
            Self::Swarm(playback) => playback.status(have),
        }
    }
}

/// torrentplayer-swarm playback state. Dropping it stops prioritizing the playhead.
pub struct SwarmPlayback {
    torrent: Torrent,
    file: usize,
    geometry: PieceGeometry,
    span: FileSpan,
    /// Last reported playhead (bytes) and byte rate.
    position: Mutex<(u64, Option<u64>)>,
}

impl std::fmt::Debug for SwarmPlayback {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SwarmPlayback")
            .field("file", &self.file)
            .finish_non_exhaustive()
    }
}

impl SwarmPlayback {
    fn update(&self, position_secs: f64, duration_secs: Option<f64>) {
        let len = self.span.len;
        let (bytes, rate) = match duration_secs.filter(|d| d.is_finite() && *d > 0.0) {
            Some(duration) => {
                let fraction = (position_secs / duration).clamp(0.0, 1.0);
                // Average bitrate is a good estimate until the container is probed (Phase 2).
                #[allow(
                    clippy::cast_possible_truncation,
                    clippy::cast_sign_loss,
                    clippy::cast_precision_loss
                )]
                let estimate = (
                    ((fraction * len as f64) as u64).min(len),
                    Some((len as f64 / duration) as u64),
                );
                estimate
            }
            None => (0, None),
        };
        *self.position.lock().unwrap_or_else(PoisonError::into_inner) = (bytes, rate);
        // The file index was validated when playback started.
        let _ = self.torrent.set_playhead(self.file, bytes, rate);
    }

    fn status(&self, have: &[bool]) -> PlaybackStatus {
        let (position_bytes, byte_rate) =
            *self.position.lock().unwrap_or_else(PoisonError::into_inner);
        let buffered_bytes = buffered_bytes_from(&self.geometry, self.span, position_bytes, |p| {
            usize::try_from(p)
                .ok()
                .and_then(|p| have.get(p))
                .copied()
                .unwrap_or(false)
        });
        PlaybackStatus {
            position_bytes,
            byte_rate,
            window: position_bytes..position_bytes,
            buffered_bytes,
            anchors: Vec::new(),
            plans: 0,
        }
    }
}

impl Drop for SwarmPlayback {
    fn drop(&mut self) {
        self.torrent.clear_playhead();
    }
}

/// One torrentplayer-swarm file as a media source; every request opens its own reader.
struct SwarmMedia {
    torrent: Torrent,
    file: usize,
    len: u64,
    content_type: &'static str,
    /// Open stored-only readers (see `Torrent::stored_reader`).
    stored_only: bool,
}

impl MediaSource for SwarmMedia {
    fn byte_len(&self) -> u64 {
        self.len
    }

    fn content_type(&self) -> &str {
        self.content_type
    }

    fn open(&self) -> BoxFuture<'static, io::Result<Box<dyn MediaReader>>> {
        let reader = if self.stored_only {
            self.torrent.stored_reader(self.file)
        } else {
            self.torrent.reader(self.file)
        }
        .map_err(io::Error::other);
        Box::pin(async move { reader.map(|r| Box::new(r) as Box<dyn MediaReader>) })
    }
}

/// The address librqbit's listener opens: every interface, IPv6 and IPv4 (librqbit makes
/// `[::]` dual-stack), or IPv4 loopback when the network manager keeps sockets there for tests
/// ([`torrentplayer_net::NetworkOptions::loopback_only`]).
fn rqbit_listen_ip(loopback_only: bool) -> IpAddr {
    if loopback_only {
        Ipv4Addr::LOCALHOST.into()
    } else {
        Ipv6Addr::UNSPECIFIED.into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HASH: &str = "dd8255ecdc7ca55fb0bbf81323d87062db1f6d1c";

    #[test]
    fn librqbit_listens_on_every_interface_outside_tests() {
        assert_eq!(rqbit_listen_ip(false), IpAddr::V6(Ipv6Addr::UNSPECIFIED));
        assert_eq!(rqbit_listen_ip(true), IpAddr::V4(Ipv4Addr::LOCALHOST));
    }

    #[test]
    fn thumbnails_stay_within_the_overall_limit() {
        let own = NonZeroU32::new(THUMBNAIL_RATE);
        assert_eq!(thumbnail_rate(None), own);
        let low = NonZeroU32::new(500_000);
        assert_eq!(thumbnail_rate(low), low);
        assert_eq!(thumbnail_rate(NonZeroU32::new(u32::MAX)), own);
    }

    fn magnet() -> TorrentSource {
        TorrentSource::parse_link(&format!("magnet:?xt=urn:btih:{HASH}")).unwrap()
    }

    /// librqbit alone, without a network manager or public discovery, stopping after `idle`
    /// unused.
    async fn rqbit_only(dir: &Path, idle: Duration) -> StreamEngine {
        let mut config = StreamEngineConfig::new(dir.to_path_buf());
        config.choice = EngineChoice::Rqbit;
        config.public_discovery = false;
        let mut engine = StreamEngine::start(config).await.unwrap();
        engine.rqbit_idle_stop = idle;
        engine
    }

    /// Whether a librqbit operation runs, rather than failing because its session stopped.
    async fn runs(engine: &Engine) -> bool {
        matches!(
            engine.remove(usize::MAX, false).await,
            Err(EngineError::UnknownTorrent | EngineError::Engine(_))
        )
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn unused_librqbit_stops_and_starts_again_when_needed() {
        let dir = tempfile::tempdir().unwrap();
        let idle = Duration::from_millis(200);
        let engine = rqbit_only(dir.path(), idle).await;
        let first = engine.rqbit().await.unwrap();
        let weak = first.downgrade();
        // Held by a caller: in use, however long.
        tokio::time::sleep(idle * 3).await;
        assert!(engine.rqbit.lock().await.is_some());
        assert!(runs(&first).await);
        drop(first);
        tokio::time::sleep(idle * 3).await;
        assert!(engine.rqbit.lock().await.is_none(), "stopped once unused");

        let second = engine.rqbit().await.unwrap();
        assert!(!weak.is(&second), "a new session");
        assert!(!second.is_stopping());
        assert!(runs(&second).await);
        drop(second);
        engine.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn librqbit_stops_only_after_its_last_operation_is_long_past() {
        let dir = tempfile::tempdir().unwrap();
        let idle = Duration::from_secs(1);
        let engine = rqbit_only(dir.path(), idle).await;
        drop(engine.rqbit().await.unwrap());
        tokio::time::sleep(Duration::from_millis(700)).await;
        let used = engine.rqbit().await.unwrap();
        assert!(runs(&used).await);
        drop(used);
        // 1.2 s since the session started, 0.5 s since it was last used.
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert!(engine.rqbit.lock().await.is_some());
        tokio::time::sleep(Duration::from_millis(1500)).await;
        assert!(engine.rqbit.lock().await.is_none());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_callers_share_one_librqbit_session() {
        let dir = tempfile::tempdir().unwrap();
        let engine = rqbit_only(dir.path(), Duration::from_millis(50)).await;
        for _ in 0..5 {
            let (a, b) = tokio::join!(engine.rqbit(), engine.rqbit());
            let (a, b) = (a.unwrap(), b.unwrap());
            assert!(a.downgrade().is(&b), "one session for both");
            drop((a, b));
            // Long enough for it to stop, so the next round starts one again.
            tokio::time::sleep(Duration::from_millis(150)).await;
            assert!(engine.rqbit.lock().await.is_none());
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_caller_racing_the_idle_stop_never_gets_a_stopped_session() {
        let dir = tempfile::tempdir().unwrap();
        let idle = Duration::from_millis(30);
        let engine = rqbit_only(dir.path(), idle).await;
        // Ask again right around the moment the session becomes idle long enough, so some
        // requests land while it is being stopped.
        for round in 0u32..40 {
            let first = engine.rqbit().await.unwrap();
            drop(first);
            tokio::time::sleep(idle + Duration::from_millis(u64::from(round % 5))).await;
            let next = engine.rqbit().await.unwrap();
            assert!(!next.is_stopping());
            assert!(runs(&next).await, "round {round}");
        }
    }

    fn file_source() -> TorrentSource {
        TorrentSource::from_file_bytes(vec![b'd', b'e']).unwrap()
    }

    #[test]
    fn parses_engine_choices() {
        assert_eq!(EngineChoice::parse("auto"), Some(EngineChoice::Auto));
        assert_eq!(EngineChoice::parse(""), Some(EngineChoice::Auto));
        assert_eq!(EngineChoice::parse(" Swarm "), Some(EngineChoice::Swarm));
        assert_eq!(
            EngineChoice::parse("torrentplayer"),
            Some(EngineChoice::Swarm)
        );
        assert_eq!(EngineChoice::parse("RQBIT"), Some(EngineChoice::Rqbit));
        assert_eq!(EngineChoice::parse("librqbit"), Some(EngineChoice::Rqbit));
        assert_eq!(EngineChoice::parse("libtorrent"), None);
    }

    #[test]
    fn auto_routes_every_source_to_torrentplayer_swarm() {
        let auto = EngineChoice::Auto;
        assert_eq!(auto.backend_for(&magnet()), BackendKind::Swarm);
        assert_eq!(auto.backend_for(&file_source()), BackendKind::Swarm);
        let url = TorrentSource::parse_link("https://example.org/a.torrent").unwrap();
        assert_eq!(auto.backend_for(&url), BackendKind::Swarm);
    }

    #[test]
    fn forced_choices() {
        assert_eq!(
            EngineChoice::Rqbit.backend_for(&file_source()),
            BackendKind::Rqbit
        );
        // Magnet metadata is fetched by librqbit, then the torrent runs in torrentplayer-swarm.
        assert_eq!(
            EngineChoice::Swarm.backend_for(&magnet()),
            BackendKind::Swarm
        );
    }

    /// A single-file torrent named `name` with `len` bytes in 16 KiB pieces.
    fn torrent_named(name: &str, len: u64) -> Vec<u8> {
        let pieces = len.div_ceil(16_384);
        let mut t = format!(
            "d4:infod6:lengthi{len}e4:name{}:{name}12:piece lengthi16384e6:pieces{}:",
            name.len(),
            pieces * 20
        )
        .into_bytes();
        t.extend(std::iter::repeat_n(
            0u8,
            usize::try_from(pieces * 20).unwrap(),
        ));
        t.extend_from_slice(b"ee");
        t
    }

    #[test]
    fn swarm_metadata_gets_the_same_path_checks() {
        for bad in ["CON", "a:b", "trailing.", "COM1.mp4"] {
            let meta = Metainfo::from_bytes(&torrent_named(bad, 10)).unwrap();
            assert!(
                matches!(
                    validate_swarm_metadata(&meta),
                    Err(EngineError::UnsafePath(_))
                ),
                "{bad}"
            );
        }
        let ok = Metainfo::from_bytes(&torrent_named("lecture.mp4", 10)).unwrap();
        assert!(validate_swarm_metadata(&ok).is_ok());
    }

    #[test]
    fn swarm_info_lists_files_with_kinds() {
        let meta = Metainfo::from_bytes(&torrent_named("lecture.mp4", 40_000)).unwrap();
        let info = swarm_info(7, &meta);
        assert_eq!(info.id, 7);
        assert_eq!(info.files.len(), 1);
        assert_eq!(info.files[0].kind, FileKind::Video);
        assert_eq!(info.files[0].len, 40_000);
        assert_eq!(info.piece_len, 16_384);
    }

    #[test]
    fn file_progress_counts_overlapping_downloaded_pieces() {
        let info = TorrentInfo {
            id: 1,
            info_hash: String::new(),
            name: String::new(),
            piece_len: 100,
            total_len: 350,
            files: vec![
                FileEntry {
                    index: 0,
                    path: "a".into(),
                    len: 150,
                    offset_in_torrent: 0,
                    kind: FileKind::Other,
                },
                FileEntry {
                    index: 2,
                    path: "b".into(),
                    len: 200,
                    offset_in_torrent: 150,
                    kind: FileKind::Other,
                },
            ],
        };
        // Pieces: 0..100, 100..200, 200..300, 300..350.
        let have = [true, true, false, true];
        // File a (0..150): piece 0 (100) + half of piece 1 (50).
        // File b (150..350): rest of piece 1 (50) + piece 3 (50).
        assert_eq!(file_progress(&info, &have), vec![150, 0, 100]);
    }
}
