# 0001: Project license

- **Status:** Accepted, pending review of the bundled libmpv/FFmpeg builds in Phase 5
- **Date:** 2026-10-17

## Context

TorrentStream is an open source desktop app written mostly in Rust. We want a license that:

- is familiar to Rust contributors and compatible with the crates we depend on,
- lets the code be reused widely, including in other open source projects under the GPL, and
- includes a patent grant.

TorrentStream also bundles native libraries with their own licenses:

| Component                                                                            | License                                                            | How it is used                       |
| ------------------------------------------------------------------------------------ | ------------------------------------------------------------------ | ------------------------------------ |
| librqbit                                                                             | Apache-2.0                                                         | Rust crate, statically linked        |
| Tauri and most Rust crates                                                           | MIT and/or Apache-2.0                                              | Rust crates, statically linked       |
| `cssparser`, `cssparser-macros`, `selectors`, `dtoa-short`, `option-ext` (via Tauri) | MPL-2.0                                                            | Rust crates, unmodified              |
| libmpv                                                                               | LGPL-2.1+ when built with `-Dgpl=false`; GPL-2.0+ otherwise        | Shared library, dynamically linked   |
| FFmpeg                                                                               | LGPL-2.1+ without `--enable-gpl`/`--enable-nonfree`; GPL otherwise | Shared libraries, dynamically linked |
| boringtun (candidate for WireGuard)                                                  | BSD-3-Clause                                                       | Rust crate                           |

## Decision

TorrentStream's own code is dual licensed **MIT OR Apache-2.0**, the Rust ecosystem convention. Every
source file carries an `SPDX-License-Identifier: MIT OR Apache-2.0` header, and every crate sets
`license.workspace = true`.

Dependency licenses are enforced by `cargo deny` with an explicit allow-list. MPL-2.0 is allowed
only for the specific Tauri dependencies listed above, through per-crate exceptions in
`deny.toml`. MPL-2.0 is file-level copyleft: it requires that the source of those files stays
available, which is satisfied because we use them unmodified from crates.io. Modifying them would
require publishing our changes to those files.

libmpv and FFmpeg must be **LGPL builds**, dynamically linked, so TorrentStream stays MIT OR Apache-2.0
and users can replace the libraries. Their license texts and source locations go in
`THIRD_PARTY_NOTICES.md` and the About screen.

## Alternatives considered

- **GPL-3.0-or-later for the whole app.** This would let us bundle any libmpv or FFmpeg build,
  including the common prebuilt GPL ones, without worrying about build flags. Rejected for now
  because it limits reuse of TorrentStream's crates (for example the prioritizer and range server) in
  permissively licensed projects.
- **MIT only or Apache-2.0 only.** Dual licensing gives users the patent grant of Apache-2.0 and
  the GPL-2.0 compatibility of MIT.

## Consequences

- **Packaging work in Phase 5:** most prebuilt libmpv binaries (for example the popular Windows
  builds and Homebrew's `mpv`) are built **with GPL enabled**. We will probably need to build
  libmpv and FFmpeg ourselves in CI with `-Dgpl=false` and without `--enable-gpl`. That rules out
  GPL-only components such as the x264/x265 encoders and some filters; decoding all the required
  codecs does not need them.
- If an LGPL build proves impractical on some platform, bundling a GPL libmpv would make the
  **distributed binary** subject to the GPL (the source code would remain MIT OR Apache-2.0,
  which is GPL-compatible). That would be a new decision, recorded in a superseding ADR, and the
  maintainers must approve it before any GPL binary ships.
- Contributors agree to license contributions under the same dual license (see
  CONTRIBUTING.md).
