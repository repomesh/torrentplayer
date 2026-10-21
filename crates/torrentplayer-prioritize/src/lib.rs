// SPDX-License-Identifier: MIT OR Apache-2.0

//! Pure, platform-independent piece prioritization logic for TorrentPlayer.
//!
//! Given where the viewer is in a file, how fast the video consumes bytes, and which pieces are
//! already downloaded, [`plan`] decides which byte positions the torrent engine should fetch
//! first. It contains no I/O, so the policy can be tested exhaustively.
//!
//! # How a plan is carried out
//!
//! The engine (librqbit) does not accept arbitrary piece priorities. Instead, every open file
//! stream makes the engine prefer the pieces in a fixed-size window starting at the stream's
//! position (see [`Config::engine_lookahead_bytes`]). A plan is therefore a short, ordered list
//! of [`Anchor`]s: positions where the engine adapter parks a stream. Each anchor sits on the
//! first *missing* byte of a region we care about, so its lookahead covers data we still need.
//! See `docs/adr/0005-prioritization-with-anchor-streams.md`.
//!
//! # Priorities, highest first
//!
//! 1. **Playhead:** the bytes right after the current position.
//! 2. **Container head and tail:** container indexes such as the MP4 `moov` atom or MKV cues
//!    are often at the end of the file, and seeking is impossible without them.
//! 3. **Read-ahead:** the rest of a window sized from the bitrate (default 30 seconds).
//! 4. **Next lecture:** near the end of playback, the head and tail of the next file.
//!
//! Everything else in a selected file is downloaded by the engine in the background at normal
//! priority, so later seeks are instant.

#![forbid(unsafe_code)]

mod geometry;
mod plan;

pub use geometry::{FileSpan, PieceGeometry};
pub use plan::{Anchor, AnchorFile, Config, Plan, Playback, Purpose, buffered_bytes_from, plan};
