# Architecture overview

TorrentPlayer is a [Tauri v2](https://v2.tauri.app) desktop app. A Rust backend does all the work
(torrents, streaming, playback, networking, storage) and a Svelte webview draws the interface,
including the player controls overlaid on the video.

## Data flow

```text
                 ┌──────────────────────────── Rust backend ────────────────────────────┐
                 │                                                                       │
 peers/trackers  │  ┌──────────────┐  pieces   ┌─────────────────┐   seekable reads      │
 ◄──────────────►│  │ torrentplayer-torrent│◄─────────►│ torrentplayer-prioritize│◄──────────┐           │
  (via torrentplayer-net)│  │  (librqbit)  │ priorities│  (pure logic)   │  playhead │           │
                 │  └──────┬───────┘           └─────────────────┘  + seeks  │           │
                 │         │ file streams                                    │           │
                 │         ▼                                                 │           │
                 │  ┌──────────────────────────┐   bytes    ┌───────────────┴────────┐  │
                 │  │ torrentplayer-stream             │───────────►│ torrentplayer-player           │  │
                 │  │ stream_cb adapter        │            │ libmpv (primary)       │  │
                 │  │ 127.0.0.1 HTTP range srv │───────────►│ <video> fallback       │  │
                 │  └──────────────────────────┘            └──────────┬─────────────┘  │
                 │         ▲ header pieces                             │ events/props   │
                 │  ┌──────┴───────┐   ┌──────────────┐   ┌────────────┴──────────────┐  │
                 │  │ torrentplayer-media  │   │ torrentplayer-course │   │ torrentplayer-app (Tauri shell)   │  │
                 │  │ ffprobe      │   │ file select  │   │ IPC commands + events     │  │
                 │  └──────────────┘   └──────────────┘   └────────────┬──────────────┘  │
                 │                        ┌──────────────┐             │                │
                 │                        │torrentplayer-library │◄────────────┤                │
                 │                        │  (SQLite)    │             │                │
                 │                        └──────────────┘             │                │
                 └─────────────────────────────────────────────────────┼────────────────┘
                                                                       │ IPC (docs/ipc.md)
                                                         ┌─────────────▼──────────────┐
                                                         │ Svelte webview UI          │
                                                         │ controls, status bars, ... │
                                                         └────────────────────────────┘
```

1. The user adds a torrent or magnet link. **torrentplayer-course** inspects the file list and picks the
   lecture videos; **torrentplayer-torrent**'s `StreamEngine` routes the torrent to an engine (below)
   and selects only the files being watched or queued.
2. When playback starts, **torrentplayer-prioritize** asks for the file head and tail first (container
   metadata), then a read-ahead window sized from the media's average bitrate (its length over
   the duration the player reports, or that **torrentplayer-media** read from the container header or,
   with FFmpeg installed, `ffprobe`).
3. **torrentplayer-player** reads bytes through **torrentplayer-stream**: directly via libmpv's `stream_cb`
   callback, or through the loopback HTTP range server for the fallback player. Every read and
   seek is reported back to the prioritizer, so a seek immediately reprioritizes pieces.
4. libmpv events (`time-pos`, `paused-for-cache`, `demuxer-cache-state`, ...) and torrent stats
   flow to the UI as Tauri events about once per second.
5. All peer, tracker, and DHT traffic goes through **torrentplayer-net**, which enforces the selected
   network mode and the kill switch.

## Crates

