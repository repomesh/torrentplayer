# 0006: A streaming-first BitTorrent engine (`torrentplayer-swarm`)

- **Status:** Proposed (milestone M1 implemented; librqbit remains the default until the
  benchmarks justify switching)
- **Date:** 2026-10-17

> **Update (2026-10-26, 0.3.1):** torrentplayer-swarm has been the automatic engine for every
> source since 0.2.0; librqbit is an explicit choice (`advanced.engine = "rqbit"`) and fetches
> magnet metadata for torrentplayer-swarm, which does not implement BEP 9 itself. Since built:
> web seeds (BEP 19), DHT (through the vendored `librqbit-dht`, read-only per BEP 43 unless the
> listening port is reachable), peer exchange, uploading (off for each torrent until the app
> turns it on), incoming peers, rate limits, `torrentplayer-net` socket hooks, smart ban, a
> persistent piece cache, and one 256 MiB partial-piece budget shared by all torrents. Not
> built: protocol encryption and uTP. See [Streaming](../streaming.md) and
> [Network](../network.md).

## Context

Phase 1 wrapped librqbit 9. Measured on public swarms, start-up and seek latency missed the
targets (start-up under 3 s, seeks under 1.5 s), and the causes are inside librqbit:

- It keeps a fixed 2 MiB of requests in flight per peer (`DEFAULT_PEER_REQUEST_WINDOW` = 128 ×
  16 KiB) and cannot cancel them, so after a seek new pieces wait behind old ones. With 50 peers
  sharing 10 MiB/s that wait can reach about 10 s.
- It does not support HTTP web seeds (BEP 17/19), which Internet Archive and Academic Torrents
  course torrents depend on.
- Its only prioritization lever is a fixed 32 MiB lookahead per open stream.
- It does not implement protocol encryption, which TorrentPlayer's network requirements need.

Alternatives considered were libtorrent-rasterbar (C++, has deadlines and web seeds, but adds a
C++/Boost build, an FFI crate with `unsafe`, and immature Rust bindings) and the other Rust
engines (`irontide` is GPL-3.0; `fx-torrent` is about ten months old with little adoption).

## Decision

Build TorrentPlayer's own engine, `torrentplayer-swarm`, owning the parts that decide streaming performance,
and reuse proven plumbing where it does not:

- **Own:** the request scheduler (`sched.rs`, pure and unit-tested), the peer wire codec
  (`wire.rs`, a strict parser at the untrusted-input boundary), peer connection management,
  piece verification, storage, and readers.
- **Reuse:** librqbit's standalone Apache-2.0 crates for `.torrent` parsing (`librqbit-core`)
  and HTTP/UDP trackers (`librqbit-tracker-comms`); later DHT (`librqbit-dht`) and uTP
  (`librqbit-utp`).

Scheduling policy:

1. **Deadlines.** Readers and the playhead register interests; every missing piece in an
   interest's window gets an urgency (when the player will need it at the media byte rate).
   Urgent pieces are requested first, in urgency order.
2. **Adaptive per-peer queues** of about 0.5 s at each peer's measured rate (4 to 128 blocks),
   instead of a fixed count, and at least two round trips (estimated from every block, corrected
   for the blocks queued ahead of it). Unless data is due within 4 s, queues shrink to 0.15 s so
   the next seek waits behind little data. Requests are handed out in turns of 256 KiB per
   peer, so the most urgent data is spread over all peers.
3. **Cancel on seek.** Requests no longer urgent are cancelled (`cancel` messages) so queues
   free up for the new position.
4. **Duplicate urgent blocks.** Blocks due within 1 s and outstanding for 300 ms are also
   requested from a second peer; the slower copy is cancelled.
5. **Critical phase.** When a reader starts, seeks, or has to wait for data, only the critical
   bytes ahead of it (the larger of 2 MiB and 1.5 s of media) are requested, until half of them
   are downloaded. Half, not all: a player that keeps reading moves the window, so its far end is
   always in flight.
6. **Background fill** of selected files, rarest piece first, finishing started pieces first.

Storage is an **append-only piece store** (one cache file per torrent plus an index), not the
final file layout. Writing far past the end of an ordinary file makes NTFS zero-fill the gap:
measured at about 0.9 s for a 6 GB offset on this project's development machine (SSD).
Appending is always sequential, needs no sparse-file APIs (which would require `unsafe` Win32
calls), and makes the cache-size limit and cleanup straightforward. Exporting complete files to
the normal layout can be added later.

Since 0.3.1 the index is also appended to disk (`pieces.idx`: a header with the format version and
the info hash, then one checksummed record per stored piece), so the cache survives restarts.
Pieces are not flushed as they are stored. After a crash every piece found through the index is
checked against the torrent's SHA-1 again before use, which also covers torn records and data
lost in the crash. A clean quit flushes both files and appends a marker with the cache file's
length and modification time; while it holds, the next session counts the pieces as stored at
once and checks each one when it is first read, before any byte of it is used (see
[Streaming](../streaming.md#downloaded-once)).

## Milestones

| Milestone | Scope                                                                                   | Gate                                                                    |
| --------- | --------------------------------------------------------------------------------------- | ----------------------------------------------------------------------- |
| M1        | Download-only engine: trackers, peers, scheduler, cancel, duplicates, storage, readers  | Beats librqbit on `torrentplayer-bench` (local seeder and public swarm) |
| M2        | Web seeds (BEP 19)                                                                      | MIT course torrents start in under 3 s                                  |
| M3        | Magnet links, DHT, peer exchange, uTP; preload and warm swarm                           | Cold and warm start-up within target                                    |
| M4        | Seeding and choking, rate limits, protocol encryption, `torrentplayer-net` socket hooks | Network and kill-switch tests pass                                      |
| M5        | Fuzzing, interoperability with qBittorrent and Transmission, soak tests                 | Then make it the default and retire librqbit                            |

## Consequences

- TorrentPlayer owns a protocol implementation, so it must be fuzzed and tested against other clients
  before it becomes the default. librqbit stays available behind the same benchmark interface
  until then.
- M1 does not upload, accept incoming connections, resolve magnet links, or use the DHT; those
  arrive in M3 and M4.
- Interoperability notes found so far: librqbit sends an extended handshake before its bitfield
  even when the extension bit is not advertised (so the bitfield is accepted any time before the
  first `have`), and librqbit 9's `create_torrent` writes one extra piece hash when the length is
  an exact multiple of the piece length (extra trailing hashes are ignored).
