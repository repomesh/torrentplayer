# Release process

Releases are built only by the [release workflow](../.github/workflows/release.yml), never on a
developer machine.

For testing, the [build workflow](../.github/workflows/build.yml) builds unsigned installers for
Windows (x64), macOS (Apple silicon and Intel, macOS 10.15 or later), and Linux (x64: AppImage,
`.deb`, `.rpm`) on every push to `main`, on pull requests, and on demand (**Actions → Build → Run
workflow**). Each platform's installers are kept as a workflow artifact (`installers-<platform>`)
for 14 days. They are for testing, not distribution.

## Cutting a release

1. Make sure `main` is green.
   If the release collects work on a `release/x.y.z` branch, open its PR as a draft as soon as the
   branch exists: CI (including the sign-off check) runs only on pull requests and `main`, so a
   branch without a PR can gather failures nobody sees.
2. Move the `## [Unreleased]` entries in `CHANGELOG.md` under a new `## [x.y.z] - YYYY-MM-DD`
   heading, following [Semantic Versioning](https://semver.org).
3. Bump `version` in the root `Cargo.toml` (`[workspace.package]`) and `ui/package.json`. The
   bump also changes `Cargo.lock` (every workspace crate's entry) and `ui/package-lock.json` (its
   two top-level entries); commit both. CI builds with `--locked`, so a stale `Cargo.lock` fails
   it. The app's version comes from `Cargo.toml`; `src-tauri/tauri.conf.json` sets none.
4. Open a PR titled `chore(release): vx.y.z`, get it reviewed, and merge it.
5. Tag the merge commit and push the tag:
   ```sh
   git tag -s vx.y.z -m "TorrentPlayer vx.y.z"
   git push origin vx.y.z
   ```
6. The workflow runs for tags matching `v*.*.*`. It first checks that the tag matches the version
   in `Cargo.toml` and `ui/package.json` and stops if not. It then creates one **draft** GitHub
   release for the tag (a tag with a `-` suffix, such as `v1.0.0-rc.1`, becomes a pre-release),
   builds installers for Windows, macOS, and Linux (each build job times out after 120 minutes),
   signing them when the signing secrets are configured, and uploads them to the draft with
   `SHA256SUMS.txt`, a CycloneDX SBOM, the FFmpeg source, and build provenance attestations.
   Every file on the release, the macOS `.app.tar.gz` archives included, is listed in
   `SHA256SUMS.txt` and attested. The build jobs cannot change the release; only the first and
   last jobs can. If a run fails, re-run it: it reuses the draft and replaces its files. It stops
   if the tag's release is already published.
7. Review the draft: download and smoke-test at least one installer per OS. When the builds are
   signed, check that the FFmpeg sidecar is signed too, not only the main executable: on Windows
   `signtool verify /pa` on `torrentplayer-ffmpeg*.exe` in the installed app's folder; on macOS
   `codesign -dv --verbose=4` and `spctl --assess --type execute -v` on `TorrentPlayer.app`. Then
   paste the changelog section into the notes and publish.

The SBOM lists the Rust crates of the app (`cargo cyclonedx` on `src-tauri`) only. FFmpeg and
the npm packages bundled into the webview are listed in
[`THIRD_PARTY_NOTICES.md`](../THIRD_PARTY_NOTICES.md). The tools the workflows install
(`cargo-cyclonedx`, `cargo-about`, `mdbook`, `zizmor`, `cargo-llvm-cov`) are pinned to exact
versions, and actions to commit SHAs.

## Signing secrets

Configure these in the `release` environment (**Settings → Environments → release**), and add
a maintainer there as a required reviewer, so no build can use the secrets without approval:

| Secret                                                                      | Purpose                      |
| --------------------------------------------------------------------------- | ---------------------------- |
| `APPLE_CERTIFICATE`, `APPLE_CERTIFICATE_PASSWORD`, `APPLE_SIGNING_IDENTITY` | macOS code signing           |
| `APPLE_ID`, `APPLE_PASSWORD`, `APPLE_TEAM_ID`                               | macOS notarization           |
| `WINDOWS_CERTIFICATE`, `WINDOWS_CERTIFICATE_PASSWORD`                       | Windows Authenticode signing |

None are configured yet, so releases (0.3.0 included) are unsigned: the workflow still builds
and publishes them, with a warning in the run for each platform. Users then see a SmartScreen
warning on Windows and a Gatekeeper prompt on macOS (open the app with right-click → Open, or
allow it in System Settings → Privacy & Security). Until code-signing certificates are
configured, the checksums and attestations below are how to verify a download.

## Verifying a download

```sh
# Check the checksum
sha256sum --check --ignore-missing SHA256SUMS.txt

# Check that the file was built by this repository's release workflow
gh attestation verify TorrentPlayer_x.y.z_x64-setup.exe --repo torrentplayer/torrentplayer
```
