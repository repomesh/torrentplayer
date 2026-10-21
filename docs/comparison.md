# How TorrentPlayer compares

There are several ways to watch a torrent without waiting for the whole download. This page
compares the common ones on the things that matter for watching: where the video actually
comes from, whether you can **stream and skip anywhere** while it downloads, what it costs,
what limits apply, and how it handles privacy.

Facts about other products were checked on **20 October 2026** against their official sites,
repositories, and documentation (sources below). Products change; corrections are welcome.

## At a glance

|                                       | TorrentPlayer                                            | Stremio (torrent add-ons)                               | WebTorrent Desktop            | qBittorrent + VLC                                         | Debrid / cloud (Real-Debrid, Seedr)          | webtor.io (browser)                              |
| ------------------------------------- | -------------------------------------------------------- | ------------------------------------------------------- | ----------------------------- | --------------------------------------------------------- | -------------------------------------------- | ------------------------------------------------ |
| Video comes from                      | Peers, directly to your app                              | A local streaming server on your machine, from peers    | Peers, directly               | Peers, directly                                           | The provider's servers                       | webtor.io's servers                              |
| Stream and skip anywhere              | Yes: the spot you skip to is fetched first               | Yes (local cache); seek performance not documented      | Yes                           | Plays in order only; skipping ahead of the download fails | Once the provider has fetched the file       | Yes, from the server (transcoded when needed)    |
| Speed limited by                      | The swarm and your connection                            | The swarm and your connection                           | The swarm and your connection | The swarm and your connection                             | Your plan and the provider                   | Free speed cap; paid tiers 20–100 Mbps           |
| Cost / account                        | Free, no account                                         | Free tier (guest mode, no account); paid plan optional  | Free, no account              | Free, no account                                          | Paid plans (Seedr: 2 GB, 480p free tier)     | Free basic use; paid tiers for speed and storage |
| Fully open source                     | Yes (MIT or Apache-2.0)                                  | Partly: the streaming server is proprietary             | Yes (MIT)                     | Yes (GPL)                                                 | No                                           | Yes (MIT)                                        |
| Built-in VPN / WireGuard, kill switch | Yes: WireGuard, VPN-adapter binding, SOCKS5; kill switch | No (bring your own VPN)                                 | No                            | Proxy and interface binding; no built-in VPN              | Not applicable (the service joins the swarm) | Not applicable (the service joins the swarm)     |
| Queue that downloads ahead            | Yes, sharing bandwidth with what you watch               | Download manager on the paid plan                       | No                            | Yes (as a download client)                                | Cloud storage                                | Cloud storage on paid tiers                      |
| Telemetry                             | None                                                     | Analytics and error monitoring (per its privacy policy) | Basic usage stats             | None documented                                           | Not checked                                  | Not checked                                      |
| Maintained                            | Active                                                   | Active                                                  | Last release 2020             | Active                                                    | Operating                                    | Active                                           |

## What makes TorrentPlayer different

- **Skipping is the point, not an afterthought.** TorrentPlayer's engine (`torrentplayer-swarm`) was
  written for playback: when you jump, it cancels requests that are no longer urgent, asks the
  fastest peers for the exact pieces at the new position, and duplicates the most urgent blocks
  to a second peer so one slow peer cannot stall you. Measured data-ready seek times are 0.27 s
  on a fast link and a 0.70 s median on a public swarm ([performance](performance.md)).
  General-purpose clients download in order or rarest-first; in qBittorrent, for example, a
  player can only read what has already arrived in sequence.
- **No server in the path.** Cloud and debrid services fetch the torrent on their servers and
  send you a copy, so their plan, capacity, and storage limits become yours. TorrentPlayer
  talks to the swarm directly from your computer: the only speed limits are the swarm's
  seeders and peers and your own connection, and nothing can be capped, queued, or switched
  off by a provider.
- **Privacy controls built in.** Other desktop players leave the VPN to you. TorrentPlayer
  includes a userspace WireGuard client (no administrator rights or separate VPN app), binding
  to your VPN app's adapter, and SOCKS5, all with a kill switch that never falls back to a
  direct connection, plus a status bar that shows the public addresses the internet sees.
- **A queue that thinks about bandwidth.** Everything queued downloads in the background while
  you watch, but only with what playback does not need; it pauses if playback runs low and
  resumes at full speed once you are ahead.
- **No account, no telemetry, fully open source.** Every component is MIT or Apache-2.0,
  including the streaming engine.

## Where others are a better fit

- **Watching on a TV, phone, or tablet:** Stremio and Kodi run on far more devices.
  TorrentPlayer is a desktop app (Windows, macOS, Linux).
- **Codecs the system web engine cannot play** (for example AC-3 or DTS audio on Windows):
  players built on VLC or libmpv play more formats today. TorrentPlayer warns you about such
  files; a libmpv player is on the roadmap.
- **Hiding your address from the swarm without a VPN:** cloud services join the swarm for you.
  With TorrentPlayer, use its WireGuard or proxy modes.
- **Sharing back:** TorrentPlayer's own engine does not upload yet, so it takes from the swarm
  without seeding. Full download clients seed; seeding controls are on TorrentPlayer's roadmap.

## Sources

**Stremio:** plans and pricing <https://www.stremio.com/plans>; guest mode and platforms
<https://www.stremio.com/>; proprietary streaming server
<https://github.com/Stremio/stremio-shell/issues/435>; component licenses
<https://github.com/Stremio/stremio-web>; local cache
<https://stremio.zendesk.com/hc/en-us/articles/360000281632-Why-is-Stremio-taking-disk-space>;
VPN guidance <https://stremio.zendesk.com/hc/en-us/articles/360021412831-VPN-and-Stremio>;
privacy policy <https://www.stremio.com/privacy>; releases
<https://github.com/Stremio/stremio-shell-ng/releases>.

**WebTorrent Desktop:** <https://webtorrent.io/desktop/>, FAQ <https://webtorrent.io/faq>,
repository and usage stats <https://github.com/webtorrent/webtorrent-desktop>, releases
<https://github.com/webtorrent/webtorrent-desktop/releases>.

**qBittorrent:** <https://www.qbittorrent.org/>, license
<https://github.com/qbittorrent/qBittorrent/blob/master/COPYING>, options
<https://github.com/qbittorrent/qBittorrent/wiki/Explanation-of-Options-in-qBittorrent>,
sequential playback behavior <https://github.com/qbittorrent/qBittorrent/issues/10420>.

**Real-Debrid:** API (premium required to add torrents) <https://api.real-debrid.com/>, terms
<https://real-debrid.com/terms>. **Seedr:** plans and free tier <https://www.seedr.cc/faq/>,
streaming <https://www.seedr.cc/products/streaming/>.

**webtor.io:** <https://webtor.io/>, paid tiers <https://webtor.io/donate>, repository
<https://github.com/webtor-io/web-ui>.

**Also considered:** Kodi with the Elementum add-on (open source, streams from peers on your
device, proxy settings; <https://github.com/elgatito/plugin.video.elementum>); Popcorn Time (the
torrent app's last release was in October 2024; the name now belongs to an unrelated
streaming-service directory, <https://popcorntime.app/>); Soda Player (discontinued since
2018).
