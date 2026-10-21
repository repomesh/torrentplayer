// SPDX-License-Identifier: MIT OR Apache-2.0

//! Downloads the play queue in the background, sharing the link with the video being watched.
//!
//! About once a second the downloader measures every queued torrent, asks
//! [`torrentplayer_torrent::bandwidth::Planner`] which items should download and how fast, and carries
//! that out: every queued item downloads at once (or as many as the user's maximum allows),
//! files of active items are selected and the others deselected, queued torrents nothing needs
//! are paused, and while playback needs the bandwidth background torrents get only what it
//! leaves (librqbit's, which cannot be limited one by one, run unlimited then) and pause while
//! it is starving. The video being watched is never paused or limited.
//!
//! The UI polls `queue_downloads` for each item's status, and pauses an item by saving the
//! queue with its `paused` flag (`save_queue` passes every saved queue on here).

use std::collections::{HashMap, HashSet};
use std::num::NonZeroU32;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use serde::Serialize;
use tauri::{AppHandle, Emitter as _, Manager as _, State};
use tokio::sync::Notify;
use torrentplayer_library::{DownloadOrder, Queue, QueueItem};
use torrentplayer_torrent::bandwidth::{self, Candidate, Foreground, Planner, Pressure};
use torrentplayer_torrent::{FileReadiness, TorrentId, TorrentSource, TorrentStats};
use ts_rs::TS;

use crate::core::{AppState, Core};
use crate::error::IpcError;

/// Sent (without a payload) when a torrent starts seeding after its playback ended, at most once
/// per playback.
pub(crate) const SEEDING_STARTED: &str = "seeding-started";

/// How often the downloader re-plans.
const TICK: Duration = Duration::from_secs(1);
/// Cache policy checks are deliberately much less frequent than download planning: they walk
/// the download folder and also run immediately after playback reaches the end.
const CACHE_CHECK: Duration = Duration::from_secs(60);
/// Give up adding a torrent after this long, and wait this long before trying it again.
const ADD_TIMEOUT: Duration = Duration::from_secs(60);
const RETRY_ADD_AFTER: Duration = Duration::from_secs(60);
/// Torrents being added at once, so a long queue does not fetch all of its metadata in one
/// burst. Downloads themselves are not limited by this.
const MAX_ADDING: usize = 4;
/// Below this rate an ETA would be meaningless.
const MIN_ETA_BPS: u64 = 16 * 1024;
/// Weight of the newest sample in a torrent's measured speed.
const RATE_WEIGHT: f64 = 0.2;
/// A torrent's speed counts as measured once it has downloaded this long.
const MEASURE_AFTER: Duration = Duration::from_secs(8);

/// Managed state shared by the commands and the downloader task.
#[derive(Clone)]
pub(crate) struct DownloadsState {
    inner: Arc<Inner>,
}

struct Inner {
    queue: Mutex<Queue>,
    snapshot: Mutex<DownloadsSnapshot>,
    wake: Notify,
    /// Serializes queue changes and stream starts with cache removal.
    cache_gate: tokio::sync::Mutex<()>,
}

impl DownloadsState {
    /// Starts with the saved queue.
    pub(crate) fn new(queue: Queue) -> Self {
        Self {
            inner: Arc::new(Inner {
                queue: Mutex::new(queue),
                snapshot: Mutex::new(DownloadsSnapshot::default()),
                wake: Notify::new(),
                cache_gate: tokio::sync::Mutex::new(()),
            }),
        }
    }

    /// Follows a newly saved queue (order, pauses, items) at once.
    pub(crate) async fn set_queue(&self, queue: Queue) {
        let _gate = self.inner.cache_gate.lock().await;
        *self
            .inner
            .queue
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = queue;
        self.inner.wake.notify_one();
    }

