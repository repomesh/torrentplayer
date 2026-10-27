// SPDX-License-Identifier: MIT OR Apache-2.0

import { describe, expect, it } from 'vitest';
import type { QueueItem } from './bindings/QueueItem';
import {
  PRELOAD_NEXT_LEAD_SECS,
  endCard,
  endCardTakesFocus,
  preloadTarget,
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

describe('preloadTarget', () => {
  const item = (id: string, paused = false): QueueItem => ({
    id,
    source: `https://example.org/${id}.torrent`,
    infoHash: null,
    fileIndex: null,
    title: id,
    torrentName: null,
    sizeBytes: null,
    paused,
    played: false,
  });

  it('preloads the next item, but not one the user paused', () => {
    const queue = { items: [item('a'), item('b'), item('c')], current: 'a' };
    expect(preloadTarget(queue)?.id).toBe('b');
    const paused = { ...queue, items: [item('a'), item('b', true), item('c')] };
    expect(preloadTarget(paused)).toBeNull();
    expect(preloadTarget({ ...queue, current: 'c' })).toBeNull();
  });
});

describe('endCard', () => {
  it('offers the next video when there is one, else a replay', () => {
    expect(endCard('Lecture 3')).toEqual({
      heading: 'Finished',
      next: 'Next: Lecture 3',
      primary: 'next',
    });
    expect(endCard(null)).toEqual({ heading: 'Finished', next: null, primary: 'replay' });
  });

  it('takes focus only from the player', () => {
    expect(endCardTakesFocus(true, true)).toBe(true);
    expect(endCardTakesFocus(false, false)).toBe(true);
    expect(endCardTakesFocus(false, true)).toBe(false);
  });
});
