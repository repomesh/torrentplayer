// SPDX-License-Identifier: MIT OR Apache-2.0

//! Application state: the torrent engine, the loopback stream server, and active streams.

use std::collections::HashMap;
use std::ops::RangeInclusive;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use lumen_stream::{RequestObserver, ServerConfig, StreamId, StreamServer};
use lumen_torrent::{Playback, StreamEngine, StreamEngineConfig, TorrentId};
use tokio::sync::OnceCell;

use crate::audio::TrackProbe;
use crate::error::{ErrorKind, IpcError};

/// Managed Tauri state. The engine starts lazily on first use, so a failure is reported to the
/// UI instead of preventing the window from opening.
pub(crate) struct AppState {
    core: OnceCell<Arc<Core>>,
    /// Download folder and engine choice, from the settings at start-up.
    config: StreamEngineConfig,
    /// The network manager all engine traffic goes through.
    network: lumen_net::Network,
    /// Held while the core starts, so [`AppState::unless_started`] cannot race it.
    starting: tokio::sync::Mutex<()>,
}

impl AppState {
    pub(crate) fn new(config: StreamEngineConfig, network: lumen_net::Network) -> Self {
        Self {
            core: OnceCell::new(),
            config,
            network,
            starting: tokio::sync::Mutex::new(()),
        }
    }

    pub(crate) async fn core(&self) -> Result<&Arc<Core>, IpcError> {
        if let Some(core) = self.core.get() {
            return Ok(core);
        }
        let _starting = self.starting.lock().await;
        self.core
            .get_or_try_init(|| async {
                Core::start(self.config.clone(), self.network.clone())
                    .await
                    .map(Arc::new)
            })
            .await
    }

    /// The core, if it has started.
    pub(crate) fn started(&self) -> Option<&Arc<Core>> {
        self.core.get()
    }

    /// Runs `f` only if the core has not started, keeping it from starting until `f` is done
    /// (used to delete the download cache safely). Returns `None` if the core is running.
    pub(crate) async fn unless_started<F, R>(&self, f: impl FnOnce() -> F) -> Option<R>
    where
        F: Future<Output = R>,
    {
        let _starting = self.starting.lock().await;
        if self.core.initialized() {
            None
        } else {
            Some(f().await)
        }
    }
}

/// Everything the commands operate on.
pub(crate) struct Core {
    pub(crate) engine: StreamEngine,
    pub(crate) server: StreamServer,
    pub(crate) streams: Mutex<HashMap<StreamId, ActiveStream>>,
    pub(crate) first_bytes: Arc<FirstByteObserver>,
}

/// A file being played.
pub(crate) struct ActiveStream {
    pub(crate) torrent: TorrentId,
    pub(crate) file: usize,
    pub(crate) session: Playback,
    pub(crate) metrics: PlayerMetrics,
    /// The probe of its audio and subtitle tracks, cancelled when the stream stops.
    pub(crate) tracks: TrackProbe,
}

/// Decoder-level timings reported by the webview player.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct PlayerMetrics {
    pub(crate) first_frame_ms: Option<f64>,
    pub(crate) last_seek_ms: Option<f64>,
    pub(crate) seeks: u32,
}

impl Core {
    async fn start(
        config: StreamEngineConfig,
        network: lumen_net::Network,
    ) -> Result<Self, IpcError> {
        // By default lumen-swarm for .torrent files and links, librqbit for magnet links; the
        // engine setting and LUMEN_ENGINE override (see lumen_torrent::EngineChoice).
        let engine = StreamEngine::start_with_network(config, Some(network)).await?;
        let first_bytes = Arc::new(FirstByteObserver::default());
        let server = StreamServer::start(ServerConfig {
            observer: Some(Arc::clone(&first_bytes) as Arc<dyn RequestObserver>),
            ..ServerConfig::default()
        })
        .await
        .map_err(|e| IpcError::internal(&e))?;
        tracing::info!("streaming core ready");
        Ok(Self {
            engine,
            server,
            streams: Mutex::new(HashMap::new()),
            first_bytes,
        })
    }

    /// Runs `f` on an active stream.
    pub(crate) fn with_stream<R>(
        &self,
        id: StreamId,
        f: impl FnOnce(&mut ActiveStream) -> R,
    ) -> Result<R, IpcError> {
        let mut streams = self.streams.lock().unwrap_or_else(PoisonError::into_inner);
        streams
            .get_mut(&id)
            .map(f)
            .ok_or_else(|| IpcError::new(ErrorKind::NotFound, "Unknown stream."))
    }
}

/// Records how long the stream server waited for the first bytes of each request.
#[derive(Debug, Default)]
pub(crate) struct FirstByteObserver {
    last: Mutex<HashMap<StreamId, f64>>,
}

impl FirstByteObserver {
    pub(crate) fn last_ms(&self, id: StreamId) -> Option<f64> {
        self.last
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&id)
            .copied()
    }

    pub(crate) fn forget(&self, id: StreamId) {
        self.last
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&id);
    }
}

impl RequestObserver for FirstByteObserver {
    fn on_request(&self, stream: StreamId, range: RangeInclusive<u64>) {
        tracing::debug!(
            stream,
            start = range.start(),
            end = range.end(),
            "stream request"
        );
    }

    fn on_first_byte(&self, stream: StreamId, offset: u64, waited: Duration) {
        let ms = waited.as_secs_f64() * 1000.0;
        tracing::debug!(stream, offset, ms, "first bytes served");
        self.last
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(stream, ms);
    }
}
