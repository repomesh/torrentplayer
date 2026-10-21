# Player integration

> **Status: Planned (Phase 2).** This page records the intended design.

## libmpv (primary)

`torrentplayer-player` wraps libmpv through a maintained Rust binding (the current candidate is
[`libmpv2`](https://crates.io/crates/libmpv2); this will be confirmed in Phase 2). libmpv uses
FFmpeg internally and plays nearly every container and codec, including embedded and external
subtitles.

- **Rendering:** the approach (native child window behind a transparent webview, or the libmpv
  render API with OpenGL) will be chosen after a prototype on all three platforms and recorded in
  an ADR. Requirements: the webview overlays controls on the video, and resizing and fullscreen
  are smooth and flicker-free.
- **Tuning:** `cache`, `cache-secs`, `demuxer-max-bytes`, and `demuxer-readahead-secs` are set to
  cooperate with torrent prioritization, and `hwdec=auto-safe` enables hardware decoding.
- **Events:** `time-pos`, `duration`, `paused-for-cache`, `demuxer-cache-state`, and `track-list`
  drive the UI.

## Fallback

If libmpv fails to initialize, the webview's `<video>` element plays from the loopback HTTP range
server: directly when the platform supports the format, remuxed to fragmented MP4 when the codecs
are compatible, and transcoded in real time only as a last resort.

## Sound in the built-in player (today)

Until libmpv lands, the webview's `<video>` element is the only player (`Player.svelte`).

- **Starting playback** (`ui/src/lib/autoplay.ts`): if the autoplay policy refuses sound
  (`NotAllowedError`), the video plays muted and a "Muted — click to unmute" button appears;
  pressing play, M, or the volume controls also unmutes it. A start interrupted by a pause, seek,
  or load (`AbortError`) is retried with the sound unchanged, and only while playback is still
  wanted. Nothing else mutes the video.
- **Codecs the webview cannot decode** (`ui/src/lib/audio.ts`, `ui/src/lib/converted.ts`,
  `src-tauri/src/convert.rs`, `torrentplayer-media`): WebView2 decodes AAC, MP3, Opus, Vorbis, and FLAC, but
  not Dolby (AC-3, E-AC-3/Atmos, `TrueHD`, AC-4) or DTS. The backend reads the audio codec from the
  container header (`stream_audio`). When the default track is AC-3, E-AC-3, `TrueHD`, or DTS in an
  MP4, MOV, Matroska, or WebM file, the player plays a conversion instead (`convert_audio`): the
  bundled FFmpeg reads the video from a loopback server of its own, copies the video, converts that
  track to stereo AAC, and writes fragmented MP4 with the original timestamps. The player feeds it
  to the `<video>` element through Media Source Extensions, so the seek bar, duration, and subtitles
  work as usual. Reading pauses with 60 s buffered ahead (which also pauses FFmpeg), and data more
  than 30 s behind is removed. After a seek, a new conversion starts where the buffered video at the
  new position ends, unless the running one is about to deliver it, typically playing again within 2
  s. A conversion that stops early is continued from where it stopped. If conversion fails, or for
  AC-4 and other codecs it cannot convert, the video plays without sound and a dismissible notice
  names the codec, and any other track that would play, once Chromium's decoder counters
  (`webkitAudioDecodedByteCount` stays 0 while `webkitVideoDecodedByteCount` grows) confirm that no
  audio is being decoded. If the header cannot be read, three seconds of such silent playback show a
  general notice. Without the counters, the probe alone decides. See [ADR
  0007](adr/0007-dolby-and-dts-audio.md).

## Subtitles in the built-in player (today)

- **Where they come from:** subtitle files in the torrent (`.srt`, `.vtt`, `.ass`, `.ssa`),
  matched to the video by name or a `Subs`/`Subtitles` folder (see `stream_subtitles` in the
  [IPC contract](ipc.md)), and files the user loads ("Load subtitle file…" in the CC menu).
  The backend downloads a torrent's file at background priority only when it is chosen, then
  parses it in Rust into plain-text cues (`torrentplayer-media`). Nothing is parsed in the webview.
- **Showing them** (`ui/src/lib/subtitles.ts`): each chosen file becomes a text track
  (`video.addTextTrack('subtitles', label, language)` with `VTTCue`s, text escaped), so they
  follow seeks, need no URL, and leave the CSP unchanged. Chromium scales the text with the
  video, so it grows in fullscreen. The player styles it on a dark background and lifts it
  above the controls while they show.
- **Choosing:** a CC button in the controls (dimmed, offering only "Load subtitle file…", when the
  torrent has no subtitles). Its menu
  lists Off, the video's own subtitles, files loaded from disk, other subtitle files in the
  torrent, and embedded tracks (listed but not selectable: Chromium does not render in-band text
  tracks from `<video src>`, and picture formats such as PGS and VobSub would need libmpv). C
  turns subtitles off and back on. The choice (a language, or off) is remembered for the
  session and applied to the next video, including the queue's. With
  `playback.subtitlesByDefault` on, a video starts with its subtitles in the first of
  `navigator.languages` that it has.

## Safety rules for the FFI layer

`torrentplayer-player` is the only crate allowed to use `unsafe`:

- Unsafe code lives in one `ffi` module behind a safe API.
- Every `unsafe` block has a `// SAFETY:` comment explaining why its invariants hold (enforced by
  `clippy::undocumented_unsafe_blocks`).
- Callbacks invoked by C (especially `stream_cb`) wrap their bodies in
  `std::panic::catch_unwind` and convert panics into error codes, because unwinding into C is
  undefined behavior. The release profile keeps `panic = "unwind"` so this works.
- The crate is tested under AddressSanitizer in CI, and under Miri where the code does not call
  into C.

## Licensing

A playback-only LGPL build of FFmpeg ships with the app as a separate program
(`torrentplayer-ffmpeg`, built by `scripts/build-ffmpeg.sh`); libmpv will be bundled in Phase 2.
See [ADR 0001](adr/0001-project-license.md) for how their licenses interact with TorrentPlayer's.
