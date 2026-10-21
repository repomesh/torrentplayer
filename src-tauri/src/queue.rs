// SPDX-License-Identifier: MIT OR Apache-2.0

//! The play queue: persisted in `queue.json` (see `lumen_library::QueueStore`).
//!
//! The UI owns the queue's logic (order, cursor, auto-play) and saves the whole queue after
//! each change; the backend validates it and writes it atomically. Items hold torrent links,
//! never stream URLs.
//!
//! Near the end of a video the UI asks for the next item to be preloaded (`preload_next`),
//! through the same one-slot preload as a search's best pick (see `crate::preload`).

use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};

use lumen_library::{MAX_ID_BYTES, MAX_QUEUE_ITEMS, Queue, QueueStore, StoreError};
use lumen_torrent::{FileReadiness, TorrentSource};
use serde::{Deserialize, Serialize};
use tauri::State;
use ts_rs::TS;

use crate::core::AppState;
use crate::error::IpcError;
use crate::preload::PreloadState;

/// Managed state: the queue file. Saves are serialized so two cannot interleave.
#[derive(Debug, Clone)]
pub(crate) struct QueueState {
    store: Arc<Mutex<QueueStore>>,
}

impl QueueState {
    /// A queue stored in `data_dir/queue.json`.
    pub(crate) fn new(data_dir: PathBuf) -> Self {
        Self {
            store: Arc::new(Mutex::new(QueueStore::new(data_dir))),
        }
    }

    pub(crate) fn load(&self) -> Queue {
        self.store
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .load()
            .queue
    }

    fn save(&self, queue: &Queue) -> Result<(), StoreError> {
        self.store
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .save(queue)
    }
}

/// The saved queue (empty if there is none; invalid items are dropped).
#[tauri::command]
pub(crate) async fn load_queue(state: State<'_, QueueState>) -> Result<Queue, IpcError> {
    let state = state.inner().clone();
    tokio::task::spawn_blocking(move || state.load())
        .await
        .map_err(|e| IpcError::internal(&e))
}

/// Replaces the saved queue. Rejects an invalid queue with `invalid-argument` (whose message
/// names the field), saving nothing.
#[tauri::command]
pub(crate) async fn save_queue(
    state: State<'_, QueueState>,
    downloads: State<'_, crate::downloads::DownloadsState>,
    queue: Queue,
) -> Result<(), IpcError> {
    queue
        .validate()
        .map_err(|e| IpcError::invalid(format!("queue.{e}")))?;
    // The downloader follows the new order and pauses at once, even if saving fails.
    downloads.set_queue(queue.clone()).await;
    let state = state.inner().clone();
    tokio::task::spawn_blocking(move || state.save(&queue))
        .await
        .map_err(|e| IpcError::internal(&e))?
        .map_err(|error| {
            tracing::warn!(%error, "could not save the queue");
            match error {
                StoreError::Invalid(e) => IpcError::invalid(format!("queue.{e}")),
                StoreError::Io(_) | StoreError::Encode(_) => IpcError::new(
                    crate::error::ErrorKind::Internal,
                    "The queue could not be saved.",
                ),
            }
        })
}

/// The queue's next video, to preload.
#[derive(Debug, Clone, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub(crate) struct NextVideo {
    /// Its torrent's link (`QueueItem.source`).
    pub link: String,
    /// The file to play; `null` for the torrent's main video.
    pub file_index: Option<usize>,
    /// Its torrent's info hash, if known.
    pub info_hash: Option<String>,
    /// The torrent's id if it is already added (for example the course playing).
    pub torrent_id: Option<usize>,
}

/// Starts preloading the queue's next video while `stream_id` plays, and returns once it has
/// started; follow a preload with `preload_status`. The UI calls this only once the playing
/// file is fully downloaded or nearly over with the rest arriving in time, so it never slows
/// it down. A torrent that is not added yet goes through the preload slot (see
/// `crate::preload`); in one that is (the next lecture of the same course), the file's start
/// and end are downloaded at background priority.
#[tauri::command]
pub(crate) async fn preload_next(
    state: State<'_, AppState>,
    preload: State<'_, PreloadState>,
    stream_id: u64,
    next: NextVideo,
) -> Result<(), IpcError> {
    let core = state.core().await?;
    // Only on behalf of an open stream: queue preloads belong to playback.
    let playing = core.with_stream(stream_id, |s| s.torrent)?;
    if let Some(id) = next.torrent_id {
        let file = core.engine.prefetch_file(id, next.file_index).await?;
        if id == playing {
            core.with_stream(stream_id, |s| s.session.set_next_file(Some(file)))?;
        }
        return Ok(());
    }
    let source = TorrentSource::parse_link(&next.link)?;
    preload.preload_for_queue(core, source, next.file_index, next.info_hash);
    Ok(())
}

/// A queue item whose readiness is asked for.
#[derive(Debug, Clone, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub(crate) struct ReadinessQuery {
    /// The queue item's id, echoed back.
    pub id: String,
    /// Its torrent's info hash; items without one are not answered.
    pub info_hash: Option<String>,
    /// The file; `null` for the torrent's main video.
    pub file_index: Option<usize>,
}

/// How ready a queued video is to play.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub(crate) enum ReadinessState {
    /// Downloaded completely.
    Complete,
    /// Its start and end are downloaded (as a preload leaves them): it starts at once.
    Ready,
    /// Neither yet.
    Waiting,
}

