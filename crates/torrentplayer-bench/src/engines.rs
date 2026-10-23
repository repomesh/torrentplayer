// SPDX-License-Identifier: MIT OR Apache-2.0

//! The two engines under test, behind one interface so both are measured identically.

use std::io;
use std::net::{Ipv6Addr, SocketAddr};
use std::num::NonZeroU32;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

use anyhow::{Context as _, bail};
use futures_util::future::BoxFuture;
use torrentplayer_net::{IncomingConfig, Network, NetworkConfig, NetworkOptions};
use torrentplayer_stream::{MediaReader, MediaSource};
use torrentplayer_swarm::{Metainfo, Swarm, SwarmConfig, Torrent};
use torrentplayer_torrent::{
    Engine, EngineConfig, FileKind, PlaybackInput, PlaybackSession, PlayheadPosition, TorrentId,
    TorrentSource, mime_type, protected_magnet_peers,
};

use crate::BenchConfig;
use crate::network::RECOVER_TIMEOUT;

/// Which torrent engine to benchmark.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EngineKind {
    /// librqbit, wrapped by `torrentplayer-torrent` (the current default).
    Rqbit,
    /// TorrentPlayer's own streaming engine, `torrentplayer-swarm`.
    Swarm,
}

impl std::str::FromStr for EngineKind {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> anyhow::Result<Self> {
        match s {
            "rqbit" | "librqbit" => Ok(Self::Rqbit),
            "swarm" | "torrentplayer" => Ok(Self::Swarm),
            other => bail!("unknown engine {other:?}; expected `rqbit` or `swarm`"),
        }
    }
}

impl EngineKind {
    /// The engine's DHT state file in [`BenchConfig::dht_state_dir`], named as in the app's
    /// data folder; `None` for librqbit, which the app gives no saved state.
    #[must_use]
    pub fn dht_state_file(self) -> Option<&'static str> {
        match self {
            Self::Rqbit => None,
            Self::Swarm => Some("swarm.json"),
        }
    }
}

impl std::fmt::Display for EngineKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Rqbit => "rqbit",
            Self::Swarm => "swarm",
        })
    }
}

/// A file offered by an added torrent.
#[derive(Debug, Clone)]
pub(crate) struct BenchFile {
    pub(crate) index: usize,
    pub(crate) path: String,
    pub(crate) len: u64,
    pub(crate) video: bool,
}

/// Swarm statistics common to both engines.
#[derive(Debug, Clone, Copy)]
pub(crate) struct BenchStats {
    /// Bytes received but not needed (swarm engine only).
    pub(crate) wasted_bytes: Option<u64>,
    /// Duplicate requests sent (swarm engine only).
    pub(crate) duplicate_requests: Option<u64>,
    pub(crate) peers_live: u32,
    pub(crate) peers_seen: u32,
    pub(crate) download_bps: u64,
    /// Downloaded and verified bytes.
    pub(crate) downloaded_bytes: u64,
}

/// Receives the simulated player's position.
pub(crate) trait Playhead: Send + Sync {
    fn update(&self, offset: u64);
}

/// A started engine. Stop it with [`BenchEngine::finish`] on every path, failed runs included:
/// neither engine stops by itself, and a forgotten one keeps downloading from (and uploading
/// to) its swarm while later runs are measured, and keeps its files open. Dropping an engine
/// that was not finished (a cancelled run) stops it as far as that can be done without
/// waiting.
pub(crate) struct BenchEngine {
    inner: Inner,
    /// The download directory, to tell engines apart in [`running`].
    dir: PathBuf,
    finished: bool,
}

/// Download directories of the engines started and not yet stopped, in this process.
static RUNNING: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());

/// Whether an engine downloading to `dir` is still running (for tests).
#[cfg(test)]
pub(crate) fn running(dir: &Path) -> bool {
    RUNNING
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .iter()
        .any(|d| d == dir)
}

/// Download directories of the engines dropped without [`BenchEngine::finish`] (for tests).
#[cfg(test)]
static ABANDONED: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());

/// Whether the engine downloading to `dir` was dropped without [`BenchEngine::finish`].
#[cfg(test)]
pub(crate) fn abandoned(dir: &Path) -> bool {
    ABANDONED
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .iter()
        .any(|d| d == dir)
}