    pub(crate) fn queue(&self) -> Queue {
        self.inner
            .queue
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    pub(crate) fn cache_gate(&self) -> &tokio::sync::Mutex<()> {
        &self.inner.cache_gate
    }

    fn publish(&self, snapshot: DownloadsSnapshot) {
        *self
            .inner
            .snapshot
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = snapshot;
    }
}

/// What a queued item is doing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub(crate) enum ItemState {
    /// Waiting for bandwidth or a download slot.
    Queued,
    /// Its torrent is being added (metadata is being fetched).
    Starting,
    /// Downloading in the background.
    Downloading,
    /// Downloading, but no peer is connected yet.
    Connecting,
    /// Paused by the user.
    Paused,
    /// Unticked in the queue: kept, but not played next or downloaded.
    Skipped,
    /// Fully downloaded.
    Complete,
    /// Being watched.
    Watching,
    /// Its torrent could not be added; it is tried again later.
    Failed,
}

/// One item's download status.
#[derive(Debug, Clone, Serialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub(crate) struct ItemDownload {
    /// The queue item's id.
    pub id: String,
    /// What it is doing.
    pub state: ItemState,
    /// Its torrent's download rate while it downloads, bytes per second.
    pub download_bps: u64,
    /// Its torrent's upload rate (sharing with other peers), bytes per second.
    pub upload_bps: u64,
    /// Connected peers of its torrent.
    pub peers_live: u32,
    /// Peers being connected to.
    pub peers_connecting: u32,
    /// Peers learned about.
    pub peers_seen: u32,
    /// Downloaded and verified bytes of its file.
    pub downloaded: u64,
    /// Its file's length, once the torrent is known.
    pub length: Option<u64>,
    /// Seconds until it is fully downloaded at the current rate.
    pub eta_secs: Option<f64>,
    /// Its start and end are downloaded: playing it starts at once.
    pub startable: bool,
    /// Its download rate is held back for the video being watched.
    pub limited: bool,
    /// Why a queued item is not downloading now, if it is waiting.
    pub wait_reason: Option<WaitReason>,
    /// Why it failed, if it did.
    pub error: Option<String>,
}

/// Why a queued item waits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub(crate) enum WaitReason {
    /// The video being watched is short of data; background downloads pause until it recovers.
    Playback,
    /// The user's maximum of downloads at once is reached.
    Limit,
}

/// How playback and the background share the link.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub(crate) enum LinkPressure {
    /// Nothing is playing or its buffer is comfortable: the queue downloads at full speed.
    #[default]
    Relaxed,
    /// Playback's buffer is short: the queue gets what playback leaves.
    Guarded,
    /// Playback is short of data: the queue is held back.
    Starving,
}

/// The connection as the downloader sees it.
#[derive(Debug, Clone, Default, Serialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub(crate) struct LinkUse {
    /// Estimated capacity, bytes per second (0 until measured).
    pub capacity_bps: u64,
    /// Every torrent's download rate added up.
    pub used_bps: u64,
    /// Every torrent's upload rate added up.
    pub upload_bps: u64,
    /// What the video being watched needs, if one is.
    pub playback_need_bps: Option<u64>,
    /// How playback is doing.
    pub pressure: LinkPressure,
    /// The user's maximum of items downloading at once (`null`: no limit).
    pub max_active: Option<u32>,
    /// Items downloading in the background now.
    pub active: u32,
}

/// Everything `queue_downloads` returns.
#[derive(Debug, Clone, Default, Serialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub(crate) struct DownloadsSnapshot {
    /// One entry per queue item, in queue order.
    pub items: Vec<ItemDownload>,
    /// The connection.
    pub link: LinkUse,
}

/// The latest status of every queued item and of the connection.
#[tauri::command]
#[allow(
    clippy::needless_pass_by_value,
    reason = "Tauri passes managed state by value"
)]
pub(crate) fn queue_downloads(state: State<'_, DownloadsState>) -> DownloadsSnapshot {
    state
        .inner
        .snapshot
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .clone()
}

