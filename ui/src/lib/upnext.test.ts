// SPDX-License-Identifier: MIT OR Apache-2.0

import { describe, expect, it } from 'vitest';
import { PRELOAD_NEXT_LEAD_SECS, shouldPreloadNext } from './upnext';

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