/// The readiness of one queued video, in a torrent that is added.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub(crate) struct ItemReadiness {
    /// The queue item's id.
    pub id: String,
    /// How ready it is.
    pub state: ReadinessState,
    /// Verified bytes of the file.
    pub downloaded: u64,
    /// The file's size in bytes.
    pub length: u64,
}

impl ReadinessState {
    fn of(r: &FileReadiness) -> Self {
        if r.complete() {
            Self::Complete
        } else if r.startable {
            Self::Ready
        } else {
            Self::Waiting
        }
    }
}

/// Whether a query names a torrent by a plausible info hash and has a bounded id.
fn answerable(query: &ReadinessQuery) -> Option<&str> {
    let hash = query.info_hash.as_deref()?;
    let plausible = hash.len() == 40 && hash.bytes().all(|b| b.is_ascii_hexdigit());
    (plausible && !query.id.is_empty() && query.id.len() <= MAX_ID_BYTES).then_some(hash)
}

/// How ready each queued video is to play, for the queue's "Ready" badges and its "play ready
/// videos first" option. Only items in torrents that are already added (a preload's included)
/// are answered; the others, and every item before the engine has started, are left out.
/// Reads which pieces are present and nothing else: it never starts the engine, adds a
/// torrent, or downloads anything. The UI asks every few seconds while the queue is shown.
#[tauri::command]
pub(crate) async fn queue_readiness(
    state: State<'_, AppState>,
    items: Vec<ReadinessQuery>,
) -> Result<Vec<ItemReadiness>, IpcError> {
    if items.len() > MAX_QUEUE_ITEMS {
        return Err(IpcError::invalid(format!(
            "items: at most {MAX_QUEUE_ITEMS} items."
        )));
    }
    let Some(core) = state.started() else {
        return Ok(Vec::new());
    };
    // One piece bitfield per torrent, however many of its files are queued.
    let mut pieces: HashMap<usize, Option<Vec<bool>>> = HashMap::new();
    let mut answers = Vec::new();
    for query in &items {
        let Some(id) = answerable(query).and_then(|hash| core.engine.torrent_with_hash(hash))
        else {
            continue;
        };
        if let Entry::Vacant(slot) = pieces.entry(id) {
            slot.insert(core.engine.have_pieces(id).await.ok());
        }
        let Some(Some(have)) = pieces.get(&id) else {
            continue;
        };
        if let Ok(r) = core.engine.file_readiness(id, query.file_index, have) {
            answers.push(ItemReadiness {
                id: query.id.clone(),
                state: ReadinessState::of(&r),
                downloaded: r.downloaded,
                length: r.len,
            });
        }
    }
    Ok(answers)
}

#[cfg(test)]
mod tests {
    use lumen_library::QueueItem;

    use super::*;

    #[test]
    fn saves_and_loads_through_the_state() {
        let dir = tempfile::tempdir().unwrap();
        let state = QueueState::new(dir.path().to_path_buf());
        assert_eq!(state.load(), Queue::default());
        let queue = Queue {
            items: vec![QueueItem {
                id: "a".into(),
                source: "https://example.org/course.torrent".into(),
                info_hash: None,
                file_index: None,
                title: "Lecture 1".into(),
                torrent_name: None,
                size_bytes: None,
                paused: true,
                skipped: true,
                played: false,
            }],
            current: Some("a".into()),
            order: lumen_library::DownloadOrder::Fastest,
        };
        state.save(&queue).unwrap();
        assert_eq!(state.load(), queue);
    }

    #[test]
    fn readiness_states_follow_the_pieces_present() {
        let r = |downloaded: u64, startable: bool| FileReadiness {
            file: 1,
            downloaded,
            len: 100,
            startable,
        };
        assert_eq!(ReadinessState::of(&r(100, true)), ReadinessState::Complete);
        assert_eq!(ReadinessState::of(&r(40, true)), ReadinessState::Ready);
        assert_eq!(ReadinessState::of(&r(90, false)), ReadinessState::Waiting);
        let json = serde_json::to_value(ItemReadiness {
            id: "a".into(),
            state: ReadinessState::Ready,
            downloaded: 40,
            length: 100,
        })
        .unwrap();
        assert_eq!(json["state"], "ready");
    }

    #[test]
    fn readiness_answers_only_plausible_queries() {
        let q = |id: &str, hash: Option<&str>| ReadinessQuery {
            id: id.into(),
            info_hash: hash.map(str::to_owned),
            file_index: None,
        };
        let hash = "AB".repeat(20);
        assert_eq!(answerable(&q("a", Some(&hash))), Some(hash.as_str()));
        assert_eq!(
            answerable(&q("a", None)),
            None,
            "download links without a hash"
        );
        assert_eq!(answerable(&q("a", Some("abc"))), None);
        assert_eq!(answerable(&q("", Some(&hash))), None);
        assert_eq!(answerable(&q(&"x".repeat(65), Some(&hash))), None);
    }

    #[test]
    fn invalid_queues_name_the_field() {
        let queue = Queue {
            items: Vec::new(),
            current: Some("gone".into()),
            ..Queue::default()
        };
        let error = queue.validate().unwrap_err();
        assert_eq!(
            IpcError::invalid(format!("queue.{error}")).message,
            "queue.current: no item has this id."
        );
    }
}