/// Starts the downloader; it runs until the app exits.
pub(crate) fn start(app: &AppHandle) {
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        let downloads = app.state::<DownloadsState>().inner().clone();
        let mut runtime = Runtime::default();
        loop {
            tokio::select! {
                () = tokio::time::sleep(TICK) => {}
                () = downloads.inner.wake.notified() => {}
            }
            let queue = downloads.queue();
            let state = app.state::<AppState>();
            // A queue with something to download starts the engine; otherwise only an engine
            // already running is looked at.
            let wanted = queue.items.iter().any(|i| !i.paused);
            let core = if wanted {
                state.core().await.ok()
            } else {
                state.started()
            };
            let Some(core) = core.cloned() else {
                downloads.publish(idle_snapshot(&queue));
                if runtime.cache_cleanup_due(Instant::now()) {
                    crate::settings::enforce_cache_policy(&app).await;
                }
                continue;
            };
            let snapshot = runtime.tick(&app, &core, &queue).await;
            downloads.publish(snapshot);
            if runtime.cache_cleanup_due(Instant::now()) {
                crate::settings::enforce_cache_policy(&app).await;
            }
        }
    });
}

fn idle_snapshot(queue: &Queue) -> DownloadsSnapshot {
    DownloadsSnapshot {
        items: queue
            .items
            .iter()
            .map(|item| ItemDownload {
                state: if item.skipped {
                    ItemState::Skipped
                } else if item.paused {
                    ItemState::Paused
                } else {
                    ItemState::Queued
                },
                ..ItemDownload::empty(&item.id)
            })
            .collect(),
        link: LinkUse::default(),
    }
}

impl ItemDownload {
    fn empty(id: &str) -> Self {
        Self {
            id: id.to_owned(),
            state: ItemState::Queued,
            download_bps: 0,
            upload_bps: 0,
            peers_live: 0,
            peers_connecting: 0,
            peers_seen: 0,
            downloaded: 0,
            length: None,
            eta_secs: None,
            startable: false,
            limited: false,
            wait_reason: None,
            error: None,
        }
    }
}

/// The downloader's own bookkeeping.
#[derive(Default)]
struct Runtime {
    planner: Planner,
    /// By torrent group (see [`group_of`]).
    torrents: HashMap<String, TorrentRun>,
    /// Items whose file the downloader selected, by item id: (torrent, file).
    selected: HashMap<String, (TorrentId, usize)>,
    /// The download and upload limits last applied from the settings.
    limits: Option<(Option<NonZeroU32>, Option<NonZeroU32>)>,
    /// Last cache walk and whether finishing playback requested one immediately.
    last_cache_cleanup: Option<Instant>,
    cleanup_requested: bool,
}

#[derive(Default)]
struct TorrentRun {
    id: Option<TorrentId>,
    adding: Option<tokio::task::JoinHandle<Result<TorrentId, String>>>,
    failed: Option<(Instant, String)>,
    /// Measured speed while it downloaded in the background.
    rate: Option<f64>,
    downloading_since: Option<Instant>,
    /// What the downloader last set.
    paused: bool,
    limit: Option<u64>,
    /// A playback of this torrent was active on the previous tick.
    was_watching: bool,
    /// When the most recent playback ended; starts the optional seeding timer.
    seed_since: Option<Instant>,
    /// [`SEEDING_STARTED`] was sent since the most recent playback ended.
    seeding_announced: bool,
    /// The torrentplayer-swarm upload switch last requested.
    upload_enabled: Option<bool>,
}

/// Measurements of one torrent this tick.
struct Measured {
    stats: TorrentStats,
    have: Vec<bool>,
}

/// Items of one torrent share it: grouped by info hash, or by link while the hash is unknown
/// (as the UI does).
fn group_of(item: &QueueItem) -> String {
    item.info_hash.as_ref().map_or_else(
        || format!("link:{}", item.source),
        |h| h.to_ascii_lowercase(),
    )
}

/// Whether a torrent may keep uploading after its playback ended. Ratio zero and time zero
/// independently mean no limit; whichever configured target is reached first stops seeding.
fn post_playback_seeding(
    since: Option<Instant>,
    bandwidth: &torrentplayer_settings::BandwidthSettings,
    downloaded: u64,
    uploaded: u64,
    now: Instant,
) -> bool {
    let Some(since) = since.filter(|_| bandwidth.seed_after_watching) else {
        return false;
    };
    let within_time = bandwidth.seed_time_limit_minutes == 0
        || now.saturating_duration_since(since)
            < Duration::from_secs(u64::from(bandwidth.seed_time_limit_minutes) * 60);
    #[allow(clippy::cast_precision_loss)]
    let below_ratio = bandwidth.seed_ratio_target == 0.0
        || downloaded == 0
        || uploaded as f64 / (downloaded as f64) < bandwidth.seed_ratio_target;
    within_time && below_ratio
}

