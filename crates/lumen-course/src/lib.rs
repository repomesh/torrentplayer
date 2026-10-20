// SPDX-License-Identifier: MIT OR Apache-2.0

//! Smart file selection and course grouping for TorrentStream.
//!
//! Identifies video files in a torrent, filters out samples and extras, natural-sorts
//! lectures ("Lecture 2" before "Lecture 10"), parses lecture numbers and titles, and
//! attaches related subtitles and notes. Pure logic with no I/O.
//!
//! **Status:** planned for Phase 3. See `docs/architecture.md` for how this crate fits
//! into the streaming pipeline.

#![forbid(unsafe_code)]
