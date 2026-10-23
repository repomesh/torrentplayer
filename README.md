<p align="center">
  <img src="assets/app-icon.png" alt="TorrentPlayer logo: a play button ringed by swarm peers" width="128" height="128">
</p>

<h1 align="center">TorrentPlayer</h1>

<p align="center">
  <strong>Stream at the speed of the swarm.</strong><br>
  Movies, shows, music, podcasts, audiobooks: any torrent starts in seconds and skips to any
  moment in under a second, straight from the swarm. No servers, no accounts, no limits.
</p>

<p align="center">
  <a href="https://torrentplayer.io">torrentplayer.io</a> ·
  <a href="#why-torrentplayer">Why TorrentPlayer</a> ·
  <a href="#how-it-compares">Compare</a> ·
  <a href="#features">Features</a> ·
  <a href="#performance">Performance</a> ·
  <a href="#privacy">Privacy</a> ·
  <a href="#get-torrentplayer">Get it</a> ·
  <a href="https://discord.gg/SNsKPAu7pT">Discord</a>
</p>

<p align="center">
  <a href="https://github.com/torrentplayer/torrentplayer/actions/workflows/ci.yml"><img src="https://github.com/torrentplayer/torrentplayer/actions/workflows/ci.yml/badge.svg" alt="CI status"></a>
  <a href="https://github.com/torrentplayer/torrentplayer/actions/workflows/build.yml"><img src="https://github.com/torrentplayer/torrentplayer/actions/workflows/build.yml/badge.svg" alt="Build status"></a>
  <a href="#license"><img src="https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue" alt="License: MIT OR Apache-2.0"></a>
  <img src="https://img.shields.io/badge/platforms-Windows%20%7C%20macOS%20%7C%20Linux-informational" alt="Platforms: Windows, macOS, Linux">
  <img src="https://img.shields.io/badge/telemetry-none-brightgreen" alt="Telemetry: none">
  <a href="https://discord.gg/SNsKPAu7pT"><img src="https://img.shields.io/badge/Discord-come%20talk-5865F2?logo=discord&amp;logoColor=white" alt="Discord: come talk"></a>
</p>

<p align="center">
  <a href="assets/demo/torrentplayer-demo.mp4"><img src="assets/demo/torrentplayer-demo.gif" alt="Demo: searching for Sintel, pressing Play on the best pick, and clicking around the seek bar; playback starts in half a second and each skip plays again within about a second, while the queue downloads in the background" width="720"></a>
  <br>
  <sub>Search, play, skip. Recorded live in real time; nothing is sped up. <a href="assets/demo/torrentplayer-demo.mp4">Full demo with sound (MP4, 35 s)</a>.<br>
  <em>Sintel</em> and <em>Tears of Steel</em> © Blender Foundation, CC BY 3.0.</sub>
</p>

<p align="center">
  <img src="assets/screenshots/queue-and-player.png" alt="TorrentPlayer playing a film while the rest of the queue downloads in the background, each row showing its speed, peers, progress, and time left" width="900">
  <br>
  <sub><em>Tears of Steel</em>, <em>Sintel</em>, and <em>Big Buck Bunny</em> © Blender Foundation, CC BY 3.0.</sub>
</p>

---

TorrentPlayer is a free, open-source desktop app that **plays any torrent straight from
BitTorrent**, video or audio, the way a streaming service would. Press play and it starts in
seconds; drag the seek bar to minute 47 and it is playing minute 47 a moment later, long before
the download gets there. No separate torrent client, no waiting for the file, no server in
between.

Use it for whatever you stream from torrents: films and series, music albums, podcasts,
audiobooks, lectures, and open media. You are responsible for having the right to download and
share what you play.

## Why TorrentPlayer

