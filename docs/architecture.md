# Architecture overview

TorrentPlayer is a [Tauri v2](https://v2.tauri.app) desktop app. A Rust backend does all the work
(torrents, streaming, playback, networking, storage) and a Svelte webview draws the interface,
including the player controls overlaid on the video.

## Data flow

```text
 peers, trackers, DHT, web seeds, search
        ▲
        │ every connection asks torrentplayer-net (mode, kill switch, leases)
        ▼
 torrentplayer-torrent: StreamEngine ── torrentplayer-swarm (default engine: deadline
        │                                 scheduler, persistent piece cache)
        │                            └─ librqbit (magnet metadata, or the engine when chosen;
        │                                 plans from torrentplayer-prioritize)
        │ seekable file readers
        ▼
 torrentplayer-stream: 127.0.0.1 HTTP range server (token per session or per stream)
        │                       │
        │                       └─► torrentplayer-media (container probe, user's ffprobe/ffmpeg)
        │                       └─► FFmpeg sidecar (Dolby/DTS → AAC; reads a loopback server of
        │                           its own, its fMP4 output is served here)
        ▼
 Svelte webview: <video> (built-in player), Media Source Extensions for converted audio
        ▲
        │ IPC commands and events (docs/ipc.md)
        ▼
 torrentplayer-app (src-tauri): commands, downloads planner, queue (torrentplayer-library),
 library.sqlite3, settings (torrentplayer-settings), search (torrentplayer-search)
```

1. The user adds a torrent or magnet link. **torrentplayer-torrent**'s `StreamEngine` routes the
   torrent to an engine (below) and selects only the files being watched or queued; the UI
   orders the videos in lecture order (`ui/src/lib/files.ts`).
2. When playback starts, the engine asks for the file head and tail first (container
   metadata), then a read-ahead window sized from the media's average bitrate (its length over
   the duration the player reports, or that **torrentplayer-media** read from the container header or,
   with FFmpeg installed, `ffprobe`). See [Streaming](streaming.md).
