// SPDX-License-Identifier: MIT OR Apache-2.0

//! The librqbit session wrapper.

use std::borrow::Cow;
use std::collections::HashSet;
use std::io;
use std::net::SocketAddr;
use std::num::NonZeroU32;
use std::panic::AssertUnwindSafe;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use futures_util::FutureExt as _;
use futures_util::future::BoxFuture;
use librqbit::api::{Api, TorrentIdOrHash};
use librqbit::dht::DhtPersistenceConfig;
use librqbit::limits::LimitsConfig;
use librqbit::{
    AddTorrent, AddTorrentOptions, AddTorrentResponse, ConnectionOptions, DhtSessionConfig,
    ListenerOptions, ManagedTorrent, Session, SessionOptions, TorrentStatsState,
};
use tokio::sync::RwLock;
use tokio_util::sync::CancellationToken;
use torrentplayer_net::{ModeKind, Network};
use torrentplayer_prioritize::{FileSpan, PieceGeometry};
use torrentplayer_stream::{MediaReader, MediaSource};
use tracing::{debug, info, warn};

use crate::media::{FileKind, mime_type};
use crate::paths::{UnsafePath, validate_components};
use crate::source::{
    PublicResolver, SourceError, TorrentSource, fetch_torrent_file, http_client_builder,
};

/// Identifies a torrent within an [`Engine`].
pub type TorrentId = usize;

/// Most files accepted in one torrent.
pub const MAX_FILES: usize = 50_000;

/// Engine settings.
// Independent switches, each passed to a different librqbit option.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone)]
pub struct EngineConfig {
    /// Where downloaded files are stored. Each multi-file torrent gets a sub-folder.
    pub download_dir: PathBuf,
    /// Accept incoming peer connections on this address (TCP only). `None`, the default, makes
    /// outgoing connections only, which avoids firewall prompts but limits speed. Ignored in
    /// protected network modes: librqbit's listener cannot be pinned to the protected path.
    pub listen_addr: Option<SocketAddr>,
    /// Ask the router to forward the listening port (UPnP), with `listen_addr`.
    pub enable_upnp: bool,
    /// Use the BitTorrent DHT to find peers (needed for most magnet links).
    pub enable_dht: bool,
    /// Where the DHT's routing tables and port are kept between runs (direct mode only).
    /// `None` starts from the bootstrap nodes every time. The app sets none: a restored table
    /// slowed magnet metadata (see `StreamEngineConfig::dht_state_dir`).
    pub dht_state_file: Option<PathBuf>,
    /// Announce to trackers listed in torrents.
    pub enable_trackers: bool,
    /// Discover peers on the local network.
    pub enable_local_discovery: bool,
    /// Size of librqbit's pool for blocking work. Every open file stream holds one slot for its
    /// lifetime, and disk I/O shares the pool, so it must exceed the number of streams.
    pub blocking_permits: usize,
    /// How long to wait for a magnet link's metadata.
    pub metadata_timeout: Duration,
    /// Client name and version announced to peers and trackers.
    pub client_name: String,
    /// Maximum connected peers per torrent (librqbit default if `None`). Each peer keeps a fixed
    /// 2 MiB of requests in flight, so on a bandwidth-limited link fewer peers means newly
    /// prioritized pieces wait behind less queued data.
    pub peer_limit: Option<usize>,
    /// Maximum download rate in bytes per second, if limited.
    pub download_limit_bps: Option<NonZeroU32>,
    /// Maximum upload rate in bytes per second, if limited.
    pub upload_limit_bps: Option<NonZeroU32>,
    /// TorrentPlayer's network manager. If a protected mode (proxy or tunnel) is
    /// selected when the session starts, the session gets no DHT, no listening socket, no local
    /// discovery, and no UDP trackers, and reaches peers and trackers only through the network's
    /// loopback bridge. `None` connects directly.
    pub network: Option<Network>,
    /// Download `.torrent` links (and follow their redirects) to the local network too. Off by
    /// default: links come from untrusted sites. For tests with a local server.
    pub allow_local_links: bool,
}

impl EngineConfig {
    /// Defaults suitable for the desktop app, storing downloads in `download_dir`.
    #[must_use]
    pub fn new(download_dir: PathBuf) -> Self {
        Self {
            download_dir,
            listen_addr: None,
            enable_upnp: false,
            enable_dht: true,
            dht_state_file: None,
            enable_trackers: true,
            enable_local_discovery: false,
            blocking_permits: 32,
            metadata_timeout: Duration::from_secs(120),
            client_name: format!("TorrentPlayer {}", env!("CARGO_PKG_VERSION")),
            peer_limit: None,
            download_limit_bps: None,
            upload_limit_bps: None,
            network: None,
            allow_local_links: false,
        }
    }
}

