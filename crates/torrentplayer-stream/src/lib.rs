// SPDX-License-Identifier: MIT OR Apache-2.0

//! Stream feeds that serve torrent data to the player.
//!
//! [`StreamServer`] is a loopback HTTP server with `Range` support. It feeds the webview's
//! `<video>` element (the fallback player) and can also feed libmpv. It is bound to
//! `127.0.0.1` on a random port, every request must carry a random per-session token and the
//! exact `Host` header, and only explicitly published streams are reachable. See
//! `docs/threat-model.md`. It also serves live streams of unknown length, such as converted
//! audio, once each, to the app's own pages (see [`LiveSource`]).
//!
//! The libmpv `stream_cb` adapter is added in Phase 2.

#![forbid(unsafe_code)]

pub mod range;
mod server;
mod token;

pub use server::{
    LiveSource, MediaReader, MediaSource, PublishedStream, RequestObserver, ServerConfig,
    ServerError, StreamId, StreamServer,
};
pub use token::SessionToken;
