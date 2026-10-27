// SPDX-License-Identifier: MIT OR Apache-2.0

import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { pollWhileVisible, reportDue, type Visibility } from './poll';

class FakeDocument implements Visibility {
  visibilityState: DocumentVisibilityState = 'visible';
  private readonly listeners = new Set<() => void>();

  addEventListener(_type: 'visibilitychange', listener: () => void): void {
    this.listeners.add(listener);
  }

  removeEventListener(_type: 'visibilitychange', listener: () => void): void {
    this.listeners.delete(listener);
  }

  show(state: DocumentVisibilityState): void {
    this.visibilityState = state;
    for (const listener of this.listeners) {
      listener();
    }
  }

  get listening(): number {
    return this.listeners.size;
  }
}

/** A poll whose calls finish only when the test says so. */
function controlledPoll(): { poll: () => Promise<void>; calls: number; finish: () => void } {
  const pending: (() => void)[] = [];
  const state = {
    calls: 0,
    poll: (): Promise<void> => {
      state.calls += 1;
      return new Promise<void>((resolve) => pending.push(resolve));
    },
    finish: (): void => {
      pending.shift()?.();
    },
  };
  return state;
}

describe('pollWhileVisible', () => {
  beforeEach(() => {
    vi.useFakeTimers();
  });

  afterEach(() => {
    vi.useRealTimers();
  });

  it('polls at once and then on every interval', async () => {
    const doc = new FakeDocument();
    const poll = vi.fn(() => Promise.resolve());
    const stop = pollWhileVisible(poll, 1000, doc);
    expect(poll).toHaveBeenCalledTimes(1);
    await vi.advanceTimersByTimeAsync(3000);
    expect(poll).toHaveBeenCalledTimes(4);
    stop();
  });

  it('skips ticks while the last call is still waiting', async () => {
    const doc = new FakeDocument();
    const poll = controlledPoll();
    const stop = pollWhileVisible(poll.poll, 1000, doc);
    await vi.advanceTimersByTimeAsync(3000);
    expect(poll.calls).toBe(1);
    poll.finish();
    await vi.advanceTimersByTimeAsync(1000);
    expect(poll.calls).toBe(2);
    stop();
  });

  it('keeps polling after a call fails', async () => {
    const doc = new FakeDocument();
    const poll = vi.fn(() => Promise.reject(new Error('gone')));
    const stop = pollWhileVisible(poll, 1000, doc);
    await vi.advanceTimersByTimeAsync(1000);
    expect(poll).toHaveBeenCalledTimes(2);
    stop();
  });

  it('pauses while hidden and polls as soon as the window shows', async () => {
    const doc = new FakeDocument();
    const poll = vi.fn(() => Promise.resolve());
    const stop = pollWhileVisible(poll, 1000, doc);
    await vi.advanceTimersByTimeAsync(0);
    doc.show('hidden');
    await vi.advanceTimersByTimeAsync(5000);
    expect(poll).toHaveBeenCalledTimes(1);
    doc.show('visible');
    expect(poll).toHaveBeenCalledTimes(2);
    stop();
  });

  it('stops polling and listening once stopped', async () => {
    const doc = new FakeDocument();
    const poll = vi.fn(() => Promise.resolve());
    const stop = pollWhileVisible(poll, 1000, doc);
    stop();
    await vi.advanceTimersByTimeAsync(5000);
    doc.show('visible');
    expect(poll).toHaveBeenCalledTimes(1);
    expect(doc.listening).toBe(0);
  });
});

describe('reportDue', () => {
  const last = { position: 12, duration: 600, at: 1000 };

  it('reports the first position', () => {
    expect(reportDue(null, 0, null, 0, 30_000)).toBe(true);
  });

  it('reports a changed position or duration', () => {
    expect(reportDue(last, 13, 600, 2000, 30_000)).toBe(true);
    expect(reportDue(last, 12, 601, 2000, 30_000)).toBe(true);
  });

  it('repeats an unchanged position only after a while', () => {
    expect(reportDue(last, 12, 600, 2000, 30_000)).toBe(false);
    expect(reportDue(last, 12, 600, 31_000, 30_000)).toBe(true);
  });
});
