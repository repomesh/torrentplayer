// SPDX-License-Identifier: MIT OR Apache-2.0

import { describe, expect, it } from 'vitest';
import { downloadedRuns, formatTime, fractionAt, playedFraction } from './seekbar';

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
