# Release process

Releases are built only by the [release workflow](../.github/workflows/release.yml), never on a
developer machine.

For testing, the [build workflow](../.github/workflows/build.yml) builds unsigned installers for
Windows (x64), macOS (Apple silicon and Intel), and Linux (x64: AppImage, `.deb`, `.rpm`) on
every push to `main`, on pull requests, and on demand (**Actions → Build → Run workflow**). Each
platform's installers are kept as a workflow artifact (`installers-<platform>`) for 14 days.
Unsigned builds trigger SmartScreen on Windows and Gatekeeper on macOS; they are not for
distribution.

## Cutting a release

1. Make sure `main` is green.
2. Move the `## [Unreleased]` entries in `CHANGELOG.md` under a new `## [x.y.z] - YYYY-MM-DD`
   heading, following [Semantic Versioning](https://semver.org).
3. Bump `version` in the root `Cargo.toml` (`[workspace.package]`) and `ui/package.json`.
4. Open a PR titled `chore(release): vx.y.z`, get it reviewed, and merge it.
5. Tag the merge commit and push the tag:
   ```sh
   git tag -s vx.y.z -m "TorrentPlayer vx.y.z"
   git push origin vx.y.z
   ```
6. The workflow creates one **draft** GitHub release for the tag, builds and signs installers
   for Windows, macOS, and Linux, then uploads them to the draft with `SHA256SUMS.txt`, a
   CycloneDX SBOM, the FFmpeg source, and build provenance attestations. Every file on the
   release, the macOS `.app.tar.gz` archives included, is listed in `SHA256SUMS.txt` and
   attested. The build jobs cannot change the release; only the first and last jobs can. If a
   run fails, re-run it: it reuses the draft and replaces its files. It stops if the tag's
   release is already published.
7. Review the draft (download and smoke-test at least one installer per OS), paste the changelog
   section into the notes, and publish.

## Required secrets

Configure these in the `release` environment (**Settings → Environments → release**), and add
a maintainer there as a required reviewer, so no build can use the secrets without approval:

| Secret                                                                      | Purpose                      |
| --------------------------------------------------------------------------- | ---------------------------- |
| `APPLE_CERTIFICATE`, `APPLE_CERTIFICATE_PASSWORD`, `APPLE_SIGNING_IDENTITY` | macOS code signing           |
| `APPLE_ID`, `APPLE_PASSWORD`, `APPLE_TEAM_ID`                               | macOS notarization           |
| `WINDOWS_CERTIFICATE`, `WINDOWS_CERTIFICATE_PASSWORD`                       | Windows Authenticode signing |

Without them the workflow still builds, but the installers are unsigned. Unsigned builds must not
be published as releases.

## Verifying a download

```sh
# Check the checksum
sha256sum --check --ignore-missing SHA256SUMS.txt

# Check that the file was built by this repository's release workflow
gh attestation verify TorrentPlayer_x.y.z_x64-setup.exe --repo torrentplayer/torrentplayer
```
