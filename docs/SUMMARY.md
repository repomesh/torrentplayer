# Summary

[Introduction](introduction.md)
[How TorrentPlayer compares](comparison.md)

# Design

- [Architecture overview](architecture.md)
- [Streaming and prioritization](streaming.md)
- [Player integration](player.md)
- [Network modes and kill switch](network.md)
- [Threat model](threat-model.md)

# Reference

- [IPC contract](ipc.md)
- [Configuration](configuration.md)

# Guides

- [Building and development](building.md)
- [Performance and benchmarking](performance.md)
- [Troubleshooting](troubleshooting.md)
- [Release process](release.md)

# Decisions

- [Architecture Decision Records](adr/README.md)
  - [0001: Project license](adr/0001-project-license.md)
  - [0002: Cargo workspace layout](adr/0002-workspace-layout.md)
  - [0003: Frontend framework](adr/0003-frontend-framework.md)
  - [0004: Generating IPC types with ts-rs](adr/0004-ipc-type-generation.md)
  - [0005: Prioritization on librqbit with anchor streams](adr/0005-prioritization-with-anchor-streams.md)
  - [0006: A streaming-first BitTorrent engine](adr/0006-streaming-engine.md)
  - [0007: Dolby and DTS audio in the built-in player](adr/0007-dolby-and-dts-audio.md)
  - [0008: Application-local WireGuard tunnel](adr/0008-application-wireguard-tunnel.md)
  - [0009: Remove the VPN adapter mode](adr/0009-remove-vpn-adapter-mode.md)
