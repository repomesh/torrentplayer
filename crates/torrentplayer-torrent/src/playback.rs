// SPDX-License-Identifier: MIT OR Apache-2.0

//! Carries out prioritization plans while a file is being played.
//!
//! A [`PlaybackSession`] runs a background task that re-plans whenever the playhead moves (and
//! on a short timer, as pieces arrive), then parks one engine stream per anchor. Parked streams
//! are never read; they exist only so librqbit prioritizes the pieces just after them.

use std::io::SeekFrom;
use std::ops::Range;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use tokio::io::AsyncSeekExt as _;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;
use torrentplayer_prioritize::{
    Anchor, AnchorFile, Config, FileSpan, PieceGeometry, Playback, plan,
};
use torrentplayer_stream::MediaReader;
use tracing::{debug, trace};

use crate::engine::{Engine, EngineError, TorrentId};

/// How often to re-plan when the playhead is not moving (pieces keep arriving).
const REPLAN_INTERVAL: Duration = Duration::from_millis(250);

/// Where the player is, as reported by the UI or a benchmark.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum PlayheadPosition {
    /// Playback time, plus the media duration once known.
    Time {
        /// Seconds from the start.
        position_secs: f64,
        /// Total duration in seconds, if known.
        duration_secs: Option<f64>,
    },
    /// A byte offset within the file.
    Bytes(u64),
}

/// Input to a [`PlaybackSession`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PlaybackInput {
    /// Where the playhead is.
    pub position: PlayheadPosition,
    /// The next lecture's file index, for prefetching near the end.
    pub next_file: Option<usize>,
}

impl Default for PlaybackInput {
    fn default() -> Self {
        Self {
            position: PlayheadPosition::Bytes(0),
            next_file: None,
        }
    }
}

/// What the session is currently doing, for metrics and the debug overlay.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PlaybackStatus {
    /// Playhead as a byte offset.
    pub position_bytes: u64,
    /// Media byte rate used for planning, if known.
    pub byte_rate: Option<u64>,
    /// Current read-ahead window within the file.
    pub window: Range<u64>,
    /// Contiguous downloaded bytes after the playhead.
    pub buffered_bytes: u64,
    /// Anchors currently parked, in priority order.
    pub anchors: Vec<Anchor>,
    /// Number of plans computed so far.
    pub plans: u64,
}

impl PlaybackStatus {
    /// Seconds of media buffered after the playhead, if the byte rate is known.
    #[must_use]
    pub fn buffered_secs(&self) -> Option<f64> {
        // Precision loss is irrelevant at display resolution.
        #[allow(clippy::cast_precision_loss)]
        self.byte_rate
            .filter(|&r| r > 0)
            .map(|r| self.buffered_bytes as f64 / r as f64)
    }
}

/// Prioritizes one file while it plays. Dropping it stops prioritization.
#[derive(Debug)]
pub struct PlaybackSession {
    input: watch::Sender<PlaybackInput>,
    status: Arc<Mutex<PlaybackStatus>>,
    cancel: CancellationToken,
}

impl PlaybackSession {
    /// Reports a new playhead position. Seeks take effect immediately.
    pub fn update(&self, input: PlaybackInput) {
        self.input.send_replace(input);
    }

    /// Reports a new playhead position, keeping the next lecture set with
    /// [`PlaybackSession::set_next_file`]. Seeks take effect immediately.
    pub fn update_position(&self, position: PlayheadPosition) {
        self.input.send_modify(|input| input.position = position);
    }

    /// Sets the next lecture (a file of the same torrent), or clears it with `None`. It is
    /// selected for download at once, and its head and tail are prioritized near the end of
    /// this one (see `torrentplayer_prioritize::Config::next_lecture_lead`).
    pub fn set_next_file(&self, next_file: Option<usize>) {
        self.input.send_if_modified(|input| {
            let changed = input.next_file != next_file;
            input.next_file = next_file;
            changed
        });
    }