fn stopped(dir: &Path) {
    let mut running = RUNNING.lock().unwrap_or_else(PoisonError::into_inner);
    if let Some(i) = running.iter().position(|d| d == dir) {
        running.swap_remove(i);
    }
}

enum Inner {
    Rqbit {
        engine: Engine,
        id: Option<TorrentId>,
        /// The shared network of a protected mode ([`BenchConfig::shared_network`]), for peer
        /// discovery of magnet links as in the app.
        protected: Option<Network>,
        public_discovery: bool,
    },
    Swarm {
        swarm: Swarm,
        torrent: Option<Torrent>,
        /// The shared network of a protected mode, as for [`Inner::Rqbit`].
        protected: Option<Network>,
        /// A direct network manager of this engine's own, whose listener takes incoming peers
        /// ([`BenchConfig::incoming`]); stopped with the engine.
        owned: Option<Network>,
        /// How librqbit is set up to fetch a magnet link's metadata for this engine, as the
        /// app hands magnet links to torrentplayer-swarm.
        resolver: Box<EngineConfig>,
        public_discovery: bool,
    },
}

/// The network every engine of a run uses: the shared one of a protected mode, or `None` to
/// connect directly. Benchmarks never start a protected network per run (see
/// [`crate::network`]); one must be started once and put in [`BenchConfig::shared_network`].
fn protected_network(config: &BenchConfig) -> anyhow::Result<Option<Network>> {
    if !config.public_swarm || config.network.is_none() {
        return Ok(None);
    }
    let shared = config
        .shared_network
        .as_ref()
        .context("a protected network mode needs its shared network started first")?;
    Ok(Some(shared.network().clone()))
}

/// librqbit's settings for `config`, as the app sets them (also for the swarm engine's
/// metadata resolver).
fn rqbit_config(dir: PathBuf, config: &BenchConfig, network: Option<&Network>) -> EngineConfig {
    let mut c = EngineConfig::new(dir);
    c.enable_dht = config.public_swarm && config.dht;
    c.enable_trackers = (config.public_swarm && config.trackers) || config.local_tracker;
    c.download_limit_bps = config.download_limit_bps;
    c.peer_limit = config.peer_limit;
    // As in the app's protected modes: no DHT, listener, or UDP of librqbit's own, and peers
    // and trackers through the network's loopback bridge.
    c.network = network.cloned();
    c
}

impl BenchEngine {
    pub(crate) async fn start(
        kind: EngineKind,
        dir: PathBuf,
        config: &BenchConfig,
    ) -> anyhow::Result<Self> {
        if let Some(shared) = config
            .shared_network
            .as_ref()
            .filter(|_| config.public_swarm)
        {
            // The tunnel reconnects by itself; wait for it rather than starting another.
            shared.ready(RECOVER_TIMEOUT).await?;
        }
        let protected = protected_network(config)?;
        let public_discovery = config.dht || config.trackers;
        let inner = match kind {
            EngineKind::Rqbit => {
                let mut c = rqbit_config(dir.clone(), config, protected.as_ref());
                // librqbit runs its own listener, on every interface as in the app (direct mode
                // only). It announces no port for a loopback listener.
                if config.incoming {
                    c.listen_addr = Some((Ipv6Addr::UNSPECIFIED, 0).into());
                }
                Inner::Rqbit {
                    engine: Engine::start(c).await?,
                    id: None,
                    protected,
                    public_discovery,
                }
            }
            EngineKind::Swarm => {
                let resolver = Box::new(rqbit_config(
                    dir.join("metadata"),
                    config,
                    protected.as_ref(),
                ));
                let mut c = SwarmConfig::new(dir.clone());
                c.scheduler = config.scheduler.clone();
                c.enable_trackers =
                    (config.public_swarm && config.trackers) || config.local_tracker;
                c.enable_dht = config.public_swarm && config.dht;
                c.dht_state_file = config
                    .dht_state_dir
                    .as_ref()
                    .zip(kind.dht_state_file())
                    .map(|(d, f)| d.join(f));
                // As in the app: everything goes through the network manager of the selected
                // mode. With incoming peers, it also listens where the mode allows (direct:
                // every interface; WireGuard: inside the tunnel; SOCKS5: nowhere) and hands
                // accepted connections to the engine, which announces the port to trackers.
                let owned = if protected.is_none() && config.incoming {
                    Some(Network::start(NetworkConfig::default(), NetworkOptions::default()).await?)
                } else {
                    None
                };
                if let Some(network) = protected.as_ref().or(owned.as_ref()) {
                    if config.incoming {
                        network.set_incoming(IncomingConfig {
                            enabled: true,
                            port: 0,
                            upnp: false,
                        });
                        c.accept_incoming = true;
                    }
                    c.network = Some(network.clone());
                }
                c.allow_local_web_seeds = config.local_web_seeds;
                c.enable_web_seeds = config.web_seeds;
                c.download_limit_bps = config.download_limit_bps;
                if let Some(limit) = config.peer_limit {
                    c.peer_limit = limit;
                }
                if let Some(connecting) = config.max_connecting {
                    c.max_connecting = connecting;
                }
                let swarm = match Swarm::start(c).await {
                    Ok(swarm) => swarm,
                    Err(error) => {
                        if let Some(network) = owned {
                            network.shutdown();
                        }
                        return Err(error.into());
                    }
                };
                Inner::Swarm {
                    swarm,
                    torrent: None,
                    protected,
                    owned,
                    resolver,
                    public_discovery,
                }
            }
        };
        RUNNING
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(dir.clone());
        Ok(Self {
            inner,
            dir,
            finished: false,
        })
    }

