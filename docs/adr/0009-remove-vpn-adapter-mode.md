# 0009: Remove the VPN adapter mode

- **Status:** Proposed (implemented)
- **Date:** 2026-10-21
- **Amends:** [0008](0008-application-wireguard-tunnel.md)

## Context

Until 0.3.0 TorrentPlayer had four connection modes: Direct, VPN (binding every socket to the
network interface of the user's VPN app), SOCKS5, and the built-in WireGuard tunnel. The VPN
mode needed a per-platform socket-pinning layer (`SO_BINDTODEVICE`, `IP_BOUND_IF`, the Windows
strong host model), an interface picker with VPN-name heuristics and warnings for Hyper-V
switches and Tailscale, an interface health check, bound UDP sockets for DHT and UDP trackers,
and a listener pinned to the adapter for incoming peers. Its health check only proved the
adapter was up with an address, not that it reached the internet, and DNS still went to the
system resolver. Users of a VPN app normally connect it at the system level, where the app's
own kill switch applies, and the built-in WireGuard mode covers providers that publish WireGuard
configurations with a kill switch inside TorrentPlayer.

## Decision

Remove the VPN adapter mode. The modes are Direct, SOCKS5, and WireGuard. Users of a VPN app
connect it and choose Direct, or import their provider's WireGuard configuration.

Saved settings that selected the mode (`network.mode = "bindInterface"`) must keep loading and
must not silently lose the user's protection:

- The settings migration (schema version 4) rewrites the mode to `direct`, drops
  `network.interface`, and sets `network.vpnAdapterModeRemoved`. The rewrite applies to every
  file whatever its version, so hand-edited files and test harnesses that still write the old
  value load too, and it is saved at once.
- While that flag is set and the mode is Direct, the app passes a hold to the network manager
  (`NetworkConfig::hold`): all traffic is blocked, as behind the kill switch, until the user
  acknowledges the change. A notice in Settings → Network and beside the status bar's network
  indicator explains the change; dismissing it clears the flag and lets Direct connect. Choosing
  SOCKS5 or WireGuard lifts the hold too, since those modes have their own kill switch.

Holding traffic was chosen over connecting directly at once: the user had chosen to send nothing
outside their VPN, and the app cannot tell whether a VPN app is connected now. A paused download
after an update is an inconvenience; a swarm seeing the user's real address is not reversible.

## Alternatives considered

- **Keep the mode.** It duplicates what VPN apps do at the system level, adds platform-specific
  socket code and an interface picker to maintain, and its checks could not prove the adapter
  reached the internet.
- **Migrate to Direct and connect immediately, with only a notice.** Simpler, but it could expose
  the user's address before they read the notice.
- **Refuse to start, or reset the network settings.** Loses the user's other network settings
  (proxy, WireGuard, incoming peers) or blocks the app for a recoverable situation.

## Consequences

- `torrentplayer-net` loses `bind.rs`, `Route::Bind`, the `list_interfaces` IPC command, the
  `if-addrs` dependency, and `NetworkStatus.bound`. Direct, SOCKS5, and WireGuard paths, the kill
  switch, and the generic listener for incoming peers in Direct mode are unchanged.
- Users who relied on the mode get a one-time notice and paused traffic until they confirm.
- In Direct mode TorrentPlayer cannot tell whether a VPN app is connected; the status bar's
  public-IP check and **Check connection** show the address peers see.