/// Metadata fetched by [`Engine::resolve`].
#[derive(Debug, Clone)]
pub struct Resolved {
    /// The `.torrent` file, with the magnet link's trackers.
    pub torrent_bytes: bytes::Bytes,
    /// Peers found while fetching the metadata.
    pub seen_peers: Vec<SocketAddr>,
}

/// Why a protected session refuses `.torrent` files and links (see `Engine::session_source`).
const PROTECTED_TORRENT_FILE: &str = "While a VPN or proxy mode is on, .torrent files open in \
     TorrentPlayer's engine only. Set the torrent engine to Automatic.";

/// Why an operation ended early: its session was stopped, usually for a network mode change.
const SESSION_STOPPED: &str = "The network settings changed while this torrent was opening. \
     Try again.";

/// Why a magnet link cannot be resolved in a protected mode: no peers were found through the
/// DHT and trackers of the mode (see `protected_magnet_peers`). The hint names what usually
/// causes it in each mode.
fn no_magnet_peers(mode: ModeKind) -> &'static str {
    match mode {
        ModeKind::WireGuard => {
            "No peers were found for this magnet link through the WireGuard tunnel. Check that \
             the tunnel is connected (its server completed a recent handshake); the DHT and \
             trackers run inside it. Or try a .torrent file."
        }
        ModeKind::Socks5 => {
            "No peers were found for this magnet link through the SOCKS5 proxy. The proxy may \
             not support UDP (UDP ASSOCIATE), which the DHT and UDP trackers need; try a \
             .torrent file or a link with an HTTP tracker."
        }
        ModeKind::Direct => {
            "No peers were found for this magnet link; try a .torrent file or a link with an \
             HTTP tracker."
        }
    }
}

/// Errors from the torrent engine.
#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    /// The torrent source was refused.
    #[error(transparent)]
    Source(#[from] SourceError),
    /// A file path in the torrent is unsafe; the torrent was refused.
    #[error("the torrent contains an unsafe file path: {0}")]
    UnsafePath(#[from] UnsafePath),
    /// The torrent has more than [`MAX_FILES`] files.
    #[error("the torrent has too many files")]
    TooManyFiles,
    /// The metadata of a magnet link could not be found in time.
    #[error("timed out looking for the torrent's metadata; there may be no peers")]
    MetadataTimeout,
    /// No torrent with that id.
    #[error("unknown torrent")]
    UnknownTorrent,
    /// No playable file with that index.
    #[error("unknown file")]
    UnknownFile,
    /// The `.torrent` file could not be parsed. The message describes the format problem and
    /// contains no local paths.
    #[error("the .torrent file is invalid: {0}")]
    InvalidTorrent(String),
    /// The selected engine cannot handle this request. The message is safe to show.
    #[error("{0}")]
    Unsupported(&'static str),
    /// librqbit reported an error. The message may contain local paths: log it, but do not
    /// show it to users verbatim.
    #[error("torrent engine error: {0}")]
    Engine(String),
}

fn engine_err(e: impl std::fmt::Display) -> EngineError {
    EngineError::Engine(format!("{e:#}"))
}

/// The client for downloading `.torrent` files: routed through `network` if given, and to public
/// hosts only unless `allow_local` (see `crate::source::http_client_builder`).
pub(crate) fn http_for(
    user_agent: &str,
    network: Option<&Network>,
    allow_local: bool,
) -> Result<reqwest::Client, EngineError> {
    let builder = http_client_builder(user_agent, allow_local);
    let builder = match (network, allow_local) {
        (Some(network), false) => network.configure_http_public(builder).map_err(engine_err)?,
        (Some(network), true) => network.configure_http(builder).map_err(engine_err)?,
        (None, false) => builder.dns_resolver(PublicResolver),
        (None, true) => builder,
    };
    builder.build().map_err(engine_err)
}

/// One file in a torrent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileEntry {
    /// librqbit's index for the file.
    pub index: usize,
    /// Path inside the torrent, `/`-separated.
    pub path: String,
    /// Length in bytes.
    pub len: u64,
    /// Absolute offset of the file in the torrent.
    pub offset_in_torrent: u64,
    /// Classification by extension.
    pub kind: FileKind,
}

/// Metadata of an added torrent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TorrentInfo {
    /// The torrent's id in this engine.
    pub id: TorrentId,
    /// Hex info hash.
    pub info_hash: String,
    /// Display name.
    pub name: String,
    /// Piece length in bytes.
    pub piece_len: u32,
    /// Total length of all files in bytes.
    pub total_len: u64,
    /// Files, excluding BEP 47 padding files and symlinks.
    pub files: Vec<FileEntry>,
}

