# 0008: Application-local WireGuard tunnel

- **Status:** Proposed (implemented); amended by [0009](0009-remove-vpn-adapter-mode.md)
- **Date:** 2026-10-18

> **Amended in 0.3.0:** the separate VPN choice (the bound-interface mode) described below was
> removed by [ADR 0009](0009-remove-vpn-adapter-mode.md). Users of VPN apps, OpenVPN-based ones
> included, now connect them at the system level and choose Direct, or use this WireGuard mode.

## Context

TorrentPlayer could import WireGuard files but could not connect them. Users also confused local
Hyper-V switches with VPN adapters. Protected traffic must never fall back to the host's
internet connection, and importing a configuration must not require administrator rights.

## Decision

Expose separate VPN and WireGuard choices. VPN uses the existing bound-interface mode for
connections established by provider apps, including OpenVPN. WireGuard runs inside TorrentPlayer,
using boringtun for the protocol and smoltcp for the TCP/IP stack. Configured DNS queries
also travel inside the tunnel. Endpoint bootstrap resolution uses the system resolver.
Librqbit's HTTP tracker client still resolves tracker names locally before using its SOCKS5
bridge, an existing limitation shared with SOCKS5 mode; search and torrentplayer-swarm use tunneled DNS.

Use a vendored copy of tokio-wireguard 0.1.3 (MIT OR Apache-2.0) to supply Tokio sockets.
The upstream version lacks preshared keys and handshake reporting. Local changes add both,
start handshakes, update boringtun to 0.7.1 (BSD-3-Clause), and fix socket registration,
TCP handshake readiness, write wakeups, polling fairness, and safe waker construction.
The source and upstream MIT copyright are preserved under `vendor/tokio-wireguard`.
Maintaining these changes is TorrentPlayer's responsibility; they are covered by local peer tests.
Boringtun's routing tables use BSD-2-Clause, a permissive license added to `deny.toml`.

The small dns-parser dependency (MIT OR Apache-2.0) parses DNS wire responses without adding
a system resolver or TLS runtime. It is an older, stable protocol parser; TorrentPlayer validates
response size, source, transaction ID, and response flags. Query encoding is bounded and
does not use its panic-prone query builder.

Existing APIs return Tokio TCP streams. A private loopback socket pair relays each stream
into WireGuard; its accepting socket checks the exact reserved source address and port.
Applying new settings or shutting down closes the tunnel and revokes connection leases.
Traffic remains blocked until an authenticated handshake succeeds and whenever its age
reaches 180 seconds. No system routes, VPN services, or network adapters are created.

## Alternatives considered

- OS WireGuard services require privileged adapter management and system route changes.
- onetun provides fixed port forwards instead of the arbitrary destinations TorrentPlayer needs.
- Implementing the protocol or TCP stack locally would duplicate established libraries.
- Importing OpenVPN files needs a separate runtime and lifecycle; the VPN option instead
  supports existing OpenVPN app connections through their adapter.

## Consequences

WireGuard configuration files now establish real application-local connections, including
search and peer traffic. Wrong keys or missing credentials fail closed. One peer and one
address per family are supported. Configured DNS servers are required for host names.
UDP tracker announces and their DNS lookups run inside the tunnel and bootstrap magnet metadata
fetching. The follow-up for issue #19 also routes DHT and bootstrap DNS through scoped tunnel
sockets. uTP remains disabled in this mode. Incoming peers (issue #21), when turned on, are
accepted only on the tunnel address inside the userspace stack and relayed like outgoing
connections. Throughput of the
userspace stack remains a performance consideration for large numbers of peers.
