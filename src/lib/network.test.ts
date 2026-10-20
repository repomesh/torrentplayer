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
  trafficPaused,
  interfaceAdvice,
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
    bound: null,
    lastCheckMs: null,
    connectedSinceMs: null,
    events: [],
    ...overrides,
  };
}

const socks = {
  mode: 'socks5',
  endpoint: '10.64.0.1:1080',
  udpAllowed: false,
} satisfies Partial<NetworkStatus>;

beforeEach(() => {
  invoke.mockReset();
  listen.mockReset();
});

describe('describeNetwork', () => {
  it('shows direct mode in gray', () => {
    const s = describeNetwork(status(), 0);
    expect(s).toMatchObject({ tone: 'direct', label: 'Direct · no VPN' });
    expect(s.details[0]).toBe('Mode: Direct (no VPN or proxy)');
  });

  it('names the proxy host or interface when protected', () => {
    const now = 3 * 3_600_000;
    const s = describeNetwork(
      status({ ...socks, state: { kind: 'connected' }, connectedSinceMs: now - 125 * 60_000 }),
      now,
    );
    expect(s).toMatchObject({ tone: 'protected', label: 'Protected · 10.64.0.1' });
    expect(s.details).toContain('Endpoint: 10.64.0.1:1080');
    expect(s.details).toContain('Connected for 2 h 5 min');
    expect(s.details).toContain('DHT and UDP trackers are off in this mode.');

    const bound = describeNetwork(
      status({
        mode: 'bindInterface',
        endpoint: 'wg0',
        bound: 'wg0 (10.2.0.2)',
        state: { kind: 'connected' },
        udpAllowed: false,
      }),
      0,
    );
    expect(bound.label).toBe('Bound · wg0');
    expect(bound.details).toContain('Interface: wg0');
    expect(bound.details).toContain('In use: wg0 (10.2.0.2)');
  });

  it('is yellow while checking and red while traffic is paused', () => {
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
      label: 'Kill switch: traffic paused (the proxy is unreachable)',
    });
    expect(blocked.details.some((line) => line.includes('torrent traffic is paused'))).toBe(true);
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

  it('report paused traffic only when the kill switch is engaged', () => {
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
    ).toBe('VPN/proxy disconnected — traffic paused');
    expect(
      pausedMessage(status({ ...socks, killSwitchEngaged: true, state: { kind: 'connecting' } })),
    ).toBe('Connecting to the VPN or proxy…');
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

describe('VPN adapter guidance', () => {
  it('explains local Hyper-V switches and Tailscale exit nodes', () => {
    expect(interfaceAdvice('vEthernet (Default Switch)')).toContain('local Hyper-V');
    expect(interfaceAdvice('vEthernet (WSL (Hyper-V firewall))')).toContain('local Hyper-V');
    expect(interfaceAdvice('Tailscale')).toContain('exit node');
    expect(interfaceAdvice('vEthernet (External Switch)')).toBeNull();
    expect(interfaceAdvice('ProtonVPN')).toBeNull();
  });
});
