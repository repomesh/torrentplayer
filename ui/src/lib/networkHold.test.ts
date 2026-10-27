// SPDX-License-Identifier: MIT OR Apache-2.0

import { beforeEach, describe, expect, it, vi } from 'vitest';

const invoke = vi.fn();
vi.mock('@tauri-apps/api/core', () => ({ invoke }));
vi.mock('@tauri-apps/api/event', () => ({ listen: vi.fn() }));

const {
  HOLD_BUTTON,
  REMOVED_MODE_HELD,
  REMOVED_MODE_NOTICE,
  UNREADABLE_HELD,
  UNREADABLE_NOTICE,
  dismissHoldNotice,
  holdApplies,
  holdNotice,
  showHoldNotice,
} = await import('./networkHold');

beforeEach(() => {
  invoke.mockReset();
});

describe('the removed VPN adapter mode notice', () => {
  it('says what changed and what to use instead', () => {
    expect(REMOVED_MODE_NOTICE).toContain('VPN adapter mode was removed in 0.3.0');
    expect(REMOVED_MODE_NOTICE).toContain('keep your VPN app connected');
    expect(REMOVED_MODE_NOTICE).toContain('built-in WireGuard mode');
    expect(REMOVED_MODE_NOTICE).toContain('kill switch');
    expect(REMOVED_MODE_HELD).toMatch(/blocked until you dismiss/);
    expect(holdNotice('vpnAdapterModeRemoved', true)).toEqual({
      text: REMOVED_MODE_NOTICE,
      held: REMOVED_MODE_HELD,
    });
    expect(holdNotice('vpnAdapterModeRemoved', false).held).toBeNull();
    expect(HOLD_BUTTON.vpnAdapterModeRemoved).toBe('VPN adapter mode removed');
  });
});

describe('the unreadable settings notice', () => {
  it('says traffic is held and where to choose a connection', () => {
    expect(UNREADABLE_NOTICE).toContain('could not be read, so TorrentPlayer is not connecting');
    expect(UNREADABLE_NOTICE).toContain('Settings → Network & VPN');
    expect(UNREADABLE_HELD).toMatch(/blocked until you choose Connect directly/);
    expect(holdNotice('settingsUnreadable', true)).toEqual({
      text: UNREADABLE_NOTICE,
      held: UNREADABLE_HELD,
    });
    expect(HOLD_BUTTON.settingsUnreadable).toBe('Network settings unreadable');
  });

  it('does not claim traffic is held once another mode is chosen', () => {
    const notice = holdNotice('settingsUnreadable', false);
    expect(notice.held).toBeNull();
    expect(notice.text).not.toContain('not connecting');
    expect(notice.text).toContain('could not be read');
  });
});

describe('the hold', () => {
  it('is shown until dismissed, and holds traffic only in Direct mode', () => {
    for (const hold of ['vpnAdapterModeRemoved', 'settingsUnreadable'] as const) {
      expect(showHoldNotice({ hold })).toBe(true);
      expect(holdApplies({ mode: 'direct', hold })).toBe(true);
      for (const mode of ['socks5', 'wireGuard'] as const) {
        expect(holdApplies({ mode, hold })).toBe(false);
      }
    }
    expect(showHoldNotice({ hold: null })).toBe(false);
    expect(holdApplies({ mode: 'direct', hold: null })).toBe(false);
  });

  it('saves the dismissal', async () => {
    invoke.mockResolvedValue({ settings: {} });
    await dismissHoldNotice();
    expect(invoke).toHaveBeenCalledWith('update_settings', {
      patch: { network: { hold: null } },
    });
  });

  it('rejects when the dismissal cannot be saved', async () => {
    invoke.mockRejectedValue({ kind: 'internal', message: 'disk full' });
    await expect(dismissHoldNotice()).rejects.toBeDefined();
  });
});
