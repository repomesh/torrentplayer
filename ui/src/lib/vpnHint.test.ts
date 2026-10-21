// SPDX-License-Identifier: MIT OR Apache-2.0

import { beforeEach, describe, expect, it, vi } from 'vitest';

const invoke = vi.fn();
const listen = vi.fn();
vi.mock('@tauri-apps/api/core', () => ({ invoke }));
vi.mock('@tauri-apps/api/event', () => ({ listen }));

const {
  VPN_HINT,
  VPN_POST_URL,
  dismissVpnHint,
  followSeedingStarted,
  followVpnHint,
  openVpnPost,
  showVpnHint,
} = await import('./vpnHint');

type Handler = (event: { payload: unknown }) => void;

/** The handler registered for `event`. */
function handlerFor(event: string): Handler {
  const call = listen.mock.calls.find((c) => c[0] === event);
  if (!call) {
    throw new Error(`no listener for ${event}`);
  }
  return call[1] as Handler;
}

function view(dismissed: boolean): unknown {
  return { settings: { network: { mode: 'direct', vpnHintDismissed: dismissed } } };
}

beforeEach(() => {
  invoke.mockReset();
  listen.mockReset();
  listen.mockResolvedValue(() => undefined);
});

describe('the VPN note', () => {
  it('is shown only in Direct mode until dismissed', () => {
    expect(showVpnHint('direct', false)).toBe(true);
    expect(showVpnHint('direct', true)).toBe(false);
    for (const mode of ['bindInterface', 'socks5', 'wireGuard'] as const) {
      expect(showVpnHint(mode, false)).toBe(false);
    }
  });

  it('is one sentence that does not talk about legality', () => {
    expect(VPN_HINT.split('. ')).toHaveLength(1);
    expect(VPN_HINT).not.toMatch(/legal/i);
    expect(VPN_HINT).toContain('Windscribe');
  });

  it('opens the blog post and saves the dismissal', async () => {
    invoke.mockResolvedValue(view(true));
    openVpnPost();
    expect(invoke).toHaveBeenCalledWith('open_link', { link: VPN_POST_URL });
    await dismissVpnHint();
    expect(invoke).toHaveBeenCalledWith('update_settings', {
      patch: { network: { vpnHintDismissed: true } },
    });
  });

  it('follows the saved dismissal, preferring a newer event', async () => {
    let answer: (value: unknown) => void = () => undefined;
    invoke.mockReturnValue(
      new Promise((resolve) => {
        answer = resolve;
      }),
    );
    const seen: boolean[] = [];
    const stop = followVpnHint((network) => seen.push(network.vpnHintDismissed));
    handlerFor('settings-changed')({ payload: (view(true) as { settings: unknown }).settings });
    answer(view(false));
    await Promise.resolve();
    await Promise.resolve();
    expect(seen).toEqual([true]);
    stop();
    handlerFor('settings-changed')({ payload: (view(false) as { settings: unknown }).settings });
    expect(seen).toEqual([true]);
  });

  it('hears when seeding starts', () => {
    const onStart = vi.fn();
    followSeedingStarted(onStart);
    handlerFor('seeding-started')({ payload: null });
    expect(onStart).toHaveBeenCalledTimes(1);
  });
});
