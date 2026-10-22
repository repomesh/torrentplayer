# Contributing to TorrentPlayer

Thanks for helping build TorrentPlayer! This guide explains how to propose changes and what reviewers
look for. By participating, you agree to follow the [Code of Conduct](CODE_OF_CONDUCT.md).

## Ways to contribute

- **Come talk** on the [TorrentPlayer Discord server](https://discord.gg/SNsKPAu7pT): ask questions, share ideas,
  and help other users. Decisions are still made in issues and pull requests, so anything worth
  keeping ends up there.
- **Report bugs or performance regressions** with the issue templates. For streaming problems,
  include the metrics from the debug overlay (press <kbd>D</kbd>) and, if possible, a link to
  the openly licensed torrent you were watching. `cargo bench-suite <list file>` benchmarks your
  own files on a local swarm and writes a report you can attach (see
  [Benchmark your own files](../docs/performance.md#benchmark-your-own-files)).
- **Suggest features** with the feature request template. Explain the problem before the solution.
- **Improve documentation.** Docs PRs are always welcome.
- **Write code.** For anything bigger than a small fix, open an issue first so we can agree on the
  approach. Significant design decisions are recorded as [ADRs](../docs/adr/).

Security vulnerabilities must be reported privately; see [SECURITY.md](SECURITY.md).

## Development setup

Follow [Building from source](../README.md#building-from-source) and
[docs/building.md](../docs/building.md). Run every CI check locally before opening a PR:

```sh
# Rust
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps
cargo deny check

# Frontend (in ui/; format:check covers the whole repository)
cd ui
npm run format:check
npm run lint
npm run check
npm test
```

## Pull request checklist

- [ ] The change is focused; unrelated refactors go in separate PRs.
- [ ] All the checks above pass.
- [ ] New behavior has tests. Parsers and prioritization logic also get property tests or fuzz
      targets where they handle untrusted input.
- [ ] Public Rust items have rustdoc comments, with `# Errors`, `# Panics`, and `# Safety`
      sections where they apply.
- [ ] User-facing changes are noted under `## [Unreleased]` in [CHANGELOG.md](../CHANGELOG.md).
- [ ] Docs in `docs/` are updated if behavior, configuration, or the IPC contract changed.
- [ ] Any new dependency is justified in the PR description (why it is needed, its license,
      and its maintenance status), and `cargo deny check` passes.
- [ ] Every commit is signed off (see below).

## Commit messages

We use [Conventional Commits](https://www.conventionalcommits.org):

```
feat(stream): prefetch the next lecture's metadata near the end of playback
fix(net): pause torrents before reconnecting the tunnel
docs(adr): record the stream_cb vs HTTP feed decision
```

Common types: `feat`, `fix`, `perf`, `docs`, `test`, `refactor`, `build`, `ci`, `chore`.
Use the crate name without the `torrentplayer-` prefix as the scope (`stream`, `net`, `player`, `ui`, ...).

## Developer Certificate of Origin

Every commit must be signed off to certify the
[Developer Certificate of Origin](https://developercertificate.org), confirming you have the
right to submit the work under the project's license. Add the sign-off with `git commit -s`,
which appends:

```
Signed-off-by: Your Name <your.email@example.com>
```

CI rejects pull requests that contain commits without a sign-off. To fix an existing branch, run
`git rebase --signoff main` and force-push.

## Code style

The rules are enforced by tooling, so in most cases you only need to run the formatters.

### Rust

- Formatting: `rustfmt` with the settings in [`rustfmt.toml`](../rustfmt.toml).
- Linting: Clippy with `pedantic` enabled and warnings denied. If you must allow a lint, put the
  `#[allow(...)]` on the smallest possible item with a comment explaining why.
- Follow the [Rust API Guidelines](https://rust-lang.github.io/api-guidelines/).
- **No panics on runtime data.** `unwrap`, `expect`, `panic!`, `todo!`, and `unimplemented!` are
  denied outside tests. Return typed errors (`thiserror`) from library crates and use `anyhow`
  only in the app shell.
- **No `println!`.** Use `tracing` with structured fields. Never log secrets.
- **No `unsafe`** except in `torrentplayer-player`, which wraps libmpv. There, every `unsafe` block needs a
  `// SAFETY:` comment, and adding new unsafe code needs maintainer sign-off.
- Async code uses Tokio. Do not block async threads (use `spawn_blocking`), do not hold locks
  across `.await`, and use bounded channels.
- Keep pure logic (sorting, parsing, prioritization) in crates without Tauri or FFI
  dependencies so it is easy to test.

### Frontend

- TypeScript in strict mode, formatted by Prettier and linted by ESLint.
- All backend calls go through `ui/src/lib/ipc.ts`. IPC payload types are generated from Rust by
  `ts-rs` into `ui/src/lib/bindings/` (run `cargo test -p torrentplayer-app` to regenerate) and must not be
  edited by hand.

### File headers

Every source file starts with an SPDX license identifier:

```rust
// SPDX-License-Identifier: MIT OR Apache-2.0
```

## Review and merging

- `main` is protected: PRs need passing CI and one approving review from a maintainer
  (two for changes to `unsafe` code, the network/kill-switch logic, or release workflows).
- Maintainers squash-merge, so the PR title must itself be a valid Conventional Commit.

## Licensing of contributions

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in
TorrentPlayer by you, as defined in the Apache-2.0 license, shall be dual licensed under MIT OR
Apache-2.0, without any additional terms or conditions.
