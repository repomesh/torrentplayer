// SPDX-License-Identifier: MIT OR Apache-2.0

import { afterEach, describe, expect, it, vi } from 'vitest';
import {
  AUTOPLAY_COUNTDOWN_SECS,
  Countdown,
  PRELOAD_NEXT_LEAD_SECS,
  shouldPreloadNext,
} from './upnext';

describe('shouldPreloadNext', () => {
  const base = {
    fileDownloaded: 500,
    fileLength: 1000,
    downloadBps: 100,
    position: 0,
    duration: 3600,
  };

  it('preloads once the current file is complete, wherever playback is', () => {
    expect(shouldPreloadNext({ ...base, fileDownloaded: 1000 })).toBe(true);
    expect(shouldPreloadNext({ ...base, fileDownloaded: 1000, duration: Number.NaN })).toBe(true);
  });

  it('waits until playback is nearly over and the rest arrives well in time', () => {
    const nearEnd = { ...base, position: 3600 - PRELOAD_NEXT_LEAD_SECS + 10 };
    // 500 bytes at 100 B/s is 5 s, well inside the 110 s left.
    expect(shouldPreloadNext(nearEnd)).toBe(true);
    expect(shouldPreloadNext({ ...nearEnd, position: 0 }), 'too early').toBe(false);
    expect(shouldPreloadNext({ ...nearEnd, downloadBps: 5 }), '100 s to go: too slow').toBe(false);
    expect(shouldPreloadNext({ ...nearEnd, downloadBps: 0 })).toBe(false);
    expect(shouldPreloadNext({ ...nearEnd, duration: Number.NaN }), 'duration unknown').toBe(false);
  });
});

describe('Countdown', () => {
  afterEach(() => {
    vi.useRealTimers();
  });

  it('ticks down once a second and then fires', () => {
    vi.useFakeTimers();
    const ticks: number[] = [];
    const done = vi.fn();
    const countdown = new Countdown((s) => ticks.push(s), done);
    countdown.start(3);
    expect(countdown.running).toBe(true);
    vi.advanceTimersByTime(2000);
    expect(ticks).toEqual([3, 2, 1]);
    expect(done).not.toHaveBeenCalled();
    vi.advanceTimersByTime(1000);
    expect(done).toHaveBeenCalledOnce();
    expect(countdown.running).toBe(false);
    expect(countdown.secondsLeft).toBe(0);
  });

  it('can be cancelled or restarted', () => {
    vi.useFakeTimers();
    const done = vi.fn();
    const countdown = new Countdown(() => undefined, done);
    countdown.start();
    expect(countdown.secondsLeft).toBe(AUTOPLAY_COUNTDOWN_SECS);
    vi.advanceTimersByTime(2000);
    countdown.cancel();
    vi.advanceTimersByTime(10_000);
    expect(done).not.toHaveBeenCalled();
    countdown.start(2);
    countdown.start(2);
    vi.advanceTimersByTime(2000);
    expect(done).toHaveBeenCalledOnce();
  });
});
