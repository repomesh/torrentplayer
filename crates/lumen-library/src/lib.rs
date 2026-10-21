// SPDX-License-Identifier: MIT OR Apache-2.0

//! Local library storage for TorrentStream.
//!
//! - [`QueueStore`]: the play queue ([`Queue`]), stored as versioned JSON (`queue.json` in the
//!   app data folder) with atomic writes. Items keep only what is needed to play them again,
//!   never stream URLs.
//! - [`LibraryStore`]: courses, watch progress, and resume positions in a local SQLite database.
//!   Values are written with parameterized queries and the database is kept in a user-only
//!   directory. See `docs/architecture.md` for how this crate fits into the app.

#![forbid(unsafe_code)]

mod progress;
mod queue;

pub use progress::{
    LIBRARY_FILE_NAME, LibraryError, LibraryItem, LibraryStore, Media, resume_position,
};
pub use queue::{
    DownloadOrder, Loaded, MAX_ID_BYTES, MAX_MAGNET_TRACKERS, MAX_QUEUE_ITEMS, MAX_SOURCE_BYTES,
    MAX_TITLE_CHARS, QUEUE_FILE_NAME, QUEUE_INVALID_FILE_NAME, QUEUE_VERSION, Queue, QueueError,
    QueueItem, QueueStore, StoreError,
};
