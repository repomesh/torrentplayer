# Configuration

Every setting can be changed on the **Settings** page (the gear in the bottom bar, or
<kbd>Ctrl</kbd>+<kbd>,</kbd>; <kbd>Esc</kbd> returns). Clicking the network indicator opens it
at **Network & VPN**. During playback, the gear in the player's controls (or
<kbd>Ctrl</kbd>+<kbd>,</kbd>) opens Settings over the video: the video pauses, and **Back to the
video** (or <kbd>Esc</kbd>) resumes it at the same position. Changes are checked as they are
made and saved automatically about half a second later.

## Where settings are stored

| What                    | Where                                                                       |
| ----------------------- | --------------------------------------------------------------------------- |
| Settings                | `settings.json` in the app config folder (see below)                        |
| Passwords and keys      | The OS credential store, never in `settings.json` (see [Secrets](#secrets)) |
| Downloaded lecture data | `TorrentStream` in your Downloads folder, unless you choose another folder  |

The app config folder is `%APPDATA%\io.torrentstream.desktop` on Windows,
`~/Library/Application Support/io.torrentstream.desktop` on macOS, and
`~/.config/io.torrentstream.desktop` on Linux. The default download folder is
`TorrentStream` in your Downloads folder (`%USERPROFILE%\Downloads\TorrentStream`,
`~/Downloads/TorrentStream`), or `downloads` in the app's local data folder on systems without
one. Earlier versions kept downloads in the local data folder (`%LOCALAPPDATA%\<identifier>\downloads`);
unless you chose a folder yourself, they are moved to the new default at start-up. Inside it,
`lumen-swarm` holds TorrentStream's engine cache (one `pieces.dat` per torrent, not playable
files) and `librqbit` holds downloads made with the librqbit engine (the `rqbit` engine
choice) as their own files.

Versions from before the rename to TorrentStream used `io.github.lumen-app.lumen` for these
folders. At start-up, TorrentStream moves anything still there into the new folders (settings,
the queue, and the download cache; nothing is overwritten), so the old folder disappears once
it is empty. Close any old copy of the app first: files it has open are moved at a later start.

`settings.json` is plain JSON with a `version` field. It is written atomically (to a temporary
file that is flushed and then renamed over the old one), so a crash cannot leave a half-written
file. On Linux and macOS it is created readable only by you (mode `0600`, folder `0700`); on
Windows the per-user config folder is already private to your account.

Loading never fails:

- A missing file means the defaults.
- A damaged file (not JSON) is renamed to `settings.invalid.json` and the defaults are used.
- A section with a wrong type (for example text where a number belongs) is reset to its
  defaults; the other sections are kept. Out-of-range values are reset individually.
- A file written by a newer TorrentStream is read as far as possible, and a copy is kept as
  `settings.newer.json`, because saving drops the fields this version does not know.

Every problem is logged and shown at the top of the Settings page. Older files are upgraded by
`lumen_settings::migrate`, which gains one step per schema change. Version 2 removed
`network.killSwitch` (the kill switch is always on in protected modes, see [Network](#network));
a version 1 file loads normally and the field is dropped the next time settings are saved.
Version 3 added the optional seeding time target; older files keep ratio-only seeding.

## Settings

**When it applies:** _now_ means immediately; _next start_ means the next time TorrentStream starts;
_later_ means the setting is saved now and takes effect when the feature ships (the Settings page
tags these "Coming in a later milestone").

### Playback

| Setting (`settings.json`)     | Default | Valid values  | Applies                        |
| ----------------------------- | ------- | ------------- | ------------------------------ |
| `playback.defaultSpeed`       | `1.0`   | 0.5–3         | when the next video starts     |
| `playback.autoplayNext`       | `true`  | on / off      | at once                        |
| `playback.resumePosition`     | `true`  | on / off      | when the next video starts     |
| `playback.hardwareDecoding`   | `auto`  | `auto`, `off` | later (with the libmpv player) |
| `playback.optimizeQueue`      | `false` | on / off      | at once                        |
| `playback.keepCourseOrder`    | `true`  | on / off      | at once                        |
| `playback.subtitlesByDefault` | `false` | on / off      | when the next video starts     |
| `playback.seekThumbnails`     | `true`  | on / off      | at once                        |

The speed menu in the player changes the current video only; the default applies to each video
as it starts. `subtitlesByDefault` ("Show subtitles automatically") turns on the video's own
subtitles in the first of the system's languages that they have; whatever is chosen in the CC
menu (or with C) is remembered for the rest of the session instead. `autoplayNext` plays the queue's next video after a 5-second "Up next" countdown.
`optimizeQueue` ("Play ready videos first", also in the queue panel) moves queued videos that are
ready to play (downloaded, or their start and end are) ahead of those still downloading, without
moving the video playing; with `keepCourseOrder`, files of one torrent keep their order. Files
saved before these two existed read them as their defaults. With `resumePosition`, reopening an
unfinished video at least 10 seconds in offers its saved position or a fresh start. Positions are
kept even while this preference is off, so the Library can still show watch progress.
`seekThumbnails` ("Seek bar
previews") shows a frame above the seek bar where the pointer is, for the parts already
downloaded; it needs FFmpeg (see `advanced.ffmpegDir`).

### Downloads and storage

| Setting                    | Default | Valid values                                      | Applies    |
| -------------------------- | ------- | ------------------------------------------------- | ---------- |
| `storage.downloadDir`      | `null`  | `null` (default folder) or a full path; see below | next start |
| `storage.cacheLimitGb`     | `null`  | `null` (unlimited) or 1–100 000                   | now        |
| `storage.cleanupAfterDays` | `null`  | `null` (never) or 1–3650                          | now        |

The download folder holds everything TorrentStream downloads: the videos themselves and its cache, in
`lumen-swarm/` and `librqbit/` folders inside it. Type a path, or press **Choose folder…** to
pick one with the system folder picker. The new folder is used from the next start (the page says
so); data already downloaded stays in the old folder, and **Use default** goes back to the
default folder.

A download folder must be a full path (not relative), must not be the root of a drive, and cannot
contain `.` or `..` components, control characters, or device prefixes (`\\?\`, `\\.\`). TorrentStream
creates it if needed and checks that it can write there before saving.

The **Cache** row shows the current use and configured limit. TorrentStream marks a download as
finished when playback reaches the end, then checks the storage policy at startup, after playback,
and periodically while it runs. It removes the oldest eligible downloads first until the cache is
within the limit; the age setting independently removes eligible downloads older than the selected
number of days.

**Clear now…** removes every finished download that is no longer in the queue. A queued torrent or
one with an open player stream is always protected. Cleanup deletes only validated info-hash
folders inside TorrentStream's own `lumen-swarm/` and `librqbit/` cache folders. Unrecognized and
legacy layouts count toward the displayed size but are not removed automatically because they
cannot be associated safely with a finished torrent.

### Bandwidth and seeding

| Setting                          | Default | Valid values                        | Applies    |
| -------------------------------- | ------- | ----------------------------------- | ---------- |
| `bandwidth.downloadLimitKib`     | `0`     | 0 (unlimited) to 1 048 576 KiB/s    | now        |
| `bandwidth.uploadLimitKib`       | `0`     | 0 (unlimited) to 1 048 576 KiB/s    | now        |
| `bandwidth.seedAfterWatching`    | `true`  | on / off                            | now        |
| `bandwidth.seedRatioTarget`      | `2.0`   | 0–100 (0 has no ratio limit)        | now        |
| `bandwidth.seedTimeLimitMinutes` | `0`     | 0–525 600 (0 has no time limit)     | now        |
| `bandwidth.peerLimit`            | `40`    | 5–500                               | next start |
| `bandwidth.maxDownloads`         | `0`     | 0–100 (0 downloads the whole queue) | now        |

The page shows limits in KB/s or MB/s; they are stored in KiB/s.
Both engines upload while a torrent is active. After playback finishes, **Keep sharing after
watching** leaves it seeding until the first configured target is reached. The ratio is uploaded
piece bytes divided by verified downloaded bytes; setting either target to 0 disables only that
target. The upload limit covers both engines together. The peer cap needs a restart because the
librqbit session fixes it when it starts.

### Search

| Setting                           | Default | Valid values | Applies                          |
| --------------------------------- | ------- | ------------ | -------------------------------- |
| `search.sources.emberglow`        | `true`  | on / off     | later                            |
| `search.sources.internetArchive`  | `false` | on / off     | later (source not available yet) |
| `search.sources.academicTorrents` | `false` | on / off     | later (source not available yet) |
| `search.hideAdult`                | `true`  | on / off     | now (from the next search)       |
| `search.openLicenseOnly`          | `false` | on / off     | later                            |
| `search.preloadTopResult`         | `true`  | on / off     | now (from the next search)       |
| `search.resultsPerSearch`         | `100`   | 10–500       | later                            |

Emberglow searches all of its indexers that it searches by default (53 listed in October 2026;
it leaves out 1337x and ones failing repeatedly). 15 of them list only adult content: while
`search.hideAdult` is on their results are hidden (by category label and Torznab id), and the
fallback that asks indexers in groups does not ask them at all. `search.preloadTopResult` also
turns the metadata prefetch of the other results on screen on or off (see
[IPC](ipc.md#prefetch_torrents)).

### Network

The network layer (`lumen-net`) applies these as soon as they are saved; see
[Network modes and kill switch](network.md).

| Setting                        | Default   | Valid values                                          | Applies                    |
| ------------------------------ | --------- | ----------------------------------------------------- | -------------------------- |
| `network.mode`                 | `direct`  | `direct`, `bindInterface`, `socks5`, `wireGuard`      | now                        |
| `network.interface`            | `null`    | an interface name (for `bindInterface`)               | now                        |
| `network.socks5.host`          | `""`      | a host name or IP address, without scheme or port     | now                        |
| `network.socks5.port`          | `1080`    | 1–65535                                               | now                        |
| `network.socks5.username`      | `null`    | 1–255 bytes                                           | now                        |
| `network.socks5.proxyDns`      | `true`    | on: the proxy resolves names; off: TorrentStream does | now                        |
| `network.socks5.proxyTrackers` | `true`    | on: trackers through the proxy; off: none at all      | now                        |
| `network.socks5.proxyDht`      | `true`    | DHT through UDP ASSOCIATE; requires proxy UDP support | now                        |
| `network.wireguard`            | empty     | set by importing a `.conf` file (non-secret parts)    | now in WireGuard mode      |
| `network.encryption`           | `prefer`  | `prefer`, `require`, `off`                            | later                      |
| `network.ipv6`                 | `disable` | `disable`, `tunnel`                                   | now (protected modes only) |

**Kill switch:** always on when a VPN or proxy mode is selected, so there is no setting for it.
If the interface or proxy stops working, torrent traffic pauses and searches are refused; traffic
resumes by itself when it works again. TorrentStream never falls back to a direct connection. Earlier versions
had a `network.killSwitch` toggle that had no effect; it is dropped on upgrade.

**DHT:** uses sockets bound to the VPN adapter or inside WireGuard. In SOCKS5 mode,
`proxyDht` enables discovery through UDP ASSOCIATE; proxies without UDP support cannot
provide DHT, and TorrentStream never sends it around them.

**VPN:** use an existing VPN app connection (including OpenVPN or a provider's app), then choose
its tunnel adapter. This is saved as `bindInterface` for compatibility. Hyper-V local switches
cannot establish a VPN. Tailscale requires an exit node for internet destinations.

**WireGuard:** choose **WireGuard** and import a provider's `.conf` file to connect inside TorrentStream.
**Check configuration** previews it without keys; **Save configuration** stores the keys in the
credential store and the remaining fields in `network.wireguard`, then connects. A missing key
or failed handshake blocks traffic. No external VPN app or administrator rights are required.
The file must contain a `DNS` server to resolve search, trackers, and other host names inside
the tunnel. UDP trackers and DHT use the tunnel too. See [Network modes](network.md).

The SOCKS5 password and the WireGuard private and preshared keys are [secrets](#secrets).

### Advanced

| Setting                 | Default | Valid values                              | Applies    |
| ----------------------- | ------- | ----------------------------------------- | ---------- |
| `advanced.engine`       | `auto`  | `auto`, `swarm`, `rqbit`                  | next start |
| `advanced.debugOverlay` | `true`  | on / off                                  | now        |
| `advanced.logLevel`     | `info`  | `error`, `warn`, `info`, `debug`, `trace` | now        |
| `advanced.ffmpegDir`    | `null`  | `null` (look on `PATH`) or a full path    | now        |

`auto` and `swarm` use TorrentStream's engine (lumen-swarm) for every source. For a magnet link,
librqbit fetches only the metadata, then hands the torrent and the peers it found to lumen-swarm.
`rqbit` is the compatibility choice that uses librqbit for the download too. The environment variables below override `engine` and
`logLevel`, and the Settings page says so when they do.

FFmpeg is optional and not bundled yet. With `ffprobe` and `ffmpeg` in `ffmpegDir` (or its
`bin` folder), or on `PATH` when it is `null`, TorrentStream finds the duration of videos whose
header gives none, so read-ahead and the buffer estimate work for them, and shows seek-bar
previews. The Settings page shows what was found and its version. An LGPL build is enough (for
example the `lgpl` builds at <https://github.com/BtbN/FFmpeg-Builds/releases>, or a package
manager's `ffmpeg`). The programs only ever read what is already downloaded, through the local
stream server; see [IPC](ipc.md#ffmpeg-optional).

## Secrets

Proxy passwords and WireGuard keys are stored in the OS credential store: Windows Credential
Manager, the macOS Keychain, or the Secret Service on Linux (GNOME Keyring or KWallet). Entries
are filed under the service `io.github.lumen-app.lumen` with the account names
`network.socks5.password`, `network.wireguard.private-key`, and
`network.wireguard.preshared-key`.

- They never appear in `settings.json`, logs, error messages, or `Debug` output. In memory they
  are held in zeroizing, redacting `secrecy::SecretString` values.
- The Settings page can set or clear a secret and see whether one is set; it can never read one
  back.
- If no credential store is available (for example a Linux system without a Secret Service
  provider), secrets cannot be saved and the page says so. There is no plaintext fallback.

## Environment variables

| Variable            | Default | Effect                                                                                                                                                                                                                                 |
| ------------------- | ------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `RUST_LOG`          | unset   | Log filter, using [`tracing-subscriber` `EnvFilter` syntax](https://docs.rs/tracing-subscriber/latest/tracing_subscriber/filter/struct.EnvFilter.html), for example `RUST_LOG=lumen_stream=debug,info`. Overrides `advanced.logLevel`. |
| `LUMEN_ENGINE`      | unset   | `auto`, `swarm`, or `rqbit`, for development. Overrides `advanced.engine`.                                                                                                                                                             |
| `LUMEN_PROFILE_DIR` | unset   | An absolute folder for a separate profile: settings in `config`, the queue in `data`, downloads in `local`. For tests and portable installs. On Windows, set `WEBVIEW2_USER_DATA_FOLDER` too to move the webview's data.               |
