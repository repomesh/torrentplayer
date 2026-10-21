# Security Policy

TorrentPlayer handles untrusted data from the internet (torrent metadata, peer traffic, media files,
subtitles) and protects users' network privacy (VPN, WireGuard, and proxy modes with a kill
switch). We take security reports seriously and appreciate responsible disclosure.

## Supported versions

TorrentPlayer has not had a release yet. Once it does, security fixes will be made for the latest
minor release only.

| Version | Supported |
| ------- | --------- |
| `main`  | ✅        |

## Reporting a vulnerability

**Please do not report security vulnerabilities through public GitHub issues, discussions, pull
requests, or the Discord server.**

Report them privately through
[GitHub private vulnerability reporting](https://github.com/torrentplayer/torrentplayer/security/advisories/new).
If you cannot use GitHub, email the security contact listed in [MAINTAINERS.md](MAINTAINERS.md).

Please include as much of the following as you can:

- The type of issue (for example path traversal, IPC permission bypass, traffic leak outside the
  tunnel, memory safety issue in the libmpv wrapper).
- The affected component and version or commit.
- Step-by-step reproduction instructions, and a proof of concept if you have one.
- The impact, as you understand it.

## What to expect

- We acknowledge your report within **3 business days**.
- We send an initial assessment within **10 business days**.
- We aim to release a fix within **90 days** of the report, and sooner for critical issues.
- We coordinate disclosure with you, publish a GitHub Security Advisory, and request a CVE when
  appropriate. We credit you in the advisory unless you prefer to remain anonymous.

## Scope

Examples of issues we especially want to hear about:

- Anything that lets a torrent, peer, media file, or subtitle write outside the download
  directory, execute code, or crash the app.
- Traffic (peer connections, trackers, DHT, DNS, IPv6) leaking outside the selected VPN, tunnel,
  or proxy, or the kill switch failing open.
- Other local programs or websites gaining access to the local streaming server or to IPC
  commands.
- Exposure of WireGuard keys, proxy credentials, or other secrets.
- Supply chain issues in our build or release pipeline.

Out of scope: vulnerabilities in third-party dependencies that are already publicly known (please
report those upstream; we track them with `cargo deny` and Dependabot), and issues that require
an attacker who already has full control of the user's account.

## Security practices

How TorrentPlayer is designed to resist these threats is documented in the
[threat model](../docs/threat-model.md).