| Crate                              | Responsibility                                                                                                                                                                      | Depends on Tauri/FFI? | Phase |
| ---------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | --------------------- | ----- |
| `torrentplayer-torrent`            | `StreamEngine` facade over both engines; librqbit session, file selection, path validation, source limits                                                                           | No                    | 1     |
| `torrentplayer-swarm`              | TorrentPlayer's streaming-first BitTorrent engine: deadline scheduler, adaptive queues, cancel on seek, piece store                                                                 | No                    | 1     |
| `torrentplayer-prioritize`         | Piece prioritization decisions; no I/O                                                                                                                                              | No                    | 1     |
| `torrentplayer-stream`             | `stream_cb` adapter and loopback HTTP range server                                                                                                                                  | No                    | 1     |
| `torrentplayer-media`              | Track and duration probe of container headers; subtitle matching and parsing (pure Rust); the user's optional `ffprobe` (durations) and `ffmpeg` (seek-bar thumbnails), locked down | No                    | 1, 2  |
| `torrentplayer-player`             | libmpv wrapper and fallback player; **only crate with `unsafe`**                                                                                                                    | FFI                   | 2     |
| `torrentplayer-course`             | Video detection, filtering, natural sort, course grouping; no I/O                                                                                                                   | No                    | 3     |
| `torrentplayer-library`            | Play queue (versioned JSON, atomic writes); SQLite library, progress, resume positions                                                                                              | No                    | 1, 4  |
| `torrentplayer-search`             | Search: emberglow event stream (fan-out fallback), strict parsing, merging, relevance and watch-score ranking, best pick                                                            | No                    | 4     |
| `torrentplayer-net`                | Network modes, WireGuard, SOCKS5, kill switch                                                                                                                                       | No                    | 5     |
| `torrentplayer-app` (`src-tauri/`) | Tauri shell: IPC commands, events, app state                                                                                                                                        | Tauri                 | 0     |

### Engine choice

`torrentplayer_torrent::StreamEngine` sends every source to **torrentplayer-swarm** by default, with the same
safety checks on every path. For a magnet link, librqbit starts lazily as a metadata-only
resolver and hands its metadata and discovered peers to torrentplayer-swarm. `TORRENTPLAYER_ENGINE=auto|swarm|rqbit`
overrides the choice. Downloads go to separate
`torrentplayer-swarm/` and `librqbit/` sub-folders. See [ADR 0006](adr/0006-streaming-engine.md).

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

Keeping logic out of `torrentplayer-app` means it can be unit-tested without a webview, and keeping
`unsafe` inside `torrentplayer-player` means only one crate needs FFI-level review.
See [ADR 0002](adr/0002-workspace-layout.md).

## Frontend

The UI lives in `ui/src/` (Svelte 5 + TypeScript, built by Vite into `ui/dist/`). It talks to the
backend only through `ui/src/lib/ipc.ts`, using types generated from Rust (see
[IPC contract](ipc.md)). The webview loads only bundled assets: no remote content, no inline
scripts, and a strict Content Security Policy (see [Threat model](threat-model.md)).

### Navigation

`ui/src/App.svelte` is the shell: a narrow rail at the left (`NavRail.svelte`) and one view at a
time. Every view stays reachable from the rail.

```text
┌──────┬────────────────────────────────────────────────┐
│ ‹  › │                                                │  ‹ › Back / Forward
│      │   Search · Open · Files · Playing · Settings   │      (Alt+←/→, mouse side buttons)
│  ⌕   │                                                │  ⌕  Search box and results (Ctrl+F)
│  ▢   │   Playing (the one queue):                     │  ▢  Open: link, .torrent file, torrents
│  ≡   │   ┌──────────────────┬─────────────────────┐   │     opened this session (Ctrl+O)
│  ▶³  │   │ Queue        ⋯ ‹ │                     │   │  ≡  Files: the file explorer
│      │   │ ▾ Course (3/12)  │      <video>        │   │  ▶  Playing: the video and the queue,
│      │   │ ├ ☑ Lecture 1 ▶  │   (or "Pick some-   │   │     with its upcoming count
│      │   │ ├ ☑ Lecture 2    │    thing from the   │   │
│      │   │ └ ☐ Lecture 3    │    queue or search")│   │
│  ⚙   │   │   Single video   │                     │   │  ⚙  Settings (Ctrl+,)
│      │   └──────────────────┴─────────────────────┘   │
└──────┴────────────────────────────────────────────────┘
```

- **History** (`ui/src/lib/nav.ts`) is browser-style back and forward over small view
  descriptors. Each search is its own entry, so Back moves between recent searches.
