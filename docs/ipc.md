# IPC contract

The webview talks to the Rust backend only through Tauri commands (request/response) and events
(backend → frontend push). This page is the single source of truth for that contract.

## Rules

- **Declare, then grant.** Every command is listed in `src-tauri/build.rs` (which makes Tauri
  generate an `allow-<command>` permission) and granted to a window in
  `src-tauri/capabilities/*.json`. Ungranted commands are rejected by Tauri.
- **Validate in Rust.** Treat every argument as untrusted. Validate ranges, lengths, and formats
  before use, and return a typed error rather than panicking.
- **Generated types.** Payload types derive `ts_rs::TS` with `#[ts(export)]`. Running
  `cargo test -p torrentplayer-app` writes them to `ui/src/lib/bindings/`. Never edit those files by hand;
  CI fails if they are out of date.
- **One wrapper module.** Frontend code calls commands only through `ui/src/lib/ipc.ts`.
- **Naming.** Commands are `snake_case` verbs or nouns (`app_info`, `add_torrent`). Events are
  `kebab-case` with a subsystem prefix (`stream-stats`, `net-state-changed`). Payload fields are
  `camelCase` (`#[serde(rename_all = "camelCase")]`).

## Errors

Commands that can fail will return `Result<T, IpcError>`, where `IpcError` serializes as:

```ts
{
  kind: string;
  message: string;
}
```

`kind` is a stable machine-readable code; `message` is human-readable and never contains secrets
or full paths from the user's home directory (engine errors are logged in full and replaced with
a generic message).

| `kind`                | Meaning                                                                                                        |
| --------------------- | -------------------------------------------------------------------------------------------------------------- |
| `invalid-argument`    | An argument failed validation.                                                                                 |
| `not-found`           | Unknown torrent, file, or stream.                                                                              |
| `source-refused`      | The link or `.torrent` file was refused (scheme, size, malformed, or unsupported by the engine).               |
| `unsafe-torrent`      | A file path in the torrent could escape the download folder, or it has too many files.                         |
| `metadata-timeout`    | A magnet link's metadata could not be found in time (likely no peers).                                         |
| `engine`              | The torrent engine failed; details are in the log.                                                             |
| `search-unavailable`  | The search service is rate limiting, down, timed out, or sent an unreadable response; retrying later may help. |
| `kill-switch`         | The selected proxy or tunnel is not working, or traffic is held, so the request was blocked; nothing was sent. |
| `network-unavailable` | A network request (such as the connection check) failed.                                                       |
| `cancelled`           | The request was cancelled: a search replaced by a newer one or stopped with `cancel_search`. Show nothing.     |
| `audio-conversion`    | The audio could not be converted for the built-in player; details are in the log.                              |
| `internal`            | Something unexpected failed; details are in the log.                                                           |

`errorMessage` in `ui/src/lib/ipc.ts` shows `message` as is, and for `kill-switch` and
`network-unavailable` adds what it means and what to do (nothing was sent; traffic resumes by
itself; check Settings → Network & VPN).

## Engines

Torrents are served by one of two engines behind the same commands (`torrentplayer_torrent::StreamEngine`):

- **torrentplayer-swarm**, TorrentPlayer's own streaming-first engine, for `.torrent` files and http(s) links to
  them. See [ADR 0006](adr/0006-streaming-engine.md).
- **librqbit** for magnet links (torrentplayer-swarm resolves magnets from milestone M3). It starts
  lazily, the first time a magnet link is added.

The `TORRENTPLAYER_ENGINE` environment variable overrides the choice: `auto` (default), `swarm` (magnet
links are refused with `source-refused`), or `rqbit` (everything through librqbit). The engine
serving a stream is reported in `StreamStats.engine` and logged when the stream starts.

## Commands

### `app_info`

Returns static information about the running build, for the About screen.

|            |                                                |
| ---------- | ---------------------------------------------- |
| Arguments  | none                                           |
| Returns    | [`AppInfo`](../ui/src/lib/bindings/AppInfo.ts) |
| Errors     | none                                           |
| Permission | `allow-app-info` (granted to `main`)           |

```ts
type AppInfo = {
  name: string; // "TorrentPlayer"
  version: string; // semantic version, e.g. "0.1.0"
  license: string; // SPDX expression, "MIT OR Apache-2.0"
};
```

### `add_torrent_link`

Adds a torrent from a magnet link or an http(s) link to a `.torrent` file, with no files
selected. Only `magnet:`, `http:`, and `https:` are accepted; downloaded `.torrent` files are
limited to 16 MiB. Links and their redirects must lead to public hosts: addresses and names that
resolve to the local network (private, loopback, link-local, `localhost`) are refused, and
redirects are followed at most 5 times, only to http(s), and never from https to http. An http(s) link that redirects to a
magnet link (as search sites' download links, such as `search.emberglow.tech/download/…`, do
for indexers that only have magnets) adds that magnet link (`StreamEngine::resolve_source`;
adds, preloads, and prefetches resolve links the same way). What each link gave is remembered
(256 links), so following it again sends no request; links are never logged, since search sites
put tokens in them. The search service's own download links count against the search rate
limiter. Adding a torrent that is already present returns the existing one. `TorrentSummary.magnet` is a magnet link that adds the torrent again; for
torrents in torrentplayer-swarm it carries up to 20 of their trackers (about 4 KiB at most).

|            |                                                                  |
| ---------- | ---------------------------------------------------------------- |
| Arguments  | `{ link: string }` (at most 16 KiB)                              |
| Returns    | [`TorrentSummary`](../ui/src/lib/bindings/TorrentSummary.ts)     |
| Errors     | `source-refused`, `unsafe-torrent`, `metadata-timeout`, `engine` |
| Permission | `allow-add-torrent-link`                                         |

### `add_torrent_file`

As `add_torrent_link`, from the raw bytes of a `.torrent` file sent as the request body
(`invoke('add_torrent_file', bytes)` with a `Uint8Array`). At most 16 MiB.

|            |                                                              |
| ---------- | ------------------------------------------------------------ |
| Arguments  | raw request body                                             |
| Returns    | [`TorrentSummary`](../ui/src/lib/bindings/TorrentSummary.ts) |
| Errors     | `invalid-argument` (not raw bytes), as `add_torrent_link`    |
| Permission | `allow-add-torrent-file`                                     |

`TorrentSummary` lists `files` with videos first; each has an `index` (used to start a stream),
`path`, `length`, and `kind` (`video`, `subtitle`, `document`, `other`).

### `start_stream`

Selects a video file for download, starts prioritizing it, and publishes it on the loopback
stream server.

|            |                                                         |
| ---------- | ------------------------------------------------------- |
| Arguments  | `{ torrentId: number, fileIndex: number }`              |
| Returns    | [`StreamInfo`](../ui/src/lib/bindings/StreamInfo.ts)    |
| Errors     | `not-found`, `invalid-argument` (not a video), `engine` |
| Permission | `allow-start-stream`                                    |

`StreamInfo.url` contains the per-session token: give it to the player and never log or
persist it. `resumePositionSecs` is a saved position worth offering when
`playback.resumePosition` is enabled, otherwise `null`; completed media, the first 10 seconds,
and the final 10 seconds are not offered.

### `report_playback`

