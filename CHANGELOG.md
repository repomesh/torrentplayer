# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this
project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Changed

- Less memory with many torrents: partly downloaded pieces now share one 256 MiB budget across the
  whole engine instead of up to 256 MiB per torrent. The video being played keeps a quarter of it to
  itself, so background torrents cannot crowd out its downloads, and paused or stalled torrents give
  their share back. Huge torrents also need less memory per peer: a 100,000-piece torrent with 50
  peers keeps 625 KB of piece maps instead of 5 MB.

### Fixed

- Audio conversion retries temporary startup and stream-read failures twice before falling back
  to silent playback. Seeking again cancels a pending retry. When large videos fill the media
  buffer after backward seeks, distant future segments are evicted before retrying the append.

## [0.3.1] - 2026-10-23

### Added

- The player has visible Rewind 10 seconds and Forward 10 seconds buttons, also available
  in fullscreen. They preserve pause state and stay within the media's duration.
- `cargo bench-suite <list file>` benchmarks your own media files, torrents, and magnet links
  and writes `report.md` and `results.json` (see "Benchmark your own files" in
  `docs/performance.md`). `torrentplayer-bench local --media FILE` (`cargo bench-file`) seeds a
  real file on a local swarm and requests what a player needs from its container (MP4 `moov`
  wherever it is, Matroska header and `Cues`, the keyframe before each seek) instead of a flat
  amount, using the container's duration. Public swarms run only with `--public`, from peers
  only (`--web-seeds` adds a swarm-engine row with the torrent's web seeds). The report starts
  with a guide to reading it and gives each entry a verdict against the start-up and seek
  targets. `--wireguard FILE` and `--socks5 HOST:PORT` run the public entries through the app's
  protected network modes, over one tunnel or proxy connection for the whole run. A
  ready-made list of openly licensed films and Linux images is in
  `crates/torrentplayer-bench/lists/benchmarks.txt`, with `prepare-media.sh` for its local files.
- The benchmark suite's `report.md` opens with a summary table of the whole run (every entry,
  engine, and scenario, with the targets met marked), also printed at the end and saved as
  `summary` in `results.json`. `torrentplayer-bench render <results.json>` rewrites a saved
  run's report in the current layout.

### Changed

- The queue is now manual. Nothing in it is paused, removed, or reordered for you: playing a
  video further down no longer marks the videos in between as skipped (also when the app
  starts). Only merging the same file of one torrent queued twice still happens on its own.
- Skipping is gone. Unticking a video in the queue removes it, with Undo; a file of a torrent
  with several videos stays listed, so ticking it adds it again. Pause keeps a video without
  downloading or uploading it. Queues saved with skipped videos load them as paused.
- The queue downloads in its own order. The "In order" / "Fastest first" choice, "Play ready
  videos first" (`playback.optimizeQueue`) with its "Keep each torrent's order" option
  (`playback.keepCourseOrder`), and the "Fastest" badge are removed; queues and settings saved
  by older versions load as before and drop these on the next save. Instead, **Sort by…** in
  the queue's ⋯ menu reorders the queue once, with Undo, by name, recently added, size,
  progress, download speed, peers, or time left. Each torrent's files stay together, and
  nothing re-sorts later.
- When a video ends it is marked played and stays on its end. The "Up next" countdown and the
  "Play the next item in the queue automatically" setting (`playback.autoplayNext`) are gone;
  pick the next video yourself, or press Shift+N.
- Queued torrents keep running and uploading, finished or not, watched or not, until you pause
  or remove them. They are no longer paused when nothing is left to download, and seeding no
  longer stops on its own after playback, so the "Keep sharing after watching", "Stop seeding
  after", and "Stop seeding at ratio" settings (`bandwidth.seedAfterWatching`,
  `bandwidth.seedRatioTarget`, `bandwidth.seedTimeLimitMinutes`) are removed. Older settings
  files load as before and drop them on the next save.
- Pausing a torrent, or the whole queue, stops both downloading and uploading until you resume
  it; pausing one video of a torrent stops its download, while the torrent keeps sharing for its
  other queued videos. Pausing now applies to finished videos too, and the VPN note appears the
  first time a torrent uploads instead of after playback. The only automatic hold is
  the one that protects the video playing: background downloads that wait for it now say
  "Waiting for playback", and it never changes your own pauses. A finished video that is
  sharing reads "Downloaded · seeding".
- Downloaded data is kept across restarts: torrents in the queue, finished ones included, no longer
  download everything again each time TorrentPlayer starts. After a normal quit the pieces are
  available at once and each is checked against the torrent's hashes when first played or shared;
  after a crash everything is checked in the background first, on at most two threads, the video
  being played first. Caches from earlier versions can't be reused and start empty once.
- Faster peer discovery: DHT lookups send each query once instead of twice (a bug in the vendored
  DHT), and the first lookup no longer stalls on a fresh node. DHT-only torrents found their first
  peer in about 1 s instead of 2–12 s (up to 40 s).
- DHT lookups follow what each torrent needs: every 20 s while it downloads with few peers, every
  5 minutes once it has enough, and none while it is paused or has nothing to download. A lookup
  that finds nobody is retried within seconds. Idle torrents no longer send about 19 DHT packets a
  second each.
- With incoming peers on, TorrentPlayer announces its listening port in the DHT (BEP 5) while it is
  listening in the current mode, never in SOCKS5 mode; otherwise its DHT queries are marked
  read-only (BEP 43).