- **State survives navigation.** The search page stays mounted (hidden), so the query,
  results, filters, and preload are as they were. Scroll positions and focus are remembered
  per view. Leaving the player unmounts it but keeps the stream open (it keeps downloading).
  Coming back resumes at the same position. Playing something else, or a failed start, closes
  the stream.
- **Queue** (`ui/src/lib/queue.ts`, persisted by `torrentplayer-library`) is a playlist with a cursor.
  It is manual: a video that ends is marked played and stays on its end; nothing plays next on
  its own. Moving the cursor never changes an item's flags, and only the user skips, pauses, or
  removes items (the same file of one torrent queued twice is merged into one entry). The
  playlist panel beside the player collapses with Q, and Shift+N plays the next item. Fullscreen is requested on the stage that holds the player,
  not the player itself, so it stays on when the next video's player replaces the last one.
- **Library and resume** (`torrentplayer-library`, `ui/src/lib/library.ts`) identify a playable file by
  `(info hash, file index)` and store its safe replay link, display metadata, last position,
  duration, and completion state in `library.sqlite3`. Ordinary playback is checkpointed every
  five seconds; the first report, seeks, completion, stream stop, and app shutdown force a save.
  Checkpoints carry a process-local revision so blocking writes cannot move the saved playhead
  backward if they finish out of order. An unavailable database disables history for that run,
  never playback. The `library-changed` event updates the Library and queue without polling.
  Reopening an unfinished item at least 10 seconds in (and not within the final 10 seconds) offers
  Resume or Start over when `playback.resumePosition` is on.
- **One queue, in Playing.** The playlist panel beside the player is the queue (there is no
  separate Queue page); Playing is reachable with nothing playing, showing a placeholder. Its
  ⋯ menu holds "Play ready videos first", "Keep course order", "Clear played", and "Clear all".
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
- **Play ready videos first** (`playback.optimizeQueue`, off by default). Every 4 s while the
  queue is shown, `queue_readiness` reports which items are downloaded or have their start
  and end. When that set changes, `optimizeQueue` moves ready torrents (and, without "Keep
  course order", ready files within a torrent) ahead of the rest. It never moves played items,
  the video playing, or the rest of its torrent, and keeps every other order; a toast offers
  Undo (`restoreOrder`).
- **Background downloads** (`src-tauri/src/downloads.rs`, `torrentplayer_torrent::bandwidth`). Queued
  items download without pressing Play, all at once unless the user sets a maximum
  (`bandwidth.maxDownloads`, 0 = no limit). Once a second the downloader measures every
  torrent, and the planner decides how fast each may go. The video being watched comes first:
  while its buffer is short, background torrents share only what its media rate (×1.5) leaves
  of the link's measured capacity, higher-ranked torrents getting more (1, 1/2, 1/3, …;
  librqbit's cannot be limited one by one), and while it is starving they pause, connections
  kept, until it recovers. Once the buffer is comfortable the limits lift. The item after the
  current one ranks first, then the rest after the current item, then items before it
  (played, or passed over); within that "In order" follows the queue and "Fastest first"
  prefers the fastest torrents, keeping a torrent's own files in order (`Queue.order`). This
  ranking only matters with a maximum of downloads at once. Torrents are added four at a time so
  a long queue does not fetch all of its metadata at once. Items keep paused, skipped, and
  completed state independently (`QueueItem.paused`, `.skipped`, `.played`), and only the user
  sets `paused` and `skipped`. Paused, skipped, finished, and waiting items' files are
  deselected. A queued torrent keeps running and uploading, finished or not, watched or not,
  until the user pauses all of its items (one pause stops its download and upload together) or
  removes them from the queue. The only automatic hold is the playback protection above, shown
  as "Waiting for playback"; it never changes `QueueItem.paused`. The settings' download limit covers everything together (both engines, playback
  included) and caps the planner's estimate; one upload limit covers both engines. Queue rows
  show speed, peers, progress, and time left, coloured green (playable), yellow (downloading
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