- **Stream it. Skip it. Instantly.** Most torrent players download from the start and make you
  wait for the part you want. TorrentPlayer's own streaming engine fetches **the exact moment you
  jump to, first**, so skipping ahead resumes in under a second: 0.3 s on a fast link and 0.7 s
  (median) on a public swarm in our [measurements](#performance). Skip the intro, jump to the
  lecture's second half, scrub back and forth; it keeps up.
- **Full speed, limited only by the swarm.** Video comes directly from the people sharing it.
  How fast it arrives depends on the torrent's seeders and peers and on your own connection,
  never on a provider's plan, a server's capacity, or a "free tier".
- **No servers, no dependencies.** There is no TorrentPlayer streaming server, cloud cache, or
  relay, and no account to sign in to. Your computer talks to the swarm directly, so nothing
  in between can throttle it, fill up, or shut it off. (The built-in search asks a public
  torrent index; it is optional, and pasting a magnet link or opening a `.torrent` file needs
  nothing but the swarm.)
- **None of the usual limits.** No subscription, ads, watch-time limits, quality caps, storage
  quotas, or speed tiers. Watch at the quality the file has, for as long as you like.
- **Private by design.** No telemetry, analytics, or crash reporting. Built-in WireGuard and SOCKS5,
  with a kill switch that never falls back to a direct connection, or your own VPN app.
- **Free and open source,** every part of it, including the streaming engine. MIT or
  Apache-2.0, your choice.

## How it compares

|                                        | **TorrentPlayer**                   | Stremio (torrent add-ons)                | WebTorrent Desktop   | qBittorrent + VLC                   | Debrid / cloud services           | webtor.io (browser)           |
| -------------------------------------- | ----------------------------------- | ---------------------------------------- | -------------------- | ----------------------------------- | --------------------------------- | ----------------------------- |
| Stream and skip anywhere               | ✅ the spot you skip to comes first | ✅                                       | ✅                   | ❌ plays in order only              | Only after the service fetches it | ✅ via its servers            |
| Video straight from peers, no server   | ✅                                  | Via a local streaming server             | ✅                   | ✅                                  | ❌ provider's servers             | ❌ webtor.io's servers        |
| Speed limited only by the swarm        | ✅                                  | ✅                                       | ✅                   | ✅                                  | ❌ plan and provider limits       | ❌ free speed cap, paid tiers |
| Free, no account                       | ✅                                  | ✅ free tier, guest mode                 | ✅                   | ✅                                  | ❌ paid plans                     | ✅ basic use                  |
| Fully open source                      | ✅                                  | Partly (streaming server is proprietary) | ✅                   | ✅                                  | ❌                                | ✅                            |
| Built-in VPN (WireGuard) + kill switch | ✅                                  | ❌ bring your own                        | ❌                   | ❌ proxy and interface binding only | Not applicable                    | Not applicable                |
| Queue that downloads ahead             | ✅ shares bandwidth with playback   | Paid plan                                | ❌                   | As a download client                | Cloud storage                     | Paid tiers                    |
| No telemetry                           | ✅                                  | ❌ analytics (per privacy policy)        | ❌ basic usage stats | ✅ none documented                  | Not checked                       | Not checked                   |
| Actively maintained                    | ✅                                  | ✅                                       | Last release 2020    | ✅                                  | ✅                                | ✅                            |

Checked on 20 October 2026. Stremio and Kodi reach more devices (TVs, phones), and VLC- or
mpv-based players play more codecs today; see the [full comparison](docs/comparison.md) for
sources, details, and where other tools are the better fit.

## Features

### Streaming built for skipping

- **TorrentPlayer's own engine** (`torrentplayer-swarm`), written for playback rather than bulk
  downloading. It requests the playhead first, then the file's index and a read-ahead window
  sized from the video's bitrate. When you skip, it cancels requests that are no longer
  urgent, sends the pieces at the new position to the fastest peers, and duplicates the most
  urgent blocks to a second peer so one slow peer cannot stall playback.
- **Finds peers fast.** HTTP and UDP trackers, the DHT, and peer exchange, with quick first
  connection attempts and peers that worked before tried first.
- **Paste, drop, or search.** Magnet links, `.torrent` files and links (drag one onto the
  window), or the built-in search.
- **Instant start for the best result.** Each search marks a "Best pick" and quietly preloads
  its first minutes, so pressing play on it starts at once. One preload at a time, never
  uploaded, deleted if unused, and off with one switch.

### A queue that downloads ahead, intelligently

- **Everything you queue downloads in the background** while you watch, so the next video is
  ready before you get to it. Set a maximum in Settings if you prefer fewer at once.
- **What you are watching always comes first.** Every second TorrentPlayer measures your
  connection and playback's needs. While the buffer is short, the rest of the queue gets only
  what playback leaves; if playback runs low, the rest pauses briefly to let it catch up, and
  keeps going if the video's own swarm is the bottleneck. Once you are comfortably ahead,
  everything runs at full speed again.
- **Live status for every item:** download and upload speed, peers, percent downloaded, and
  time left, with a bar
  that is green when it is ready to play, yellow while it downloads, and grey while it
  waits for peers. A "Fastest" badge points out the video that would play smoothest now.
- **Gives back to the swarm.** The built-in engine uploads every verified piece it has while a
  torrent is active. After playback, keep sharing until a ratio or time target is reached, with
  one upload limit across both engines.
- **In order or fastest first.** Series, albums, and courses download in sequence, or the fastest
  torrents go first while each course keeps its lecture order. Pause any video, or a whole
  torrent, with one click.

### A player for video and audio

- A clean, cinematic player with keyboard controls (Space, ←/→ to skip, F for fullscreen, M to
  mute, C for captions), playback speed, and auto-play of the next item in the queue.
- **Music, podcasts, and audiobooks** (MP3, FLAC, AAC/M4A/M4B, Ogg, Opus, WAV) stream and skip
  just like video, with a "now playing" view; an album starts at its first track and its tracks
  queue in order.
- **Subtitles** found in the torrent and matched to the video, or loaded from a file.
- **Honest status** instead of an endless spinner: download rate, peers, and seconds buffered
  ahead, plus a debug overlay (D) with a live piece map.
- **Dolby and DTS audio play with sound:** AC-3, E-AC-3, TrueHD, and DTS tracks, which the
  system's web engine cannot decode, are converted to AAC on the fly by a bundled, playback-only
  FFmpeg; the video itself is untouched.

### Find and organize

- **Search public torrent indexers** from inside the app. Results stream in as each indexer
  answers, ranked by relevance and by how watchable they are: seeders, resolution, source
  quality, HDR, whether the built-in player can play them, and a size check that catches fake
  4K. Adult categories are excluded by default.
- **Courses, not file lists.** Lectures are natural-sorted ("Lecture 2" before "Lecture 10"), a
  torrent's videos appear as one course in the queue, and a file explorer previews pictures,
  text, and subtitles inside a torrent before you play anything.
- **Resume across restarts.** Watch progress is saved locally, shown in the queue and Library,
  and reopening an unfinished video offers to continue from its last position or start over.

### Network and privacy controls

- **Three connection modes:** direct (with your VPN provider's app connected, OpenVPN-based
  apps included, traffic goes through it); a **built-in WireGuard tunnel** that runs inside the
  app with no administrator rights or separate VPN app; or a SOCKS5 proxy.
- **Kill switch in the WireGuard and SOCKS5 modes.** If the tunnel or proxy drops, all traffic
  stops and open connections close. Nothing falls back to your direct connection, and traffic
  resumes by itself when the tunnel is back. With your own VPN app, use its kill switch.
- **Incoming peer connections, if you want them** (off by default, so there is no firewall
  prompt): on a chosen or random port, with optional UPnP in Direct mode, or inside the
  WireGuard tunnel on a port your provider forwards. Never through a SOCKS5 proxy.
- **See your speed and where you appear from.** The status bar shows the total download and
  upload speed of every torrent, and your public IPv4 and IPv6 addresses as the
  internet sees them, through the current mode (optional; see [Privacy](#privacy)).
- **Bandwidth limits** for download and upload, applied to the whole app instantly.
- **Seeding controls** for sharing after playback, ratio and time targets, and a peer cap per
  torrent.

## Performance

Measured with the bundled benchmark tool, `torrentplayer-bench`, which acts like a real player through
the app's stream server. Every run starts from an empty cache, and every seek is measured from
a fresh session, so nothing is already downloaded. It reports **data-ready** time: until the
player has the bytes it needs.

| Swarm                                                           | Start-up, median (p95) | Skip, median (p95)  |
| --------------------------------------------------------------- | ---------------------- | ------------------- |
| Local seeder, 8 MiB/s link                                      | 0.35 s (0.36 s)        | **0.27 s** (0.30 s) |
| Local seeder, 2 MiB/s link                                      | 1.08 s (1.08 s)        | **0.68 s** (0.80 s) |
| Public swarm (Ubuntu 24.04 ISO, ~600 peers), capped at 10 MiB/s | 1.2–2.8 s              | **0.70 s** (1.47 s) |

Public swarms vary between runs; that row comes from three runs per configuration. Since 0.2.0
magnet links stream through TorrentPlayer's engine too (librqbit only fetches their metadata);
in 0.1.0 they used the embedded librqbit engine, which took 3.7 s to start and 2.7 s per skip
on the same swarm. See [docs/performance.md](docs/performance.md) for the methodology, and run
it yourself:

```sh
cargo run -p torrentplayer-bench --release -- local --upload-mibps 8 --runs 5
cargo run -p torrentplayer-bench --release -- torrent path/to/file.torrent --runs 3 --download-mibps 10
```

## Privacy

TorrentPlayer has **no telemetry, analytics, crash reporting, or accounts**, and it does not
phone home. Its only network traffic is:

- **BitTorrent traffic** for what you play or queue, directly with that torrent's peers (and its
  trackers, the DHT, or its web seeds).
- **Searches you run**, sent to the public index at
  [search.emberglow.tech](https://search.emberglow.tech), which sees your search terms. Search is
  optional: you can always paste a magnet link or open a `.torrent` file instead.
- **`.torrent` files** you link to, fetched from where they are hosted.
- **The public-IP check** in the status bar, which asks [icanhazip.com](https://icanhazip.com)
  (`ipv4.` and `ipv6.icanhazip.com`) for the addresses it sees: at start-up, when the connection
  changes, and every 10 minutes. It carries no identifiers and can be turned off in Settings →
  Network.

All of it goes through your selected VPN, WireGuard tunnel, or proxy, and nothing is sent while
the kill switch is engaged. See the [threat model](docs/threat-model.md) and
[network modes](docs/network.md). Your queue, settings, and watch progress stay on this computer;
the playback library is a user-only local SQLite database and is never sent anywhere.

## Get TorrentPlayer

TorrentPlayer is an early preview. The latest version is **0.3.1**; see the
[changelog](CHANGELOG.md) for what changed. You can:

- **Download a release** from the [Releases](https://github.com/torrentplayer/torrentplayer/releases)
  page: installers for Windows, macOS, and Linux with SHA-256 checksums, an SBOM, and build
  provenance attestations ([how to verify them](docs/release.md#verifying-a-download)). They
  are not code-signed yet, so Windows SmartScreen and macOS Gatekeeper will warn on first launch.
- **Download a test build.** Every push to `main` builds unsigned installers for Windows, macOS,
  and Linux: open the latest [Build workflow run](https://github.com/torrentplayer/torrentplayer/actions/workflows/build.yml)
  and download the `installers-<platform>` artifact. Unsigned builds trigger SmartScreen on
  Windows and Gatekeeper on macOS.
- **Build from source** (below).

| Platform                         | Packages                 |
| -------------------------------- | ------------------------ |
| Windows 10/11 (x64)              | `.msi`, `.exe` (NSIS)    |
| macOS 13+ (Apple Silicon, Intel) | `.dmg`, `.app`           |
| Linux (x64)                      | AppImage, `.deb`, `.rpm` |

The player uses your system's web engine (WebView2 on Windows, WebKit on macOS and Linux), so it
plays the formats that engine supports: commonly MP4, WebM, and MKV with H.264, VP9, or AV1
video and AAC, Opus, or MP3 audio. Dolby (AC-3, E-AC-3, TrueHD) and DTS audio in MP4 and MKV
files is converted to AAC as it plays.

## Building from source

You need:

- **Rust**: the toolchain is pinned in [`rust-toolchain.toml`](rust-toolchain.toml) and installed
  automatically by [rustup](https://rustup.rs). Minimum supported Rust: **1.90**.
- **Node.js 22 or newer** and npm.
- **Tauri's system dependencies** for your OS
  ([Tauri prerequisites](https://v2.tauri.app/start/prerequisites/)):
  - Windows: Microsoft C++ Build Tools and WebView2 (preinstalled on Windows 11).
  - macOS: Xcode Command Line Tools.
  - Linux (Debian/Ubuntu):
    ```sh
    sudo apt install libwebkit2gtk-4.1-dev build-essential curl wget file \
      libxdo-dev libssl-dev libayatana-appindicator3-dev librsvg2-dev
    ```

Then:

```sh
git clone https://github.com/torrentplayer/torrentplayer.git
cd torrentplayer/ui    # npm commands run in the frontend folder
npm ci
npm run tauri dev      # run the app with hot reload
npm run tauri build    # build optimized installers for your platform
```

See [docs/building.md](docs/building.md) for the full development workflow, including how to run
every CI check locally.

## Documentation

- [Introduction](docs/introduction.md) and [architecture](docs/architecture.md): how torrent data
  becomes video on screen
- [Streaming engine](docs/streaming.md) and [player](docs/player.md)
- [How TorrentPlayer compares](docs/comparison.md), with sources
- [Network modes, VPN, and kill switch](docs/network.md)
- [Configuration](docs/configuration.md): settings, folders, and secrets
- [Performance and benchmarking](docs/performance.md)
- [IPC contract](docs/ipc.md): every command and event between the UI and the backend
- [Threat model](docs/threat-model.md) and [troubleshooting](docs/troubleshooting.md)
- [Release process](docs/release.md) and [architecture decisions](docs/adr/)

The docs also build as a book with [mdBook](https://rust-lang.github.io/mdBook/):
`mdbook serve docs`.

## Roadmap

Working today: the streaming engine (for `.torrent` files and magnet links), player, search, the
queue with background downloads, the WireGuard and SOCKS5 modes with a kill switch, and
bandwidth limits. Next:

- [x] Seeding controls: keep sharing after watching, ratio and time targets, peer limits
- [x] Cache size limit and automatic clean-up of old downloads
- [x] Resume where you left off
- [x] Dolby and DTS audio (converted as it plays)
- [x] Incoming peer connections, with optional UPnP
- [ ] More search sources: Internet Archive, Academic Torrents, and your own
- [ ] libmpv player for codecs the system web engine cannot play
- [ ] Signed releases for every platform

## FAQ

**Is it legal?** BitTorrent is a distribution protocol, and TorrentPlayer is built for openly
licensed and public-domain material. Search covers general public torrent indexers whose results
are **not** license-checked, so you are responsible for having the right to download and share
what you play.

**Why is a video slow to start?** Speed comes from the swarm. A torrent with few seeders or
peers can only be as fast as they are; the queue shows each video's peers and speed so you can
pick one that plays smoothly, or switch the queue to "Fastest first".

**Does TorrentPlayer upload?** Yes. Both torrent engines share verified pieces while a torrent is
active, under the global upload limit. Settings can keep sharing after playback until either a
ratio or time target is reached.

**What happens if my VPN drops?** In the WireGuard and SOCKS5 modes, the kill switch stops all
traffic and closes connections until the tunnel is back; nothing falls back to a direct
connection. If you use your own VPN app (Direct mode), turn on that app's kill switch. See
[network modes](docs/network.md).

**Where are videos stored?** In `TorrentPlayer` inside your Downloads folder, unless you choose
another folder in Settings → Storage, which shows its size and limit and safely clears finished,
unqueued downloads (see
[configuration](docs/configuration.md)).

## Community

Questions, ideas, or just want to talk about TorrentPlayer? Join the
[TorrentPlayer Discord server](https://discord.gg/SNsKPAu7pT). Bugs and feature requests are tracked in
[GitHub issues](https://github.com/torrentplayer/torrentplayer/issues), so they don't get lost
in chat.

## Contributing

Contributions are welcome. Please read [CONTRIBUTING.md](.github/CONTRIBUTING.md) first; all commits are
signed off under the [Developer Certificate of Origin](https://developercertificate.org), and the
project follows the [Contributor Covenant Code of Conduct](.github/CODE_OF_CONDUCT.md).

To report a security vulnerability, **do not open a public issue**; follow
[SECURITY.md](.github/SECURITY.md).

## License

TorrentPlayer is licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or <https://www.apache.org/licenses/LICENSE-2.0>)
- MIT License ([LICENSE-MIT](LICENSE-MIT) or <https://opensource.org/licenses/MIT>)

at your option. Unless you explicitly state otherwise, any contribution intentionally submitted
for inclusion in TorrentPlayer by you, as defined in the Apache-2.0 license, shall be dual
licensed as above, without any additional terms or conditions.

TorrentPlayer builds on open-source software under its own licenses, including
[Tauri](https://tauri.app), [librqbit](https://github.com/ikatson/rqbit),
[boringtun](https://github.com/cloudflare/boringtun), and [Svelte](https://svelte.dev); see
[THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md). Content you watch belongs to its creators.
