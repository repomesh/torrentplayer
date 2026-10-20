// SPDX-License-Identifier: MIT OR Apache-2.0

//! The two engines under test, behind one interface so both are measured identically.

use std::io;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context as _, bail};
use futures_util::future::BoxFuture;
use lumen_stream::{MediaReader, MediaSource};
use lumen_swarm::{Metainfo, Swarm, SwarmConfig, Torrent};
use lumen_torrent::{
    Engine, EngineConfig, FileKind, PlaybackInput, PlaybackSession, PlayheadPosition, TorrentId,
    TorrentSource, mime_type,
};

use crate::BenchConfig;

/// Which torrent engine to benchmark.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EngineKind {
    /// librqbit, wrapped by `lumen-torrent` (the current default).
    Rqbit,
    /// TorrentStream's own streaming engine, `lumen-swarm`.
    Swarm,
}

impl std::str::FromStr for EngineKind {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> anyhow::Result<Self> {
        match s {
            "rqbit" | "librqbit" => Ok(Self::Rqbit),
            "swarm" | "lumen" => Ok(Self::Swarm),
            other => bail!("unknown engine {other:?}; expected `rqbit` or `swarm`"),
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

pub(crate) enum BenchEngine {
    Rqbit {
        engine: Engine,
        id: Option<TorrentId>,
    },
    Swarm {
        swarm: Swarm,
        torrent: Option<Torrent>,
    },
}

impl BenchEngine {
    pub(crate) async fn start(
        kind: EngineKind,
        dir: PathBuf,
        config: &BenchConfig,
    ) -> anyhow::Result<Self> {
        Ok(match kind {
            EngineKind::Rqbit => {
                let mut c = EngineConfig::new(dir);
                c.enable_dht = config.public_swarm && config.dht;
                c.enable_trackers = config.public_swarm || config.local_tracker;
                c.download_limit_bps = config.download_limit_bps;
                c.peer_limit = config.peer_limit;
                Self::Rqbit {
                    engine: Engine::start(c).await?,
                    id: None,
                }
            }
            EngineKind::Swarm => {
                let mut c = SwarmConfig::new(dir);
                c.scheduler = config.scheduler.clone();
                c.enable_trackers = config.public_swarm || config.local_tracker;
                c.enable_dht = config.public_swarm && config.dht;
                c.allow_local_web_seeds = config.local_web_seeds;
                c.enable_web_seeds = config.web_seeds;
                c.download_limit_bps = config.download_limit_bps;
                if let Some(limit) = config.peer_limit {
                    c.peer_limit = limit;
                }
                if let Some(connecting) = config.max_connecting {
                    c.max_connecting = connecting;
                }
                Self::Swarm {
                    swarm: Swarm::start(c).await?,
                    torrent: None,
                }
            }
        })
    }

    /// Adds the torrent and returns its name and files.
    pub(crate) async fn add(
        &mut self,
        source: TorrentSource,
        peers: Vec<SocketAddr>,
    ) -> anyhow::Result<(String, Vec<BenchFile>)> {
        match self {
            Self::Rqbit { engine, id } => {
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
            Self::Swarm { swarm, torrent } => {
                let TorrentSource::File(bytes) = source else {
                    bail!("the swarm engine needs a .torrent file (magnet links arrive in M3)");
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
                *torrent = Some(swarm.add(meta, peers)?);
                Ok((name, files))
            }
        }
    }

    pub(crate) fn media(&self, file: &BenchFile) -> anyhow::Result<Arc<dyn MediaSource>> {
        Ok(match self {
            Self::Rqbit { engine, id } => Arc::new(engine.media_source(added(*id)?, file.index)?),
            Self::Swarm { torrent, .. } => Arc::new(SwarmMedia {
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
        match self {
            Self::Rqbit { engine, id } => {
                let session = engine
                    .start_playback(added(*id)?, file.index, config.prioritization.clone())
                    .await?;
                Ok(Box::new(RqbitPlayhead {
                    session,
                    len: file.len,
                    duration_secs: config.duration_secs,
                }))
            }
            Self::Swarm { torrent, .. } => {
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
        Ok(match self {
            Self::Rqbit { engine, id } => {
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
            Self::Swarm { torrent, .. } => {
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

    /// Removes the torrent and stops the engine.
    pub(crate) async fn finish(self) -> anyhow::Result<()> {
        match self {
            Self::Rqbit { engine, id } => {
                if let Some(id) = id {
                    engine.remove(id, true).await?;
                }
                engine.shutdown().await;
            }
            Self::Swarm { swarm, torrent } => {
                if let Some(t) = torrent {
                    t.stop();
                }
                swarm.shutdown();
            }
        }
        Ok(())
    }
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
