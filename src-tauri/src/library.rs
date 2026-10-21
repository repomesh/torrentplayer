// SPDX-License-Identifier: MIT OR Apache-2.0

//! Playback history and resume positions.
//!
//! SQLite work runs on blocking threads. Successful changes are pushed to the webview so the
//! Library and queue can update without polling.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{SystemTime, UNIX_EPOCH};

use lumen_library::{LibraryError, LibraryItem, LibraryStore, Media};
use tauri::{AppHandle, Emitter as _, State};

use crate::error::{ErrorKind, IpcError};

/// Backend-to-frontend event carrying one changed [`LibraryItem`].
pub(crate) const LIBRARY_CHANGED: &str = "library-changed";

/// Managed access to the SQLite library. The connection is serialized and only touched by one
/// blocking task at a time.
#[derive(Debug, Clone)]
pub(crate) struct LibraryState {
    inner: Arc<Mutex<LibraryInner>>,
    revision: Arc<AtomicU64>,
}

#[derive(Debug)]
struct LibraryInner {
    store: Option<LibraryStore>,
    latest: HashMap<(String, u32), u64>,
}

impl LibraryState {
    /// Opens the library in the app's data directory. An unavailable database disables history
    /// for this run instead of preventing the player from starting.
    pub(crate) fn new(data_dir: PathBuf) -> Self {
        let store = LibraryStore::open(data_dir).map_or_else(
            |error| {
                tracing::warn!(%error, "playback library is unavailable");
                None
            },
            Some,
        );
        Self {
            inner: Arc::new(Mutex::new(LibraryInner {
                store,
                latest: HashMap::new(),
            })),
            revision: Arc::new(AtomicU64::new(1)),
        }
    }

    /// A process-local ordering token assigned while the active-stream lock is held. It prevents
    /// blocking tasks that happen to run out of order from moving a saved playhead backwards.
    pub(crate) fn revision(&self) -> u64 {
        self.revision.fetch_add(1, Ordering::Relaxed)
    }

    /// Records that media was opened, preserving any existing position.
    pub(crate) async fn opened(
        &self,
        app: &AppHandle,
        media: Media,
    ) -> Result<LibraryItem, IpcError> {
        let inner = Arc::clone(&self.inner);
        let item = run(move || {
            let mut inner = inner.lock().unwrap_or_else(PoisonError::into_inner);
            inner
                .store
                .as_mut()
                .ok_or(LibraryError::Unavailable)?
                .opened(&media, unix_time())
        })
        .await?;
        emit(app, &item);
        Ok(item)
    }

    /// Saves progress and pushes the changed item to the webview.
    pub(crate) async fn record(
        &self,
        app: &AppHandle,
        media: Media,
        position_secs: f64,
        duration_secs: Option<f64>,
        revision: u64,
    ) -> Result<(), IpcError> {
        let inner = Arc::clone(&self.inner);
        let item = run(move || {
            record_ordered(
                &mut inner.lock().unwrap_or_else(PoisonError::into_inner),
                &media,
                position_secs,
                duration_secs,
                revision,
            )
        })
        .await?;
        if let Some(item) = item {
            emit(app, &item);
        }
        Ok(())
    }

    /// Saves progress during shutdown, when spawning work on the runtime is no longer useful.
    pub(crate) fn record_sync(
        &self,
        media: &Media,
        position_secs: f64,
        duration_secs: Option<f64>,
        revision: u64,
    ) {
        let result = record_ordered(
            &mut self.inner.lock().unwrap_or_else(PoisonError::into_inner),
            media,
            position_secs,
            duration_secs,
            revision,
        );
        if let Err(error) = result {
            tracing::warn!(%error, "could not flush playback progress");
        }
    }

    async fn list(&self) -> Result<Vec<LibraryItem>, IpcError> {
        let inner = Arc::clone(&self.inner);
        run(move || {
            inner
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .store
                .as_ref()
                .ok_or(LibraryError::Unavailable)?
                .list()
        })
        .await
    }
}

fn record_ordered(
    inner: &mut LibraryInner,
    media: &Media,
    position_secs: f64,
    duration_secs: Option<f64>,
    revision: u64,
) -> Result<Option<LibraryItem>, LibraryError> {
    let key = (media.info_hash.to_ascii_lowercase(), media.file_index);
    if inner
        .latest
        .get(&key)
        .is_some_and(|latest| *latest > revision)
    {
        return Ok(None);
    }
    let Some(store) = inner.store.as_mut() else {
        // Opening the database already logged the cause. Playback remains usable and does not
        // emit the same warning every five seconds while history is disabled.
        return Ok(None);
    };
    let item = store.record(media, position_secs, duration_secs, unix_time())?;
    inner.latest.insert(key, revision);
    Ok(Some(item))
}

/// Returns the playback library, most recently used first.
#[tauri::command]
pub(crate) async fn library_items(
    state: State<'_, LibraryState>,
) -> Result<Vec<LibraryItem>, IpcError> {
    state.list().await
}

async fn run<T: Send + 'static>(
    operation: impl FnOnce() -> Result<T, LibraryError> + Send + 'static,
) -> Result<T, IpcError> {
    tokio::task::spawn_blocking(operation)
        .await
        .map_err(|error| IpcError::internal(&error))?
        .map_err(|error| {
            tracing::warn!(%error, "playback library operation failed");
            IpcError::new(
                ErrorKind::Internal,
                "The playback library could not be updated.",
            )
        })
}

fn emit(app: &AppHandle, item: &LibraryItem) {
    if let Err(error) = app.emit(LIBRARY_CHANGED, item) {
        tracing::warn!(%error, "could not emit library change");
    }
}

fn unix_time() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unix_time_is_a_plausible_non_secret_timestamp() {
        assert!(unix_time() > 1_700_000_000);
    }

    #[test]
    fn unavailable_database_disables_history_instead_of_construction() {
        let dir = tempfile::tempdir().unwrap();
        let not_a_directory = dir.path().join("plain-file");
        std::fs::write(&not_a_directory, b"not a directory").unwrap();
        let state = LibraryState::new(not_a_directory);
        assert!(
            state
                .inner
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .store
                .is_none()
        );
    }

    #[test]
    fn stale_blocking_checkpoint_cannot_replace_a_newer_one() {
        let dir = tempfile::tempdir().unwrap();
        let mut inner = LibraryInner {
            store: Some(LibraryStore::open(dir.path()).unwrap()),
            latest: HashMap::new(),
        };
        let media = Media {
            info_hash: "dd8255ecdc7ca55fb0bbf81323d87062db1f6d1c".to_owned(),
            file_index: 2,
            source: "magnet:?xt=urn:btih:dd8255ecdc7ca55fb0bbf81323d87062db1f6d1c".to_owned(),
            title: "Lecture 2".to_owned(),
            torrent_name: "Course".to_owned(),
            size_bytes: Some(100),
        };
        assert!(
            record_ordered(&mut inner, &media, 80.0, Some(100.0), 2)
                .unwrap()
                .is_some()
        );
        assert!(
            record_ordered(&mut inner, &media, 20.0, Some(100.0), 1)
                .unwrap()
                .is_none()
        );
        assert!(
            (inner.store.as_ref().unwrap().list().unwrap()[0].position_secs - 80.0).abs()
                < f64::EPSILON
        );
    }
}
