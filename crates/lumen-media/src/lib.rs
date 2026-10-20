// SPDX-License-Identifier: MIT OR Apache-2.0

//! Media probing for TorrentStream.
//!
//! [`probe_tracks`] reads a video file's container header (MP4, MOV, Matroska, or `WebM`) and
//! lists its audio and subtitle tracks, so the player can say when the built-in (webview)
//! player will play a video without sound (Chromium-based webviews decode AAC, MP3, Opus,
//! Vorbis, and FLAC, but not Dolby (AC-3, E-AC-3, `TrueHD`) or DTS audio) and list embedded
//! subtitles.
//!
//! The file is untrusted, so the parsers are strict and bounded: every size is checked against
//! its parent and the file length, the walk follows fixed paths (no unbounded recursion), and
//! limits cap the boxes, elements, tracks, and reads visited. Nothing panics on any input. The
//! parsers read only the box and element headers they need, so an MP4 whose `moov` box is at
//! the end costs a few small reads near the end, not a read of the whole file.
//!
//! Subtitle files: [`subtitle_files`] finds the ones in a torrent that belong to a video, and
//! [`parse_subtitles`] turns SRT, `WebVTT`, and ASS/SSA files into plain-text cues, as carefully.
//!
//! **Planned for Phase 2:** probing with ffprobe (duration, bitrate for read-ahead sizing and
//! ETA estimates) and seek-bar thumbnails. See `docs/architecture.md`.

#![forbid(unsafe_code)]

mod audio;
mod cues;
mod language;
mod matching;
mod mkv;
mod mp4;
mod probe;
mod sparse;
mod tracks;

pub use audio::{AudioCodec, AudioTrack};
pub use cues::{
    Cue, MAX_CUES, MAX_SUBTITLE_BYTES, SubtitleError, SubtitleFormat, Subtitles, parse_subtitles,
};
pub use language::{Language, language};
pub use matching::{MAX_SUBTITLE_FILES, SUBTITLE_EXTENSIONS, SubtitleFile, subtitle_files};
pub use probe::{MAX_READS, ProbeError, probe_tracks};
pub use tracks::{Container, SubtitleCodec, SubtitleTrack, TrackReport};