3. The built-in player (the system web engine's `<video>`) reads bytes from the loopback HTTP
   range server in **torrentplayer-stream**. Every read and seek moves the engine's deadlines, and
   the player reports its position (`report_playback`), so a seek immediately reprioritizes
   pieces. Dolby and DTS audio is converted by the bundled FFmpeg sidecar and fed to Media Source
   Extensions ([ADR 0007](adr/0007-dolby-and-dts-audio.md)).
4. Torrent and stream statistics are polled by the UI about once a second; settings, network,
   and library changes arrive as Tauri events.
5. All peer, tracker, DHT, search, and `.torrent` download traffic goes through
   **torrentplayer-net**, which enforces the selected network mode and the kill switch.

## Crates

| Crate                              | Responsibility                                                                                                                                                                      | Depends on Tauri/FFI? | Phase |
| ---------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | --------------------- | ----- |
| `torrentplayer-torrent`            | `StreamEngine` facade over both engines; librqbit session, file selection, path validation, source limits, preload, metadata prefetch, bandwidth planner                            | No                    | 1     |
| `torrentplayer-swarm`              | TorrentPlayer's streaming-first BitTorrent engine: deadline scheduler, adaptive queues, cancel on seek, persistent piece cache, DHT, PEX, web seeds, uploads, smart ban             | No                    | 1     |
| `torrentplayer-prioritize`         | Piece prioritization plans (anchors for the librqbit engine) and piece geometry; no I/O                                                                                             | No                    | 1     |
| `torrentplayer-stream`             | Loopback HTTP range server (a `stream_cb` adapter for libmpv is planned)                                                                                                            | No                    | 1     |
| `torrentplayer-media`              | Track and duration probe of container headers; subtitle matching and parsing (pure Rust); the user's optional `ffprobe` (durations) and `ffmpeg` (seek-bar thumbnails), locked down | No                    | 1, 2  |
| `torrentplayer-player`             | Placeholder for the planned libmpv wrapper; the only crate meant to hold FFI `unsafe`; not used by the app yet                                                                      | FFI (planned)         | 2     |
| `torrentplayer-course`             | Video detection, filtering, natural sort, course grouping; no I/O; placeholder, not used by the app yet                                                                             | No                    | 3     |
| `torrentplayer-library`            | Play queue (versioned JSON, atomic writes); SQLite library, progress, resume positions                                                                                              | No                    | 1, 4  |
| `torrentplayer-search`             | Search: emberglow event stream (fan-out fallback), strict parsing, merging, relevance and watch-score ranking, best pick                                                            | No                    | 4     |
| `torrentplayer-settings`           | Settings model, defaults and ranges, validated patches, migrations, versioned JSON store (schema 5), OS credential store                                                            | No                    | 5     |
| `torrentplayer-net`                | Network modes, WireGuard, SOCKS5, kill switch, incoming peers and UPnP                                                                                                              | No                    | 5     |
| `torrentplayer-bench`              | Streaming benchmarks: local seeders with link emulation, public swarms, suites and reports (see [Performance](performance.md))                                                      | No                    | –     |
| `torrentplayer-app` (`src-tauri/`) | Tauri shell: IPC commands, events, app state, downloads planner, FFmpeg sidecar                                                                                                     | Tauri                 | 0     |

### Engine choice

`torrentplayer_torrent::StreamEngine` sends every source to **torrentplayer-swarm** by default, with the same
safety checks on every path. For a magnet link, librqbit starts lazily as a metadata-only
resolver and hands its metadata and discovered peers to torrentplayer-swarm. Its session (DHT
node, trackers, listener) stops after a minute with no torrents and nothing in flight, and the
next magnet link starts a new one. The `advanced.engine` setting (`auto`, `swarm`, or `rqbit`)
changes the choice, and `TORRENTPLAYER_ENGINE=auto|swarm|rqbit` overrides the setting. Downloads
go to separate `torrentplayer-swarm/` and `librqbit/` sub-folders. See
[ADR 0006](adr/0006-streaming-engine.md).

### Piece cache and memory

torrentplayer-swarm keeps each torrent's verified pieces in an append-only cache with an index
(`pieces.dat`, `pieces.idx`), so queued torrents are not downloaded again after a restart; after
a clean quit the pieces are checked when first read, after a crash by one engine-wide rechecker
on at most two threads (see [Streaming](streaming.md#downloaded-once)). Partly downloaded
pieces share one 256 MiB budget across all of the engine's torrents
(`SchedulerConfig::max_in_progress_bytes`, `sched::PieceBudget`): torrents nobody is playing or
reading stop starting pieces at three quarters of it, urgent pieces are never held back, and a
paused torrent, or a stalled one while the budget is contended, gives its partial pieces back.
A torrent added to the engine uploads nothing until the app turns uploading on for it; peers
that sent data which failed a piece's hash check are banned once the piece verifies ("smart
ban", see [Threat model](threat-model.md#network-privacy)). On Windows, the webview is asked
for a low memory target while the window is minimized (`src-tauri/src/window.rs`) and for the
normal one when it is restored.

### Search, preload, and prefetch

A search reads search.emberglow.tech's event stream (`torrentplayer_search::EmberglowProvider`): one
request, one event per indexer as it answers, parsed by a bounded server-sent-events reader
and merged and ranked by TorrentPlayer as results arrive. Without the stream it falls back to asking
the indexers in small groups. `src-tauri/src/search.rs` forwards progress to the webview at
most every 150 ms and starts one search at a time (a new one, or `cancel_search`, stops the
running one). Two kinds of background work follow the results:

- **Preload** (best pick only): once the best pick has held for 2 s or the search finishes
  (`torrentplayer_search::settle`), `StreamEngine::preload` downloads the start and end of its video
  into torrentplayer-swarm; at most two per search.
- **Metadata prefetch** (the other results on screen): `prefetch_torrents` asks
  `StreamEngine::prefetch_metadata` for up to 20 of them, three at a time. Only metadata is
  fetched; it is kept in the engine (64 torrents, LRU) with the peers seen, so adding or
  preloading one of them skips the metadata lookup. A new search stops the prefetch.

Results that indexers offer only as a download link on the search service are kept; the engine
follows such a link (`StreamEngine::resolve_source`) to the `.torrent` file or, usually, the
magnet link it redirects to, remembers the answer, and then treats it like any magnet link. The
prefetch tries a result's download link before its magnet link and follows at most four per
request; download links count against the search rate limiter.

See [IPC](ipc.md#search_torrents_progressive) for the limits and the network rules.

Keeping logic out of `torrentplayer-app` means it can be unit-tested without a webview. Every
library crate declares `#![forbid(unsafe_code)]` except `torrentplayer-player` (`deny`, for its
planned FFI). `torrentplayer-app` denies `unsafe_code` and allows it only in
`set_memory_target` in `src-tauri/src/window.rs`, two WebView2 COM calls with `// SAFETY:`
comments; its binary forbids it. See [ADR 0002](adr/0002-workspace-layout.md).

## Frontend

The UI lives in `ui/src/` (Svelte 5 + TypeScript, built by Vite into `ui/dist/`). It talks to the
backend only through `ui/src/lib/ipc.ts`, using types generated from Rust (see
[IPC contract](ipc.md)). The webview loads only bundled assets: no remote content, no inline
scripts, and a strict Content Security Policy (see [Threat model](threat-model.md)).

### Navigation

`ui/src/App.svelte` is the shell: a narrow rail at the left (`NavRail.svelte`) and one view at a
time. Every view stays reachable from the rail.

```text
┌──────┬─────────────────────────────────────────────────┐
│ ‹  › │                                                 │  ‹ › Back / Forward
│      │  Search · Open · Library · Files · Queue · ⚙    │      (Alt+←/→, mouse side buttons)
│  ⌕   │                                                 │  ⌕  Search box and results (Ctrl+F)
│  ▢   │   Queue (the video and the one queue):          │  ▢  Open: link, .torrent file, torrents
│  ▤   │   ┌──────────────────┬─────────────────────┐    │     opened this session (Ctrl+O)
│  ≡   │   │ Queue        ⋯ ‹ │                     │    │  ▤  Library: watch history, resume
│  ▶³  │   │ ▾ Course (3/12)  │      <video>        │    │  ≡  Files: the file explorer
│      │   │ ├ ☑ Lecture 1 ▶  │   (or "Pick some-   │    │  ▶  Queue: the video and the queue,
│      │   │ ├ ☑ Lecture 2    │    thing from the   │    │     with its upcoming count
│      │   │ └ ☐ Lecture 3    │    queue or search")│    │
│  ⚙   │   │   Single video   │                     │    │  ⚙  Settings (Ctrl+,)
│      │   └──────────────────┴─────────────────────┘    │
└──────┴─────────────────────────────────────────────────┘
```

- **History** (`ui/src/lib/nav.ts`) is browser-style back and forward over small view
  descriptors. Each search is its own entry, so Back moves between recent searches.
- **State survives navigation.** The search page stays mounted (hidden), so the query,
  results, filters, and preload are as they were. Scroll positions and focus are remembered
  per view. Leaving the player unmounts it but keeps the stream open (it keeps downloading).
  Coming back resumes at the same position. Playing something else, or a failed start, closes
  the stream.
- **Queue** (`ui/src/lib/queue.ts`, persisted by `torrentplayer-library`) is a playlist with a
  cursor. It is manual: a video that ends is marked played and stays on its end; nothing plays next
  on its own. Moving the cursor never changes an item's flags, and only the user pauses, removes, or
  reorders items (the same file of one torrent queued twice is merged into one entry). The playlist
  always includes the current file, including direct Play from search, files, and the Library.
  A new direct play is added first; playing an existing entry keeps its position. The current
  row offers Pause/Resume playback, Stop (keeps it queued), and Stop & remove. Upcoming rows
  pause background transfers instead. Removing the current file, its torrent group, or clearing
  the queue closes its stream and saves progress. Undo restores entries without starting them.
  Clear played preserves an active replay. A full queue rejects a new play with a clear error.
  The playlist panel beside the player collapses with Q, and Shift+N plays the next item. Fullscreen is requested
  on the stage that holds the player, not the player itself, so it stays on when the next video's
  player replaces the last one.
  The player's Rewind 10 seconds and Forward 10 seconds buttons use the same seeking path as
  ←/→, preserve pause state, and clamp to the start and end. They also appear in fullscreen.
- **Library and resume** (`torrentplayer-library`, `ui/src/lib/library.ts`) identify a playable file by
  `(info hash, file index)` and store its safe replay link, display metadata, last position,
  duration, and completion state in `library.sqlite3`. Ordinary playback is checkpointed every
  30 seconds; the first report, seeks, completion, stream stop, and app shutdown force a save.
  Checkpoints carry a process-local revision so blocking writes cannot move the saved playhead
  backward if they finish out of order. An unavailable database disables history for that run,
  never playback. The `library-changed` event updates the Library and queue without polling.
  Explicit torrent removal stops playback, persists removal of its queue entries, cancels
  background writers, and deletes its cache and saved files before deleting its history.
  The `library-removed` event updates the Library; late checkpoints cannot recreate removed
  entries until the user explicitly opens the torrent again.
  Reopening an unfinished item at least 10 seconds in (and not within the final 10 seconds)
  automatically continues from its saved position when `playback.resumePosition` is on, with a
  "Continuing from…" chip offering **Start over**.
- **Queue navigation.** The Queue navigation item opens the player and its playlist panel
  together. With nothing playing, the player shows a placeholder. The panel's ⋯ menu holds
  "Sort by…", "Pause all", "Clear played", and "Clear all". "Sort by…"
  (`sortQueue`) reorders the queue once, with Undo (`restoreOrder`), by name, recently added
  (`QueueItem.addedAt`), size, progress, download speed, peers, or time left; the live keys read
  the current `queue_downloads` snapshot once. Each torrent's files stay together in their
  order, ties and unknown values keep the current order, and nothing re-sorts later.
- **Torrent groups as a tree** (`QueueList.svelte`, an ARIA treeview). A torrent's queued files
  stay together (`groupKey`: the info hash, or the link) and new files join their group in
  lecture order. A torrent with several videos is a node whose videos (all of them once its
  files are known, `groupTree`; queued ones ticked) hang beneath it in lecture order: the
  name plays one now, the tick queues or removes it. ↑/↓ move, ←/→ collapse and expand,
  Enter plays, Space ticks, Alt+↑/↓ (or dragging the torrent row) moves the whole torrent.
  "Choose files…" (`FileChooser.svelte`) is there for bulk changes.
- **Files** (`FileExplorer.svelte`, `ui/src/lib/explorer.ts`) lists the torrents in the queue
  and those opened this session; a torrent only in the queue is added when chosen (details
  only). The chosen torrent's videos fill the top half (lecture order, Play and + Queue), its
  other files the bottom half by folder, and the selected file's preview sits beside them
  (below on narrow windows). `preview_file` shows pictures as `data:` URLs (JPEG, PNG, WebP by
  their bytes, within the thumbnail limits), text and subtitles as plain text (64 KiB), a
  video's first frame only when its start and end are downloaded, and metadata for anything
  else; it never renders HTML or PDF and never opens files. Pictures and text are fetched at
  background priority, capped, and a newer selection stops the previous wait.
- **Readiness.** Every 4 s while the queue is shown, `queue_readiness` reports which items are
  downloaded or have their start and end, for the queue's "Ready" labels. The queue's order
  changes only when the user moves items.
- **Background downloads** (`src-tauri/src/downloads.rs`, `torrentplayer_torrent::bandwidth`).
  Queued items download without pressing Play, all at once unless the user sets a maximum
  (`bandwidth.maxDownloads`, 0 = no limit). Once a second the downloader measures every torrent, and
  the planner decides how fast each may go. The video being watched comes first: while its buffer is
  short, background torrents share only what its media rate (×1.5) leaves of the link's measured
  capacity, higher-ranked torrents getting more (1, 1/2, 1/3, …; librqbit's cannot be limited one by
  one), and while it is starving they pause, connections kept, until it recovers. Once the buffer is
  comfortable the limits lift. The item after the current one ranks first, then the rest after the
  current item, then items before it (played, or passed over), each in queue order. This ranking
  only matters with a maximum of downloads at once. Torrents are added four at a time so a long
  queue does not fetch all of its metadata at once. Items keep paused and completed state
  independently (`QueueItem.paused`, `.played`), and only the user sets `paused`; unticking a video
  removes it. Paused, finished, and waiting items' files are deselected. A queued torrent keeps
  running and uploading, finished or not, watched or not, until the user pauses all of its items
  (one pause stops its download and upload together) or removes them from the queue. The only
  automatic hold is the playback protection above, shown as "Waiting for playback"; it never changes
  `QueueItem.paused`. The settings' download limit covers everything together (both engines,
  playback included) and caps the planner's estimate; one upload limit covers both engines. Queue
  rows show speed, peers, progress, and time left, coloured green (playable), yellow (downloading
  from peers), or grey (waiting).
- **Preloading the next item** starts only once the playing file is fully downloaded, or
  nearly over with the rest arriving in time, so it never competes with the video being
  watched. A torrent not added yet shares the one preload slot with a search's best pick (the
  queued file, not the engine's choice; while a video plays the queue wins). The next lecture
  of a torrent already added (the course playing) is prefetched inside it: torrentplayer-swarm wants
  its start and end at background priority (`Torrent::select_range`), behind the playhead;
  librqbit selects the file and prioritizes its ends in the last minute of playback.
- **Search filters** (`ui/src/lib/filters.ts`) are client-side over the results on screen and
  stay set for the session. The category menu sits in the search box, and the search service
  is asked for that category (Torznab ids, `categoryIds`). The best pick's preload
  does not follow the filters, so tweaking them never churns downloads; when the filters hide
  the best pick, the filter bar says so.

## Cross-cutting rules

- **Errors:** library crates return typed errors (`thiserror`); `anyhow` is used only in
  `torrentplayer-app`. Nothing panics on network data, file contents, or user input.
- **Async:** one Tokio runtime. Blocking and FFI work runs on `spawn_blocking` threads.
  Seeks and shutdowns cancel in-flight work with cancellation tokens.
- **Logging:** `tracing` spans follow a request through the streaming pipeline
  (`torrent → prioritizer → feed → player`) so latency can be attributed to a stage.