/// Lifecycle state of a torrent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TorrentState {
    /// Checking existing data or resolving metadata.
    Initializing,
    /// Connected to the swarm.
    Live,
    /// Paused.
    Paused,
    /// Stopped by an error.
    Error,
}

/// Live statistics for a torrent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TorrentStats {
    /// Lifecycle state.
    pub state: TorrentState,
    /// Download rate in bytes per second.
    pub download_bps: u64,
    /// Upload rate in bytes per second.
    pub upload_bps: u64,
    /// Piece payload bytes uploaded since the torrent was added.
    pub uploaded_bytes: u64,
    /// Peers with an established connection.
    pub peers_live: u32,
    /// Peers being connected to.
    pub peers_connecting: u32,
    /// Distinct peers learned about.
    pub peers_seen: u32,
    /// Downloaded and verified bytes per file, indexed like [`FileEntry::index`].
    pub file_progress: Vec<u64>,
    /// Downloaded and verified bytes of the selected files.
    pub progress_bytes: u64,
    /// The engine's error message, if the torrent stopped with one. May contain paths.
    pub error: Option<String>,
}

/// The torrent engine: one librqbit session plus TorrentPlayer's safety checks.
///
/// Cheap to clone; clones share the session.
#[derive(Clone)]
pub struct Engine {
    inner: Arc<Inner>,
}

struct Inner {
    session: Arc<Session>,
    api: Api,
    http: reqwest::Client,
    config: EngineConfig,
    /// Whether the session was started for a protected network mode.
    protected: bool,
    /// Cancelled by [`Engine::shutdown`]: librqbit operations in flight end at once.
    /// librqbit's own `Session::stop` does not end them, as they run in the caller's task (a
    /// magnet link's metadata fetch connects to peers for up to `metadata_timeout`).
    stopping: CancellationToken,
    /// Held shared by every librqbit operation, and exclusively by [`Engine::shutdown`], which
    /// so waits until the operations in flight have ended.
    operations: RwLock<()>,
}

impl std::fmt::Debug for Engine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Engine")
            .field("config", &self.inner.config)
            .finish_non_exhaustive()
    }
}

impl Engine {
    /// Starts a session. Must be called from inside a multi-threaded Tokio runtime.
    ///
    /// # Errors
    ///
    /// Returns an error if the download directory cannot be created or librqbit fails to
    /// start (for example, if the listen port is taken).
    pub async fn start(config: EngineConfig) -> Result<Self, EngineError> {
        tokio::fs::create_dir_all(&config.download_dir)
            .await
            .map_err(engine_err)?;
        // UDP (DHT, uTP, UDP trackers, local discovery) cannot go through the bridge, so a
        // protected session has none of it, and no listening socket.
        let protected = config
            .network
            .as_ref()
            .is_some_and(|n| n.policy().protected);
        let session = match Self::session(&config, protected, true).await {
            Ok(session) => session,
            // The saved DHT port may be taken now; a fresh node picks another.
            Err(error) if config.dht_state_file.is_some() && !protected => {
                warn!(%error, "starting librqbit without its saved DHT state");
                Self::session(&config, protected, false).await?
            }
            Err(error) => return Err(error),
        };
        let http = http_for(
            &config.client_name,
            config.network.as_ref(),
            config.allow_local_links,
        )?;
        info!(protected, "torrent engine started");
        Ok(Self {
            inner: Arc::new(Inner {
                api: Api::new(Arc::clone(&session), None),
                session,
                http,
                config,
                protected,
                stopping: CancellationToken::new(),
                operations: RwLock::new(()),
            }),
        })
    }

    /// Runs a librqbit operation, unless the session is stopping. One in flight when
    /// [`Engine::shutdown`] starts is dropped at once, closing its connections, and the session
    /// stops only after that. Operations must not nest: shutdown waits between them.
    async fn operation<T>(
        &self,
        op: impl Future<Output = Result<T, EngineError>>,
    ) -> Result<T, EngineError> {
        let _running = self.inner.operations.read().await;
        if self.inner.stopping.is_cancelled() {
            return Err(EngineError::Unsupported(SESSION_STOPPED));
        }
        tokio::select! {
            biased;
            () = self.inner.stopping.cancelled() => Err(EngineError::Unsupported(SESSION_STOPPED)),
            result = op => result,
        }
    }

