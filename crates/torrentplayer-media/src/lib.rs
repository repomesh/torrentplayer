// SPDX-License-Identifier: MIT OR Apache-2.0

//! Media probing for TorrentPlayer.
//!
//! [`probe_tracks`] reads a video file's container header (MP4, MOV, Matroska, or `WebM`) and
//! lists its audio and subtitle tracks and its duration, so the player can say when the built-in (webview)
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
//! FFmpeg ([`FfmpegTools`]), when the user has it installed: `ffprobe` for the duration and
//! bitrate of files whose header gives none (read-ahead sizing, buffer estimates, and the
//! bandwidth planner use them), and `ffmpeg` for seek-bar thumbnails. Both read the media
//! from the loopback stream server and are locked down to it. See `docs/architecture.md`.
//!
//! Layout: [`probe_layout`] reads where a player finds the header, the media, and the video
//! keyframes (MP4 sync samples, Matroska cue points), for the streaming benchmark.
//!
//! Converted audio: [`init_segment_len`] and [`mse_type`] read the fragmented MP4 that the audio
//! conversion writes, for Media Source Extensions.
//!

#![forbid(unsafe_code)]

mod audio;
mod cues;
mod ffmpeg;
mod fragmented;
mod language;
mod layout;
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
pub use ffmpeg::{
    FORMAT_WHITELIST, FfmpegError, FfmpegTools, MAX_THUMBNAIL_BYTES, MAX_THUMBNAIL_WIDTH,
    MediaInfo, PROXY_ENV, ffmpeg_command, version,
};
pub use fragmented::{InitError, MAX_INIT_SEGMENT, init_segment_len, mse_type};
pub use language::{Language, language};
pub use layout::{MAX_LAYOUT_READS, MAX_SEEK_POINTS, MediaLayout, SeekPoint, probe_layout};
pub use matching::{MAX_SUBTITLE_FILES, SUBTITLE_EXTENSIONS, SubtitleFile, subtitle_files};
pub use probe::{MAX_READS, ProbeError, probe_tracks};
pub use tracks::{Container, MAX_DURATION_MS, SubtitleCodec, SubtitleTrack, TrackReport};
