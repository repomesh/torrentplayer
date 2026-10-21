// SPDX-License-Identifier: MIT OR Apache-2.0

import { beforeEach, describe, expect, it, vi } from 'vitest';

const invoke = vi.fn();
vi.mock('@tauri-apps/api/core', () => ({ invoke }));
vi.mock('@tauri-apps/api/event', () => ({ listen: vi.fn() }));

const {
  REMOVED_MODE_HELD,
  REMOVED_MODE_NOTICE,
  dismissRemovedModeNotice,
  removedModeHolds,
  showRemovedModeNotice,
} = await import('./removedMode');

beforeEach(() => {
  invoke.mockReset();
});

describe('the removed VPN adapter mode notice', () => {
  it('says what changed and what to use instead', () => {
    expect(REMOVED_MODE_NOTICE).toContain('VPN adapter mode was removed in 0.3.0');
    expect(REMOVED_MODE_NOTICE).toContain('keep your VPN app connected');
    expect(REMOVED_MODE_NOTICE).toContain('built-in WireGuard mode');
    expect(REMOVED_MODE_NOTICE).toContain('kill switch');
    expect(REMOVED_MODE_HELD).toMatch(/paused until you dismiss/);
  });

  it('is shown until dismissed, and holds traffic only in Direct mode', () => {
    expect(showRemovedModeNotice({ vpnAdapterModeRemoved: true })).toBe(true);
    expect(showRemovedModeNotice({ vpnAdapterModeRemoved: false })).toBe(false);
    expect(removedModeHolds({ mode: 'direct', vpnAdapterModeRemoved: true })).toBe(true);
    expect(removedModeHolds({ mode: 'direct', vpnAdapterModeRemoved: false })).toBe(false);
    for (const mode of ['socks5', 'wireGuard'] as const) {
      expect(removedModeHolds({ mode, vpnAdapterModeRemoved: true })).toBe(false);
    }
  });

  it('saves the dismissal', async () => {
    invoke.mockResolvedValue({ settings: {} });
    await dismissRemovedModeNotice();
    expect(invoke).toHaveBeenCalledWith('update_settings', {
      patch: { network: { vpnAdapterModeRemoved: false } },
    });
  });

  it('rejects when the dismissal cannot be saved', async () => {
    invoke.mockRejectedValue({ kind: 'internal', message: 'disk full' });
    await expect(dismissRemovedModeNotice()).rejects.toBeDefined();
  });
});