    /// A librqbit session for `config`. With `persist_dht`, the DHT's routing tables and port
    /// are loaded from and saved to [`EngineConfig::dht_state_file`]. A protected session has
    /// no DHT at all.
    async fn session(
        config: &EngineConfig,
        protected: bool,
        persist_dht: bool,
    ) -> Result<Arc<Session>, EngineError> {
        let persistence = config
            .dht_state_file
            .clone()
            .filter(|_| persist_dht)
            .map(|file| DhtPersistenceConfig {
                config_filename: Some(file),
                dump_interval: None,
            });
        let options = SessionOptions {
            dht: (config.enable_dht && !protected).then(|| DhtSessionConfig {
                persistence,
                ..DhtSessionConfig::default()
            }),
            // A protected session never contacts trackers itself (see `session_magnet`).
            disable_trackers: !config.enable_trackers || protected,
            disable_local_service_discovery: !config.enable_local_discovery || protected,
            listen: config
                .listen_addr
                .filter(|_| !protected)
                .map(|listen_addr| ListenerOptions {
                    listen_addr,
                    enable_upnp_port_forwarding: config.enable_upnp,
                    ..ListenerOptions::default()
                }),
            connect: config
                .network
                .as_ref()
                .filter(|_| protected)
                .map(|network| ConnectionOptions {
                    proxy_url: Some(network.bridge_proxy_url().expose().to_owned()),
                    ..ConnectionOptions::default()
                }),
            runtime_worker_threads: Some(config.blocking_permits.max(8)),
            client_name_and_version: Some(config.client_name.clone()),
            peer_limit: config.peer_limit,
            ratelimits: LimitsConfig {
                download_bps: config.download_limit_bps,
                upload_bps: config.upload_limit_bps,
            },
            persistence: None,
            fastresume: false,
            ..SessionOptions::default()
        };
        Session::new_with_opts(config.download_dir.clone(), options)
            .await
            .map_err(engine_err)
    }

    /// Whether the session was started for a protected network mode. Its DHT, listening
    /// socket, and proxy are fixed at start, so a session for the other kind of mode must be
    /// replaced (see `StreamEngine::apply_network`).
    #[must_use]
    pub fn protected(&self) -> bool {
        self.inner.protected
    }

