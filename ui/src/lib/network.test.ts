// SPDX-License-Identifier: MIT OR Apache-2.0

import { beforeEach, describe, expect, it, vi } from 'vitest';
import type { NetworkStatus } from './bindings/NetworkStatus';

const invoke = vi.fn();
const listen = vi.fn();
vi.mock('@tauri-apps/api/core', () => ({ invoke }));
vi.mock('@tauri-apps/api/event', () => ({ listen }));

const {
  describeNetwork,
  describePublicIp,
  maskIp,
  followNetwork,
  formatUptime,
  hostOf,
  pausedMessage,
  queueBlockedNotice,
  trafficPaused,
  describeIncoming,
  INCOMING_HELP,
  MODE_NAMES,
} = await import('./network');

function status(overrides: Partial<NetworkStatus> = {}): NetworkStatus {
  return {
    mode: 'direct',
    state: { kind: 'direct' },
    endpoint: null,
    trafficAllowed: true,
    killSwitchEngaged: false,
    udpAllowed: true,
    trackersAllowed: true,
    lastCheckMs: null,
    connectedSinceMs: null,
    events: [],
    incoming: { kind: 'off' },
    ...overrides,
  };
}

const socks = {
  mode: 'socks5',
  endpoint: '10.64.0.1:1080',
  udpAllowed: true,
} satisfies Partial<NetworkStatus>;

beforeEach(() => {
  invoke.mockReset();
  listen.mockReset();
});

describe('describeIncoming', () => {
  it('describes each listener state', () => {
    expect(describeIncoming({ kind: 'off' })).toBe('Off');
    expect(
      describeIncoming({
        kind: 'listening',
        port: 51413,
        addresses: ['0.0.0.0:51413', '[::]:51413'],
        upnp: true,
      }),
    ).toBe('Listening on port 51413 (all interfaces) · UPnP requested');
    expect(
      describeIncoming({
        kind: 'listening',
        port: 51413,
        addresses: ['10.64.0.2:51413'],
        upnp: false,
      }),
    ).toBe('Listening on port 51413 (10.64.0.2:51413)');
    expect(describeIncoming({ kind: 'paused', reason: 'proxy down' })).toBe(
      'Closed while traffic is blocked',
    );
    expect(describeIncoming({ kind: 'unsupported', reason: 'no SOCKS5 listening' })).toBe(
      'Not available: no SOCKS5 listening',
    );
    expect(describeIncoming({ kind: 'failed', reason: 'port in use' })).toBe(
      'Could not open the port: port in use',
    );
    expect(describeIncoming({ kind: 'idle', reason: 'waiting' })).toBe('Not listening: waiting');
  });

  it('explains every mode, and that SOCKS5 never listens', () => {
    expect(INCOMING_HELP.socks5).toMatch(/does not listen/);
    expect(INCOMING_HELP.direct).toMatch(/firewall/);
    expect(INCOMING_HELP.wireGuard).toMatch(/only inside the WireGuard tunnel/);
  });
});

describe('describeNetwork', () => {
  it('shows direct mode in gray', () => {
    const s = describeNetwork(status(), 0);
    expect(s).toMatchObject({ tone: 'direct', label: 'Direct · no VPN' });
    expect(s.details[0]).toBe('Mode: Direct (no VPN or proxy)');
  });

  it('offers exactly Direct, SOCKS5, and WireGuard', () => {
    expect(Object.keys(MODE_NAMES)).toEqual(['direct', 'socks5', 'wireGuard']);
    expect(Object.keys(INCOMING_HELP)).toEqual(['direct', 'socks5', 'wireGuard']);
  });

  it('names the proxy host or WireGuard endpoint when protected', () => {
    const now = 3 * 3_600_000;
    const s = describeNetwork(
      status({ ...socks, state: { kind: 'connected' }, connectedSinceMs: now - 125 * 60_000 }),
      now,
    );
    expect(s).toMatchObject({ tone: 'protected', label: 'Protected · 10.64.0.1' });
    expect(s.details).toContain('Endpoint: 10.64.0.1:1080');
    expect(s.details).toContain('Connected for 2 h 5 min');
    expect(s.details).not.toContain('DHT and UDP trackers are off in this mode.');

    const wg = describeNetwork(
      status({
        mode: 'wireGuard',
        endpoint: 'vpn.example.com:51820',
        state: { kind: 'connected' },
      }),
      0,
    );
    expect(wg.label).toBe('Protected · vpn.example.com:51820');
    expect(wg.details).toContain('Endpoint: vpn.example.com:51820');
  });

  it('says traffic is held in Direct mode after the VPN adapter mode was removed', () => {
    const held = status({
      state: { kind: 'error', reason: 'the VPN adapter mode was removed' },
      trafficAllowed: false,
      killSwitchEngaged: true,
      udpAllowed: false,
    });
    expect(describeNetwork(held, 0)).toMatchObject({
      tone: 'blocked',
      label: 'Traffic blocked: the VPN adapter mode was removed',
    });
    expect(trafficPaused(held)).toBe(true);
    expect(pausedMessage(held)).toBe(
      'Traffic blocked — choose a connection in Settings → Network & VPN',
    );
  });

  it('is yellow while checking and red while traffic is blocked', () => {
    const paused = { ...socks, trafficAllowed: false, killSwitchEngaged: true };
    expect(describeNetwork(status({ ...paused, state: { kind: 'connecting' } }), 0)).toMatchObject({
      tone: 'pending',
      label: 'Connecting…',
    });
    expect(
      describeNetwork(status({ ...paused, state: { kind: 'reconnecting', reason: 'x' } }), 0).label,
    ).toBe('Reconnecting…');
    const blocked = describeNetwork(
      status({
        ...paused,
        state: { kind: 'killSwitchActive', reason: 'the proxy is unreachable' },
        events: [
          { atMs: 1, level: 'info', message: 'Connected: SOCKS5 proxy 10.64.0.1:1080' },
          { atMs: 2, level: 'warn', message: 'Connection lost' },
        ],
      }),
      0,
    );
    expect(blocked).toMatchObject({
      tone: 'blocked',
      label: 'Kill switch: traffic blocked (the proxy is unreachable)',
    });
    expect(blocked.details.some((line) => line.includes('torrent traffic is blocked'))).toBe(true);
    // Newest event first.
    const events = blocked.details.filter((line) => line.includes('Conn'));
    expect(events[0]).toMatch(/Connection lost$/);
    expect(
      describeNetwork(status({ ...paused, state: { kind: 'error', reason: 'no host' } }), 0).label,
    ).toBe('Network error: no host');
  });
});