- A paused torrent no longer announces itself to its trackers; resuming announces it again at once.
- Lower CPU and disk use: paused and finished torrents re-plan once a second instead of ten times;
  playback history is saved every 30 s instead of every 5 s (seeks, stopping and quitting still
  save at once); the DHT node list is saved every 10 minutes and only when it changed; the queue
  panel no longer animates its progress bars (about 1 % of a core for the UI during downloads
  instead of about 9 %) and stops checking downloads while the window is minimized; the VPN tunnel,
  player and status bar wake up less often.
- The queue says when the kill switch blocks downloads, with a button to Network & VPN settings,
  instead of "Finding peers…" forever.
- When a video ends, a card offers Play next (Shift+N) or Replay, also in fullscreen. Nothing plays
  on its own.
- Resuming no longer asks: with "Resume where I left off" on, the video continues and a chip offers
  "Start over".
- Clearer queue pause: its own icon, "Paused by you", P to pause or resume from the keyboard, and the
  queue's buttons reachable with Tab. Removing a torrent's videos and "Clear played" offer Undo
  instead of asking first.
- Toasts stack, stay while you read them, keep errors until closed, and show in fullscreen.
- Buffering says how many peers were found and how fast it downloads, and tells you when no one is
  sharing the file. The player says when it converts audio, and when the conversion failed.
- Plainer wording: "sharing" instead of "seeding", "blocked" for the kill switch, time left instead
  of ETA, and "May not play here (format: …)" instead of pointing to a full player that doesn't
  exist yet. The rail's "Playing" item is now "Queue". Hardware decoding and protocol encryption
  settings are hidden until a player or engine applies them.

### Fixed

- Playing files always have a visible queue entry. The active row offers Pause/Resume, Stop,
  and Stop & remove; removing it saves progress and stops playback, and Undo restores the
  entry without autoplay. File lists also allow queued videos to be removed.
- Quitting no longer blocks the shutdown deadline while flushing piece caches or playback
  history on slow storage. Those writes run on blocking workers, and the app waits up to two
  seconds before exiting; an unfinished cache flush is checked again on the next start.
- The architecture guide now describes automatic resume with a Start over chip and the Queue
  navigation label used in 0.3.1.
- `cargo bench-suite … --download-mibps 0` now runs public entries with no download limit (labelled
  "no cap"); before, 0 fell back to the 10 MiB/s default.
- `cargo bench-suite … --wireguard`/`--socks5` now downloads the `.torrent` links of public
  entries through the tunnel or proxy (waiting for it like the runs do) instead of directly,
  and, in every mode, only from public hosts: a link or redirect to `localhost` or a private,
  loopback, or link-local address is refused, as in the app.
- A benchmark run that fails (no such file, a start-up time-out, an engine error) now stops its
  engine. Before, the engine kept downloading from and uploading to the swarm while later
  suite entries were measured, skewing their results, and on Windows kept the run's temporary
  folder from being deleted. The same holds for `torrentplayer-bench limit` runs.
- The benchmark list's Debian DVD entry is no longer filed under "swarms without web seeds",
  and the suite's error for an unsupported link scheme now mentions http:// links.