    /// What librqbit is given for `source`. In a protected session, magnet links lose every
    /// tracker (see `Engine::session_magnet`), and `.torrent` files are refused: their tracker
    /// lists cannot be filtered, and librqbit would contact their trackers itself.
    async fn session_source(
        &self,
        source: TorrentSource,
        has_peers: bool,
    ) -> Result<AddTorrent<'static>, EngineError> {
        match source {
            TorrentSource::Magnet(link) => {
                Ok(AddTorrent::from_url(self.session_magnet(link, has_peers)?))
            }
            _ if self.inner.protected => Err(EngineError::Unsupported(PROTECTED_TORRENT_FILE)),
            TorrentSource::Url(url) => Ok(AddTorrent::from_bytes(checked_torrent(
                fetch_torrent_file(&self.inner.http, &url, self.inner.config.allow_local_links)
                    .await?,
            )?)),
            TorrentSource::File(bytes) => Ok(AddTorrent::from_bytes(checked_torrent(bytes)?)),
        }
    }

    fn session_magnet(&self, link: String, has_peers: bool) -> Result<String, EngineError> {
        let Some(network) = self.inner.config.network.as_ref() else {
            return Ok(link);
        };
        if !self.inner.protected {
            return Ok(link);
        }
        // No trackers at all: librqbit reaches HTTP trackers through a `socks5://` proxy URL
        // (its peer connector accepts no other), so its HTTP client would resolve their host
        // names with the system resolver, outside the tunnel or proxy and the kill switch, and
        // it would keep announcing whatever the tracker setting later says. The caller
        // announces to them through the network instead and passes the peers they name (see
        // `protected_magnet_peers`).
        if !has_peers {
            return Err(EngineError::Unsupported(no_magnet_peers(
                network.status().mode,
            )));
        }
        let mut url = url::Url::parse(&link).map_err(|_| SourceError::Invalid)?;
        let pairs: Vec<(String, String)> = url
            .query_pairs()
            .filter(|(key, _)| key != "tr")
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect();
        url.query_pairs_mut().clear().extend_pairs(pairs);
        Ok(url.into())
    }

    /// Where the session was asked to listen, and whether with UPnP; `None` if it was not
    /// (always the case in a protected session).
    #[must_use]
    pub fn requested_listen(&self) -> Option<(SocketAddr, bool)> {
        self.inner
            .config
            .listen_addr
            .filter(|_| !self.inner.protected)
            .map(|addr| (addr, self.inner.config.enable_upnp))
    }

    /// The address peers can connect to, if listening is enabled.
    #[must_use]
    pub fn listen_addr(&self) -> Option<SocketAddr> {
        self.inner.session.listen_addr()
    }

    /// Adds a torrent without selecting any files, after validating every file path.
    ///
    /// For magnet links this waits (up to [`EngineConfig::metadata_timeout`]) until the
    /// metadata has been fetched from peers.
    ///
    /// # Errors
    ///
    /// Returns an error if the source is refused, the metadata cannot be fetched, a file path
    /// is unsafe, or librqbit fails.
    pub async fn add(&self, source: TorrentSource) -> Result<TorrentInfo, EngineError> {
        self.add_with_peers(source, Vec::new()).await
    }

    /// Like [`Engine::add`], but also connects to `peers` directly. Used by tests and
    /// benchmarks with a local seeder.
    ///
    /// # Errors
    ///
    /// As for [`Engine::add`].
    pub async fn add_with_peers(
        &self,
        source: TorrentSource,
        peers: Vec<SocketAddr>,
    ) -> Result<TorrentInfo, EngineError> {
        self.operation(self.add_now(source, peers)).await
    }

    async fn add_now(
        &self,
        source: TorrentSource,
        peers: Vec<SocketAddr>,
    ) -> Result<TorrentInfo, EngineError> {
        let session = &self.inner.session;
        let first = self.session_source(source, !peers.is_empty()).await?;

        // Step 1: fetch and inspect the metadata without creating any files.
        let listed = match self.list_only(first, peers.clone()).await? {
            AddTorrentResponse::ListOnly(listed) => listed,
            AddTorrentResponse::AlreadyManaged(id, _) | AddTorrentResponse::Added(id, _) => {
                return self.info(id);
            }
        };
        validate_metadata(&listed.info)?;

        // Step 2: add it for real, from the validated bytes, with no files selected. Files are
        // selected only when the user plays or queues them.
        let mut initial_peers = listed.seen_peers;
        initial_peers.extend(peers);
        let add = AddTorrentOptions {
            only_files: Some(Vec::new()),
            // The download directory belongs to TorrentPlayer; allow resuming over earlier data.
            overwrite: true,
            initial_peers: Some(initial_peers),
            // Keep every torrent in an independently reclaimable directory. The metadata was
            // validated above, and an info hash is always a safe single path component.
            sub_folder: Some(listed.info_hash.as_string()),
            ..AddTorrentOptions::default()
        };
        let response = session
            .add_torrent(AddTorrent::from_bytes(listed.torrent_bytes), Some(add))
            .await
            .map_err(engine_err)?;
        let id = match response {
            AddTorrentResponse::Added(id, _) | AddTorrentResponse::AlreadyManaged(id, _) => id,
            AddTorrentResponse::ListOnly(_) => {
                return Err(EngineError::Engine("unexpected list-only response".into()));
            }
        };
        debug!(torrent = id, "torrent added");
        self.info(id)
    }

    /// Fetches and validates a torrent's metadata without adding it: no files are created and
    /// no content is downloaded or uploaded. Returns `None` if librqbit already manages it.
    /// Used to hand magnet links to torrentplayer-swarm, which cannot fetch metadata itself.
    ///
    /// # Errors
    ///
    /// As for [`Engine::add`].
    pub async fn resolve(
        &self,
        source: TorrentSource,
        peers: Vec<SocketAddr>,
    ) -> Result<Option<Resolved>, EngineError> {
        // A protected session was given the magnet link without its trackers; the torrent
        // still lists them for the engine it is handed to, which contacts them safely.
        let trackers = match &source {
            TorrentSource::Magnet(link) if self.inner.protected => magnet_trackers(link),
            _ => Vec::new(),
        };
        self.operation(async {
            let first = self.session_source(source, !peers.is_empty()).await?;
            let AddTorrentResponse::ListOnly(listed) = self.list_only(first, peers).await? else {
                return Ok(None);
            };
            validate_metadata(&listed.info)?;
            let torrent_bytes = if trackers.is_empty() {
                listed.torrent_bytes
            } else {
                with_trackers(&listed.torrent_bytes, &trackers)?
            };
            Ok(Some(Resolved {
                torrent_bytes,
                seen_peers: listed.seen_peers,
            }))
        })
        .await
    }

    /// Fetches a torrent's metadata (from peers, for a magnet link) without adding it.
    async fn list_only(
        &self,
        source: AddTorrent<'static>,
        peers: Vec<SocketAddr>,
    ) -> Result<AddTorrentResponse, EngineError> {
        let list_only = AddTorrentOptions {
            list_only: true,
            initial_peers: Some(peers),
            ..AddTorrentOptions::default()
        };
        // `.torrent` files are checked before they get here (`session_source`), but metadata
        // from peers is not: librqbit's checks panic on some invalid lengths.
        let fetch = AssertUnwindSafe(self.inner.session.add_torrent(source, Some(list_only)))
            .catch_unwind();
        let listed = tokio::time::timeout(self.inner.config.metadata_timeout, fetch)
            .await
            .map_err(|_| EngineError::MetadataTimeout)?
            .map_err(|_| EngineError::InvalidTorrent("the torrent's lengths are invalid".into()))?
            .map_err(engine_err)?;
        if let AddTorrentResponse::ListOnly(listed) = &listed {
            torrentplayer_swarm::check_lengths(&listed.torrent_bytes)
                .map_err(|e| EngineError::InvalidTorrent(e.to_string()))?;
        }
        Ok(listed)
    }

    fn handle(&self, id: TorrentId) -> Result<Arc<ManagedTorrent>, EngineError> {
        self.inner
            .session
            .get(TorrentIdOrHash::Id(id))
            .ok_or(EngineError::UnknownTorrent)
    }

    /// Returns a torrent's metadata.
    ///
    /// # Errors
    ///
    /// Returns an error if the torrent is unknown or its metadata is not resolved.
    pub fn info(&self, id: TorrentId) -> Result<TorrentInfo, EngineError> {
        let handle = self.handle(id)?;
        let info_hash = handle.info_hash().as_string();
        let name = handle.name().unwrap_or_else(|| info_hash.clone());
        handle
            .with_metadata(|meta| {
                let lengths = meta.info.lengths();
                let files = meta
                    .file_infos
                    .iter()
                    .enumerate()
                    .filter(|(_, f)| !f.attrs.padding && !f.attrs.symlink)
                    .map(|(index, f)| {
                        let path = f
                            .relative_filename
                            .components()
                            .map(|c| c.as_os_str().to_string_lossy())
                            .collect::<Vec<_>>()
                            .join("/");
                        FileEntry {
                            index,
                            kind: FileKind::from_name(&path),
                            path,
                            len: f.len,
                            offset_in_torrent: f.offset_in_torrent,
                        }
                    })
                    .collect();
                TorrentInfo {
                    id,
                    info_hash: info_hash.clone(),
                    name: name.clone(),
                    piece_len: lengths.default_piece_length(),
                    total_len: lengths.total_length(),
                    files,
                }
            })
            .map_err(engine_err)
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
        let entry = info
            .files
            .iter()
            .find(|f| f.index == file)
            .ok_or(EngineError::UnknownFile)?;
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

    /// Adds a file to the set being downloaded. Files already selected are left alone.
    ///
    /// # Errors
    ///
    /// Returns an error if the torrent or file is unknown or librqbit fails.
    pub async fn select_file(&self, id: TorrentId, file: usize) -> Result<(), EngineError> {
        self.operation(async {
            let handle = self.handle(id)?;
            if !self.info(id)?.files.iter().any(|f| f.index == file) {
                return Err(EngineError::UnknownFile);
            }
            handle.wait_until_initialized().await.map_err(engine_err)?;
            let mut selected: HashSet<usize> = handle
                .only_files()
                .unwrap_or_default()
                .into_iter()
                .collect();
            if selected.insert(file) {
                self.inner
                    .session
                    .update_only_files(&handle, &selected)
                    .await
                    .map_err(engine_err)?;
            }
            Ok(())
        })
        .await
    }

    /// Removes a file from the set being downloaded. Others are left alone; what is downloaded
    /// stays.
    ///
    /// # Errors
    ///
    /// Returns an error if the torrent is unknown or librqbit fails.
    pub async fn deselect_file(&self, id: TorrentId, file: usize) -> Result<(), EngineError> {
        self.operation(async {
            let handle = self.handle(id)?;
            handle.wait_until_initialized().await.map_err(engine_err)?;
            let mut selected: HashSet<usize> = handle
                .only_files()
                .unwrap_or_default()
                .into_iter()
                .collect();
            if selected.remove(&file) {
                self.inner
                    .session
                    .update_only_files(&handle, &selected)
                    .await
                    .map_err(engine_err)?;
            }
            Ok(())
        })
        .await
    }

    /// Changes the session's rate limits (bytes per second; `None` removes one), shared by
    /// every torrent.
    pub fn set_rate_limits(&self, download: Option<NonZeroU32>, upload: Option<NonZeroU32>) {
        self.inner.session.ratelimits.set_download_bps(download);
        self.inner.session.ratelimits.set_upload_bps(upload);
    }

    /// Pauses or resumes a torrent: while paused it downloads nothing and has no peers.
    ///
    /// # Errors
    ///
    /// Returns an error if the torrent is unknown or librqbit fails.
    pub async fn set_paused(&self, id: TorrentId, paused: bool) -> Result<(), EngineError> {
        self.operation(async {
            let handle = self.handle(id)?;
            if handle.is_paused() == paused {
                return Ok(());
            }
            if paused {
                self.inner.session.pause(&handle).await
            } else {
                self.inner.session.unpause(&handle).await
            }
            .map_err(engine_err)
        })
        .await
    }

    /// Opens a seekable stream over one file. While open, the engine prioritizes the pieces
    /// just ahead of the stream's position.
    ///
    /// # Errors
    ///
    /// Returns an error if the torrent or file is unknown or librqbit fails.
    pub async fn open_stream(
        &self,
        id: TorrentId,
        file: usize,
    ) -> Result<impl MediaReader + use<>, EngineError> {
        // librqbit's `FileStream` is not nameable outside the crate, hence `impl Trait`.
        let handle = self.handle(id)?;
        self.operation(async { handle.stream(file).await.map_err(engine_err) })
            .await
    }

    /// Which pieces of a torrent are downloaded and verified, indexed by piece.
    ///
    /// # Errors
    ///
    /// Returns an error if the torrent is unknown or not yet initialized.
    pub fn have_pieces(&self, id: TorrentId) -> Result<Vec<bool>, EngineError> {
        let (bits, count) = self
            .inner
            .api
            .api_dump_haves(TorrentIdOrHash::Id(id))
            .map_err(engine_err)?;
        Ok(bits.iter().take(count as usize).map(|bit| *bit).collect())
    }

    /// Current statistics for a torrent.
    ///
    /// # Errors
    ///
    /// Returns an error if the torrent is unknown.
    pub fn stats(&self, id: TorrentId) -> Result<TorrentStats, EngineError> {
        let handle = self.handle(id)?;
        let stats = handle.stats();
        let (download_bps, upload_bps) = handle.live().map_or((0, 0), |live| {
            (
                live.down_speed_estimator().bps(),
                live.up_speed_estimator().bps(),
            )
        });
        let peers = stats.live.as_ref().map(|l| &l.snapshot.peer_stats);
        Ok(TorrentStats {
            state: match stats.state {
                TorrentStatsState::Initializing { .. } => TorrentState::Initializing,
                TorrentStatsState::Live => TorrentState::Live,
                TorrentStatsState::Paused => TorrentState::Paused,
                TorrentStatsState::Error => TorrentState::Error,
            },
            download_bps,
            upload_bps,
            uploaded_bytes: stats.uploaded_bytes,
            peers_live: peers.map_or(0, |p| p.live),
            peers_connecting: peers.map_or(0, |p| p.connecting),
            peers_seen: peers.map_or(0, |p| p.seen),
            file_progress: stats.file_progress,
            progress_bytes: stats.progress_bytes,
            error: stats.error,
        })
    }

    /// Removes a torrent, optionally deleting its downloaded files.
    ///
    /// # Errors
    ///
    /// Returns an error if the torrent is unknown or librqbit fails.
    pub async fn remove(&self, id: TorrentId, delete_files: bool) -> Result<(), EngineError> {
        self.operation(async {
            self.inner
                .session
                .delete(TorrentIdOrHash::Id(id), delete_files)
                .await
                .map_err(engine_err)
        })
        .await
    }

    /// Stops every torrent and closes connections. Operations in flight (such as fetching a
    /// magnet link's metadata from peers) end first, with an error; this returns once they
    /// have, so no connection of theirs outlives it. Later operations fail at once.
    pub async fn shutdown(&self) {
        self.inner.stopping.cancel();
        let _stopped = self.inner.operations.write().await;
        self.inner.session.stop().await;
    }

    /// A [`MediaSource`] that serves one file of a torrent over the stream server.
    ///
    /// # Errors
    ///
    /// Returns an error if the torrent or file is unknown.
    pub fn media_source(&self, id: TorrentId, file: usize) -> Result<TorrentMedia, EngineError> {
        let info = self.info(id)?;
        let entry = info
            .files
            .iter()
            .find(|f| f.index == file)
            .ok_or(EngineError::UnknownFile)?;
        Ok(TorrentMedia {
            engine: self.clone(),
            id,
            file,
            len: entry.len,
            content_type: mime_type(&entry.path),
        })
    }
}