describe('helpers', () => {
  it('strip ports from endpoints', () => {
    expect(hostOf('10.0.0.1:1080')).toBe('10.0.0.1');
    expect(hostOf('[::1]:1080')).toBe('::1');
    expect(hostOf('proxy.example.org:9050')).toBe('proxy.example.org');
    expect(hostOf('wg0')).toBe('wg0');
  });

  it('format uptime', () => {
    expect(formatUptime(45_000)).toBe('under a minute');
    expect(formatUptime(12 * 60_000)).toBe('12 min');
    expect(formatUptime(76 * 3_600_000)).toBe('3 d 4 h');
  });

  it('report blocked traffic only when the kill switch is engaged', () => {
    expect(trafficPaused(null)).toBe(false);
    expect(trafficPaused(status())).toBe(false);
    expect(trafficPaused(status({ ...socks, killSwitchEngaged: true }))).toBe(true);
    expect(pausedMessage(status())).toBeNull();
    expect(
      pausedMessage(
        status({
          ...socks,
          killSwitchEngaged: true,
          state: { kind: 'reconnecting', reason: 'the proxy is unreachable' },
        }),
      ),
    ).toBe('VPN/proxy disconnected — downloads blocked until it reconnects');
    expect(
      pausedMessage(status({ ...socks, killSwitchEngaged: true, state: { kind: 'connecting' } })),
    ).toBe('Connecting to the VPN or proxy…');
  });

  it('explains blocked downloads above the queue', () => {
    expect(queueBlockedNotice(null)).toBeNull();
    expect(queueBlockedNotice(status())).toBeNull();
    expect(queueBlockedNotice(status({ ...socks, killSwitchEngaged: true }))).toBe(
      'Downloads are blocked: the VPN or proxy is not connected. They continue by themselves when it reconnects.',
    );
    expect(queueBlockedNotice(status({ killSwitchEngaged: true }))).toMatch(
      /choose a connection in Network & VPN settings/,
    );
  });
});

describe('followNetwork', () => {
  it('reports the current status, then events, until stopped', async () => {
    let handler: ((event: { payload: NetworkStatus }) => void) | undefined;
    const unlisten = vi.fn();
    listen.mockImplementation((name: string, fn: typeof handler) => {
      expect(name).toBe('network-status');
      handler = fn;
      return Promise.resolve(unlisten);
    });
    invoke.mockResolvedValue(status());
    const seen: string[] = [];
    const stop = followNetwork((s) => seen.push(s.state.kind));
    await vi.waitFor(() => {
      expect(seen).toEqual(['direct']);
    });
    handler?.({ payload: status({ ...socks, state: { kind: 'connecting' } }) });
    expect(seen).toEqual(['direct', 'connecting']);
    stop();
    expect(unlisten).toHaveBeenCalled();
    handler?.({ payload: status() });
    expect(seen).toHaveLength(2);
  });
});

describe('public addresses', () => {
  const ip = {
    ip: '203.0.113.7',
    ipv4: '203.0.113.7',
    ipv6: '2001:db8::7',
    mode: 'wireGuard' as const,
    service: 'icanhazip.com',
    checkedAtMs: 1_000_000,
  };

  it('shows the IPv4 address and lists both with the time of the check', () => {
    const d = describePublicIp(ip, 1_000_000 + 3 * 60_000);
    expect(d.short).toBe('203.0.113.7');
    expect(d.details).toEqual([
      'Public IPv4: 203.0.113.7',
      'Public IPv6: 2001:db8::7',
      'Checked 3 min ago via icanhazip.com',
    ]);
  });

  it('can hide the addresses, keeping their shape', () => {
    expect(maskIp('203.0.113.7')).toBe('•••.•••.•••.•••');
    expect(maskIp('2001:db8::7')).toBe('••••:••••:…');
    const d = describePublicIp(ip, 1_000_000, true);
    expect(d.short).toBe('•••.•••.•••.•••');
    expect(d.details.slice(0, 2)).toEqual([
      'Public IPv4: •••.•••.•••.•••',
      'Public IPv6: ••••:••••:…',
    ]);
    expect(describePublicIp({ ...ip, ipv6: null }, 1_000_000, true).details[1]).toBe(
      'Public IPv6: none',
    );
  });

  it('falls back to IPv6 and says when an address is missing', () => {
    const d = describePublicIp({ ...ip, ip: '2001:db8::7', ipv4: null }, 1_000_000);
    expect(d.short).toBe('2001:db8::7');
    expect(d.details[0]).toBe('Public IPv4: none');
    expect(d.details[2]).toBe('Checked just now via icanhazip.com');
  });
});