    /// Adds the torrent and returns its name and files. As in the app, a magnet link in a
    /// protected mode gets peers from the mode's own DHT and UDP trackers first, and for the
    /// swarm engine librqbit fetches its metadata only.
    pub(crate) async fn add(
        &mut self,
        source: TorrentSource,
        mut peers: Vec<SocketAddr>,
    ) -> anyhow::Result<(String, Vec<BenchFile>)> {
        match &mut self.inner {
            Inner::Rqbit {
                engine,
                id,
                protected,
                public_discovery,
            } => {
                if let (TorrentSource::Magnet(link), Some(network)) = (&source, protected.as_ref())
                {
                    peers.extend(protected_magnet_peers(network, link, *public_discovery).await);
                }
                let info = engine.add_with_peers(source, peers).await?;
                *id = Some(info.id);
                let files = info
                    .files
                    .into_iter()
                    .map(|f| BenchFile {
                        index: f.index,
                        video: f.kind == FileKind::Video,
                        path: f.path,
                        len: f.len,
                    })
                    .collect();
                Ok((info.name, files))
            }
            Inner::Swarm {
                swarm,
                torrent,
                protected,
                resolver,
                public_discovery,
                ..
            } => {
                let bytes = match source {
                    TorrentSource::File(bytes) => bytes,
                    TorrentSource::Magnet(link) => {
                        if let Some(network) = protected.as_ref() {
                            peers.extend(
                                protected_magnet_peers(network, &link, *public_discovery).await,
                            );
                        }
                        let (bytes, seen) =
                            resolve_magnet((**resolver).clone(), link, peers.clone()).await?;
                        peers.extend(seen);
                        bytes
                    }
                    TorrentSource::Url(_) => {
                        bail!("the swarm engine needs the .torrent file; download it first")
                    }
                };
                let meta = Metainfo::from_bytes(&bytes)?;
                let name = meta.name.clone();
                let files = meta
                    .files
                    .iter()
                    .enumerate()
                    .filter(|(_, f)| !f.padding)
                    .map(|(index, f)| {
                        let path = f.path.join("/");
                        BenchFile {
                            index,
                            video: FileKind::from_name(&path) == FileKind::Video,
                            path,
                            len: f.len,
                        }
                    })
                    .collect();
                peers.sort_unstable();
                peers.dedup();
                *torrent = Some(swarm.add(meta, peers)?);
                Ok((name, files))
            }
        }
    }

