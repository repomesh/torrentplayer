# Building and development

## Prerequisites

Install the toolchains listed in the [README](../README.md#building-from-source). `rustup`
picks up the pinned toolchain from `rust-toolchain.toml` automatically.

## Everyday commands

The frontend lives in `ui/`, and every `npm` command runs there (from the repository root,
`npm --prefix ui run <script>` does the same). `npm run tauri` starts the Tauri CLI from the
repository root, so paths passed to it, such as `--config src-tauri/tauri.sidecar.conf.json`, are
relative to the root. `cargo` commands run from the root.

| Task                                       | Command                                  |
| ------------------------------------------ | ---------------------------------------- |
| Install frontend dependencies              | `npm ci` (in `ui/`)                      |
| Run the app with hot reload                | `npm run tauri dev` (in `ui/`)           |
| Build installers for your OS               | `npm run tauri build` (in `ui/`)         |
| Rust tests (also regenerates IPC bindings) | `cargo test --workspace`                 |
| Frontend tests                             | `npm test` (in `ui/`)                    |
| Open API docs                              | `cargo doc --workspace --no-deps --open` |
| Preview this guide                         | `mdbook serve docs`                      |

Note that `cargo` commands that build `torrentplayer-app` need the frontend built first
(`npm run build` in `ui/`), because Tauri embeds `ui/dist/` at compile time.

## Running the CI checks locally

CI runs these on Windows, macOS, and Linux. Run them before pushing:

```sh
(cd ui && npm ci && npm run build)

cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace
git diff --exit-code ui/src/lib/bindings   # IPC bindings must be committed
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps
cargo deny check                           # cargo install cargo-deny --locked

cd ui
npm run format:check   # the whole repository, including Markdown and YAML
npm run lint           # ui/ and scripts/
npm run check
npm test
```

## Repository layout

```text
crates/              Rust library crates, one per subsystem (see architecture.md)
src-tauri/           Tauri app shell (torrentplayer-app): commands, capabilities, bundling config
ui/                  Svelte + TypeScript frontend, with its Vite, ESLint, and Prettier configs
ui/src/lib/bindings/ TypeScript types generated from Rust by ts-rs (do not edit)
docs/                This guide (mdBook) and Architecture Decision Records
scripts/             FFmpeg sidecar build and benchmark helpers
vendor/              Patched copies of third-party crates
.cargo/              Cargo settings and the cargo-about config for THIRD_PARTY_NOTICES.md
.github/             CI, release, and security workflows; issue and PR templates; the
                     contributing, security, and governance policies
```

## Platform notes

### Windows

The MSVC toolchain (Visual Studio Build Tools with the "Desktop development with C++" workload)
is the supported setup and is what CI uses. If you build with the GNU toolchain instead, make sure
MinGW's `bin/` directory comes before Git Bash's `/usr/bin` on `PATH`, because Git Bash ships an
unrelated `link` command that shadows the MSVC linker.

Tauri needs a Common Controls v6 application manifest to start, including in test binaries.
`src-tauri/build.rs` embeds `windows-app-manifest.xml` into every linked target for this reason.

### Linux

Install the WebKitGTK packages from the README. CI checks and tests on Ubuntu 24.04; the Linux
builds and releases (AppImage, `.deb`) are made on Ubuntu 22.04, the oldest supported version, so
they run on older glibc versions.

### FFmpeg (audio conversion)

Optional for development. Without it, videos with Dolby or DTS audio play without sound and
show a notice, and search marks and ranks such releases as having no sound. `scripts/build-ffmpeg.sh <target triple> src-tauri/binaries` builds the
playback-only LGPL FFmpeg (on Linux, macOS, or Windows with MinGW; it cross-compiles for Windows
on Linux with mingw-w64). `npm run tauri build -- --config src-tauri/tauri.sidecar.conf.json`
(in `ui/`) then bundles it as CI does. Development builds also take its path from `TORRENTPLAYER_FFMPEG`.

To play a local file through the app, seed it with
`cargo run -p torrentplayer-bench --release -- seed <file> <file.torrent>`, which logs its address, and
start a development build with `TORRENTPLAYER_DEV_AUTOPLAY='<file.torrent>|0||<address>'`.

### libmpv

Not needed: the built-in player uses the system web engine. libmpv is planned for a later full
player (see [Player integration](player.md)); build instructions will be added when it lands.

### FFmpeg for durations and thumbnails

FFmpeg is optional at run time (see `advanced.ffmpegDir` in [Configuration](configuration.md)).
The tests in `crates/torrentplayer-media/tests/ffmpeg.rs` run `ffprobe` and `ffmpeg` against the
loopback stream server when they are on `PATH` or in `TORRENTPLAYER_FFMPEG_DIR`, and are skipped (with a
note) otherwise:

```sh
TORRENTPLAYER_FFMPEG_DIR=/path/to/ffmpeg/bin cargo test -p torrentplayer-media --test ffmpeg
```
