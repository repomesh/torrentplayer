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

Tauri embeds the built frontend (`ui/dist/`) at compile time. Run `npm ci && npm run build`
in `ui/` before building `torrentplayer-app` with `cargo`.

### Linux: `webkit2gtk-4.1` not found

Install the system packages listed in the [README](../README.md#building-from-source).

## Running

### Enable debug logging

```sh
RUST_LOG=debug npm run tauri dev   # in ui/
```

### Linux: no sound, `autoaudiosink not found`

On Linux the web engine (WebKitGTK) plays audio and video through GStreamer. If the log says
`GStreamer element autoaudiosink not found`, or audio and video play silently, the GStreamer
plugins are missing. The `.deb` and `.rpm` packages depend on them since 0.3.1 and the AppImage
bundles them; for an older version or a build from source, install them:

```sh
sudo apt install gstreamer1.0-plugins-base gstreamer1.0-plugins-good gstreamer1.0-libav   # Debian, Ubuntu
sudo dnf install gstreamer1-plugins-base gstreamer1-plugins-good gstreamer1-plugin-libav  # Fedora
```

`gstreamer1.0-libav` decodes H.264 and AAC, which most videos use.

### A video plays without sound

- **Dolby or DTS audio:** the web engine cannot decode AC-3, E-AC-3 (DD+, Atmos), `TrueHD`, or
  DTS, so since 0.3.0 the bundled FFmpeg converts that audio to stereo AAC as the video plays
  (search says "audio: converted to stereo" for such releases). If a notice still names the
  codec, the build has no bundled FFmpeg (local development builds, by default), or the file is
  one the conversion does not read (AVI, MPEG-TS, AC-4 audio): pick another release; without
  FFmpeg, search marks these as "no sound in built-in player" and prefers AAC or Opus audio. See
  [ADR 0007](adr/0007-dolby-and-dts-audio.md).
- **"Muted — click to unmute" is shown:** the webview refused autoplay with sound. Click the
  button, or press M.
- **Neither:** check the volume control and the system mixer. The log line `audio probed` (at the
  default `info` level) names the stream's codec and track count.

### The queue says downloads are blocked

In the WireGuard and SOCKS5 modes the kill switch holds all traffic while the tunnel or proxy is
down, and downloads continue by themselves when it is back; Settings → Network & VPN shows why it
is blocked. In Direct mode, traffic is held when `settings.json` could not be read ("Your network
settings could not be read…"): choose your VPN or proxy again, or select **Connect directly**. See
[Configuration](configuration.md#where-settings-are-stored).

### A queued video says "Waiting for playback"

The video you are watching needs the bandwidth, so background downloads wait for it to get ahead.
They continue on their own; your own pauses are never changed.

### A video does not start: "No one is sharing this file right now"

No peer has connected for 30 seconds, so nobody seems to be sharing it. Pick another search result
with more seeders, or keep waiting. Buffering otherwise shows how many peers were found and how
fast the video downloads.

### "May not play here (format: …)"

The system web engine probably cannot play that container or codec (for example AVI or Xvid).
Pick another release; a libmpv player for these is planned.
