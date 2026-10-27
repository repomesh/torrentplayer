# Streaming and prioritization

> **Status: Implemented.** TorrentPlayer's engine (`torrentplayer-swarm`, the default) applies
> this policy in its deadline scheduler (`crates/torrentplayer-swarm/src/sched.rs`); for the
> librqbit engine, `torrentplayer-prioritize` plans it as anchors
> ([ADR 0005](adr/0005-prioritization-with-anchor-streams.md)). Measured start-up and seek times
> are in [Performance and benchmarking](performance.md). Where this page and the code differ,
> the code is authoritative.

## Goals

| Metric                       | Target (healthy swarm) |
| ---------------------------- | ---------------------- |
| Time to first frame          | < 3 s                  |
| Seek to playback             | < 1.5 s                |
| Buffer ahead during playback | ≈ 30 s                 |

## Download only what is watched

Torrents are added with no files selected. A file is selected only when the user plays or queues
it. This keeps disk use and swarm load proportional to what is actually watched.

## Downloaded once

The engine stores verified pieces in an append-only cache file per torrent (`pieces.dat`) and
appends where each one is to an index next to it (`pieces.idx`), so pieces downloaded before a
restart are used again.

When the app quits, the engine flushes each cache that changed to disk and appends a clean marker
to its index, naming the cache file's length and modification time. If the next session finds
the marker last in the index and the cache file unchanged, the stored pieces count as downloaded
at once, with no check up front: each is checked against the torrent's SHA-1 hash when it is first
read for playback or for a peer, and only then are its bytes used. A piece that fails then is
downloaded again, and the next session checks the whole cache.

On exit, cache flushes and the final playback-history save run on blocking workers. The app
waits up to two seconds for them; slow storage does not block that deadline. If exiting
interrupts a flush before its clean marker is complete, the next session checks the cache as
after a crash.

After a crash (or a cache changed since), every stored piece is checked in the background when
the torrent is added; each becomes available (to the player, and to peers if sharing is on) as
soon as it passes, and is not requested from peers meanwhile. The engine checks on two threads
at most, whatever the number of torrents, so a long queue does not compete with playback for the
disk: first the pieces a player is waiting for, then the rest of the torrents being played, then
the other torrents in the order they were added. A piece that fails is downloaded again. The
index is never trusted on its own: a torn or damaged record, or one pointing past the end of the
cache file, is skipped. A cache file with more unused bytes than the whole torrent (pieces stored
again over many sessions) starts empty instead of being compacted.

A torrent with nothing to download (paused, complete, or with every selected file downloaded and
nothing playing) re-plans once a second instead of ten times; selecting a file, playing, seeking, or
resuming re-plans at once.

## Memory for partial pieces

Pieces being downloaded are held in memory until they verify. All torrents of the engine share
one budget of 256 MiB for them (`SchedulerConfig::max_in_progress_bytes`), not one each. Torrents
that nobody is playing or reading stop starting new pieces at three quarters of it, so the video
being played always has a quarter to itself; urgent pieces are never held back, and every
torrent may always work on at least one piece. A paused torrent gives its partial pieces back at
once, and while the budget is contended so does a stalled one, for each piece that has had no
block requested or received for 60 s.

## Piece priorities

The scheduler's policy is pure logic, unit-tested without a network: given the file layout,
piece availability, the playhead, readers, and the bitrate, it decides which blocks to request
from which peers (`sched.rs`). `torrentplayer-prioritize` turns the same goals into anchor
positions for librqbit.

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
  probes, each under a token of its own; the audio conversion reads its input from a loopback
  server of its own, and its output is served here. Connections that have not
  presented the token are kept apart from the player's (at most 8, closed after 10 seconds or
  once refused); of the 16 connections the player may hold, one that has read and written
  nothing for 30 seconds (a player paused with a full buffer) is closed only when another
  connection needs its place, and when all 16 are busy a request is answered
  `503 Service Unavailable`.
- **`stream_cb`** (for the libmpv player, Phase 2, see [Player integration](player.md)): libmpv
  would call back into Rust for reads and seeks with no HTTP overhead. Whether it or the HTTP
  server feeds libmpv will be measured and recorded in an ADR.

## Metrics

Exposed to the developer debug overlay (<kbd>D</kbd>, unless the `advanced.debugOverlay` setting
is off): time to first frame, seek-to-playback latency, buffer ahead in seconds, download speed,
peer count, and a piece map. See [Performance and benchmarking](performance.md).
