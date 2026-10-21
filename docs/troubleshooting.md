# Troubleshooting

## Building

### `link: extra operand` errors on Windows

Git Bash's `/usr/bin/link` is shadowing the MSVC linker. Build from a "Developer PowerShell for
VS" prompt, or move the Visual Studio tools before `/usr/bin` on your `PATH`. See
[Building — Windows](building.md#windows).

### Test binaries exit with `STATUS_ENTRYPOINT_NOT_FOUND` (0xc0000139) on Windows

The Common Controls v6 manifest was not embedded. `src-tauri/build.rs` does this for every
target; make sure you have not replaced its `WindowsAttributes` settings, then run
`cargo clean -p torrentplayer-app` and rebuild.

### `frontendDist ... doesn't exist`

Tauri embeds the built frontend at compile time. Run `npm ci && npm run build` before building
`torrentplayer-app` with `cargo`.

### Linux: `webkit2gtk-4.1` not found

Install the system packages listed in the [README](../README.md#building-from-source).

## Running

### Enable debug logging

```sh
RUST_LOG=debug npm run tauri dev
```

### A video plays without sound

- **A notice names Dolby or DTS audio:** the built-in player cannot decode AC-3, E-AC-3 (DD+,
  Atmos), `TrueHD`, or DTS. Pick another release: search marks these as "no sound in built-in
  player" and prefers releases with AAC or Opus audio. The full player (Phase 2) will play them;
  see [ADR 0007](adr/0007-dolby-and-dts-audio.md).
- **"Muted — click to unmute" is shown:** the webview refused autoplay with sound. Click the
  button, or press M.
- **Neither:** check the volume control and the system mixer. The log line `audio probed` (at the
  default `info` level) names the stream's codec and track count.

Streaming, playback, and network issues will get their own sections here as those features
land.
