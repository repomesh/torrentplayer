// SPDX-License-Identifier: MIT OR Apache-2.0

//! The librqbit session wrapper.

use std::borrow::Cow;
use std::collections::HashSet;
use std::io;
use std::net::SocketAddr;
use std::num::NonZeroU32;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use futures_util::future::BoxFuture;
use librqbit::api::{Api, TorrentIdOrHash};
use librqbit::limits::LimitsConfig;
use librqbit::{
    AddTorrent, AddTorrentOptions, AddTorrentResponse, ConnectionOptions, DhtSessionConfig,
    ListenerOptions, ManagedTorrent, Session, SessionOptions, TorrentStatsState,
};
use lumen_net::Network;
use lumen_prioritize::{FileSpan, PieceGeometry};
use lumen_stream::{MediaReader, MediaSource};
use tracing::{debug, info};

use crate::media::{FileKind, mime_type};
use crate::paths::{UnsafePath, validate_components};
use crate::source::{SourceError, TorrentSource, fetch_torrent_file, http_client_builder};

/// Identifies a torrent within an [`Engine`].
pub type TorrentId = usize;

/// Most files accepted in one torrent.
pub const MAX_FILES: usize = 50_000;

/// Engine settings.
#[derive(Debug, Clone)]
pub struct EngineConfig {
    /// Where downloaded files are stored. Each multi-file torrent gets a sub-folder.
    pub download_dir: PathBuf,
    /// Accept incoming peer connections on this address. `None` makes outgoing connections
    /// only, which avoids firewall prompts but limits speed (configurable from Phase 5).
    pub listen_addr: Option<SocketAddr>,
    /// Use the BitTorrent DHT to find peers (needed for most magnet links).
    pub enable_dht: bool,
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
    /// TorrentStream's network manager. If a protected mode (VPN interface, proxy, or tunnel) is
    /// selected when the session starts, the session gets no DHT, no listening socket, no local
    /// discovery, and no UDP trackers, and reaches peers and trackers only through the network's
    /// loopback bridge. `None` connects directly.
    pub network: Option<Network>,
}

impl EngineConfig {
    /// Defaults suitable for the desktop app, storing downloads in `download_dir`.
    #[must_use]
    pub fn new(download_dir: PathBuf) -> Self {
        Self {
            download_dir,
            listen_addr: None,
            enable_dht: true,
            enable_trackers: true,
            enable_local_discovery: false,
            blocking_permits: 32,
            metadata_timeout: Duration::from_secs(120),
            client_name: format!("TorrentStream {}", env!("CARGO_PKG_VERSION")),
            peer_limit: None,
            download_limit_bps: None,
            upload_limit_bps: None,
            network: None,
        }
    }
}