impl Runtime {
    fn cache_cleanup_due(&mut self, now: Instant) -> bool {
        let due = self.cleanup_requested
            || self
                .last_cache_cleanup
                .is_none_or(|last| now.saturating_duration_since(last) >= CACHE_CHECK);
        if due {
            self.cleanup_requested = false;
            self.last_cache_cleanup = Some(now);
        }
        due
    }

    #[allow(clippy::too_many_lines)]
    async fn tick(
        &mut self,
        app: &AppHandle,
        core: &Arc<Core>,
        queue: &Queue,
    ) -> DownloadsSnapshot {
        let now = Instant::now();
        let engine = &core.engine;
        // The user's bandwidth limits cover everything: both engines, playback included. They
        // follow the settings within a tick, and cap the link estimate the plan is based on.
        let settings_state = app.state::<crate::settings::SettingsState>();
        let settings = settings_state.current();
        let cache_root = settings_state.active_download_dir();
        let limits = crate::settings::rate_limits(&settings.bandwidth);
        if self.limits != Some(limits) {
            engine.set_rate_limits(limits.0, limits.1).await;
            self.planner
                .meter
                .set_cap(limits.0.map(|l| u64::from(l.get())));
            self.limits = Some(limits);
        }
        self.collect_adds().await;

        // Torrents of the queue, as far as they are added.
        for item in &queue.items {
            let run = self.torrents.entry(group_of(item)).or_default();
            if run.id.is_none_or(|id| engine.info(id).is_err()) {
                run.id = item
                    .info_hash
                    .as_deref()
                    .and_then(|h| engine.torrent_with_hash(h));
            }
        }
        let groups: HashSet<String> = queue.items.iter().map(group_of).collect();
        let open_torrents: HashSet<TorrentId> = core
            .streams
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .values()
            .map(|stream| stream.torrent)
            .collect();
        let removed: Vec<TorrentId> = self
            .torrents
            .iter()
            .filter(|(group, run)| {
                !groups.contains(*group)
                    && run.adding.is_none()
                    && run.id.is_none_or(|id| !open_torrents.contains(&id))
            })
            .filter_map(|(_, run)| run.id)
            .collect();
        for id in removed {
            let _ = engine.set_upload_enabled(id, false);
            let _ = engine.set_paused(id, true).await;
        }
        self.torrents.retain(|g, run| {
            groups.contains(g)
                || run.adding.is_some()
                || run.id.is_some_and(|id| open_torrents.contains(&id))
        });

        // Measure every torrent once (the watched one and ones outside the queue included, so
        // the meter sees the whole link).
        let mut measured: HashMap<TorrentId, Measured> = HashMap::new();
        for id in engine.torrent_ids() {
            let Ok(stats) = engine.stats(id).await else {
                continue;
            };
            let have = engine.have_pieces(id).await.unwrap_or_default();
            measured.insert(id, Measured { stats, have });
        }
        let total_bps: u64 = measured.values().map(|m| m.stats.download_bps).sum();
        let total_upload_bps: u64 = measured.values().map(|m| m.stats.upload_bps).sum();

        // The video being watched (the newest stream). A stream parked at its reported end is
        // no longer foreground; seeking it back makes it foreground again on the next tick.
        let (watched, status) = {
            let streams = core.streams.lock().unwrap_or_else(PoisonError::into_inner);
            streams
                .iter()
                .max_by_key(|(id, _)| **id)
                .map_or((None, None), |(_, s)| {
                    let have = measured
                        .get(&s.torrent)
                        .map_or(&[][..], |m| m.have.as_slice());
                    let status = s.session.status(have);
                    let ended = engine.info(s.torrent).ok().is_some_and(|info| {
                        info.files
                            .iter()
                            .find(|file| file.index == s.file)
                            .is_some_and(|file| file.len > 0 && status.position_bytes >= file.len)
                    });
                    if ended {
                        (None, None)
                    } else {
                        (Some((s.torrent, s.file)), Some(status))
                    }
                })
        };
        let foreground = watched.zip(status).and_then(|((torrent, file), status)| {
            let m = measured.get(&torrent)?;
            let complete = engine
                .file_readiness(torrent, Some(file), &m.have)
                .is_ok_and(|r| r.complete());
            #[allow(clippy::cast_precision_loss)]
            let buffer_secs = status.buffered_secs().or_else(|| {
                Some(status.buffered_bytes as f64 / bandwidth::FALLBACK_NEED_BPS as f64)
            });
            Some(Foreground {
                byte_rate: status.byte_rate,
                buffer_secs,
                download_bps: m.stats.download_bps,
                complete,
            })
        });

        // Each item's file and progress.
        let readiness: HashMap<&str, FileReadiness> = queue
            .items
            .iter()
            .filter_map(|item| {
                let id = self.torrents.get(&group_of(item))?.id?;
                let m = measured.get(&id)?;
                let file = item.file_index.and_then(|f| usize::try_from(f).ok());
                let r = engine.file_readiness(id, file, &m.have).ok()?;
                Some((item.id.as_str(), r))
            })
            .collect();
        let torrent_of: HashMap<&str, TorrentId> = queue
            .items
            .iter()
            .filter_map(|item| {
                let id = self.torrents.get(&group_of(item))?.id?;
                Some((item.id.as_str(), id))
            })
            .collect();
        let watching = |item: &QueueItem| {
            let torrent = torrent_of.get(item.id.as_str()).copied();
            watched.is_some_and(|(t, f)| {
                Some(t) == torrent && readiness.get(item.id.as_str()).is_some_and(|r| r.file == f)
            })
        };
        let current = queue
            .current
            .as_ref()
            .and_then(|c| queue.items.iter().position(|i| &i.id == c));
        let up_next = current.map_or(0, |i| i + 1);

        let candidates: Vec<Candidate> = queue
            .items
            .iter()
            .enumerate()
            .filter(|(_, item)| {
                !item.paused
                    && !item.skipped
                    && !watching(item)
                    && !readiness
                        .get(item.id.as_str())
                        .is_some_and(FileReadiness::complete)
            })
            .map(|(position, item)| {
                let run = self.torrents.get(&group_of(item));
                Candidate {
                    id: item.id.clone(),
                    group: group_of(item),
                    position,
                    up_next: position == up_next,
                    skipped: current.is_some_and(|c| position <= c),
                    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                    rate_bps: run
                        .filter(|r| {
                            r.downloading_since
                                .is_some_and(|t| now.saturating_duration_since(t) >= MEASURE_AFTER)
                        })
                        .and_then(|r| r.rate)
                        .map(|r| r as u64),
                }
            })
            .collect();
        let order = match queue.order {
            DownloadOrder::InOrder => bandwidth::DownloadOrder::InOrder,
            DownloadOrder::Fastest => bandwidth::DownloadOrder::Fastest,
        };
        let max_active = usize::try_from(settings.bandwidth.max_downloads)
            .ok()
            .filter(|n| *n > 0);
        let plan = self
            .planner
            .plan(now, total_bps, foreground, &candidates, order, max_active);
        let active: HashSet<&str> = plan.active.iter().map(String::as_str).collect();

        // Start adding the torrents of active items that are not added yet, a few at a time.
        let mut adding = self
            .torrents
            .values()
            .filter(|r| r.adding.is_some())
            .count();
        for item in queue
            .items
            .iter()
            .filter(|i| active.contains(i.id.as_str()))
        {
            let run = self.torrents.entry(group_of(item)).or_default();
            let retry_due = run
                .failed
                .as_ref()
                .is_none_or(|(at, _)| now.saturating_duration_since(*at) >= RETRY_ADD_AFTER);
            if run.id.is_none() && run.adding.is_none() && retry_due && adding < MAX_ADDING {
                run.adding = Some(spawn_add(app, core, item.source.clone()));
                adding += 1;
            }
        }

        // Select active items' files; deselect the rest (never the watched file).
        let mut wanted: HashMap<TorrentId, bool> = HashMap::new();
        for item in &queue.items {
            let Some(id) = self.torrents.get(&group_of(item)).and_then(|r| r.id) else {
                continue;
            };
            let file = readiness.get(item.id.as_str()).map(|r| r.file);
            let is_active = active.contains(item.id.as_str());
            *wanted.entry(id).or_default() |= is_active;
            match (is_active, file, self.selected.get(&item.id).copied()) {
                (true, Some(file), None) if engine.download_file(id, file).await.is_ok() => {
                    self.selected.insert(item.id.clone(), (id, file));
                }
                (false, _, Some((torrent, file))) if !watching(item) => {
                    let _ = engine.stop_downloading_file(torrent, file).await;
                    self.selected.remove(&item.id);
                }
                _ => {}
            }
        }
        // Items gone from the queue stop downloading.
        let ids: HashSet<&str> = queue.items.iter().map(|i| i.id.as_str()).collect();
        let gone: Vec<(String, (TorrentId, usize))> = self
            .selected
            .iter()
            .filter(|(id, _)| !ids.contains(id.as_str()))
            .map(|(id, sel)| (id.clone(), *sel))
            .collect();
        for (id, (torrent, file)) in gone {
            if watched != Some((torrent, file)) {
                let _ = engine.stop_downloading_file(torrent, file).await;
            }
            self.selected.remove(&id);
        }

        // Pause queued torrents nothing needs; limit or pause background ones as planned.
        let watched_torrent = watched.map(|(t, _)| t);
        for (group, run) in &mut self.torrents {
            let Some(id) = run.id else {
                continue;
            };
            let Some(m) = measured.get(&id) else {
                continue;
            };
            let foreground = watched_torrent == Some(id);
            let needed = wanted.get(&id).copied().unwrap_or(false);
            if foreground {
                if !run.was_watching
                    && let Ok(info) = engine.info(id)
                {
                    crate::cache::mark_unfinished(&cache_root, &info.info_hash);
                }
                run.was_watching = true;
                run.seed_since = None;
                run.seeding_announced = false;
            } else if run.was_watching {
                run.was_watching = false;
                run.seed_since = Some(now);
                if let (Ok(info), Ok(backend)) = (engine.info(id), engine.backend(id))
                    && let Err(error) =
                        crate::cache::mark_finished(&cache_root, backend, &info.info_hash)
                {
                    tracing::warn!(%error, "could not mark a download as finished");
                }
                self.cleanup_requested = true;
            }
            let seeding = post_playback_seeding(
                run.seed_since,
                &settings.bandwidth,
                m.stats.progress_bytes,
                m.stats.uploaded_bytes,
                now,
            );
            // Once per playback end: changing the seeding settings later does not resend it.
            if seeding && !run.seeding_announced {
                run.seeding_announced = true;
                if let Err(error) = app.emit(SEEDING_STARTED, ()) {
                    tracing::warn!(%error, "could not send the seeding-started event");
                }
            }
            let upload_enabled = foreground || needed || seeding;
            if run.upload_enabled != Some(upload_enabled)
                && engine.set_upload_enabled(id, upload_enabled).is_ok()
            {
                run.upload_enabled = Some(upload_enabled);
            }
            let pause = !foreground
                && if needed {
                    plan.pause_background
                } else {
                    !seeding
                };
            let limit = if foreground || !needed {
                None
            } else {
                plan.limits.get(group).copied()
            };
            if pause != run.paused && engine.set_paused(id, pause).await.is_ok() {
                run.paused = pause;
            }
            if limit != run.limit {
                let bps = limit.and_then(|l| NonZeroU32::new(u32::try_from(l).unwrap_or(u32::MAX)));
                if engine.set_download_limit(id, bps).is_ok() {
                    run.limit = limit;
                }
            }
            // Speed is measured while it downloads in the background.
            if needed && !pause && !foreground {
                run.downloading_since.get_or_insert(now);
                #[allow(clippy::cast_precision_loss)]
                let sample = m.stats.download_bps as f64;
                run.rate = Some(
                    run.rate
                        .map_or(sample, |r| r * (1.0 - RATE_WEIGHT) + sample * RATE_WEIGHT),
                );
            } else {
                run.downloading_since = None;
            }
        }

        // The snapshot for the UI.
        let items = queue
            .items
            .iter()
            .map(|item| {
                let run = self.torrents.get(&group_of(item));
                let id = run.and_then(|r| r.id);
                let m = id.and_then(|id| measured.get(&id));
                let r = readiness.get(item.id.as_str());
                let is_active = active.contains(item.id.as_str());
                let is_watching = watching(item);
                let complete = r.is_some_and(FileReadiness::complete);
                let state = if is_watching {
                    ItemState::Watching
                } else if complete {
                    ItemState::Complete
                } else if item.skipped {
                    ItemState::Skipped
                } else if item.paused {
                    ItemState::Paused
                } else if run.is_some_and(|r| r.id.is_none() && r.failed.is_some()) {
                    ItemState::Failed
                } else if !is_active || (plan.pause_background && id.is_some()) {
                    ItemState::Queued
                } else if id.is_none() {
                    ItemState::Starting
                } else if m.is_none_or(|m| m.stats.peers_live == 0) {
                    ItemState::Connecting
                } else {
                    ItemState::Downloading
                };
                let moving = matches!(
                    state,
                    ItemState::Watching | ItemState::Downloading | ItemState::Connecting
                );
                let download_bps = if moving {
                    m.map_or(0, |m| m.stats.download_bps)
                } else {
                    0
                };
                #[allow(clippy::cast_precision_loss)]
                let eta_secs = r
                    .filter(|_| download_bps >= MIN_ETA_BPS)
                    .map(|r| r.len.saturating_sub(r.downloaded) as f64 / download_bps as f64);
                ItemDownload {
                    state,
                    download_bps,
                    upload_bps: m.map_or(0, |m| m.stats.upload_bps),
                    peers_live: m.map_or(0, |m| m.stats.peers_live),
                    peers_connecting: m.map_or(0, |m| m.stats.peers_connecting),
                    peers_seen: m.map_or(0, |m| m.stats.peers_seen),
                    downloaded: r.map_or(0, |r| r.downloaded),
                    length: r.map(|r| r.len),
                    eta_secs,
                    startable: r.is_some_and(|r| r.startable),
                    limited: is_active && plan.limits.contains_key(&group_of(item)),
                    wait_reason: match state {
                        ItemState::Queued if !is_active => Some(WaitReason::Limit),
                        ItemState::Queued => Some(WaitReason::Playback),
                        _ => None,
                    },
                    error: run.and_then(|r| r.failed.as_ref().map(|(_, e)| e.clone())),
                    ..ItemDownload::empty(&item.id)
                }
            })
            .collect();
        DownloadsSnapshot {
            items,
            link: LinkUse {
                capacity_bps: self.planner.meter.capacity_bps(),
                used_bps: self.planner.meter.current_bps(),
                upload_bps: total_upload_bps,
                playback_need_bps: foreground.map(|f| f.need_bps()),
                // Starving shows as such only while the background is paused for it; otherwise
                // the background is limited, as when guarded.
                pressure: match plan.pressure {
                    Pressure::Relaxed => LinkPressure::Relaxed,
                    Pressure::Starving if plan.pause_background => LinkPressure::Starving,
                    Pressure::Guarded | Pressure::Starving => LinkPressure::Guarded,
                },
                max_active: max_active.map(|n| u32::try_from(n).unwrap_or(u32::MAX)),
                active: u32::try_from(plan.active.len()).unwrap_or(u32::MAX),
            },
        }
    }