/// The `http`, `https`, and `udp` trackers (`tr`) of a magnet link, without duplicates.
fn magnet_trackers(link: &str) -> Vec<url::Url> {
    let Ok(url) = url::Url::parse(link) else {
        return Vec::new();
    };
    let mut trackers: Vec<url::Url> = Vec::new();
    for (_, value) in url.query_pairs().filter(|(key, _)| key == "tr") {
        if let Ok(tracker) = url::Url::parse(&value)
            && matches!(tracker.scheme(), "http" | "https" | "udp")
            && !trackers.contains(&tracker)
        {
            trackers.push(tracker);
        }
    }
    trackers
}

/// `torrent` (a `.torrent` file) with `trackers` as its tracker list instead of its own. The
/// info dictionary, and so the info hash, is kept byte for byte.
fn with_trackers(torrent: &[u8], trackers: &[url::Url]) -> Result<bytes::Bytes, EngineError> {
    let parsed = librqbit::torrent_from_bytes(torrent)
        .map_err(|e| EngineError::InvalidTorrent(format!("{e:#}")))?;
    let mut out = b"d13:announce-listl".to_vec();
    for tracker in trackers {
        // One tier per tracker, all tried, as for a magnet link's.
        let tracker = tracker.as_str();
        out.extend_from_slice(format!("l{}:{tracker}e", tracker.len()).as_bytes());
    }
    out.extend_from_slice(b"e4:info");
    out.extend_from_slice(parsed.info.raw_bytes.as_ref());
    out.push(b'e');
    Ok(out.into())
}