/// Metadata fetched by [`Engine::resolve`].
pub(crate) struct Resolved {
    /// The `.torrent` file, with the magnet link's trackers.
    pub(crate) torrent_bytes: bytes::Bytes,
    /// Peers found while fetching the metadata.
    pub(crate) seen_peers: Vec<SocketAddr>,
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

/// The client for downloading `.torrent` files, routed through `network` if given.
pub(crate) fn http_for(
    user_agent: &str,
    network: Option<&Network>,
) -> Result<reqwest::Client, EngineError> {
    let mut builder = http_client_builder(user_agent);
    if let Some(network) = network {
        builder = network.configure_http(builder).map_err(engine_err)?;
    }
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

/// The torrent engine: one librqbit session plus TorrentStream's safety checks.
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
        let options = SessionOptions {
            // DHT state is not persisted yet; it moves into TorrentStream's data directory in Phase 4.
            dht: (config.enable_dht && !protected).then(|| DhtSessionConfig {
                persistence: None,
                ..DhtSessionConfig::default()
            }),
            disable_trackers: !config.enable_trackers,
            disable_local_service_discovery: !config.enable_local_discovery || protected,
            listen: config
                .listen_addr
                .filter(|_| !protected)
                .map(|listen_addr| ListenerOptions {
                    listen_addr,
                    enable_upnp_port_forwarding: false,
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
        let session = Session::new_with_opts(config.download_dir.clone(), options)
            .await
            .map_err(engine_err)?;
        let http = http_for(&config.client_name, config.network.as_ref())?;
        info!(protected, "torrent engine started");
        Ok(Self {
            inner: Arc::new(Inner {
                api: Api::new(Arc::clone(&session), None),
                session,
                http,
                config,
                protected,
            }),
        })
    }

    /// Whether the session was started for a protected network mode. Its DHT, listening
    /// socket, and proxy are fixed at start, so a session for the other kind of mode must be
    /// replaced (see `StreamEngine::apply_network`).
    #[must_use]
    pub fn protected(&self) -> bool {
        self.inner.protected
    }

    /// What librqbit is given for `source`. In a protected session, magnet links lose their UDP
    /// trackers (and every tracker if trackers are off), and `.torrent` files are refused:
    /// their tracker lists cannot be filtered, and librqbit would contact UDP trackers directly.
    async fn session_source(
        &self,
        source: TorrentSource,
        has_peers: bool,
    ) -> Result<AddTorrent<'static>, EngineError> {
        match source {
            TorrentSource::Magnet(link) => {
                Ok(AddTorrent::from_url(self.session_magnet(link, has_peers)?))
            }
            _ if self.inner.protected => Err(EngineError::Unsupported(
                "While a VPN or proxy mode is on, .torrent files open in TorrentStream's engine only.                  Set the torrent engine to Automatic.",
            )),
            TorrentSource::Url(url) => Ok(AddTorrent::from_bytes(
                fetch_torrent_file(&self.inner.http, &url).await?,
            )),
            TorrentSource::File(bytes) => Ok(AddTorrent::from_bytes(bytes)),
        }
    }

    fn session_magnet(&self, link: String, has_peers: bool) -> Result<String, EngineError> {
        let Some(network) = self.inner.config.network.as_ref() else {
            return Ok(link);
        };
        if !self.inner.protected {
            return Ok(link);
        }
        let trackers_allowed = network.policy().trackers_allowed;
        let mut url = url::Url::parse(&link).map_err(|_| SourceError::Invalid)?;
        let pairs: Vec<(String, String)> = url
            .query_pairs()
            .filter(|(key, value)| {
                key != "tr"
                    || (trackers_allowed
                        && (value.starts_with("http://") || value.starts_with("https://")))
            })
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect();
        if !has_peers && !pairs.iter().any(|(key, _)| key == "tr") {
            return Err(EngineError::Unsupported(
                "No peers or usable HTTP trackers were found through the selected network. The proxy may not support UDP; try another tracker or a .torrent file.",
            ));
        }
        url.query_pairs_mut().clear().extend_pairs(pairs);
        Ok(url.into())
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
        let session = &self.inner.session;
        let first = self.session_source(source, !peers.is_empty()).await?;

        // Step 1: fetch and inspect the metadata without creating any files.
        let list_only = AddTorrentOptions {
            list_only: true,
            initial_peers: Some(peers.clone()),
            ..AddTorrentOptions::default()
        };
        let listed = tokio::time::timeout(
            self.inner.config.metadata_timeout,
            session.add_torrent(first, Some(list_only)),
        )
        .await
        .map_err(|_| EngineError::MetadataTimeout)?
        .map_err(engine_err)?;
        let listed = match listed {
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
            // The download directory belongs to TorrentStream; allow resuming over earlier data.
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
    /// Used to hand magnet links to lumen-swarm, which cannot fetch metadata itself.
    ///
    /// # Errors
    ///
    /// As for [`Engine::add`].
    pub(crate) async fn resolve(
        &self,
        source: TorrentSource,
        peers: Vec<SocketAddr>,
    ) -> Result<Option<Resolved>, EngineError> {
        let first = self.session_source(source, !peers.is_empty()).await?;
        let list_only = AddTorrentOptions {
            list_only: true,
            initial_peers: Some(peers),
            ..AddTorrentOptions::default()
        };
        let listed = tokio::time::timeout(
            self.inner.config.metadata_timeout,
            self.inner.session.add_torrent(first, Some(list_only)),
        )
        .await
        .map_err(|_| EngineError::MetadataTimeout)?
        .map_err(engine_err)?;
        let AddTorrentResponse::ListOnly(listed) = listed else {
            return Ok(None);
        };
        validate_metadata(&listed.info)?;
        Ok(Some(Resolved {
            torrent_bytes: listed.torrent_bytes,
            seen_peers: listed.seen_peers,
        }))
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
    }

    /// Removes a file from the set being downloaded. Others are left alone; what is downloaded
    /// stays.
    ///
    /// # Errors
    ///
    /// Returns an error if the torrent is unknown or librqbit fails.
    pub async fn deselect_file(&self, id: TorrentId, file: usize) -> Result<(), EngineError> {
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
        handle.stream(file).await.map_err(engine_err)
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
        self.inner
            .session
            .delete(TorrentIdOrHash::Id(id), delete_files)
            .await
            .map_err(engine_err)
    }

    /// Stops every torrent and closes connections.
    pub async fn shutdown(&self) {
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
