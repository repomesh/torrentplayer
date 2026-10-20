# Building and development

## Prerequisites

Install the toolchains listed in the [README](../README.md#building-from-source). `rustup`
picks up the pinned toolchain from `rust-toolchain.toml` automatically.

## Everyday commands

| Task                                       | Command                                  |
| ------------------------------------------ | ---------------------------------------- |
| Install frontend dependencies              | `npm ci`                                 |
| Run the app with hot reload                | `npm run tauri dev`                      |
| Build installers for your OS               | `npm run tauri build`                    |
| Rust tests (also regenerates IPC bindings) | `cargo test --workspace`                 |
| Frontend tests                             | `npm test`                               |
| Open API docs                              | `cargo doc --workspace --no-deps --open` |
| Preview this guide                         | `mdbook serve docs`                      |

Note that `cargo` commands that build `lumen-app` need the frontend built first (`npm run build`),
because Tauri embeds `dist/` at compile time.

## Running the CI checks locally

CI runs these on Windows, macOS, and Linux. Run them before pushing:

```sh
npm ci && npm run build

cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace
git diff --exit-code src/lib/bindings   # IPC bindings must be committed
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps
cargo deny check                        # cargo install cargo-deny --locked

npm run format:check
npm run lint
npm run check
npm test
```

## Repository layout

```text
crates/           Rust library crates, one per subsystem (see architecture.md)
src-tauri/        Tauri app shell (lumen-app): commands, capabilities, bundling config
src/              Svelte + TypeScript frontend
src/lib/bindings/ TypeScript types generated from Rust by ts-rs (do not edit)
docs/             This guide (mdBook) and Architecture Decision Records
.github/          CI, release, and security workflows; issue and PR templates
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

Install the WebKitGTK packages from the README. CI uses Ubuntu 24.04.

### libmpv and FFmpeg

Required from Phase 2. Instructions will be added here.
