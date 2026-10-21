<!-- Thanks for contributing! The PR title must be a Conventional Commit, e.g. "fix(stream): ...". -->

## What and why

<!-- What does this change, and what problem does it solve? Link issues with "Fixes #123". -->

## How it was tested

<!-- Tests added, manual checks, and for streaming changes, metrics before/after. -->

## Checklist

- [ ] CI checks pass locally (see [CONTRIBUTING.md](../CONTRIBUTING.md#development-setup))
- [ ] Tests cover the new behavior
- [ ] Public Rust items are documented
- [ ] `CHANGELOG.md` updated under `[Unreleased]` (for user-facing changes)
- [ ] Docs updated (`docs/`, including `docs/ipc.md` for IPC changes)
- [ ] New dependencies are justified below, with their licenses
- [ ] No new `unsafe` outside `torrentplayer-player`; any `unsafe` has `// SAFETY:` comments
- [ ] All commits are signed off (`git commit -s`)

## New dependencies

<!-- Name, version, license, why it is needed, and why an existing dependency won't do. Delete if none. -->