/// A `.torrent` file whose lengths librqbit can be given (see
/// [`torrentplayer_swarm::check_lengths`]): its own checks panic on some invalid ones.
fn checked_torrent(bytes: bytes::Bytes) -> Result<bytes::Bytes, EngineError> {
    torrentplayer_swarm::check_lengths(&bytes)
        .map_err(|e| EngineError::InvalidTorrent(e.to_string()))?;
    Ok(bytes)
}

/// Refuses torrents whose names or file paths could escape the download directory.
fn validate_metadata(
    info: &librqbit::ValidatedTorrentMetaV1Info<librqbit::ByteBufOwned>,
) -> Result<(), EngineError> {
    if let Some(name) = info.name()
        && !name.is_empty()
    {
        validate_components(&[name.as_ref()])?;
    }
    let mut count = 0usize;
    for details in info.iter_file_details() {
        count += 1;
        if count > MAX_FILES {
            return Err(EngineError::TooManyFiles);
        }
        let parts: Vec<Cow<'_, str>> = details.filename.iter_components().collect();
        validate_components(&parts)?;
    }
    Ok(())
}

/// One torrent file exposed as a [`MediaSource`].
#[derive(Debug, Clone)]
pub struct TorrentMedia {
    engine: Engine,
    id: TorrentId,
    file: usize,
    len: u64,
    content_type: &'static str,
}