    pub(crate) fn media(&self, file: &BenchFile) -> anyhow::Result<Arc<dyn MediaSource>> {
        Ok(match &self.inner {
            Inner::Rqbit { engine, id, .. } => {
                Arc::new(engine.media_source(added(*id)?, file.index)?)
            }
            Inner::Swarm { torrent, .. } => Arc::new(SwarmMedia {
                torrent: torrent.clone().context("no torrent added")?,
                file: file.index,
                len: file.len,
                content_type: mime_type(&file.path),
            }),
        })
    }

    /// Starts playback of `file`: selects it and returns where to report the playhead.
    pub(crate) async fn play(
        &self,
        file: &BenchFile,
        config: &BenchConfig,
        byte_rate: u64,
    ) -> anyhow::Result<Box<dyn Playhead>> {
        match &self.inner {
            Inner::Rqbit { engine, id, .. } => {
                let session = engine
                    .start_playback(added(*id)?, file.index, config.prioritization.clone())
                    .await?;
                Ok(Box::new(RqbitPlayhead {
                    session,
                    len: file.len,
                    duration_secs: config.duration_secs,
                }))
            }
            Inner::Swarm { torrent, .. } => {
                let torrent = torrent.clone().context("no torrent added")?;
                torrent.select_file(file.index)?;
                let playhead = SwarmPlayhead {
                    torrent,
                    file: file.index,
                    byte_rate: config.duration_secs.map(|_| byte_rate),
                };
                playhead.update(0);
                Ok(Box::new(playhead))
            }
        }
    }

    pub(crate) fn stats(&self) -> anyhow::Result<BenchStats> {
        Ok(match &self.inner {
            Inner::Rqbit { engine, id, .. } => {
                let s = engine.stats(added(*id)?)?;
                BenchStats {
                    wasted_bytes: None,
                    duplicate_requests: None,
                    peers_live: s.peers_live,
                    peers_seen: s.peers_seen,
                    download_bps: s.download_bps,
                    downloaded_bytes: s.progress_bytes,
                }
            }
            Inner::Swarm { torrent, .. } => {
                let s = torrent.as_ref().context("no torrent added")?.stats();
                // Display-only conversions of small counts and rates.
                #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                BenchStats {
                    wasted_bytes: Some(s.wasted_bytes),
                    duplicate_requests: Some(s.duplicate_requests),
                    peers_live: u32::try_from(s.peers_live).unwrap_or(u32::MAX),
                    peers_seen: u32::try_from(s.peers_seen).unwrap_or(u32::MAX),
                    download_bps: s.download_bps.max(0.0) as u64,
                    downloaded_bytes: s.downloaded_bytes,
                }
            }
        })
    }

    /// Changes the engine's download limit (bytes per second), as the app does when the
    /// setting changes.
    pub(crate) fn set_download_limit(&self, limit: Option<NonZeroU32>) {
        match &self.inner {
            Inner::Rqbit { engine, .. } => engine.set_rate_limits(limit, None),
            Inner::Swarm { swarm, .. } => swarm.set_download_limit(limit),
        }
    }

    /// Removes the torrent and stops the engine. Call it whether or not the run succeeded.
    pub(crate) async fn finish(mut self) -> anyhow::Result<()> {
        self.finished = true;
        stopped(&self.dir);
        match &mut self.inner {
            Inner::Rqbit { engine, id, .. } => {
                let removed = match id.take() {
                    Some(id) => engine.remove(id, true).await,
                    None => Ok(()),
                };
                // Stopped even if the torrent could not be removed.
                engine.shutdown().await;
                removed?;
            }
            Inner::Swarm { .. } => self.inner.stop_swarm(),
        }
        Ok(())
    }
}

impl Inner {
    /// Stops the swarm engine (it stops without waiting).
    fn stop_swarm(&mut self) {
        if let Self::Swarm {
            swarm,
            torrent,
            owned,
            ..
        } = self
        {
            if let Some(t) = torrent.take() {
                t.stop();
            }
            swarm.shutdown();
            // The shared network of a protected mode outlives the run.
            if let Some(network) = owned.take() {
                network.shutdown();
            }
        }
    }
}