- A WireGuard tunnel whose server stops answering is no longer shown as Connected. The tunnel
  now counts as connected only while its server completes handshakes and answers traffic within
  20 seconds; otherwise the kill switch pauses traffic with the reason ("the WireGuard server is
  not answering tunnel traffic" or "has not completed a recent handshake"), and traffic resumes
  by itself when the server answers again. An idle or blocked tunnel is probed with one tiny
  packet inside it (a DNS query, or without a usable DNS server an empty datagram the server
  does not forward), less often the longer it stays idle; a tunnel left unanswered for 6 minutes
  is replaced by a new one. Benchmarks over WireGuard now report such a tunnel as down.
- In the WireGuard and SOCKS5 modes, a magnet link that finds no peers now says what to check in
  that mode (the tunnel's handshake, or the proxy's UDP support) instead of always blaming the
  proxy, and the message for `.torrent` files in these modes no longer has stray spaces.
- A long-running WireGuard tunnel keeps renewing its keys promptly. After 64 key exchanges
  (about two hours of traffic, sooner when handshakes repeat) every later one was refused as a
  flood and stalled for at least 5 seconds, or failed with a server that ignores WireGuard
  cookies, until the app was restarted.
- Search result thumbnails no longer delete a folder named `thumbnails` in the download folder.
  The app kept their temporary downloads there and emptied it when it started fetching them, so
  a folder of yours with that name lost its contents. Thumbnail downloads now live in the app's
  own `torrentplayer-swarm/thumbnails` cache folder, and a `thumbnails` folder in the download
  folder is never touched.
- A malformed `.torrent` file (from a file, a link, or a search result) no longer crashes the
  engine that opens it. Files with a piece length of 0 or above 64 MiB, no data, or file
  lengths too large to add up are now refused as not a valid `.torrent` file, with the reason,
  before either engine reads them.
- Switching between Direct and WireGuard or SOCKS5 now stops librqbit's work in progress, such
  as fetching a magnet link's details from peers, before the new mode takes effect. Before, a
  fetch already running kept connecting to peers directly after a switch to WireGuard or
  SOCKS5, outside the tunnel or proxy and the kill switch, for up to two minutes. The torrent
  being opened fails with a message to try again.
- In the WireGuard and SOCKS5 modes, tracker host names are no longer looked up outside the
  tunnel or proxy. With the librqbit engine, and while fetching a magnet link's details, HTTP
  tracker names were resolved by the system's DNS, even while the kill switch was blocking
  traffic. TorrentPlayer now contacts those trackers itself, with the names resolved by the
  tunnel or proxy, and librqbit contacts none, so turning "Send tracker requests through the
  proxy" off also stops every tracker request of torrents already open.
- Links to `.torrent` files (pasted, or from search results) can no longer reach your local
  network. A link, or a redirect it sends the app to, that leads to a private, loopback, or
  link-local address or to `localhost` (such as your router or a cloud metadata address) is now
  refused, in every network mode, and a redirect from https to http is no longer followed.
- Settings that cannot be read no longer switch a VPN or proxy user to a direct connection. When
  `settings.json` exists but is damaged, unreadable (for example locked by an antivirus or sync
  client, or too large), or names a connection mode this version does not know, TorrentPlayer now
  starts with all traffic paused and a notice ("Your network settings could not be read…") until
  you choose a connection in Settings → Network & VPN or select **Connect directly**. The file
  is kept as `settings.invalid.json` and is never overwritten while it cannot be backed up. One
  bad network value (such as an unknown IPv6 policy) now resets only itself, not the connection
  mode. A first run without a settings file still starts in Direct mode.
- FFmpeg runs for durations and seek-bar thumbnails no longer follow the `http_proxy` environment
  variable, which sent the private stream address (with its access token) to that proxy. They
  now share the hardening of the audio conversion.
- Search no longer labels Dolby (AC-3, E-AC-3, TrueHD) and DTS releases "no sound in built-in
  player" or ranks them down when the bundled FFmpeg converts their audio to stereo; they now
  say "audio: converted to stereo". Builds without FFmpeg keep the warning.
- The download folder and the FFmpeg folder can now be chosen only with **Choose folder…**; the
  fields are no longer typed in, and the app refuses a new folder from anywhere else. Network
  (UNC) and device paths are refused, so checking a folder can no longer make Windows send your
  sign-in credentials to another computer, and an FFmpeg folder inside the download folder is
  refused, so TorrentPlayer never runs a program that came from a torrent.
- The queue no longer holds the same video twice when one entry was queued from a search result
  (its main video, file not chosen yet) and the other names the file, or when two indexers list
  the same torrent with and without its hash. Such entries merge as soon as the torrent's files
  are known, queues saved with duplicates are cleaned up, and the "Queued" mark on search
  results shows them. Queueing other episodes of a season pack still works. (#55)
- A download folder chosen inside the app's own data folder from before the rename (for example
  `%LOCALAPPDATA%\io.torrentstream.desktop\downloads`) is now followed when that folder moves to
  its new name, instead of the settings still pointing at the old path.
- Importing a WireGuard configuration is now all or nothing: if its settings cannot be saved, the
  previous keys are put back instead of new keys being paired with the old configuration, and no
  other settings change can slip in during the import.
- Cache clean-up no longer removes the torrent whose video the file explorer is previewing, and
  closing the explorer now ends its video preview instead of leaving it published.
- Undoing the removal of a queued video puts that video back in its place instead of restoring
  the whole queue as it was, which lost anything played, moved, or queued in the meantime.
- Indexer links from search results no longer open when they point at this computer or a
  private network through an IPv4-mapped IPv6 address, an IPv6 link-local address
  (`fe80::/10`), or the shared `100.64.0.0/10` range; they are checked like peer addresses.
- In the SOCKS5 and WireGuard modes, a search result's "Open the indexer's page" link now says
  that the page opens in your browser, outside TorrentPlayer's proxy or tunnel.
- Documentation caught up with 0.3: the comparison page no longer says the engine does not upload
  or that Dolby and DTS audio is only warned about, and names the LGPL and MPL-2.0 parts that
  ship with the app; the security policy lists 0.3.x as supported; the streaming page is no
  longer marked as planned; the build guide says which Ubuntu versions CI and releases use and
  that libmpv is not needed.
- In WireGuard mode, opening a connection or UDP socket no longer fails now and then with "not a
  suitable bind address" when the randomly chosen tunnel port is already in use; the next free port
  is used.
- In WireGuard mode, a forged WireGuard cookie reply (which anyone who knows the server's public key
  and sees the handshake can make) no longer redirects the tunnel's traffic to its sender; the
  tunnel follows the server only on authenticated handshake and data messages. Handshake initiations
  are rate-limited before the key agreement, so a flood of them costs little CPU.
- A WireGuard tunnel keeps sending its keepalives and key renewals while a lot of traffic (or junk)
  arrives; before, a steady stream of incoming datagrams held them back, which could expire the
  session and block traffic as "has not completed a recent handshake". Packets waiting inside the
  tunnel are also capped instead of piling up in memory.
- In WireGuard mode, connections inside the tunnel now close when the kill switch engages or the
  mode changes, even while the app still holds them and the server is silent; before, each one kept
  about 80 KiB and a task until the tunnel closed. Tunnel connections whose peer stops responding
  are dropped after 2 minutes, and incoming-connection listeners reserve far less memory.
- A connection to a host name started in Direct mode can no longer go out directly after you switch
  to a protected mode (or a hold applies) while its name is being resolved; the route is re-checked
  before each address is dialled.
- In SOCKS5 mode, one failed proxy check no longer closes every connection: a connected proxy must
  fail two checks in a row (about a second apart) before the kill switch engages, and failed
  connections to the proxy trigger an extra check at most once per check interval. Providers with
  connection caps no longer cause a drop-and-reconnect loop; a proxy that is really down still
  blocks traffic within seconds.
- A WireGuard endpoint given as a host name now works when its first address does not: IPv4
  addresses are tried first, a tunnel that completes no handshake for 15 seconds moves on to the
  next address, and the name is resolved again whenever the tunnel is restarted (after 6 minutes
  without a session or an answer), so a server that moved is found again. Before, only the first
  address was used, and an IPv6 address listed first never connected on an IPv4-only network.
- WireGuard configurations whose DNS line includes a search domain (for example
  `DNS = 10.64.0.1, corp.example`) can be imported and connect; the search domain is ignored, and
  the import says so. The import also says when `ListenPort` is ignored (TorrentPlayer uses a
  random local port) and when `PersistentKeepalive = off` is (the tunnel keeps a 25-second
  keepalive).
- Adding a torrent again while the player was still finishing a request on its previous
  instance no longer empties the engine's piece cache under that request, which could hand the
  player zeros or the bytes of other pieces. Reads from the cache also stay inside the piece
  they ask for.
- A peer that sends bad data can no longer stall playback in TorrentPlayer's engine. Data a peer
  was not asked for is dropped, so it cannot spoil pieces other peers are fetching. A piece that
  fails its hash check is fetched again from one peer, and once it verifies, every peer that had
  sent different data for it is banned ("smart ban") — and only those: before, a bad peer and an
  honest one that both sent blocks of failed pieces tied, nobody was banned, and in a thin swarm
  the pieces could fail again and again.
- Peers banned for bad data are now refused when they connect to the listening port again, from
  any port, and are no longer dialed at another port of the same address.
- Another program on the computer can no longer lock the player out of the loopback stream
  server by opening connections and sending nothing: connections that have not presented the
  stream token are kept apart, at most 8 at a time (the oldest is closed for a new one), and are
  closed after 10 seconds or once refused. A player that stopped reading a response no longer
  holds its connection forever: after 30 seconds without progress it is closed when another
  request needs the place. When all 16 places are busy, a request is now answered
  `503 Service Unavailable` instead of having its connection dropped.
- A torrent added to TorrentPlayer's engine no longer serves pieces to peers that connect in the
  moment before the app applies the upload setting: uploading is now off until it is turned on.
- Settings that an older TorrentStream still running kept open during the update are no longer
  lost: when they could not be moved to TorrentPlayer's folder, the app started with defaults
  and connected directly, even for a WireGuard or SOCKS5 user. They are now copied; if even that
  fails, traffic is held until you close the older app and restart, or choose a connection.
- An FFmpeg folder inside the download folder, saved by an earlier version, is no longer used:
  files there can come from torrents. FFmpeg is looked for as if no folder were set.
- A WireGuard tunnel that slept (a laptop suspended) for more than six minutes now tries a new
  handshake when the computer wakes up, instead of closing every connection to start over.
- A peer that failed a piece's hash check and then re-sent it slowly no longer holds up
  playback: a much faster peer takes over the piece the player is waiting for, and smart ban
  still names whoever sent bad data.
- Adding a torrent a third time no longer wipes its piece cache under a stream still reading
  from the first instance.
- Data a peer sends after choking us, for blocks we had asked it for, is used instead of
  discarded as waste.
- A peer sending data nobody asked for no longer looks faster than it is, so it does not get
  more requests and the shown download speed is not inflated.
- Magnet links and torrent files can no longer make TorrentPlayer announce to HTTP trackers on
  your own network or on `localhost`, directly or through the tunnel or proxy, including through
  tracker redirects. UDP trackers are not covered yet.
- Updated `ctutils` (used by the Linux credential store's cryptography) to 0.4.5 for
  [RUSTSEC-2026-0385](https://rustsec.org/advisories/RUSTSEC-2026-0385), non-constant-time
  comparisons.
- Trackerless magnet links found no peers through the WireGuard tunnel or a SOCKS5 proxy ("input
  address stream exhausted"): the metadata lookup now gets up to 200 DHT peers instead of 20.
- Videos with chapters whose Dolby or DTS audio needs converting, such as Sintel, now play with
  sound. The conversion used to fail at once with "The audio could not be converted".

### Known limitations

- On some high-refresh displays (seen at 165 Hz with an NVIDIA GPU), 4K 60 fps video can drop about
  15 % of frames in the built-in player. Edge and Chrome drop the same on that machine, so it comes
  from the system web engine; running the display at 120 or 144 Hz may help.
- Tracker addresses on your own network are refused for HTTP trackers only: UDP trackers at a
  local address, and librqbit's own tracker requests with the `rqbit` engine in Direct mode,
  can still reach them.
- With the `rqbit` engine choice in the WireGuard and SOCKS5 modes, TorrentPlayer announces to a
  torrent's HTTP trackers for librqbit once, when it opens, and not again (librqbit itself now
  contacts no trackers in those modes, so their names are never looked up outside the tunnel or
  proxy). A long session therefore finds no new tracker peers. The automatic engine is not
  affected: torrentplayer-swarm keeps announcing.
- The settings file is now version 5 (`network.hold` replaces `network.vpnAdapterModeRemoved`).
  Going back to 0.3.0 reads it as far as possible and keeps a copy as `settings.newer.json`, but a
  held connection is not held there: 0.3.0 ignores `network.hold` and connects directly.

## [0.3.0] - 2026-10-21

### Changed

- TorrentStream is now TorrentPlayer (torrentplayer.io, `github.com/torrentplayer/torrentplayer`).
  The app identifier is `io.torrentplayer.desktop` and the binary `TorrentPlayer`. Settings, the
  queue, the download cache, the default `Downloads/TorrentStream` folder, and stored secrets
  move to the new names at start-up. The `LUMEN_*` environment variables are now
  `TORRENTPLAYER_*`, and the crates are named `torrentplayer-*`.
- New app icon: a play button ringed by swarm peers.

### Added

- Incoming peer connections can be turned on in Settings → VPN and network, on a chosen or
  random port, for more peers and better uploads (off by default, so there is still no firewall
  prompt unless chosen). TorrentPlayer listens only where the network mode allows: every
  interface in direct mode (with optional UPnP port mapping) and only inside the tunnel with
  WireGuard; never through a SOCKS5 proxy, and never while
  the kill switch blocks traffic. Settings shows whether it is listening.
- torrentplayer-swarm keeps its DHT routing tables in the app data folder between runs (Direct
  mode only), so magnet links find peers sooner after a restart. Protected modes still start
  with a fresh node and save nothing. librqbit starts from the bootstrap servers each time: from
  a saved table it fetched magnet metadata more slowly, not faster.
- In Direct mode, Settings → Network and the status bar recommend a VPN when downloading or
  seeding (connect a VPN app such as Windscribe first, or use the WireGuard mode with your
  provider's WireGuard configuration), with a link
  to the blog post on VPNs; it is shown once more when seeding starts, until dismissed
  (`network.vpnHintDismissed`).
- DHT peer discovery now runs inside the built-in WireGuard tunnel. SOCKS5
  proxies with UDP ASSOCIATE support carry DHT and UDP trackers, including magnet metadata
  discovery, without falling back to direct UDP or target DNS.
- torrentplayer-swarm now uploads verified pieces with cancellable, bounded reads, reports per-torrent
  and total upload rates, and shares the global upload limit with librqbit.
- Seeding can continue after playback until a ratio or time target is reached, and the configured
  peer cap now applies to both engines.
- Storage can cap cache size and expire old finished downloads automatically, while protecting
  everything queued or open in the player; Settings shows current use and offers a safe clear-now
  action.
- Playback positions persist across restarts in a local library, reopening unfinished media
  offers to resume or start over, and watch progress appears in both the Library and queue.
- Seek-bar previews: with FFmpeg installed (on `PATH` or in a folder chosen in Settings), pointing
  at the seek bar shows a frame of the video, for the parts already downloaded.
- The duration of a video now comes from its container header (MP4, Matroska) or, with FFmpeg,
  `ffprobe` when the player cannot tell, so read-ahead and buffering work for such files too.
  FFmpeg reads only what is already downloaded and is locked down to the local stream.
- Videos with Dolby (AC-3, E-AC-3, TrueHD) or DTS audio now play with sound: a bundled,
  playback-only LGPL FFmpeg converts the audio to AAC as the video plays, copying the video, and
  the player keeps the full seek bar and restarts the conversion on far seeks.
- `torrentplayer-bench` measures streaming with incoming peers only (`local --inbound`), how
  closely an engine holds a download limit that changes mid-transfer (`limit`), and start-up
  from a saved DHT state (`--dht-state`, `--no-trackers`). See `docs/performance.md`.

### Removed

- The VPN adapter network mode (binding all traffic to a VPN app's network adapter, saved as
  `bindInterface`) and its adapter picker. The modes are now Direct, SOCKS5 proxy, and built-in
  WireGuard: keep a VPN app connected at the system level and choose Direct, or import your
  provider's WireGuard configuration for a kill switch inside TorrentPlayer (see
  [ADR 0009](docs/adr/0009-remove-vpn-adapter-mode.md)). Saved settings that used it load as
  Direct, and TorrentPlayer pauses all torrent traffic and searches until you dismiss the notice
  in Settings → Network or beside the status bar's network indicator (or choose SOCKS5 or
  WireGuard), so nothing is sent outside your VPN without your confirmation. Settings files are
  now version 4.

### Fixed

- A skipped queue item no longer says "Skipped" twice; its second line shows how much of it is
  already downloaded, if any.
- The download speed shown for torrentplayer-swarm torrents no longer runs up to a quarter
  above the real rate while peers come and go (as they do under a download limit), so the
  queue's "in use" figure stays within the configured limit.
- Changing the download limit while torrentplayer-swarm downloads from many peers is now
  smooth. Lowering it used to let every waiting peer through at once (about 1.5 MB within
  0.2 s with 40 peers) and then deliver nothing for over a second, which could stall playback;
  waits now stretch or shrink to the new limit, and under a limit each peer is read one block
  at a time so no peer waits long for its turn.
- Under a download limit, torrentplayer-swarm no longer asks many peers for more than the limit
  lets through before requests time out. Those requests were fetched twice, so with 40 peers
  up to a third of the limited bandwidth went to data that was thrown away.

### Fixed

- Releases: the parallel builds no longer create duplicate draft releases (v0.2.0 got two).
  One job creates the draft and one uploads every file, so the macOS `.app.tar.gz` archives now
  have checksums and attestations too.

## [0.2.0] - 2026-10-20

### Changed

- Automatic engine selection now streams magnet links with lumen-swarm, TorrentStream's own
  engine, so magnets start and skip as fast as `.torrent` files. librqbit remains the metadata
  resolver and the explicit compatibility engine.

### Fixed

- Magnet links on TorrentStream's engine now find peers through the DHT (in Direct mode), so a
  magnet whose trackers are dead or missing keeps finding peers after the first ones leave.
- The DHT can no longer start in a protected mode when a mode change and a peer lookup happen
  at the same moment: the start re-checks the network policy under the same lock as the stop.
- Playing a search result, a torrent's file, or a file from the chooser adds it to the front of
  the queue and makes it current; it no longer plays outside the queue.
- When playback starves, background downloads now pause only briefly to see whether that helps,
  and keep going (limited) when the video's own swarm is the bottleneck.
- Search results now show when their video is playing, up next, or elsewhere in the queue, and
  disable the Queue button for entries that are already queued.
- Removing every queued file in a torrent now uses a distinct trash-and-count control and asks
  for confirmation, so it cannot be mistaken for removing one video row.
- The queue now records completed videos separately from its cursor: watched items show as
  Played, videos jumped over show as Skipped, and Clear played preserves skipped videos.

### Known limitations

- Installers are not code-signed yet: Windows SmartScreen and macOS Gatekeeper warn the first
  time you open the app.
- The built-in player uses the system web engine, which cannot decode Dolby (AC-3, E-AC-3,
  TrueHD) or DTS audio; the app warns instead of playing silently.

## [0.1.0] - 2026-10-20

The first public preview: stream any torrent, video or audio, and skip to any moment.

### Highlights

- Streaming built for skipping: TorrentStream's own engine fetches the playhead first and, on a
  skip, the exact spot you jump to, from the fastest peers.
- Paste a magnet link, drop a `.torrent` file, or search public torrent indexers with a "Best
  pick" that preloads so it starts at once.
- A queue that downloads ahead while you watch, with live speed, peers, progress, and time left
  per item; what plays always comes first. Playing a search result puts it first in the queue.
- Video, music, podcasts, and audiobooks in one player, with subtitles and keyboard controls.
- Private by design: no telemetry or accounts; direct, VPN adapter, built-in WireGuard, or
  SOCKS5 modes with a kill switch.
- Windows (x64), macOS (Apple silicon and Intel), and Linux (AppImage, `.deb`, `.rpm`).

### Known limitations

- Installers are not code-signed yet: Windows SmartScreen and macOS Gatekeeper warn the first
  time you open the app.
- Magnet links use the embedded librqbit engine by default (Settings → Advanced → Engine switches
  them to TorrentStream's engine); `.torrent` files use TorrentStream's engine.
- The built-in player uses the system web engine, which cannot decode Dolby (AC-3, E-AC-3,
  TrueHD) or DTS audio; the app warns instead of playing silently.
- Desktop only.

### VPN connections

- Separate VPN and WireGuard choices in Network & VPN. VPN uses an existing VPN app's tunnel
  adapter, lists likely VPN adapters first, and explains local Hyper-V switches and Tailscale
  exit nodes. Bound status no longer implies VPN encryption or internet connectivity.
- Imported WireGuard configurations now connect inside TorrentStream using a userspace tunnel, with
  credential-store keys, optional preshared keys, tunneled DNS, authenticated handshake checks,
  automatic recovery, and connection shutdown when settings change. No administrator rights
  or system route changes are required. UDP trackers and their DNS use the tunnel; DHT remains
  disabled in this mode.
- Correctly hide loopback adapters even when they also have non-loopback addresses.
- WireGuard processes incoming replies before immediate TCP stack work, preventing search
  DNS and HTTP requests from stalling while the tunnel still appears connected. Search
  diagnostics retain underlying network errors without logging request URLs.

### Added

- An idle queue explains when loading starts and offers Play first.
- UDP-only magnets can discover peers through built-in WireGuard, including tunneled tracker
  DNS. Tracker bootstrap retains successful replies when other trackers time out.
- Repeatable streaming reports compare both engines under bandwidth limits, latency, peer
  dropouts, and 2 GiB/8 GiB workloads at a 4K media rate. A separate WebView probe measures
  actual 4K decoding and seeks. Large benchmark fixtures use bounded working memory.
- Search results no longer show license labels or the license-check notice.

- Search results and filters fill the available window width. Titles, metadata, and sources
  wrap instead of being cut off; actions move below the text in narrow layouts, and expanded
  details fit smaller windows.

- `LUMEN_PROFILE_DIR` keeps settings, the queue, and downloads in a separate folder, for tests and
  portable installs.
- Project scaffolding: Cargo workspace with one crate per subsystem, a Tauri v2 app shell, and a
  Svelte + TypeScript frontend.
- Dual MIT OR Apache-2.0 licensing, Code of Conduct, contributing guide, security policy, and
  governance documents.
- Security baseline: strict Content Security Policy, least-privilege Tauri capabilities, and
  `unsafe` code forbidden outside the libmpv wrapper crate.
- CI quality gates on Windows, macOS, and Linux: formatting, Clippy, tests, docs, MSRV,
  cargo-deny, frontend lint/format/typecheck/tests, DCO sign-off, and coverage.
- Documentation: architecture overview, IPC contract, threat model, building guide, and ADRs.
- The player says when a video's audio cannot be played by the built-in player (Dolby Digital,
  Dolby Digital Plus/Atmos, `TrueHD`, DTS), and mentions a playable alternative track. The codec
  comes from a new bounded container-header probe in `lumen-media` (MP4/MOV, Matroska/WebM;
  `stream_audio` command), confirmed by the webview's decoder counters, which also catch cases
  the probe cannot read. ADR 0007 records how these codecs will be played.
- Search detects audio codecs in release titles (DD5.1, DDP/DD+, E-AC-3, AC3, DTS, DTS-HD, DTS-X,
  `TrueHD`, Atmos, AAC, Opus), says "no sound in built-in player" for Dolby- or DTS-only
  releases, and ranks them lower, so the best pick prefers releases that play with sound.
- Subtitles (CC) in the player. Subtitle files in the torrent (`.srt`, `.vtt`, `.ass`, `.ssa`)
  are matched to the video by name or a `Subs` folder, with their language read from the file
  name; others are listed as "other subtitles", and a local file can be loaded too. A chosen
  file is downloaded at background priority and parsed in Rust into plain-text cues (SRT and
  WebVTT, ASS/SSA dialogue; UTF-8, UTF-16, or Windows-1252; bounded and fuzzed), then shown with
  the TextTrack API. A CC button and menu (always shown; dimmed with only "Load subtitle file…" when the torrent
  has none), C to toggle, the choice remembered for the session
  and the next video, and a new `playback.subtitlesByDefault` setting ("Show subtitles
  automatically", off by default). Embedded subtitle tracks are listed but cannot be shown yet.
  New commands `stream_subtitles`, `load_subtitles`, and `parse_subtitles`, and
  `StreamEngine::read_small_file` for small files of a torrent.
- Search reads search.emberglow.tech's event stream (`/search/stream`): one request per search
  instead of about a dozen, first results after 0.4–0.7 s, and every indexer's results merged
  and ranked by TorrentStream (often 1.5–2.5 times as many as before). A strict, bounded parser reads
  the stream; without it, search falls back to asking indexers in groups. Results arrive at most
  every 150 ms, slide gently into place, keep their order while the pointer or focus is on the
  list, and say how many sources are still searching. A new search, or Stop, stops the running
  one (`cancel_search`, error kind `cancelled`), and the Search button works while searching.
- Search can be limited to categories (Movies, TV, Anime, …) on the search service itself
  (`categories` argument; adult categories are never sent).
- Results that indexers offer only as a download link on the search service (about a third)
  are shown instead of dropped. The magnet links the service looks up during a streamed search
  (`magnet` events) fill them in place: the row keeps its key and position, and merges with other
  indexers' copies once its info hash is known. Adding, preloading, or prefetching one follows the link to its
  `.torrent` file or magnet link once and remembers it; only the service's own links are
  followed, they count against the search rate limit, and links are never logged.
- Search results show a thumbnail: a picture from inside the torrent (poster, cover, folder
  image, screenshot, or Internet Archive's `__ia_thumb.jpg`; never an indexer's banner),
  downloaded piece by piece once its metadata is known, checked to be a JPEG, PNG, or WebP image,
  and shown as a `data:` URL; the best pick shows a frame from its preloaded video instead. A
  fixed box keeps rows from moving. No outside poster service is used.
- Each search result has a details view (click its text, or "Details"): quality (resolution,
  source, codecs, HDR, whether it plays here and with sound), files (videos in lecture order,
  subtitles, other files by type, sizes, piece size, private flag; "Load file list" fetches the
  metadata of that one result), swarm (peers, health, placeholder seed counts, UDP and HTTP
  trackers, and "needs Direct mode" when only UDP trackers meet a mode that blocks UDP), and
  source (indexers with a link to their page, opened in the system browser after a check;
  dates, category, release name, info hash and magnet link to copy; the torrent's comment and
  creator as plain text). It opens in place, smoothly unless reduced motion is on.
- "Videos only", on by default, hides results that are not videos (audio, ebooks, software,
  games; never ones that could be either), with a note saying how many and a one-click "Show".
  A torrent's file list shows only its videos, with "Show all files (N)".
- Play on every search result, not only the best pick, plus a Queue action. The torrent
  details of the first results shown are fetched in the background (`prefetch_torrents`,
  metadata only, kept in the engine for 64 torrents), so playing any of them skips the metadata
  lookup; those rows say "details ready". Only the best pick preloads video.
- Navigation rail with Search, Open, Files, Queue, Playing, and Settings, plus Back and Forward
  (Alt+←/→, the mouse's side buttons, Esc on pages). Going back from the video restores the
  search exactly: query, results, filters, and scroll position. Each search is a history
  entry, recent searches are suggested, and leaving the player keeps the stream open, so
  coming back resumes where the video was.
- One queue, in Playing: the separate Queue page and its rail item are gone; Playing opens
  with nothing playing too ("Pick something from the queue or search"), carries the queue's
  count, and its ⋯ menu holds Clear played, Clear all, "Play ready videos first", and "Keep
  course order". Torrents in the queue are a tree: the torrent row (name, how many of its
  videos are queued, size, how many are ready; drag or Alt+↑/↓ moves it), and under it all of
  its videos in lecture order, each with a queue tick and Play now. Arrow keys move through the
  tree, ←/→ collapse and expand, Enter plays, Space ticks.
- Files is a file explorer: the torrents in the queue and those opened this session on the
  left, the chosen torrent's videos in the top half (Play, + Queue, Queue all) and its other
  files by folder in the bottom half, and a preview of the selected file: pictures, text and
  subtitles (first 64 KB), a video's first frame once its start is downloaded, and details for
  anything else (a new `preview_file` command; HTML and PDF are never rendered, files never
  opened).
- Queue groups and "Play ready videos first". A torrent's queued files sit together;
  "Choose files…" lists its videos in lecture order to tick which are queued or play one
  now. Queued videos that are downloaded, or whose start and end are, show "Ready", and the
  optional `playback.optimizeQueue` (off by default; "Keep course order" on by default) moves
  them ahead of those still downloading, with Undo. A new `queue_readiness` command reports
  readiness from the pieces present, without downloading anything.
- Play queue: Queue on search results and on files (or "Queue all"), a collapsible playlist
  beside the player (reorder by dragging or Alt+↑/↓, remove with Undo, play now), a cancellable
  5-second "Up next" countdown at the end of a video, and Shift+N to play the next one. The
  queue is saved across restarts (`queue.json`, versioned, atomic; torrent links only, never
  stream URLs; torrents opened from a file are saved as a magnet link with up to 20 of their
  trackers). The next video is preloaded once the current one is fully downloaded, or nearly
  over with the rest arriving in time: the queued file itself, also when it is the next lecture
  of the torrent playing (its start and end at background priority) or behind a download link
  that redirects to a magnet link. Fullscreen stays on when the next video starts.
- Search filters and sorting, applied at once to the results on screen and kept for the
  session: category (a menu in the search box, also sent to the search service), source,
  minimum seeders, minimum resolution, size range, "plays here", and "no camera copies". A
  labelled Sort menu at the start of the row: best match (default), most peers, best quality
  (resolution, then Blu-ray > WEB-DL > WEBRip > HDTV > DVD > camera), most seeders, newest,
  oldest, largest, smallest, or name. Ties keep the best-match order; unknown values go last.
- An Open page for links and `.torrent` files, listing the torrents opened this session.

### Changed

- The best pick is preloaded once it has stayed best for 2 s (about 3 s into a search) instead
  of when the slowest indexer answers, and at most twice per search.
- Adult results are also recognized by their Torznab category ids, since the search service
  now includes 15 adult-only indexers; the group fallback does not ask adult-only or
  excluded-by-default indexers. Placeholder seed counts (`suspicious`) count as unknown, so
  fake listings are never the best pick.
- lumen-swarm hands out requests to peers in turns of 256 KiB, so the most urgent data is spread
  over all peers instead of queued behind one of them, and keeps request queues short unless
  data is due within 4 s (was 10 s), which makes seeks at 4K bitrates faster.

### Fixed

- Fetching one torrent's metadata no longer blocks opening or playing other torrents. Loading
  controls are scoped to each torrent, duplicate requests share a fetch, and an older load
  cannot override a newer selection or navigation.
- Nothing played with the network mode "Bind to interface" (a VPN adapter) for torrents that list
  only UDP trackers, which is most public torrents: all UDP was off in that mode, so they found no
  peers and magnet links could not even fetch their metadata. TorrentStream now announces to UDP trackers
  itself, from sockets bound to the interface's address (BEP 15, new `lumen_swarm::udp_tracker`
  and `Network::udp_socket`), keeps announcing while streaming, and hands the peers to librqbit
  for magnet metadata. librqbit's own UDP (DHT, uTP) stays off there: it cannot be pinned to an
  interface on Windows.
- Seeking while a video played could leave it paused for good. WebView2 paints the frame at the
  new position before firing `seeked` and nothing more while paused, and the seek gate (which
  holds audio back until the picture is ready) ignored frames painted before `seeked`. It now
  recognises the new position's frame by its media time, and releases any hold one second after
  `seeked` at the latest. Found by the new end-to-end test.
- An MP3 audiobook was the best pick for "feynman lectures physics". Audio-only titles (mp3,
  flac, m4b, kbps, audiobook), ebooks (pdf, epub, …), and audio or book category ids now count
  as not a video, and a torrent with no sign of video must be at least 100 MiB to be the best
  pick.
- Download links that redirect to a magnet link (such as `search.emberglow.tech/download/…`)
  failed with an empty-file error; they now add the magnet link.
- Going from the video back to the file list or search lost the search results.
- With the engine set to TorrentStream's engine, playing a magnet link failed ("Magnet links are not
  supported") unless its preload had finished; librqbit now fetches the metadata and lumen-swarm
  plays it.
- lumen-swarm stayed in its start-up (critical) phase while a player kept reading, downloading
  only just ahead of the reader, so the read-ahead buffer never built up on high-bitrate video
  (about 10 MiB/s at most on a 25 MiB/s link). The phase now ends once half of the critical
  bytes ahead of the reader are downloaded, and starts again whenever the reader has to wait.
- lumen-swarm's round-trip estimate only took samples from requests sent to an empty queue, so
  for a busy peer it drifted upwards and kept deepening that peer's queue, slowing every later
  seek. Every block now gives a sample, corrected for the blocks queued ahead of it.
- Videos could play silently muted: the player muted on any `play()` rejection, including the
  `AbortError` raised when a seek (or a pause) interrupts a starting or resuming playback, and
  never unmuted. It now mutes only when the autoplay policy refuses sound, then shows a "click
  to unmute" button. Interruptions are retried with the sound unchanged.

[Unreleased]: https://github.com/torrentplayer/torrentplayer/compare/v0.3.1...HEAD
[0.3.1]: https://github.com/torrentplayer/torrentplayer/compare/v0.3.0...v0.3.1
[0.3.0]: https://github.com/torrentplayer/torrentplayer/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/torrentplayer/torrentplayer/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/torrentplayer/torrentplayer/releases/tag/v0.1.0
