// SPDX-License-Identifier: MIT OR Apache-2.0

import { describe, expect, it } from 'vitest';
import {
  MIN_THUMBNAIL_STEP_SECS,
  THUMBNAIL_CACHE_SIZE,
  downloadedRuns,
  formatTime,
  fractionAt,
  playedFraction,
  thumbnailSource,
  thumbnailTime,
} from './seekbar';

describe('downloadedRuns', () => {
  it('merges contiguous fully downloaded cells into one solid run', () => {
    expect(downloadedRuns([255, 255, 255, 0])).toEqual([{ start: 0, end: 0.75, opacity: 1 }]);
  });

  it('keeps partial cells translucent and merges equal neighbours', () => {
    const runs = downloadedRuns([0, 128, 128, 255, 0, 51]);
    expect(runs).toHaveLength(3);
    expect(runs[0]).toEqual({ start: 1 / 6, end: 3 / 6, opacity: 128 / 255 });
    expect(runs[1]).toEqual({ start: 3 / 6, end: 4 / 6, opacity: 1 });
    expect(runs[2]).toEqual({ start: 5 / 6, end: 1, opacity: 0.2 });
  });

  it('handles empty, all-empty, and out-of-range input', () => {
    expect(downloadedRuns([])).toEqual([]);
    expect(downloadedRuns([0, 0, 0])).toEqual([]);
    expect(downloadedRuns([999, Number.NaN, -5])).toEqual([{ start: 0, end: 1 / 3, opacity: 1 }]);
  });

  it('stays small for a typical 200-cell map', () => {
    const cells = Array.from({ length: 200 }, (_, i) => (i < 50 || i > 180 ? 255 : 0));
    expect(downloadedRuns(cells)).toHaveLength(2);
  });
});

describe('geometry and time', () => {
  it('maps pointer positions to clamped fractions', () => {
    expect(fractionAt(150, 100, 200)).toBe(0.25);
    expect(fractionAt(50, 100, 200)).toBe(0);
    expect(fractionAt(500, 100, 200)).toBe(1);
    expect(fractionAt(150, 100, 0)).toBe(0);
  });

  it('computes the played fraction safely', () => {
    expect(playedFraction(30, 120)).toBe(0.25);
    expect(playedFraction(30, Number.NaN)).toBe(0);
    expect(playedFraction(30, 0)).toBe(0);
    expect(playedFraction(200, 120)).toBe(1);
  });

  it('formats times', () => {
    expect(formatTime(0)).toBe('0:00');
    expect(formatTime(65.9)).toBe('1:05');
    expect(formatTime(3725)).toBe('1:02:05');
    expect(formatTime(Number.NaN)).toBe('--:--');
  });
});

describe('thumbnailTime', () => {
  it('snaps to the start of a step of 1/120 of the video', () => {
    // A 2-hour video: one thumbnail a minute.
    expect(thumbnailTime(0.5, 7200)).toBe(3600);
    expect(thumbnailTime(0.5 + 1 / 7200, 7200)).toBe(3600);
    expect(thumbnailTime(0.5 + 61 / 7200, 7200)).toBe(3660);
  });

  it('keeps steps of at least two seconds', () => {
    expect(thumbnailTime(0.51, 100)).toBe(50);
    expect(thumbnailTime(0.53, 100)).toBe(52);
    expect(MIN_THUMBNAIL_STEP_SECS).toBe(2);
  });

  it('clamps and refuses unknown durations', () => {
    expect(thumbnailTime(-1, 100)).toBe(0);
    expect(thumbnailTime(2, 100)).toBe(98);
    expect(thumbnailTime(1, 1)).toBe(0);
    expect(thumbnailTime(0.5, Number.NaN)).toBeNull();
    expect(thumbnailTime(0.5, Number.POSITIVE_INFINITY)).toBeNull();
    expect(thumbnailTime(0.5, 0)).toBeNull();
    expect(thumbnailTime(Number.NaN, 100)).toBeNull();
  });
});

describe('thumbnailSource', () => {
  it('fetches each picture once and remembers it', async () => {
    const asked: number[] = [];
    const source = thumbnailSource((secs) => {
      asked.push(secs);
      return Promise.resolve(`data:image/jpeg;base64,${String(secs)}`);
    });
    expect(source.peek(10)).toBeUndefined();
    expect(await source.get(10)).toBe('data:image/jpeg;base64,10');
    expect(await source.get(10)).toBe('data:image/jpeg;base64,10');
    expect(source.peek(10)).toBe('data:image/jpeg;base64,10');
    expect(asked).toEqual([10]);
  });

  it('asks again where there was nothing to show', async () => {
    let downloaded = false;
    const source = thumbnailSource(() => Promise.resolve(downloaded ? 'data:x' : null));
    expect(await source.get(4)).toBeNull();
    expect(source.peek(4)).toBeUndefined();
    downloaded = true;
    expect(await source.get(4)).toBe('data:x');
  });

  it('forgets the oldest pictures beyond its limit', async () => {
    const source = thumbnailSource((secs) => Promise.resolve(String(secs)), 2);
    await source.get(1);
    await source.get(2);
    await source.get(3);
    expect(source.peek(1)).toBeUndefined();
    expect(source.peek(2)).toBe('2');
    expect(source.peek(3)).toBe('3');
    expect(THUMBNAIL_CACHE_SIZE).toBe(48);
  });
});
