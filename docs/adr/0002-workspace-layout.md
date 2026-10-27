# 0002: Cargo workspace layout

- **Status:** Accepted
- **Date:** 2026-10-17

> **Update (2026-10-26, 0.3.1):** the workspace has since gained `torrentplayer-swarm`,
> `torrentplayer-search`, `torrentplayer-settings`, and `torrentplayer-bench`;
> `torrentplayer-player` and `torrentplayer-course` are still placeholders the app does not use.
> `torrentplayer-app` declares `#![deny(unsafe_code)]` rather than `forbid`, with one documented
> exception in `src-tauri/src/window.rs` (WebView2's memory target, two COM calls with
> `// SAFETY:` comments); see [Architecture](../architecture.md#crates).

## Context

TorrentPlayer has several subsystems with very different needs: pure logic that should be exhaustively
tested (prioritization, natural sorting, range parsing), async network code, a C FFI boundary
(libmpv), and a Tauri shell. We want fast tests, clear ownership, and a small surface that needs
`unsafe` review.

## Decision

A Cargo workspace with one library crate per subsystem under `crates/`, plus the Tauri app shell
in `src-tauri/` (the location Tauri tooling expects):

- `torrentplayer-torrent`, `torrentplayer-prioritize`, `torrentplayer-stream`, `torrentplayer-media`, `torrentplayer-player`,
  `torrentplayer-course`, `torrentplayer-library`, `torrentplayer-net`, and `torrentplayer-app`.
- Pure logic crates (`torrentplayer-prioritize`, `torrentplayer-course`) have no I/O, Tauri, or FFI dependencies.
- Only `torrentplayer-player` may contain `unsafe` code. Every other crate declares
  `#![forbid(unsafe_code)]`.
- Shared settings live in the root `Cargo.toml`: `[workspace.package]` for metadata,
  `[workspace.dependencies]` for versions, and `[workspace.lints]` for lint policy.

## Alternatives considered

- **A single crate with modules.** Simpler at first, but every test would compile Tauri, lint
  policy could not differ for the FFI code, and module boundaries erode more easily.
- **Finer-grained crates** (for example separate crates for the HTTP server and `stream_cb`).
  These can be split out later if a crate grows too large; starting coarser keeps the build graph
  simple.

## Consequences

- Each crate can be tested and documented on its own, and the pure crates test in seconds.
- Crates are `publish = false` for now. Publishing reusable crates (for example the prioritizer)
  to crates.io can be revisited once their APIs stabilize.
- Some crates start as placeholders and are filled in phase by phase.