impl MediaSource for TorrentMedia {
    fn byte_len(&self) -> u64 {
        self.len
    }

    fn content_type(&self) -> &str {
        self.content_type
    }

    fn open(&self) -> BoxFuture<'static, io::Result<Box<dyn MediaReader>>> {
        let this = self.clone();
        Box::pin(async move {
            let stream = this
                .engine
                .open_stream(this.id, this.file)
                .await
                .map_err(io::Error::other)?;
            Ok(Box::new(stream) as Box<dyn MediaReader>)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn magnet_hints_name_what_fails_in_each_mode() {
        let wireguard = no_magnet_peers(ModeKind::WireGuard);
        assert!(wireguard.contains("WireGuard tunnel") && wireguard.contains("handshake"));
        assert!(!wireguard.contains("UDP ASSOCIATE"));
        let socks5 = no_magnet_peers(ModeKind::Socks5);
        assert!(socks5.contains("UDP ASSOCIATE"));
        assert!(!socks5.contains("WireGuard"));
        assert!(!PROTECTED_TORRENT_FILE.contains("  "), "no stray spaces");
    }

    #[test]
    fn handed_over_metadata_lists_the_magnet_links_trackers() {
        let link = "magnet:?xt=urn:btih:dd8255ecdc7ca55fb0bbf81323d87062db1f6d1c\
                    &tr=http%3A%2F%2Ftracker.example%2Fannounce&tr=udp%3A%2F%2Fu.example%3A1337\
                    &tr=wss%3A%2F%2Fw.example&tr=http%3A%2F%2Ftracker.example%2Fannounce";
        let trackers = magnet_trackers(link);
        let names: Vec<&str> = trackers.iter().map(url::Url::as_str).collect();
        assert_eq!(
            names,
            ["http://tracker.example/announce", "udp://u.example:1337"]
        );

        let mut torrent =
            b"d8:announce14:http://old/ann4:infod6:lengthi4e4:name5:a.mp412:piece lengthi16384e\
              6:pieces20:"
                .to_vec();
        torrent.extend_from_slice(&[7u8; 20]);
        torrent.extend_from_slice(b"ee");
        let before = torrentplayer_swarm::Metainfo::from_bytes(&torrent).unwrap();
        let after =
            torrentplayer_swarm::Metainfo::from_bytes(&with_trackers(&torrent, &trackers).unwrap())
                .unwrap();
        assert_eq!(after.info_hash, before.info_hash);
        assert_eq!(after.trackers, trackers);
    }
}
