# Streaming and prioritization

> **Status: Implemented.** The piece policy is in `torrentplayer-prioritize`, used by
> TorrentPlayer's engine (`torrentplayer-swarm`); measured start-up and seek times are in
> [Performance and benchmarking](performance.md). Where this page and the code differ, the code
> is authoritative.

## Goals

| Metric                       | Target (healthy swarm) |
| ---------------------------- | ---------------------- |
| Time to first frame          | < 3 s                  |
| Seek to playback             | < 1.5 s                |
| Buffer ahead during playback | ≈ 30 s (configurable)  |

## Download only what is watched

Torrents are added with no files selected. A file is selected only when the user plays or queues
it. This keeps disk use and swarm load proportional to what is actually watched.

## Piece priorities

Priorities are computed by `torrentplayer-prioritize`, which is pure logic: given the file layout, piece
availability, the playhead, and the bitrate, it returns an ordered set of piece requests. That
makes the policy exhaustively testable.

1. **Metadata first.** On play, request the first and last pieces of the file at once. Container
   indexes (the MP4 `moov` atom, MKV cues) are often at the end of the file, and seeking is
   impossible without them.
2. **Read-ahead window.** Keep a window ahead of the playhead sized from the bitrate
   (`bitrate × read_ahead_secs`), with the nearest pieces at the highest priority.
3. **Seek.** Immediately move the window to the new position, cancel pending requests for the old
   position, and request the target pieces from the fastest peers first.
4. **Background fill.** Once the window is satisfied, keep downloading the rest of the file at low
   priority so later seeks are instant.
5. **Next lecture.** Near the end of a lecture, prefetch the head and tail of the next one.

## Feeds

- **Loopback HTTP range server** (`torrentplayer-stream`, in use): bound to `127.0.0.1` on a random
  port, requires a random per-session token, and supports HTTP `Range` requests. The built-in
  player (the system web engine's `<video>`) reads every video through it, and so do FFmpeg's
  probes and the audio conversion, each under a token of its own.
- **`stream_cb`** (for the libmpv player, Phase 2, see [Player integration](player.md)): libmpv
  would call back into Rust for reads and seeks with no HTTP overhead. Whether it or the HTTP
  server feeds libmpv will be measured and recorded in an ADR.

## Metrics

Exposed to the developer debug overlay (<kbd>D</kbd>, unless the `advanced.debugOverlay` setting
is off): time to first frame, seek-to-playback latency, buffer ahead in seconds, download speed,
peer count, and a piece map. See [Performance and benchmarking](performance.md).
