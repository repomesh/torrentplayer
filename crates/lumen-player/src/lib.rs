// SPDX-License-Identifier: MIT OR Apache-2.0

//! Video playback for TorrentStream: libmpv (primary) with an FFmpeg-based fallback.
//!
//! This is the only crate allowed to contain `unsafe` code, because it wraps the libmpv C
//! API. All unsafe code is encapsulated behind a safe API, and callbacks invoked from C catch
//! panics so they never unwind across the FFI boundary.
//!
//! **Status:** planned for Phase 2. See `docs/architecture.md` for how this crate fits
//! into the streaming pipeline.

// Unsafe code is permitted only inside the `ffi` module once it exists, and every
// unsafe block must carry a `// SAFETY:` comment (enforced by Clippy).
#![deny(unsafe_code)]