    /// Records finished adds.
    async fn collect_adds(&mut self) {
        for run in self.torrents.values_mut() {
            if !run
                .adding
                .as_ref()
                .is_some_and(tokio::task::JoinHandle::is_finished)
            {
                continue;
            }
            let Some(handle) = run.adding.take() else {
                continue;
            };
            // Finished: this does not wait.
            match handle.await {
                Ok(Ok(id)) => {
                    run.id = Some(id);
                    run.failed = None;
                }
                Ok(Err(error)) => run.failed = Some((Instant::now(), error)),
                Err(error) => run.failed = Some((Instant::now(), error.to_string())),
            }
        }
    }
}

/// Adds a queued item's torrent in the background, as `add_torrent_link` does.
fn spawn_add(
    app: &AppHandle,
    core: &Arc<Core>,
    link: String,
) -> tokio::task::JoinHandle<Result<TorrentId, String>> {
    let app = app.clone();
    let core = Arc::clone(core);
    tokio::spawn(async move {
        let source = TorrentSource::parse_link(&link).map_err(|e| IpcError::from(e).message)?;
        // The search service counts its download links like searches.
        app.state::<crate::search::SearchState>().pace(&link).await;
        let added = tokio::time::timeout(ADD_TIMEOUT, async {
            let source = core.engine.resolve_source(source).await?;
            core.engine.add(source).await
        })
        .await;
        match added {
            Ok(Ok(info)) => {
                tracing::info!(
                    torrent = info.id,
                    "queued torrent added for background download"
                );
                Ok(info.id)
            }
            Ok(Err(error)) => Err(IpcError::from(error).message),
            Err(_) => Err("Timed out finding the torrent.".to_owned()),
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(id: &str, hash: Option<&str>, paused: bool) -> QueueItem {
        QueueItem {
            id: id.into(),
            source: format!("https://example.org/{id}.torrent"),
            info_hash: hash.map(str::to_owned),
            file_index: None,
            title: id.into(),
            torrent_name: None,
            size_bytes: None,
            paused,
            skipped: false,
            played: false,
        }
    }

    #[test]
    fn items_of_one_torrent_share_a_group() {
        let a = item("a", Some("ABCD"), false);
        let b = item("b", Some("abcd"), false);
        let c = item("c", None, false);
        assert_eq!(group_of(&a), group_of(&b));
        assert_eq!(group_of(&c), "link:https://example.org/c.torrent");
    }

    #[test]
    fn before_the_engine_starts_items_are_queued_or_paused() {
        let queue = Queue {
            items: vec![item("a", None, false), item("b", None, true)],
            ..Queue::default()
        };
        let s = idle_snapshot(&queue);
        assert_eq!(s.items[0].state, ItemState::Queued);
        assert_eq!(s.items[1].state, ItemState::Paused);
        let json = serde_json::to_value(&s).unwrap();
        assert_eq!(json["items"][1]["state"], "paused");
        assert_eq!(json["link"]["pressure"], "relaxed");
    }

    #[tokio::test]
    async fn a_saved_queue_reaches_the_downloader() {
        let state = DownloadsState::new(Queue::default());
        let queue = Queue {
            items: vec![item("a", None, true)],
            order: DownloadOrder::Fastest,
            ..Queue::default()
        };
        state.set_queue(queue.clone()).await;
        assert_eq!(state.queue(), queue);
    }

    #[test]
    fn post_playback_seeding_stops_at_the_first_target() {
        let since = Instant::now();
        let now = since + Duration::from_secs(30 * 60);
        let bandwidth = torrentplayer_settings::BandwidthSettings {
            seed_ratio_target: 2.0,
            seed_time_limit_minutes: 60,
            ..torrentplayer_settings::BandwidthSettings::default()
        };
        assert!(post_playback_seeding(
            Some(since),
            &bandwidth,
            1_000,
            1_999,
            now
        ));
        assert!(!post_playback_seeding(
            Some(since),
            &bandwidth,
            1_000,
            2_000,
            now
        ));
        assert!(!post_playback_seeding(
            Some(since),
            &bandwidth,
            1_000,
            0,
            since + Duration::from_secs(60 * 60)
        ));
    }

    #[test]
    fn post_playback_seeding_can_be_unlimited_or_disabled() {
        let now = Instant::now();
        let mut bandwidth = torrentplayer_settings::BandwidthSettings {
            seed_ratio_target: 0.0,
            seed_time_limit_minutes: 0,
            ..torrentplayer_settings::BandwidthSettings::default()
        };
        assert!(post_playback_seeding(
            Some(now.checked_sub(Duration::from_secs(1)).unwrap_or(now)),
            &bandwidth,
            1,
            u64::MAX,
            now
        ));
        bandwidth.seed_after_watching = false;
        assert!(!post_playback_seeding(Some(now), &bandwidth, 1, 0, now));
        bandwidth.seed_after_watching = true;
        assert!(!post_playback_seeding(None, &bandwidth, 1, 0, now));
    }
}