Reports the player's position, on every seek and about once a second. Seeks re-prioritize
immediately. Playback history is persisted on the first report, after a seek, at completion,
every five seconds during ordinary playback, on stream stop, and at shutdown. Database failures
are logged but never interrupt playback. Without `durationSecs` (the player cannot tell, for
example a Matroska file with no
duration in its header), the duration from the container header or `ffprobe` is used, once
known (see [FFmpeg](#ffmpeg-optional)).

|            |                                                                            |
| ---------- | -------------------------------------------------------------------------- |
| Arguments  | `{ streamId: number, positionSecs: number, durationSecs: number \| null }` |
| Returns    | nothing                                                                    |
| Errors     | `invalid-argument` (position not finite or out of range), `not-found`      |
| Permission | `allow-report-playback`                                                    |

### `record_player_metric`

Records a decoder-level timing measured in the webview: `firstFrame` (play to first frame) or
`seek` (seek to first frame). Values must be between 0 and 600 000 ms.

|            |                                                                      |
| ---------- | -------------------------------------------------------------------- |
| Arguments  | `{ streamId: number, kind: "firstFrame" \| "seek", millis: number }` |
| Returns    | nothing                                                              |
| Errors     | `invalid-argument`, `not-found`                                      |
| Permission | `allow-record-player-metric`                                         |

### `stream_stats`

Live statistics for a stream, polled about once a second by the debug overlay.

|            |                                                        |
| ---------- | ------------------------------------------------------ |
| Arguments  | `{ streamId: number }`                                 |
| Returns    | [`StreamStats`](../ui/src/lib/bindings/StreamStats.ts) |
| Errors     | `not-found`, `engine`                                  |
| Permission | `allow-stream-stats`                                   |

Fields include transfer rates, peer counts, buffered bytes (and seconds once the duration is
reported), a 200-cell `pieceMap` (0–255 per cell), player timings, and `engine`
(`"torrentplayer-swarm"` or `"librqbit"`). `anchors` (prioritized positions) is filled only by the
librqbit engine; torrentplayer-swarm schedules by deadline instead.

### `stream_audio`

The stream's audio tracks, read from its container header (MP4, MOV, Matroska, or WebM) by
`torrentplayer-media` when the stream starts. The probe reads only box and element headers through the
stream's own reader, which waits for those pieces, and gives up after 45 s. The command waits for
the probe, so the player calls it once.

|            |                                                        |
| ---------- | ------------------------------------------------------ |
| Arguments  | `{ streamId: number }`                                 |
| Returns    | [`StreamAudio`](../ui/src/lib/bindings/StreamAudio.ts) |
| Errors     | `not-found` (unknown or stopped stream)                |
| Permission | `allow-stream-audio`                                   |

```ts
type StreamAudio = {
  probed: boolean; // false: unsupported container, malformed header, or timed out
  trackCount: number; // 0: the video has no sound
  primary: AudioTrackDto | null; // the default track, else the first
  alternative: AudioTrackDto | null; // a decodable track, when `primary` is not
  convertible: boolean; // `primary` does not decode, and `convert_audio` can convert it
};
type AudioTrackDto = {
  codec: string; // "aac", "ac3", "eac3", "truehd", "dts", ...
  label: string; // "Dolby Digital (AC-3)"
  decodes: boolean | null; // whether the built-in player decodes it; null: varies or unknown
};
```

When `convertible`, the player plays the stream through `convert_audio`. Otherwise it shows a
notice when the codec cannot be decoded, after Chromium's decoder counters confirm that no audio
is being decoded (see `ui/src/lib/audio.ts`). The same probe lists the embedded subtitle tracks that
`stream_subtitles` returns.

### `convert_audio`

Starts converting a stream's primary audio track (Dolby or DTS) to AAC with the bundled FFmpeg,
from `start` seconds, and stops the stream's previous conversion. Waits for the probe and for
FFmpeg's initialization segment (at most 60 s). The result is a live stream: fragmented MP4 with
the video copied and the original timestamps, read once with `fetch` and fed to Media Source
Extensions (see `ui/src/lib/converted.ts` and [ADR 0007](adr/0007-dolby-and-dts-audio.md)).

|            |                                                                                                      |
| ---------- | ---------------------------------------------------------------------------------------------------- |
| Arguments  | `{ streamId: number, start: number }`                                                                |
| Returns    | [`ConvertedStream`](../ui/src/lib/bindings/ConvertedStream.ts)                                       |
| Errors     | `not-found`, `invalid-argument` (bad `start`, or audio that needs no conversion), `audio-conversion` |
| Permission | `allow-convert-audio`                                                                                |

```ts
type ConvertedStream = {
  url: string; // loopback URL with a token, readable once; do not log it
  mime: string; // for MediaSource.addSourceBuffer: video/mp4; codecs="avc1.64001f,mp4a.40.2"
  duration: number | null; // seconds, if the container states it
};
```

### `stream_subtitles`

The subtitles available for a stream, at once (it does not wait).

- **`files`**: the `.srt`, `.vtt`, `.ass`, and `.ssa` files in the torrent (at most 200), matched
  to the playing video first: the same name (`Movie.srt`, `Movie.en.srt`), a `Subs` or
  `Subtitles` folder next to it (`Subs/Movie.en.srt`, `Subs/Movie/2_English.srt`), or every file
  when the torrent has one video. The others have `matched: false` ("other subtitles"). The
  language (BCP 47) and the `forced` and SDH flags come from the file name.
- **`embedded`**: tracks in the container (Matroska `S_TEXT/*`, PGS, VobSub; MP4 `tx3g`, `wvtt`,
  `stpp`, `c608`). It is `null` until the probe has finished, so the player asks again once
  `stream_audio` resolves. The built-in player cannot show these yet.

|            |                                                                |
| ---------- | -------------------------------------------------------------- |
| Arguments  | `{ streamId: number }`                                         |
| Returns    | [`StreamSubtitles`](../ui/src/lib/bindings/StreamSubtitles.ts) |
| Errors     | `not-found`                                                    |
| Permission | `allow-stream-subtitles`                                       |

### `load_subtitles`

Downloads one subtitle file of the stream's torrent and returns its cues. The file is downloaded
at background priority (`StreamEngine::read_small_file`), so the video keeps priority, and read
only once complete; the command gives up after 60 s. Parsing happens in Rust (`torrentplayer-media`):
SRT and WebVTT fully, ASS/SSA dialogue text only; UTF-8 (with or without a byte order mark),
UTF-16, or Windows-1252. The text is plain (markup and control characters removed); at most
5 MiB, 20 000 cues, and 600 characters per cue.

|            |                                                                                        |
| ---------- | -------------------------------------------------------------------------------------- |
| Arguments  | `{ streamId: number, fileIndex: number }` (a subtitle file of the stream's torrent)    |
| Returns    | [`SubtitleCues`](../ui/src/lib/bindings/SubtitleCues.ts)                               |
| Errors     | `invalid-argument` (not a subtitle file, too large, unreadable), `engine`, `not-found` |
| Permission | `allow-load-subtitles`                                                                 |

```ts
type SubtitleCues = {
  format: string; // "srt", "webvtt", or "ass"
  cues: { startMs: number; endMs: number; text: string }[]; // plain text, `\n` line breaks
  truncated: boolean; // more than 20 000 cues; the rest were dropped
};
```

### `parse_subtitles`

Parses a subtitle file the user chose ("Load subtitle file…"), sent as the raw request body
(`invoke('parse_subtitles', bytes)`), the same way as `load_subtitles`.

|            |                                                          |
| ---------- | -------------------------------------------------------- |
| Arguments  | raw request body (at most 5 MiB)                         |
| Returns    | [`SubtitleCues`](../ui/src/lib/bindings/SubtitleCues.ts) |
| Errors     | `invalid-argument`                                       |
| Permission | `allow-parse-subtitles`                                  |

The player adds cues with the TextTrack API (`addTextTrack` and `VTTCue`, text escaped), so they
never pass through a URL or HTML (see `ui/src/lib/subtitles.ts`).

### `stop_stream`

Unpublishes a stream and stops prioritizing it. Downloaded data is kept.

|            |                        |
| ---------- | ---------------------- |
| Arguments  | `{ streamId: number }` |
| Returns    | nothing                |
| Errors     | `not-found`            |
| Permission | `allow-stop-stream`    |

### `dev_autoplay`

Development builds only (always `null` in release builds): adds the torrent named by
`TORRENTPLAYER_DEV_AUTOPLAY=<link or .torrent path>|<file index>|<seek fractions>|<peers>` so decoder
timings can be measured automatically. The optional peers are addresses to connect to, such as
a local `torrentplayer-bench seed` (see [Building](building.md)).

|            |                                                                  |
| ---------- | ---------------------------------------------------------------- |
| Arguments  | none                                                             |
| Returns    | [`DevAutoplay`](../ui/src/lib/bindings/DevAutoplay.ts) or `null` |
| Errors     | as `add_torrent_link`                                            |
| Permission | `allow-dev-autoplay`                                             |

### `search_torrents`

Searches public torrent indexers through [search.emberglow.tech](https://search.emberglow.tech),
using all of its indexers, in one `/search` request. The service answers within about 6 s,
leaving out indexers slower than that; prefer
[`search_torrents_progressive`](#search_torrents_progressive). Results are validated in
`torrentplayer-search`: titles are plain text (at most 300 characters, no control characters), magnet
links have the form `magnet:?xt=urn:btih:…` (built from the info hash when the indexer gave
none), details links are http(s) only, results that cannot be added are dropped, duplicates are
merged by info hash (or by download link), and adult results (an adult category label, or a Torznab category id in
6000–6999) are excluded unless the `search.hideAdult` setting is off (read on every search; the
10-minute cache keeps the two apart). Placeholder seed counts the service marks `suspicious`
(32767 on fake listings) count as unknown. The service has no license data, so
`license` is always `null` (unknown) for now. Add a result with
`add_torrent_link(result.link)`.

Some indexers give neither a magnet link nor an info hash (about a third of results in October
2026). The service then offers a `download` link of its own
(`https://search.emberglow.tech/download/<token>`) that resolves the torrent through the
indexer: a `302` to its magnet link, or the `.torrent` file. While a search runs the service
looks up the magnet links of the best-seeded of these itself (20 per stream, 10 per
`/search`). Such results are kept with `infoHash` and `magnet` `null`, `download` set, and
`link` equal to `download`, until their magnet link is known. A result's `key` comes from the
first copy seen (`download:` and the link, or else the info hash) and does not change when its
magnet link arrives or another indexer's copy of the same torrent is merged into it (copies merge
by info hash once it is known). Only links on the service's own host, under `/download/`,
with no credentials, are kept, because the best pick's link is followed without the user asking
(to preload it). Following a new link costs the service a request to the indexer (up to about
6 s), so TorrentPlayer follows one only to add, preload, or prefetch it (see
[`prefetch_torrents`](#prefetch_torrents)).

Results are ranked by relevance first (titles containing every query word, then more of
them), then by a watch score: seeders (log-scaled) and swarm health, resolution, source
(camera copies, telesyncs, and screeners rank last), HDR, whether the built-in player can play
the container, and a size check that flags fake 4K. The formula is documented in
`crates/torrentplayer-search/src/rank.rs`. Each result carries its parsed `quality`, a short `reason`
(for example `1080p WEB-DL · 412 seeders · plays in built-in player`), its `quality.kind`
(`video`, `notVideo` for audio, ebooks, software, and games, or `unknown`; with
`quality.audio`, `quality.audioPlays`, and `quality.audioConverted` for the audio codec the title
names (Dolby and DTS play, converted to stereo, when the build has the bundled FFmpeg); `health` and
`suspicious` from the search service; the search page hides
`notVideo` results by default and never hides `unknown` ones), and `bestPick`: at most
one result, always first, that contains every query word, has seeders, is a video, and is not a
camera copy, fake 4K, or a file the built-in player cannot open. Audiobooks, music, and ebooks
(titles naming mp3, flac, m4b, a kbps bitrate, pdf, epub, and so on, without any video word, or
Torznab category ids for audio or books) are not videos; a torrent with no sign either way must
be at least 100 MiB. Finished searches are kept for 10 minutes
(the service's own cache lifetime), so repeating one (in any letter case or spacing) is
instant. When the search finishes, the best pick is preloaded (see
[`preload_status`](#preload_status)) if the `search.preloadTopResult` setting is on.

|            |                                                                                                            |
| ---------- | ---------------------------------------------------------------------------------------------------------- |
| Arguments  | `{ query: string, categories?: number[] }` (query trimmed; 1–200 characters; no control characters)        |
| Returns    | [`SearchResponseDto`](../ui/src/lib/bindings/SearchResponseDto.ts): `results`, `indexers`, `excludedAdult` |
| Errors     | `invalid-argument`, `search-unavailable`                                                                   |
| Permission | `allow-search-torrents`                                                                                    |

The query is sent to the search service through the selected network mode (see
[Network](#network)); while the kill switch is engaged the search fails with
`search-unavailable`.

`categories` limits the search to some Torznab categories, sent as repeated `category`
parameters: for example 2000 Movies, 5000 TV, 5070 Anime, 3000 Audio, 7000 Books, 8000 Other (a
parent includes its subcategories). Empty or missing means all. At most 4 ids, each in
1000–8999; adult ids (6000–6999) are always refused with `invalid-argument`. The service then
asks only the indexers that carry those categories (faster) and no longer leaves out Console,
PC, and Books results. Searches in different categories are cached apart.

### `search_torrents_progressive`

Like `search_torrents`, but sends the merged, ranked results so far over a channel as the
indexers answer. It reads the service's event stream (`GET /search/stream`, server-sent
events): one request per search, one `indexer` event per indexer as it finishes (the first
after about 0.2 s, the first results after 0.4–0.7 s), a `magnet` event for each result offered
only as a download link whose magnet link the service looked up (matched by its `download`
link; the row is updated in place), then a `done` event after the slowest indexer answers or
the service's 20 s limit. TorrentPlayer merges and ranks every indexer's results
itself, so the final list is not cut to the service's seeder-ranked top 100. The indexer list
(`GET /indexers`, kept for 10 minutes) is fetched alongside the stream to give `total`, the
number of indexers; until it arrives `total` is 0.

The stream is untrusted input: a strict parser keeps at most 4 MiB per event and 16 MiB and
400 events per stream, accepts at most 5,000 results, and stops reading after 45 s. A
malformed or oversized event after the first is skipped; a stream that breaks after some
events ends the search with what arrived (indexers that had not answered get an error, and the
result is not cached). If the service does not offer the stream, or the first thing it sends
is not a readable event, the command falls back to asking the indexers in small groups
through `/search`: fast indexers four at a time, slow ones on their own, at most 12 requests at
once; indexers the service skips, excludes by default (1337x), or that are adult-only (unless
`search.hideAdult` is off) are not asked. A `422`, a rate limit after one retry, or a backend
outage (`502`–`504`, or the stream's `error` event before any results) fails the search
instead. A client-side rate limiter (burst 20, 50 a minute) keeps TorrentPlayer under the service's
limit either way.

Progress reaches the channel at most every 150 ms (the last report at once), so the burst of
indexers answering in the first second re-renders the list a few times rather than dozens.
Starting another progressive search, or [`cancel_search`](#cancel_search), stops this one: it
fails with `cancelled` and decides nothing further. A new search also stops the metadata
prefetch of the previous results (see [`prefetch_torrents`](#prefetch_torrents)).

The best pick is preloaded (see [`preload_status`](#preload_status)) once it has settled, if
the `search.preloadTopResult` setting is on: when it has stayed the best pick for 2 s, or when
the search finishes, whichever is first. If a late indexer changes the best pick after that,
the final one is preloaded when the search finishes, so a search starts at most two preloads
(usually one). A search that finishes without a best pick evicts the previous preload. The
rule is `torrentplayer_search::settle`.

|            |                                                                                                                                         |
| ---------- | --------------------------------------------------------------------------------------------------------------------------------------- |
| Arguments  | `{ query: string, categories?: number[], onProgress: Channel<SearchProgressDto> }` (see [`search_torrents`](#search_torrents))          |
| Channel    | [`SearchProgressDto`](../ui/src/lib/bindings/SearchProgressDto.ts): `response` (results so far), `completed`, `total`, `done`, `cached` |
| Returns    | [`SearchResponseDto`](../ui/src/lib/bindings/SearchResponseDto.ts), equal to the last progress `response`                               |
| Errors     | `invalid-argument`; `search-unavailable`; `cancelled`                                                                                   |
| Permission | `allow-search-torrents-progressive`                                                                                                     |

Use `searchTorrentsProgressive(query, onProgress)` in `ui/src/lib/ipc.ts`, which creates the
channel.

### `cancel_search`

Stops the running progressive search, if any: it fails with `cancelled` and preloads nothing
more. The metadata prefetch is not stopped.

|            |                       |
| ---------- | --------------------- |
| Arguments  | none                  |
| Returns    | nothing               |
| Errors     | none                  |
| Permission | `allow-cancel-search` |

### `prefetch_torrents`

Fetches the torrent details (metadata only) of search results in the background, so that
playing any of them starts sooner; only the best pick also preloads video. Pass the results on
screen, top first (`key`, `magnet`, `download`, `infoHash`): the first 20 usable ones are
fetched, at most three at a time and each for at most 20 s, and each one's key and info hash are
sent to `onReady` as it is ready. A result with a download link on the search service is
fetched through it first (plain HTTP when the link serves the `.torrent` file, which also works
without the DHT; most redirect to a magnet link), the torrent it gives is checked against
`infoHash` when that is known, and the magnet link is used if the link fails, is refused (16 MiB
limit, public hosts and http(s)-only redirects without downgrades), or gives another torrent. Download links count against the
search rate limiter, and only the first four per call are followed: each new one costs the
service a request to an indexer, and its operators ask clients to follow only links they will
use. A newer
call, or a new search, stops the running one. For a magnet link librqbit fetches the metadata
from peers (no files are created, nothing is downloaded or uploaded); a `.torrent` link is
downloaded. The metadata gets the same checks as an add (file paths, file count) and is kept in
the engine with the peers seen (64 torrents, 64 MiB at most, least recently used evicted), so
adding or preloading that torrent later skips asking peers for it: in torrentplayer-swarm it uses the
kept metadata and peers, and with librqbit (automatic engine, magnet links) it is added from
the kept metadata and still finds peers through the DHT and trackers. Concurrent fetches of the
same torrent (a prefetch and an add) wait for each other. Failures are only logged; playing a
result fetches its metadata itself if needed. Traffic follows the network mode: protected modes
have no DHT, so magnet links without working trackers fail there, and in protected modes
librqbit is given the magnet link, not the kept metadata. Does nothing while
`search.preloadTopResult` is off. Starts the engine.

|            |                                                                                                       |
| ---------- | ----------------------------------------------------------------------------------------------------- |
| Arguments  | `{ results: PrefetchRequest[], onReady: Channel<PrefetchedDto> }` (at most 200 results)               |
| Channel    | [`PrefetchedDto`](../ui/src/lib/bindings/PrefetchedDto.ts): `key` (`SearchResultDto.key`), `infoHash` |
| Returns    | nothing, once every result is done or a newer call or search stops it                                 |
| Errors     | `invalid-argument` (too many results), `engine` if the engine cannot start                            |
| Permission | `allow-prefetch-torrents`                                                                             |

Use `prefetchTorrents(results, onReady)` in `ui/src/lib/ipc.ts` with
[`PrefetchRequest`](../ui/src/lib/bindings/PrefetchRequest.ts)s. The search page asks for the first
12 results shown (without the best pick) once the list has held still for 1.5 s, and marks the
ready ones "details ready".

### `fetch_thumbnails`

Finds a thumbnail for each of these search results: a picture from inside the torrent. Pass
the results on screen whose metadata is known (prefetched with
[`prefetch_torrents`](#prefetch_torrents), or the preloaded best pick), top first: the first
12 are tried, two at a time and each for at most 20 s, and each one's outcome is sent to
`onReady` (`dataUrl` is `null` when the torrent has no suitable picture or it could not be
fetched). A newer call, or a new search, stops the running one. Does nothing while
`search.preloadTopResult` is off.

The picture is chosen from the torrent's file list (`torrentplayer_torrent::thumbnail`): JPEG, PNG, or
WebP only (never SVG or GIF), 2 KiB to 2 MiB, preferring `__ia_thumb.jpg` (Internet Archive),
then names like poster, cover, folder, thumb, and screenshot, and never an indexer's banner
(names with `www`, `yts`, `rarbg`, `torrent`, or ending in a domain such as `.com`). Only that
file's pieces are downloaded, at most 8 MiB of them, and checked against the torrent's hashes; a
torrent already in torrentplayer-swarm gives the picture from there, any other is fetched by a separate
torrentplayer-swarm limited to 2 MiB/s and 8 peers, from the peers seen while fetching its metadata and
its trackers and web seeds, uploading nothing, and its cache is deleted afterwards. The bytes
must start like a JPEG, PNG, or WebP file, whatever the name says, and reach the webview only as
a `data:image/…;base64,` URL (the content security policy allows `data:` images and nothing
else from outside). No outside service (such as a poster API) is asked: that would tell it what
the user searches for.

|            |                                                                                                    |
| ---------- | -------------------------------------------------------------------------------------------------- |
| Arguments  | `{ results: ThumbnailRequest[], onReady: Channel<ThumbnailDto> }` (`key`, `infoHash`; at most 200) |
| Channel    | [`ThumbnailDto`](../ui/src/lib/bindings/ThumbnailDto.ts): `key` (`SearchResultDto.key`), `dataUrl` |
| Returns    | nothing, once every result is done or a newer call or search stops it                              |
| Errors     | `invalid-argument` (too many results), `engine` if the engine cannot start                         |
| Permission | `allow-fetch-thumbnails`                                                                           |

The search page shows each picture in a fixed box at the left of its row (larger for the best
pick), so nothing moves when one arrives.

### `preview_frame`

The preloaded best pick's video, served by the local stream server so the search page can show
a frame from a few seconds in, which is already downloaded (a paused, muted `<video>`; only
for videos the webview decodes). `null` unless a preload is ready. It does not claim the
preload, does not count as playing, and replaces the previous one. The URL carries the stream
server's session token. Never starts the engine.

|            |                                                                                             |
| ---------- | ------------------------------------------------------------------------------------------- |
| Arguments  | none                                                                                        |
| Returns    | [`PreviewFrameDto`](../ui/src/lib/bindings/PreviewFrameDto.ts) or `null`: `infoHash`, `url` |
| Errors     | none                                                                                        |
| Permission | `allow-preview-frame`                                                                       |

### `torrent_details`

A search result's details from its torrent's metadata, for the expandable details view: every
file, piece size, total size, trackers (UDP and HTTP counted apart, since protected network
modes block UDP) and web seeds, and the `.torrent` file's optional `private` flag, `comment`,
`created by`, and `creation date`. Those extra fields are read by a small bounded bencode
scanner (only a file that parses to the end counts) and kept as plain text of at most 500
characters without control characters; render them as text only. `null` if the metadata is not
known. Without `load` nothing is sent and the engine is not started; with `load` the metadata
is fetched first (metadata only, at most 30 s: through the search service's download link, then
the magnet link, as [`prefetch_torrents`](#prefetch_torrents) does), which starts the engine.

|            |                                                                                                 |
| ---------- | ----------------------------------------------------------------------------------------------- |
| Arguments  | `{ infoHash: string \| null, magnet: string \| null, download: string \| null, load: boolean }` |
| Returns    | [`TorrentDetailsDto`](../ui/src/lib/bindings/TorrentDetailsDto.ts) or `null`                    |
| Errors     | with `load`: `source-refused`, `metadata-timeout`, `unsafe-torrent`, `engine`                   |
| Permission | `allow-torrent-details`                                                                         |

### `open_link`

Opens an indexer's page (a result's `detailsUrl`) in the system browser. Only http(s) links
with a host name, no user name or password, at most 2 KiB, and not on this computer or the local
network (`localhost`, `.local`, and every address `torrentplayer_net::is_public_ip` rejects:
loopback, private, link-local, `100.64.0.0/10`, IPv4-mapped IPv6, and other reserved ranges) are
opened; the link is handed to the system's own handler as one argument (`rundll32 url.dll` on
Windows, `open` on macOS, `xdg-open` on Linux), never through a shell, and is not logged. The
browser does not use TorrentPlayer's network mode, so in the SOCKS5 and WireGuard modes the
result details say the page opens outside the proxy or tunnel.

|            |                                                   |
| ---------- | ------------------------------------------------- |
| Arguments  | `{ link: string }`                                |
| Returns    | nothing                                           |
| Errors     | `invalid-argument` for any other link, `internal` |
| Permission | `allow-open-link`                                 |

### `preload_status`

The background preload of the last search's best pick, or `null` if there is none. When a
search's best pick has settled (see [`search_torrents_progressive`](#search_torrents_progressive)),
preloading is on, and no video is playing, the backend
fetches the torrent's metadata (librqbit, metadata only) and downloads the start of its best
video file (the largest the built-in player can open, or the first episode of a series pack)
into torrentplayer-swarm: about three minutes' worth (the file size ÷ 30, between 24 and 96 MiB) plus
its last 2 MiB. Limits: one preload at a time (the next search's best pick evicts it, deleting
its cache), a 3-minute time limit, a byte cap of 1.5 × the target + 8 MiB, and nothing is
uploaded. Adding the same magnet link with `add_torrent_link` returns the preloaded torrent
(`claimed: true`), so playing it starts from data already on disk; if its metadata is still
being fetched, the add waits for it instead of fetching it again. An unplayed preload's cache
is also deleted when the engine shuts down, or at the next start if the app exits first. This
command never starts the engine. Poll it (about every 700 ms) while `phase` is `resolving` or
`buffering`.

|            |                                                                                                                                        |
| ---------- | -------------------------------------------------------------------------------------------------------------------------------------- |
| Arguments  | none                                                                                                                                   |
| Returns    | [`PreloadStatusDto`](../ui/src/lib/bindings/PreloadStatusDto.ts) or `null`: `infoHash`, `phase`, `claimed`, `fileIndex`, `progress`, … |
| Errors     | none                                                                                                                                   |
| Permission | `allow-preload-status`                                                                                                                 |

`phase` is `resolving`, `buffering`, `ready` (the target, or as much as arrived before a
limit), `failed` (with `error`), or `cancelled`. Match it to a result with
`SearchResultDto.infoHash`.

### Library

#### `library_items`

Returns the local playback library, most recently opened or watched first. A `LibraryItem` is
identified by `infoHash` plus `fileIndex` and contains a safe replay `source`, plain-text `title`
and `torrentName`, optional `sizeBytes` and `durationSecs`, `positionSecs`, `completed`, and
`updatedAtSecs`. It never contains the loopback stream URL or token.

|            |                                                          |
| ---------- | -------------------------------------------------------- |
| Arguments  | none                                                     |
| Returns    | [`LibraryItem[]`](../ui/src/lib/bindings/LibraryItem.ts) |
| Errors     | `internal` if the local database cannot be read          |
| Permission | `allow-library-items`                                    |

The database is `library.sqlite3` in the per-user app-data directory. SQLite values use
parameterized queries only. Its directory and file are forced to mode `0700` and `0600` on Unix;
on Windows they inherit the user's app-data ACL. Writes run on blocking workers, not the async
runtime.

### Queue

The play queue is a playlist with a cursor, kept by the UI (`ui/src/lib/queue.ts`) and saved
whole after each change. Each item holds only what is needed to play it again: the torrent's
link (`source`: a magnet link, or an http(s) link to a `.torrent` file; torrents opened from a
file are saved as their `TorrentSummary.magnet`, with the info hash, name, and up to 20 of the
file's trackers; a magnet link may list at most 64 trackers), `fileIndex` (`null` plays the
torrent's main video), `title`, `torrentName`, `sizeBytes`, and `infoHash`. The `paused`,
`skipped`, and `played` flags separately record the user's pauses (of download and upload
together), videos the user unticked, and videos that reached their end; only the user sets
`paused` and `skipped`. Stream URLs are never stored: links to this computer (`localhost`,
loopback addresses) are refused.

#### `load_queue`

The saved queue (`queue.json` in the app data folder), or an empty one. Invalid items are
dropped, a `current` that names no item is cleared, and a damaged file is set aside as
`queue.invalid.json`.

|            |                                                                          |
| ---------- | ------------------------------------------------------------------------ |
| Arguments  | none                                                                     |
| Returns    | [`Queue`](../ui/src/lib/bindings/Queue.ts): `items`, `current` (item id) |
| Errors     | none                                                                     |
| Permission | `allow-load-queue`                                                       |

#### `save_queue`

Validates and saves the whole queue atomically (temporary file, then rename), versioned
(`"version": 2`). At most 500 items; ids are 1–64 letters, digits, `-` or `_`, unique;
titles 1–1024 characters without control characters; `infoHash` 40 hex digits.

|            |                                                                                          |
| ---------- | ---------------------------------------------------------------------------------------- |
| Arguments  | `{ queue: Queue }`                                                                       |
| Returns    | nothing                                                                                  |
| Errors     | `invalid-argument` (message starts with the field, e.g. `queue.items[2]: …`), `internal` |
| Permission | `allow-save-queue`                                                                       |

#### `preload_next`

Prepares the queue's next video on behalf of the open stream `streamId`. The UI asks only once
the playing file cannot be slowed down: it is fully downloaded, or less than two minutes of
playback are left and the rest of the file arrives in under half that time
(`shouldPreloadNext` in `ui/src/lib/upnext.ts`).

- **A torrent not added yet** goes through the same one-slot preload as a search's best pick
  (see [`preload_status`](#preload_status)): the start and end of `fileIndex` if that is a
  video of the torrent, otherwise of the file a preload would choose. A link to a `.torrent`
  file is fetched first; one that redirects to a magnet link preloads that. While a video
  plays, this preload wins over a search's best pick: a search neither replaces nor evicts it.
- **A torrent already added** (`torrentId`, usually the course playing, after "Queue all") is
  prepared inside it: torrentplayer-swarm downloads the file's start and end at background priority,
  after the playhead's read-ahead; librqbit selects the whole file and, for the torrent
  playing, prioritizes its start and end in the last minute of playback.

|            |                                                                                                                                          |
| ---------- | ---------------------------------------------------------------------------------------------------------------------------------------- |
| Arguments  | `{ streamId: number, next: NextVideo }`: [`NextVideo`](../ui/src/lib/bindings/NextVideo.ts) `link`, `fileIndex`, `infoHash`, `torrentId` |
| Returns    | nothing, once started                                                                                                                    |
| Errors     | `source-refused` (bad link), `not-found` (stream or torrent), `engine`                                                                   |
| Permission | `allow-preload-next`                                                                                                                     |

#### `queue_readiness`

How ready queued videos are to play, for the queue's "Ready" labels. Each answer is `complete` (downloaded), `ready` (its start
and end are present, as much as a preload leaves, so it starts at once), or `waiting`. Only
items in torrents that are already added (a preload's included) and that name a 40-hex info
hash are answered; the others, and everything before the engine has started, are left out. It
reads which pieces are present and nothing else: it never starts the engine, adds a torrent, or
downloads anything. Each torrent's pieces are read once per call. The UI asks every 4 s while
the queue is on screen.

|            |                                                                                                                                                                              |
| ---------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Arguments  | `{ items: ReadinessQuery[] }` (at most 500): [`ReadinessQuery`](../ui/src/lib/bindings/ReadinessQuery.ts) `id`, `infoHash`, `fileIndex`                                      |
| Returns    | [`ItemReadiness`](../ui/src/lib/bindings/ItemReadiness.ts)`[]`: `id`, `state` ([`ReadinessState`](../ui/src/lib/bindings/ReadinessState.ts)), `downloaded`, `length` (bytes) |
| Errors     | `invalid-argument` (too many items)                                                                                                                                          |
| Permission | `allow-queue-readiness`                                                                                                                                                      |

#### `queue_downloads`

What each queued item is downloading in the background and how the connection is shared with
the video being watched (see "Background downloads" in [architecture](architecture.md)). Each
item reports a `state` (`queued`, `starting`, `connecting`, `downloading`, `paused`,
`complete`, `watching`, `failed`), its torrent's rate and peers, its file's downloaded bytes
and length, the time left at the current rate, whether it can start at once (`startable`),
whether its rate is held back for playback (`limited`), whether its torrent shares with other
peers now (`uploading`), and why a queued item waits (`waitReason`: `playback`, shown as
"Waiting for playback", or `limit`). `paused` is only ever the user's pause. `link` gives the estimated capacity, the total rate,
playback's need, the pressure (`relaxed`, `guarded`, `starving`), the user's maximum of
downloads at once (`maxActive`, `null` for none), and how many items download. The downloader updates it about once a second; the call only reads
that, so it never starts the engine. Pause an item with `save_queue` (`QueueItem.paused`);
the queue downloads in its own order. The UI asks every second while the queue is on screen.

|            |                                                                                                                                                                                                     |
| ---------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Arguments  | none                                                                                                                                                                                                |
| Returns    | [`DownloadsSnapshot`](../ui/src/lib/bindings/DownloadsSnapshot.ts): `items` ([`ItemDownload`](../ui/src/lib/bindings/ItemDownload.ts)`[]`), `link` ([`LinkUse`](../ui/src/lib/bindings/LinkUse.ts)) |
| Errors     | none                                                                                                                                                                                                |
| Permission | `allow-queue-downloads`                                                                                                                                                                             |

#### `preview_file`

A preview of one file of an added torrent, for the file explorer. One runs at a time: a newer
call stops the previous one's wait (its answer is empty). Never starts the engine.

- **Pictures** (`.jpg`, `.jpeg`, `.png`, `.webp`, 2 KiB–2 MiB, the thumbnail limits): the file
  is downloaded at background priority (`StreamEngine::read_small_file`), checked by its first
  bytes (JPEG, PNG, or WebP, whatever its name says), and returned as a `data:` URL in
  `image`.
- **Text and subtitles** (`.txt`, `.nfo`, `.md`, `.srt`, `.vtt`, `.ass`, `.ssa`, `.sub`,
  `.log`, at most 512 KiB): downloaded the same way and returned as plain `text`, UTF-8 with
  bad bytes replaced and control characters removed, cut at 64 KiB (`truncated`). Render it
  as text, never as HTML.
- **Videos**: a stream URL in `video` for a still frame, only when the file's start and end
  are already downloaded (so showing it downloads nothing new); otherwise a `note`. The URL
  carries a session token: never log it. The previous preview's URL is withdrawn, also when a
  file of another kind is previewed. While it is published, its torrent is kept from cache
  clean-up.
- **Anything else** (PDFs, archives, HTML, programs): metadata only. Nothing is rendered,
  executed, or opened.

A file that does not arrive within 20 s gives a `note` ("Still downloading…").

|            |                                                                                                                     |
| ---------- | ------------------------------------------------------------------------------------------------------------------- |
| Arguments  | `{ torrentId: number, fileIndex: number }`                                                                          |
| Returns    | [`FilePreviewDto`](../ui/src/lib/bindings/FilePreviewDto.ts): `kind`, `image`, `text`, `truncated`, `video`, `note` |
| Errors     | `not-found` (torrent or file, or the engine has not started)                                                        |
| Permission | `allow-preview-file`                                                                                                |

#### `stop_preview`

Called when the file explorer closes: stops a preview still waiting and withdraws the preview
video's stream, so its torrent is no longer kept from cache clean-up.

|            |                      |
| ---------- | -------------------- |
| Arguments  | none                 |
| Returns    | nothing              |
| Errors     | none                 |
| Permission | `allow-stop-preview` |

### FFmpeg (optional)

`ffprobe` and `ffmpeg` are used when the user has them installed (in `advanced.ffmpegDir`, or on
`PATH`). They read a stream through the loopback server under a token of its own
(`StreamServer::publish_private`), withdrawn as soon as the call ends, from a source that serves
only what is already downloaded (`StreamEngine::stored_media`): they download nothing, change no
priorities, and fail at once on a missing part. Each run is locked down to that URL
(`-protocol_whitelist http,tcp` and a `-format_whitelist` of media containers), has no console
window or standard input, a time limit, and bounded output. See `torrentplayer_media::FfmpegTools`.

`start_stream` starts a duration probe: the container header's duration (MP4 `mvhd`, Matroska
`Info`), or, if it has none, `ffprobe` (up to six tries, 5 s apart, while the header downloads).
`report_playback` uses it when the player reports no duration.

#### `seek_thumbnail`

A frame of the playing video for the seek bar: the key frame at or before `positionSecs`, 320
pixels wide, as a `data:image/jpeg;base64,` URL. `null` when there is none to show: the
`playback.seekThumbnails` setting is off, `ffmpeg` is not installed, the piece at that position
(estimated from `durationSecs`) is not downloaded, `ffmpeg` failed or took more than 8 s, or a
newer call replaced this one (one runs at a time). Only a whole JPEG is returned.

|            |                                                                                               |
| ---------- | --------------------------------------------------------------------------------------------- |
| Arguments  | `{ streamId: number, positionSecs: number, durationSecs: number }`                            |
| Returns    | `string` or `null`                                                                            |
| Errors     | `invalid-argument` (times not finite, negative, or over 1 000 000 s; duration 0), `not-found` |
| Permission | `allow-seek-thumbnail`                                                                        |

#### `media_tools`

Looks for `ffprobe` and `ffmpeg` again (in `advanced.ffmpegDir` or its `bin` folder, otherwise on
`PATH`; relative `PATH` entries are skipped) and reports each one's path and the version it
prints, for the Settings page.

|            |                                                                                                                        |
| ---------- | ---------------------------------------------------------------------------------------------------------------------- |
| Arguments  | none                                                                                                                   |
| Returns    | [`MediaToolsDto`](../ui/src/lib/bindings/MediaToolsDto.ts): `ffprobe` and `ffmpeg`, each `{ path, version }` or `null` |
| Errors     | none                                                                                                                   |
| Permission | `allow-media-tools`                                                                                                    |

#### `choose_ffmpeg_dir`

Opens the system folder picker for the FFmpeg folder and saves the choice as
`advanced.ffmpegDir` (validated like any change, and refused inside a download folder). Returns the
saved `SettingsView`, or `null` if the user cancelled. This is the only way to set the folder:
`update_settings` refuses a new value, because the programs in it run.

|            |                                                                                      |
| ---------- | ------------------------------------------------------------------------------------ |
| Arguments  | none                                                                                 |
| Returns    | `SettingsView` or `null`                                                             |
| Errors     | `invalid-argument` (a network folder, or one inside the download folder), `internal` |
| Permission | `allow-choose-ffmpeg-dir`                                                            |

### Settings

Settings and secrets are documented in [Configuration](configuration.md). The commands below
run their file and credential-store work off the async runtime.

#### `get_settings`

Returns the saved settings plus what the Settings page needs to explain them: `warnings` (load
problems and incomplete choices such as a proxy without a host), `restartRequired` (dotted paths
of changes that apply after a restart), `defaultDownloadDir`, `activeDownloadDir`,
`engineOverride` (the `TORRENTPLAYER_ENGINE` value, if set), and `logLevelOverride` (`RUST_LOG` is set).

|            |                                                          |
| ---------- | -------------------------------------------------------- |
| Arguments  | none                                                     |
| Returns    | [`SettingsView`](../ui/src/lib/bindings/SettingsView.ts) |
| Errors     | none                                                     |
| Permission | `allow-get-settings`                                     |

#### `update_settings`

Applies a partial update with the same shape as
[`Settings`](../ui/src/lib/bindings/Settings.ts), containing only the changed fields, for example
`{ "bandwidth": { "peerLimit": 80 } }`; `null` clears an optional setting. The patch must be an
object of at most 64 KiB, may not contain `version`, and may name only existing settings. The
merged result is fully validated, and nothing is saved unless all of it is valid. A new
`storage.downloadDir` or `advanced.ffmpegDir` is refused (`invalid-argument`): only
`choose_download_dir` and `choose_ffmpeg_dir` set them; `null` (the default) is accepted. On success the file is saved
atomically, the log level is applied at once, and `settings-changed` is emitted.

|            |                                                                                                                   |
| ---------- | ----------------------------------------------------------------------------------------------------------------- |
| Arguments  | `{ patch: object }`                                                                                               |
| Returns    | [`SettingsView`](../ui/src/lib/bindings/SettingsView.ts)                                                          |
| Errors     | `invalid-argument` (the message starts with the setting's dotted path, e.g. `bandwidth.peerLimit: …`), `internal` |
| Permission | `allow-update-settings`                                                                                           |

#### `set_secret`

Stores a secret in the OS credential store. SOCKS5 passwords are 1–255 bytes without NUL
characters and are stored exactly as given; WireGuard keys are trimmed and must be 44 base64
characters ending in `=`. The argument string is wiped from memory after use, and messages never
contain the value. Emits `settings-changed`.

|            |                                                                                                                    |
| ---------- | ------------------------------------------------------------------------------------------------------------------ |
| Arguments  | `{ key: SecretKey, value: string }`, `key` one of `socks5Password`, `wireGuardPrivateKey`, `wireGuardPresharedKey` |
| Returns    | [`SecretStatus`](../ui/src/lib/bindings/SecretStatus.ts)                                                           |
| Errors     | `invalid-argument`; `internal` if the credential store is unavailable or refuses                                   |
| Permission | `allow-set-secret`                                                                                                 |

#### `clear_secret`

Removes a secret (succeeds if none was set). Emits `settings-changed`.

|            |                                                          |
| ---------- | -------------------------------------------------------- |
| Arguments  | `{ key: SecretKey }`                                     |
| Returns    | [`SecretStatus`](../ui/src/lib/bindings/SecretStatus.ts) |
| Errors     | `internal` if the credential store refuses               |
| Permission | `allow-clear-secret`                                     |

#### `secret_status`

Which secrets are set, and whether a credential store is available. There is no command that
returns a secret's value.

|            |                                                          |
| ---------- | -------------------------------------------------------- |
| Arguments  | none                                                     |
| Returns    | [`SecretStatus`](../ui/src/lib/bindings/SecretStatus.ts) |
| Errors     | none                                                     |
| Permission | `allow-secret-status`                                    |

#### `cache_usage`

Size, configured limit, file count, and safely reclaimable finished data in TorrentPlayer's cache
folders (`torrentplayer-swarm/`, `librqbit/`) in the download folder in use. Symbolic links are not
followed; counting stops after 200 000 entries (`complete: false`).

|            |                                                      |
| ---------- | ---------------------------------------------------- |
| Arguments  | none                                                 |
| Returns    | [`CacheUsage`](../ui/src/lib/bindings/CacheUsage.ts) |
| Errors     | none                                                 |
| Permission | `allow-cache-usage`                                  |

#### `clear_cache`

Deletes finished downloads that are no longer queued and have no open player stream. Only validated
info-hash directories under `torrentplayer-swarm/` and `librqbit/` are removed; symbolic links, legacy
layouts, and unrelated files are left alone. The result reports bytes and downloads removed. The
frontend asks for confirmation first.

|            |                                                                    |
| ---------- | ------------------------------------------------------------------ |
| Arguments  | none                                                               |
| Returns    | [`ClearCacheOutcome`](../ui/src/lib/bindings/ClearCacheOutcome.ts) |
| Errors     | `internal`                                                         |
| Permission | `allow-clear-cache`                                                |

#### `choose_download_dir`

Opens the system folder picker (modal to the window), starting at the saved download folder if it
exists, and saves the chosen folder as `storage.downloadDir` (validated, and checked to be
writable). Returns the saved `SettingsView`, or `null` if the user cancelled. This is the only way
to set the folder: `update_settings` refuses a new value (`null`, the default folder, is allowed). The webview gets no general file-dialog or
file-system permission.

|            |                                                                                  |
| ---------- | -------------------------------------------------------------------------------- |
| Arguments  | none                                                                             |
| Returns    | `SettingsView` or `null`                                                         |
| Errors     | `invalid-argument` (a network folder, or one that cannot be written), `internal` |
| Permission | `allow-choose-download-dir`                                                      |

### Network

Direct, SOCKS5, the kill switch, and the built-in WireGuard tunnel; see
[Network modes and kill switch](network.md). **Network settings are applied by saving them:**
the backend follows every `update_settings`, `set_secret`, and `clear_secret` (there is no apply
command). A Settings page saves the network section, then shows `network-status` events or
calls `network_status`. A protected mode with incomplete settings (no host, no WireGuard
configuration, a username without a stored password) is reported as state `error` with a
reason, and blocks traffic. So is Direct mode while `network.hold` is set
(settings migrated from the removed VPN adapter mode, or settings that could not be read):
traffic is held until `update_settings` clears it with `{"network": {"hold": null}}` (see
[Network](network.md#using-a-vpn-app)).

#### `network_status`

The mode, connection state, endpoint (proxy `host:port` or WireGuard endpoint; never secrets), whether traffic, UDP, and
trackers are allowed, whether the kill switch is engaged, the time of the last check, while connected the time
it connected (`connectedSinceMs`, for the uptime), the last 50 events (oldest first), and
`incoming`: whether incoming peer connections are accepted (`kind` `off`, `idle`, `paused`,
`unsupported`, `failed` with a `reason`, or `listening` with `port`, `addresses`, and `upnp`;
see [incoming peer connections](network.md#incoming-peer-connections)). `state.kind` is one of `direct`, `connecting`, `connected`, `reconnecting`,
`killSwitchActive`, `error`, `disconnected`; `reconnecting`, `killSwitchActive`, and `error`
carry a `reason`.

|            |                                                            |
| ---------- | ---------------------------------------------------------- |
| Arguments  | none                                                       |
| Returns    | [`NetworkStatus`](../ui/src/lib/bindings/NetworkStatus.ts) |
| Errors     | none                                                       |
| Permission | `allow-network-status`                                     |

#### `recheck_network`

Checks the proxy or WireGuard tunnel now instead of at the next 5-second interval (a "Try again" button
while the kill switch is engaged), waits up to 6 s for the result, and returns the status.

|            |                                                            |
| ---------- | ---------------------------------------------------------- |
| Arguments  | none                                                       |
| Returns    | [`NetworkStatus`](../ui/src/lib/bindings/NetworkStatus.ts) |
| Errors     | none                                                       |
| Permission | `allow-recheck-network`                                    |

#### `validate_wireguard_config`

Parses the text of a `wg-quick` `.conf` file (at most 64 KiB) and returns what it contains
without its keys: addresses, DNS, MTU, listen port, and per peer the public key, endpoint,
allowed ranges, keep-alive, and `hasPresharedKey`; plus `routesAllTraffic` and `warnings` for
ignored lines (script hooks are never run). Nothing is saved, and the text is wiped from memory
after parsing. The Settings page shows this before saving with
[`import_wireguard_config`](#import_wireguard_config).

|            |                                                                  |
| ---------- | ---------------------------------------------------------------- |
| Arguments  | `{ text: string }`                                               |
| Returns    | [`WireGuardSummary`](../ui/src/lib/bindings/WireGuardSummary.ts) |
| Errors     | `invalid-argument`, naming the line; never contains key material |
| Permission | `allow-validate-wireguard-config`                                |

#### `import_wireguard_config`

Imports a `wg-quick` file in one step, so its keys never come back to the webview: parses and
checks it as `validate_wireguard_config` does, refuses files with more than one `[Peer]`
(`network.wireguard` holds one), stores the private key under `wireGuardPrivateKey` and the
preshared key, if any, under `wireGuardPresharedKey` (removing an old one otherwise), and saves
the non-secret parts (addresses, DNS, MTU, the peer's public key, endpoint, allowed ranges, and
keep-alive, plus `name`) in `network.wireguard`. The merged settings are validated before any key
is stored, so an invalid file saves nothing. The text is wiped from memory after parsing. Emits
`settings-changed` once. The built-in tunnel does not exist yet, so importing does not change the
mode.

|            |                                                                                                                                          |
| ---------- | ---------------------------------------------------------------------------------------------------------------------------------------- |
| Arguments  | `{ text: string, name: string \| null }` (`name` labels it, for example the file name; trimmed)                                          |
| Returns    | [`WireGuardImport`](../ui/src/lib/bindings/WireGuardImport.ts): `summary` (no keys), `view` (`SettingsView`), `secrets` (`SecretStatus`) |
| Errors     | `invalid-argument` (the file, or `network.wireguard: …`); `internal` if the credential store is unavailable or refuses                   |
| Permission | `allow-import-wireguard-config`                                                                                                          |

#### `check_connection`

Asks icanhazip.com (`ipv4.` and `ipv6.icanhazip.com`) for the public IPv4 and IPv6 addresses,
through the current mode, and returns them with the mode used (`ip` is the IPv4 address, or
the IPv6 one without IPv4). While the kill switch is engaged it fails with `kill-switch`
without sending anything. With `network.showPublicIp` on, the answer also updates the status
bar.

|            |                                                  |
| ---------- | ------------------------------------------------ |
| Arguments  | none                                             |
| Returns    | [`PublicIp`](../ui/src/lib/bindings/PublicIp.ts) |
| Errors     | `kill-switch`, `network-unavailable`             |
| Permission | `allow-check-connection`                         |

#### `public_ip`

The status bar's latest public addresses, kept current by the app (see the public-IP check in
[network](network.md)): `null` while unknown, while traffic is blocked, or when
`network.showPublicIp` is off. Updates arrive as `network-public-ip` events. Sends nothing.

|            |                                                            |
| ---------- | ---------------------------------------------------------- |
| Arguments  | none                                                       |
| Returns    | [`PublicIp`](../ui/src/lib/bindings/PublicIp.ts) or `null` |
| Errors     | none                                                       |
| Permission | `allow-public-ip`                                          |

## Events

| Event               | Payload                                                    | When                                                               |
| ------------------- | ---------------------------------------------------------- | ------------------------------------------------------------------ |
| `settings-changed`  | [`Settings`](../ui/src/lib/bindings/Settings.ts)           | After `update_settings`, `set_secret`, or `clear_secret` succeeds. |
| `library-changed`   | [`LibraryItem`](../ui/src/lib/bindings/LibraryItem.ts)     | After an item is opened or a playback checkpoint is saved.         |
| `network-status`    | [`NetworkStatus`](../ui/src/lib/bindings/NetworkStatus.ts) | At start-up and whenever the network state changes.                |
| `network-public-ip` | [`PublicIp`](../ui/src/lib/bindings/PublicIp.ts) or `null` | When the status bar's public addresses change (see `public_ip`).   |
| `seeding-started`   | none                                                       | When a playback ends and its torrent keeps sharing (once each).    |

The main window may listen to events (`core:event:allow-listen` and `core:event:allow-unlisten`
in `src-tauri/capabilities/main.json`; it cannot emit them). The frontend follows
`network-status` with `followNetwork` in `ui/src/lib/network.ts`, which calls `network_status` once
and then applies each event. `followLibrary` registers its listener before reading
`library_items`, then overlays any changes that race the initial read. Rust subsystems get the
same settings and network notifications from `SettingsState::subscribe` (a `watch` channel) and
`Network::subscribe`. Stream statistics and download progress are polled about once a second;
watch progress is event-driven.