impl Drop for BenchEngine {
    /// A run that was cancelled (its future dropped) never reached [`BenchEngine::finish`]:
    /// stop the engine anyway. librqbit stops asynchronously, so its shutdown is spawned.
    fn drop(&mut self) {
        if self.finished {
            return;
        }
        stopped(&self.dir);
        #[cfg(test)]
        ABANDONED
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(self.dir.clone());
        tracing::warn!("an engine was not finished; stopping it");
        match &mut self.inner {
            Inner::Rqbit { engine, id, .. } => {
                let engine = engine.clone();
                let id = id.take();
                if let Ok(runtime) = tokio::runtime::Handle::try_current() {
                    runtime.spawn(async move {
                        if let Some(id) = id {
                            let _ = engine.remove(id, true).await;
                        }
                        engine.shutdown().await;
                    });
                }
            }
            Inner::Swarm { .. } => self.inner.stop_swarm(),
        }
    }
}

/// Fetches a magnet link's metadata with librqbit (metadata only: nothing is downloaded or
/// shared), as the app does for torrentplayer-swarm. Returns the `.torrent` file and the peers
/// seen.
async fn resolve_magnet(
    config: EngineConfig,
    link: String,
    peers: Vec<SocketAddr>,
) -> anyhow::Result<(bytes::Bytes, Vec<SocketAddr>)> {
    let engine = Engine::start(config).await?;
    let resolved = engine.resolve(TorrentSource::Magnet(link), peers).await;
    engine.shutdown().await;
    let resolved = resolved?.context("librqbit already had this torrent")?;
    Ok((resolved.torrent_bytes, resolved.seen_peers))
}

fn added(id: Option<TorrentId>) -> anyhow::Result<TorrentId> {
    id.context("no torrent added")
}

struct RqbitPlayhead {
    session: PlaybackSession,
    len: u64,
    duration_secs: Option<f64>,
}

impl Playhead for RqbitPlayhead {
    fn update(&self, offset: u64) {
        let position = match self.duration_secs {
            #[allow(clippy::cast_precision_loss)]
            Some(duration) => PlayheadPosition::Time {
                position_secs: duration * offset as f64 / self.len as f64,
                duration_secs: Some(duration),
            },
            None => PlayheadPosition::Bytes(offset),
        };
        self.session.update(PlaybackInput {
            position,
            next_file: None,
        });
    }
}

struct SwarmPlayhead {
    torrent: Torrent,
    file: usize,
    byte_rate: Option<u64>,
}

impl Playhead for SwarmPlayhead {
    fn update(&self, offset: u64) {
        // The file index was validated when playback started.
        let _ = self.torrent.set_playhead(self.file, offset, self.byte_rate);
    }
}

struct SwarmMedia {
    torrent: Torrent,
    file: usize,
    len: u64,
    content_type: &'static str,
}

impl MediaSource for SwarmMedia {
    fn byte_len(&self) -> u64 {
        self.len
    }

    fn content_type(&self) -> &str {
        self.content_type
    }

    fn open(&self) -> BoxFuture<'static, io::Result<Box<dyn MediaReader>>> {
        let reader = self.torrent.reader(self.file).map_err(io::Error::other);
        Box::pin(async move { reader.map(|r| Box::new(r) as Box<dyn MediaReader>) })
    }
}

#[cfg(test)]
mod tests {
    use torrentplayer_net::NetworkMode;

    use super::*;
    use crate::network::SharedNetwork;

    #[tokio::test]
    async fn every_engine_uses_the_one_shared_network() {
        let mut config = BenchConfig {
            network: Some(NetworkMode::Direct),
            ..BenchConfig::default()
        };
        // Engines never start a protected network of their own.
        assert!(protected_network(&config).is_err());
        let network = Network::start(NetworkConfig::default(), NetworkOptions::default())
            .await
            .unwrap();
        config.shared_network = Some(SharedNetwork::wrap(network));
        let starts = SharedNetwork::starts();
        for _ in 0..3 {
            assert!(protected_network(&config).unwrap().is_some());
        }
        assert_eq!(SharedNetwork::starts(), starts);
        // Local swarms always connect directly.
        config.public_swarm = false;
        assert!(protected_network(&config).unwrap().is_none());
        config.shared_network.take().unwrap().shutdown();
    }
}