    /// Returns a snapshot of the current status.
    #[must_use]
    pub fn status(&self) -> PlaybackStatus {
        self.status
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

impl Drop for PlaybackSession {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

impl Engine {
    /// Selects `file` for download and starts prioritizing it for playback.
    ///
    /// # Errors
    ///
    /// Returns an error if the torrent or file is unknown or librqbit fails.
    pub async fn start_playback(
        &self,
        id: TorrentId,
        file: usize,
        config: Config,
    ) -> Result<PlaybackSession, EngineError> {
        self.select_file(id, file).await?;
        let (geometry, span) = self.file_geometry(id, file)?;
        let (tx, rx) = watch::channel(PlaybackInput::default());
        let status = Arc::new(Mutex::new(PlaybackStatus::default()));
        let cancel = CancellationToken::new();
        let driver = Driver {
            engine: self.clone(),
            id,
            file,
            geometry,
            span,
            config,
            status: Arc::clone(&status),
            anchors: Vec::new(),
        };
        tokio::spawn(driver.run(rx, cancel.clone()));
        Ok(PlaybackSession {
            input: tx,
            status,
            cancel,
        })
    }
}

/// Looks up and selects the next lecture. A free function so the driver (which holds
/// non-`Sync` streams) is not borrowed across the await.
async fn resolve_next(engine: &Engine, id: TorrentId, file: usize) -> Option<(usize, FileSpan)> {
    let (_, span) = engine.file_geometry(id, file).ok()?;
    engine.select_file(id, file).await.ok()?;
    Some((file, span))
}

struct Parked {
    anchor: Anchor,
    file: usize,
    stream: Box<dyn MediaReader>,
}

struct Driver {
    engine: Engine,
    id: TorrentId,
    file: usize,
    geometry: PieceGeometry,
    span: FileSpan,
    config: Config,
    status: Arc<Mutex<PlaybackStatus>>,
    anchors: Vec<Parked>,
}

impl Driver {
    async fn run(mut self, mut input: watch::Receiver<PlaybackInput>, cancel: CancellationToken) {
        let mut tick = tokio::time::interval(REPLAN_INTERVAL);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut next_file: Option<(usize, FileSpan)> = None;
        loop {
            tokio::select! {
                () = cancel.cancelled() => break,
                changed = input.changed() => if changed.is_err() { break },
                _ = tick.tick() => {}
            }
            let current = *input.borrow_and_update();

            // Resolve (and select) the next lecture when it changes.
            if current.next_file != next_file.map(|(f, _)| f) {
                next_file = match current.next_file {
                    Some(f) => resolve_next(&self.engine, self.id, f).await,
                    None => None,
                };
            }

            if let Err(error) = self.step(current, next_file.map(|(_, s)| s)).await {
                debug!(%error, torrent = self.id, "re-planning failed; will retry");
            }
        }
        debug!(
            torrent = self.id,
            file = self.file,
            "playback session ended"
        );
    }

    async fn step(
        &mut self,
        input: PlaybackInput,
        next_span: Option<FileSpan>,
    ) -> Result<(), EngineError> {
        let (position_bytes, byte_rate) = self.resolve_position(input.position);
        let have = self.engine.have_pieces(self.id)?;
        let playback = Playback {
            file: self.span,
            position: position_bytes,
            byte_rate,
            next_file: next_span,
        };
        let piece_have = |p: u64| {
            usize::try_from(p)
                .ok()
                .and_then(|p| have.get(p))
                .copied()
                .unwrap_or(false)
        };
        let plan = plan(&self.geometry, &self.config, &playback, piece_have);
        self.reconcile(&plan.anchors, input.next_file).await?;

        let mut status = self.status.lock().unwrap_or_else(PoisonError::into_inner);
        status.position_bytes = position_bytes;
        status.byte_rate = byte_rate;
        status.window = plan.window;
        status.buffered_bytes = plan.buffered_bytes;
        status.anchors = plan.anchors;
        status.plans += 1;
        Ok(())
    }

    /// Converts the reported playhead to a byte offset and an average byte rate.
    fn resolve_position(&self, position: PlayheadPosition) -> (u64, Option<u64>) {
        let len = self.span.len;
        match position {
            PlayheadPosition::Bytes(b) => (b.min(len), None),
            PlayheadPosition::Time {
                position_secs,
                duration_secs,
            } => {
                let Some(duration) = duration_secs.filter(|d| d.is_finite() && *d > 0.0) else {
                    return (0, None);
                };
                let fraction = (position_secs / duration).clamp(0.0, 1.0);
                // Average bitrate is a good estimate until Phase 2 probes the container.
                #[allow(
                    clippy::cast_possible_truncation,
                    clippy::cast_sign_loss,
                    clippy::cast_precision_loss
                )]
                let (bytes, rate) = (
                    (fraction * len as f64) as u64,
                    (len as f64 / duration) as u64,
                );
                (bytes.min(len), Some(rate))
            }
        }
    }

    /// Moves, opens, or closes parked streams so they match `desired`, reusing streams where
    /// possible because opening one waits for a slot in librqbit's pool.
    async fn reconcile(
        &mut self,
        desired: &[Anchor],
        next_file: Option<usize>,
    ) -> Result<(), EngineError> {
        self.anchors.truncate(desired.len());
        for (i, anchor) in desired.iter().enumerate() {
            let file = match anchor.file {
                AnchorFile::Current => self.file,
                AnchorFile::Next => match next_file {
                    Some(f) => f,
                    None => continue,
                },
            };
            match self.anchors.get_mut(i) {
                Some(parked) if parked.file == file => {
                    if parked.anchor.offset != anchor.offset {
                        parked
                            .stream
                            .seek(SeekFrom::Start(anchor.offset))
                            .await
                            .map_err(|e| EngineError::Engine(e.to_string()))?;
                    }
                    parked.anchor = *anchor;
                }
                slot => {
                    let mut stream = Box::new(self.engine.open_stream(self.id, file).await?)
                        as Box<dyn MediaReader>;
                    stream
                        .seek(SeekFrom::Start(anchor.offset))
                        .await
                        .map_err(|e| EngineError::Engine(e.to_string()))?;
                    let parked = Parked {
                        anchor: *anchor,
                        file,
                        stream,
                    };
                    match slot {
                        Some(existing) => *existing = parked,
                        None => self.anchors.push(parked),
                    }
                }
            }
            trace!(?anchor, "anchor parked");
        }
        Ok(())
    }
}
