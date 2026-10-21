# 0004: Generating IPC types with ts-rs

- **Status:** Accepted
- **Date:** 2026-10-17

## Context

IPC payloads are defined in Rust and consumed in TypeScript. Hand-written TypeScript types drift
from the Rust definitions, which turns into runtime bugs that the type checker cannot catch.

## Decision

Payload types derive [`ts_rs::TS`](https://crates.io/crates/ts-rs) with `#[ts(export)]`.
`cargo test` writes the TypeScript definitions to `src/lib/bindings/` (configured with
`TS_RS_EXPORT_DIR` in `.cargo/config.toml`). The generated files are committed, and CI fails if
regenerating them produces a diff. Command names and signatures are documented in `docs/ipc.md`
and wrapped in `src/lib/ipc.ts`.

## Alternatives considered

- **tauri-specta.** Generates both types and typed command wrappers, which is more complete, but
  its Tauri v2 support is still a release candidate (`2.0.0-rc.*`) and we prefer stable releases
  for core tooling. Worth revisiting when it reaches 2.0.
- **Hand-written types.** Rejected because they drift.

## Consequences

- Types always stay in sync. Command wrappers in `ipc.ts` are still written by hand, but they are
  short and covered by tests.
- Contributors must run `cargo test -p torrentplayer-app` after changing an IPC type and commit the result.
