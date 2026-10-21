// SPDX-License-Identifier: MIT OR Apache-2.0

/**
 * The network indicator's reading of the backend's network status: a tone and a short label for
 * the footer and the player's status line, the lines of its tooltip, and the subscription that
 * keeps it current (`network_status` once, then every `network-status` event).
 */

import { listen } from '@tauri-apps/api/event';
import type { IncomingPeersStatus } from './bindings/IncomingPeersStatus';
import type { NetworkModeKind } from './bindings/NetworkModeKind';
import {
  NETWORK_STATUS_EVENT,
  PUBLIC_IP_EVENT,
  networkStatus,
  publicIp,
  type NetworkStatus,
  type PublicIp,
} from './ipc';

/** Gray (direct), green (protected), yellow (checking), or red (paused or failed). */
export type NetworkTone = 'direct' | 'protected' | 'pending' | 'blocked';

export interface NetworkSummary {
  tone: NetworkTone;
  /** For example `Protected · 10.64.0.1` or `Kill switch: traffic paused (…)`. */
  label: string;
  /** Lines for the tooltip: mode, endpoint, uptime, the latest events. */
  details: string[];
}

export const MODE_NAMES: Record<NetworkModeKind, string> = {
  direct: 'Direct (no VPN or proxy)',
  bindInterface: 'VPN (bound adapter)',
  socks5: 'SOCKS5 proxy',
  wireGuard: 'WireGuard',
};

/** Events shown in the tooltip. */
const TOOLTIP_EVENTS = 3;

/** `10.64.0.1:1080` → `10.64.0.1`, `[::1]:1080` → `::1`; anything else unchanged. */
export function hostOf(endpoint: string): string {
  const bracketed = /^\[([^\]]+)\]:\d+$/.exec(endpoint);
  if (bracketed?.[1] !== undefined) {
    return bracketed[1];
  }
  const plain = /^([^:]+):\d+$/.exec(endpoint);
  return plain?.[1] ?? endpoint;
}

/** `45 s` → `under a minute`, then `12 min`, `2 h 5 min`, `3 d 4 h`. */
export function formatUptime(ms: number): string {
  const minutes = Math.floor(Math.max(0, ms) / 60_000);
  if (minutes < 1) {
    return 'under a minute';
  }
  if (minutes < 60) {
    return `${String(minutes)} min`;
  }
  const hours = Math.floor(minutes / 60);
  if (hours < 24) {
    return `${String(hours)} h ${String(minutes % 60)} min`;
  }
  return `${String(Math.floor(hours / 24))} d ${String(hours % 24)} h`;
}

function clock(ms: number): string {
  return new Date(ms).toLocaleTimeString(undefined, { hour: '2-digit', minute: '2-digit' });
}

/** Where protected traffic goes, for the label: the proxy's host or the interface. */
function protectedVia(status: NetworkStatus): string {
  if (status.endpoint === null) {
    return MODE_NAMES[status.mode];
  }
  return status.mode === 'socks5' ? hostOf(status.endpoint) : status.endpoint;
}

/** What the indicator shows for `status` at time `nowMs`. */
export function describeNetwork(status: NetworkStatus, nowMs: number): NetworkSummary {
  const state = status.state;
  let tone: NetworkTone;
  let label: string;
  switch (state.kind) {
    case 'direct':
      tone = 'direct';
      label = 'Direct · no VPN';
      break;
    case 'connected':
      tone = 'protected';
      label = `${status.mode === 'bindInterface' ? 'Bound' : 'Protected'} · ${protectedVia(status)}`;
      break;
    case 'connecting':
      tone = 'pending';
      label = 'Connecting…';
      break;
    case 'reconnecting':
      tone = 'pending';
      label = 'Reconnecting…';
      break;
    case 'killSwitchActive':
      tone = 'blocked';
      label = `Kill switch: traffic paused (${state.reason})`;
      break;
    case 'error':
      tone = 'blocked';
      label = `Network error: ${state.reason}`;
      break;
    case 'disconnected':
      tone = 'blocked';
      label = 'Network stopped · traffic paused';
      break;
  }

  const details = [`Mode: ${MODE_NAMES[status.mode]}`];
  if (status.endpoint !== null) {
    details.push(
      `${status.mode === 'bindInterface' ? 'Interface' : 'Endpoint'}: ${status.endpoint}`,
    );
  }
  if (status.bound !== null) {
    details.push(`In use: ${status.bound}`);
  }
  if (status.connectedSinceMs !== null) {
    details.push(`Connected for ${formatUptime(nowMs - status.connectedSinceMs)}`);
  }
  if (status.killSwitchEngaged) {
    details.push('Kill switch: torrent traffic is paused until the connection works.');
  } else if (status.mode !== 'direct' && !status.udpAllowed) {
    details.push('DHT and UDP trackers are off in this mode.');
  }
  const recent = status.events.slice(-TOOLTIP_EVENTS).reverse();
  if (recent.length > 0) {
    details.push('', ...recent.map((e) => `${clock(e.atMs)}  ${e.message}`));
  }
  return { tone, label, details };
}

/** Whether torrent traffic is paused because a protected mode is not connected. */
export function trafficPaused(status: NetworkStatus | null): boolean {
  return status !== null && status.killSwitchEngaged;
}

/**
 * What the player says instead of "Buffering…" when the kill switch is the reason, or null
 * when traffic is not paused.
 */
