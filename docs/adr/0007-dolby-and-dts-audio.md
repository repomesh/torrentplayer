# 0007: Dolby and DTS audio in the built-in player

- **Status:** Accepted; the fallback shipped first (see [Implementation](#implementation))
- **Date:** 2026-10-18, implemented 2026-10-20

## Context

The built-in player is the webview's `<video>` element (WebView2, Chromium). It decodes AAC,
MP3, Opus, Vorbis, and FLAC, but not Dolby Digital (AC-3), Dolby Digital Plus (E-AC-3,
including Atmos), Dolby `TrueHD`, Dolby AC-4, or DTS in any variant. Measured on 2026-10-18
with Edge 155.0.4283.62, the same version as the installed WebView2 runtime: `canPlayType`
returns `""` and `MediaSource.isTypeSupported` returns `false` for `ac-3`, `ec-3`, `ac-4`,
`dtsc`, `dtsh`, `dtsl`, and `mlpa`, and `"probably"` for `mp4a.40.2`, `opus`, `flac`, and `mp3`.
Such videos play their picture silently, with no error event.

Many popular releases carry only these codecs: WEB-DL releases usually have E-AC-3
(`DDP5.1`), Blu-ray rips DTS-HD or `TrueHD`, and older rips AC-3. Users reported "no audio for
some videos".

As of this change TorrentPlayer detects the problem and says so, without fixing it:

- `torrentplayer-media` reads the container header (MP4/MOV `stsd` sample entries, Matroska `Tracks`)
  and the player shows "This video's audio is Dolby Digital (AC-3), which the built-in player
  can't play yet", mentioning a playable alternative track if there is one. Chromium's
  `webkitAudioDecodedByteCount` confirms the silence, and catches what probing cannot.
- Search ranking lowers releases whose titles name only Dolby or DTS audio, so the best pick
  prefers ones that play with sound.

This ADR records how to actually play these codecs.

## Decision

1. **Primary: libmpv in Phase 2**, through `torrentplayer-player`, as already planned (see
   [Player integration](../player.md)). libmpv's FFmpeg decodes all of these codecs natively
   (`ac3`, `eac3`, `truehd`/`mlp`, and `dca` for DTS core, DTS-HD MA lossless, and DTS Express),
   so nothing extra is needed for audio. The detection above stays as the fallback player's
   notice.
2. **Fallback, if libmpv slips or fails on a platform: an FFmpeg sidecar that transcodes only
   the audio** to AAC (or Opus) and copies the video, serving fragmented MP4 to the webview.
   This is the "transcoded in real time only as a last resort" path in the player design.
3. Until one of these ships, the built-in player plays the picture and shows the notice.

## Alternatives considered

### libmpv via `torrentplayer-player` (chosen as primary)

- **Licensing:** libmpv is LGPL-2.1+ when built with `-Dgpl=false`; FFmpeg is LGPL-2.1+ without
  `--enable-gpl`/`--enable-nonfree`. The AC-3, E-AC-3, `TrueHD`, and DTS decoders are FFmpeg's own
  LGPL code, so an LGPL build (required by [ADR 0001](0001-project-license.md)) keeps them.
- **Patents:** AC-3's patents have expired (the last ones in about 2017). E-AC-3 (2005) and
  DTS-HD (2004 onwards) are less clear: some patents may still be in force in some countries.
  MLP/`TrueHD` derives from late-1990s MLP. FFmpeg, VLC, and mpv ship these decoders without
  licenses, but the risk falls on whoever distributes binaries. The maintainers must review this
  before release (this is not legal advice). Building without `eac3`/`dca` is possible if needed.
- **Size:** about 25–40 MB for an LGPL libmpv plus FFmpeg DLLs built for playback only (popular
  prebuilt full GPL builds are 80–120 MB).
- **Windows packaging:** the DLLs are installed next to `torrentplayer.exe` as Tauri bundle resources and
  signed with the app. They are built in CI with `-Dgpl=false`, because prebuilt Windows libmpv
  builds are usually GPL.
- **Effort:** large (weeks): the FFI wrapper, the rendering approach (an ADR still to be
  written), and three platforms. That work is planned anyway, and it also brings every container,
  embedded subtitles, and audio track switching.

### FFmpeg sidecar, audio transcoded, video copied (chosen as fallback)

`ffmpeg -ss <pos> -i <loopback URL> -map 0:v:0 -map 0:a:<n> -c:v copy -c:a aac -f mp4
-movflags frag_keyframe+empty_moov+default_base_moof pipe:1`, fed to the webview through Media
Source Extensions (or a second stream-server endpoint).

- **Licensing:** an LGPL `ffmpeg.exe` with FFmpeg's native AAC encoder (or libopus, BSD). It must
  not use `libfdk_aac` (non-free). Running it as a separate process keeps LGPL obligations simple:
  ship its source or offer it, and let users replace the binary.
- **Patents:** as for libmpv on the decoding side. Encoding to AAC carries AAC's own pool, which
  webviews and operating systems already ship; Opus is royalty-free.
- **Size:** about 5–10 MB for a minimal build (only the needed demuxers, decoders, the AAC
  encoder, and the MP4 muxer); 60–80 MB for a stock static build.
- **Windows packaging:** a Tauri sidecar (`bundle.externalBin`, named per target triple), signed.
  It is spawned from Rust, never through the shell plugin, reading only the loopback URL. The
  token goes on the command line, so keep the process's lifetime short and never log the
  arguments.
- **Effort:** medium to large (2–3 weeks): process management, restarting at the new position
  on every seek (the transcoded output has no seek index), MSE playback with timestamp offsets in
  the frontend, and error handling. CPU cost is small: decoding AC-3/DTS and encoding stereo or
  5.1 AAC takes a few percent of one core. Video quality is untouched.
- Variant: when a playable track already exists (an AAC track beside the AC-3 default), `-c copy`
  of video plus that track is a cheap remux with no transcoding.

### Platform decoders (Windows Media Foundation)

Many Windows 10/11 editions include a Dolby Digital (Plus) decoder MFT, but N editions do not, and
there is nothing for DTS or `TrueHD`. Using it needs Win32/COM FFI (`unsafe`, so only in
`torrentplayer-player`), and WebView2 does not use it for `<video src>` playback (measured above). It is
Windows-only, with partial coverage, so it was not chosen.

### Decoding in the webview with WebAssembly

Demux in WASM, decode with an FFmpeg (or liba52) build compiled to WASM, play through Web Audio,
and keep the muted `<video>` in sync. Keeping audio and video in sync across seeks, stalls, and
rate changes is fragile. The CSP would need `'wasm-unsafe-eval'`, and LGPL relinking obligations
apply to the WASM blob. The effort is large and the result worse than either chosen option.

### Pure-Rust decoders

Symphonia covers AAC, MP3, FLAC, Vorbis, ALAC, and PCM, but no mature Rust AC-3, E-AC-3, DTS, or
`TrueHD` decoder exists. Writing them is a large project with the same patent questions. Not
viable now.

## Consequences

- Users get a clear explanation instead of unexplained silence, and search prefers releases that
  play with sound. Title-based detection is a heuristic: a release can name DTS and still carry
  an AAC track (titles naming both count as playable).
- Phase 2 must verify that the LGPL libmpv/FFmpeg build includes `ac3`, `eac3`, `truehd`, and
  `dca`. The maintainers must decide the E-AC-3/DTS patent question before shipping binaries.
- If the fallback sidecar is built, the threat model must cover the extra process and the stream
  token on its command line.

## Implementation

The FFmpeg sidecar was built first (issue #15), because it is far smaller than the libmpv work
and makes these videos play now. libmpv stays the plan for Phase 2.

- **Build:** `scripts/build-ffmpeg.sh` builds FFmpeg 9.0.2 from the checksummed release tarball
  with `--disable-everything --disable-autodetect`, enabling only the Matroska and MOV demuxers,
  the `ac3`, `eac3`, `truehd`, `mlp`, and `dca` decoders, the native AAC encoder, the MP4 muxer,
  and the `pipe`, `http`, and `tcp` protocols (no file access). The script fails if the
  configuration is GPL or non-free or lacks a needed component. The result is 3.7–3.8 MB per platform. CI builds
  it on each platform (Windows with the runner's MinGW) and bundles it with
  `--config src-tauri/tauri.sidecar.conf.json` as `torrentplayer-ffmpeg`, next to the app's
  executable, with FFmpeg's license files. Releases publish the source tarball. Builds without
  it (local ones, by default) keep the notice.
- **Codecs:** all four (AC-3, E-AC-3, `TrueHD`, DTS) are enabled; the maintainers accepted the
  patent question above on 2026-10-20. Audio is mixed down to stereo AAC at 192 kb/s.
- **Command:** `-ss <pos> -copyts -i <url> -map 0:V:0? -map <track> -map_chapters -1 -c:v copy
-c:a aac -ac 2 -avoid_negative_ts make_non_negative -f mp4 -movflags
frag_keyframe+empty_moov+default_base_moof+frag_discont -frag_duration 1000000 pipe:1`.
  `-copyts` and `frag_discont` keep the original timestamps, so each conversion lands at its
  place on the video's timeline in Media Source Extensions, with no offsets in the frontend. The
  track is FFmpeg's `0:i:<track_ID>` in MP4 and `0:a:<n>` in Matroska, where `<n>` counts
  disabled tracks as FFmpeg does. `-map_chapters -1` keeps chapters out: the MP4 muxer would
  write them as a text track, and a track other than video and AAC fails the conversion.
- **Input:** FFmpeg reads from a loopback stream server of its own that serves only this file,
  with its own token, so the URL on its command line grants nothing else. Proxy variables are
  removed from its environment.
- **Output:** the backend reads the initialization segment, derives the MSE type
  (`video/mp4; codecs="avc1.64001f,mp4a.40.2"`) in `torrentplayer-media`, and publishes the output as a
  live stream on the app's stream server (`/l/<id>`), readable once, with CORS only for the
  app's own origins. The CSP allows `blob:` media and `fetch` to `http://127.0.0.1:*`.
- **Seeking:** the player keeps up to 60 s ahead and 30 s behind; a seek outside that starts a
  new conversion, and the previous FFmpeg is stopped. Measured with local test files: playback
  resumes 1.5 s after a seek. FFmpeg converts about 70 times faster than real time.
- **Not converted:** AC-4 (FFmpeg has no decoder), and audio in containers the probe does not
  read (AVI, MPEG-TS).
