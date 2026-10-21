# 0005: Prioritization on librqbit with anchor streams

- **Status:** Accepted for the librqbit engine; superseded for TorrentPlayer's own engine by
  [0006](0006-streaming-engine.md)
- **Date:** 2026-10-17

## Context

Streaming needs pieces in a specific order: the bytes right after the playhead, container
metadata at both ends of the file, a read-ahead window sized from the bitrate, and the next
lecture near the end of the current one. librqbit 9 has no public API for piece priorities. Its
only lever is that every open file stream makes it prefer the pieces in a fixed 32 MiB window
after the stream's position, interleaved across all open streams.

## Decision

`torrentplayer-prioritize` computes a pure, testable _plan_: an ordered list of anchors, each on the
first missing byte of a region that matters. `torrentplayer-torrent`'s `PlaybackSession` carries out the
plan by parking one librqbit stream per anchor (never read, only positioned) and moving or
reopening them as the plan changes, at most 4 per playback because streams share librqbit's
pool with disk I/O.

Until 2 MiB are buffered at the playhead (the _critical phase_), the plan contains only the
playhead anchor. Measured on the local seeder at 2 MiB/s, placing a speculative tail anchor at
start-up slowed start-up from 52 ms to 386 ms; with the critical phase it was 50 ms.

## Alternatives considered

- **Patching librqbit** to accept priorities: a fork to maintain for one feature.
- **Relying only on the player's own reads:** no container-tail prefetch, no bitrate-sized
  read-ahead beyond 32 MiB, no next-lecture prefetch.

## Consequences

- On a single local seeder, anchors measured neutral against librqbit's built-in streaming for
  seeks; their benefits (read-ahead beyond 32 MiB, next-lecture prefetch) are not visible in that
  benchmark.
- The approach cannot fix the main seek limit, librqbit's fixed per-peer request window, which
  led to [0006](0006-streaming-engine.md).
