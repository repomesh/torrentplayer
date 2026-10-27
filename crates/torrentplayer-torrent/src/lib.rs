// SPDX-License-Identifier: MIT OR Apache-2.0

//! Torrent engines for TorrentPlayer.
//!
//! [`StreamEngine`] is what the app uses: it routes each torrent to TorrentPlayer's own
//! streaming-first engine (`torrentplayer-swarm`) or to librqbit (magnet links), behind one API. See
//! [`EngineChoice`] and the `TORRENTPLAYER_ENGINE` environment variable.
//!
//! [`Engine`] owns the librqbit session. It adds torrents with no files selected, after
//! checking every file path (see [`paths`]); selects files only when they are played or
//! queued; and exposes seekable file streams, piece availability, and swarm statistics.
//! [`PlaybackSession`] carries out the plans from `torrentplayer-prioritize` while a file plays.
//!
//! [`StreamEngine::preload`] downloads the start of a search result's best video ahead of time
//! (see [`preload`]); [`StreamEngine::prefetch_metadata`] fetches only the metadata of other
//! results, so playing any of them starts sooner.
//!
//! Sources are restricted to magnet links, http(s) links, and `.torrent` file contents (see
//! [`TorrentSource`]); `.torrent` downloads are size-limited.

#![forbid(unsafe_code)]

mod backend;
pub mod bandwidth;
mod completed;
pub mod details;
mod engine;
mod media;
mod metadata;
pub mod paths;
mod playback;
pub mod preload;
mod source;
pub mod thumbnail;

pub use backend::{
    BackendKind, ENGINE_ENV_VAR, EngineChoice, Playback, StreamEngine, StreamEngineConfig,
    SwarmPlayback, Thumbnail, protected_magnet_peers,
};
pub use completed::FileSaveStatus;
pub use engine::{
    Engine, EngineConfig, EngineError, FileEntry, MAX_FILES, Resolved, TorrentId, TorrentInfo,
    TorrentMedia, TorrentState, TorrentStats,
};
pub use media::{FileKind, mime_type};
pub use playback::{PlaybackInput, PlaybackSession, PlaybackStatus, PlayheadPosition};
pub use preload::{FileReadiness, PreloadPhase, PreloadStatus, magnet_info_hash};
pub use source::{
    MAGNET_TRACKERS, MAX_LINK_BYTES, MAX_TORRENT_FILE_BYTES, SourceError, TorrentSource,
    magnet_link,
};