export function pausedMessage(status: NetworkStatus | null): string | null {
  if (status === null || !trafficPaused(status)) {
    return null;
  }
  return status.state.kind === 'connecting'
    ? 'Connecting to the VPN or proxy…'
    : 'VPN/proxy disconnected — traffic paused';
}

/**
 * Calls `onStatus` with the current network status and then with every change, until the
 * returned function is called.
 */
export function followNetwork(onStatus: (status: NetworkStatus) => void): () => void {
  let stopped = false;
  let heard = false;
  let unlisten: (() => void) | null = null;
  listen<NetworkStatus>(NETWORK_STATUS_EVENT, (event) => {
    if (!stopped) {
      heard = true;
      onStatus(event.payload);
    }
  })
    .then((stop) => {
      if (stopped) {
        stop();
      } else {
        unlisten = stop;
      }
    })
    .catch(() => undefined);
  networkStatus()
    .then((status) => {
      // An event that arrived first is newer.
      if (!stopped && !heard) {
        onStatus(status);
      }
    })
    .catch(() => undefined);
  return () => {
    stopped = true;
    unlisten?.();
  };
}

/** An address with every digit hidden, keeping its shape: `•••.•••.•••.•••`, `••••:••••:…`. */
export function maskIp(address: string): string {
  if (address.includes(':')) {
    return '••••:••••:…';
  }
  return address
    .split('.')
    .map(() => '•••')
    .join('.');
}

/**
 * The public addresses for the status bar: `short` (the IPv4 address, or the IPv6 one without
 * IPv4) and tooltip lines with both and when they were checked. With `hidden`, the addresses
 * are masked (for screen sharing and demos).
 */
export function describePublicIp(
  ip: PublicIp,
  nowMs: number,
  hidden = false,
): { short: string; details: string[] } {
  const ago = Math.max(0, Math.round((nowMs - ip.checkedAtMs) / 60_000));
  const show = (address: string | null): string =>
    address === null ? 'none' : hidden ? maskIp(address) : address;
  return {
    short: show(ip.ipv4 ?? ip.ipv6 ?? ip.ip),
    details: [
      `Public IPv4: ${show(ip.ipv4)}`,
      `Public IPv6: ${show(ip.ipv6)}`,
      `Checked ${ago === 0 ? 'just now' : `${String(ago)} min ago`} via ${ip.service}`,
    ],
  };
}

/**
 * Follows the status bar's public addresses (null while unknown, blocked, or turned off).
 * Returns a function that stops following.
 */
export function followPublicIp(onIp: (ip: PublicIp | null) => void): () => void {
  let stopped = false;
  let heard = false;
  let unlisten: (() => void) | null = null;
  listen<PublicIp | null>(PUBLIC_IP_EVENT, (event) => {
    if (!stopped) {
      heard = true;
      onIp(event.payload);
    }
  })
    .then((stop) => {
      if (stopped) {
        stop();
      } else {
        unlisten = stop;
      }
    })
    .catch(() => undefined);
  publicIp()
    .then((ip) => {
      // An event that arrived first is newer.
      if (!stopped && !heard) {
        onIp(ip);
      }
    })
    .catch(() => undefined);
  return () => {
    stopped = true;
    unlisten?.();
  };
}

/**
 * What the incoming-peers setting does in each mode: where TorrentPlayer listens, and what the user
 * may need to do for peers to reach it.
 */
export const INCOMING_HELP: Record<NetworkModeKind, string> = {
  direct:
    'Listens on every network interface, so your firewall may ask to allow TorrentPlayer. Behind a router, forward the port or turn on UPnP.',
  bindInterface:
    'Listens only on the VPN adapter, never on your other networks. Peers reach it only if your VPN provider forwards this port to you.',
  socks5:
    'Not available with a SOCKS5 proxy: it cannot accept incoming connections, so TorrentPlayer does not listen at all in this mode.',
  wireGuard:
    'Listens only inside the WireGuard tunnel, never on your own networks. Peers reach it only if your VPN provider forwards this port to your tunnel address.',
};

/** Whether every address is a wildcard (`0.0.0.0:6881`, `[::]:6881`). */
function everyInterface(addresses: string[]): boolean {
  return addresses.every((a) => a.startsWith('0.0.0.0:') || a.startsWith('[::]:'));
}

/** One line for the incoming-connection listener's state, for the status view. */
export function describeIncoming(status: IncomingPeersStatus): string {
  switch (status.kind) {
    case 'off':
      return 'Off';
    case 'idle':
      return `Not listening: ${status.reason}`;
    case 'paused':
      return 'Closed while traffic is paused';
    case 'unsupported':
      return `Not available: ${status.reason}`;
    case 'failed':
      return `Could not open the port: ${status.reason}`;
    case 'listening': {
      const where = everyInterface(status.addresses)
        ? 'all interfaces'
        : status.addresses.join(', ');
      return `Listening on port ${String(status.port)} (${where})${status.upnp ? ' · UPnP requested' : ''}`;
    }
  }
}

/** Guidance for local virtual switches and mesh VPNs that do not imply internet routing. */
export function interfaceAdvice(name: string): string | null {
  if (/^vEthernet \((?:Default Switch\)|WSL(?:[ (]|\)))/i.test(name)) {
    return 'This is a local Hyper-V virtual network, not a VPN tunnel. It normally has no internet route for TorrentPlayer. Select your VPN app’s tunnel adapter instead.';
  }
  if (/tailscale/i.test(name)) {
    return 'For internet access through Tailscale, select an exit node in Tailscale first. A normal mesh connection only reaches your Tailscale devices.';
  }
  return null;
}
