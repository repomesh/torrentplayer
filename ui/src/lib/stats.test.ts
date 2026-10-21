// SPDX-License-Identifier: MIT OR Apache-2.0

import { describe, expect, it } from 'vitest';
import { Ewma, bufferHealth, bufferedSeconds, etaSeconds, formatEta } from './stats';

describe('Ewma', () => {
  it('starts at the first sample and moves toward new ones by the time constant', () => {
    const avg = new Ewma(3);
    expect(avg.value).toBeNull();
    expect(avg.next(1000)).toBe(1000);
    const after = avg.next(0, 1);
    expect(after).toBeCloseTo(1000 * Math.exp(-1 / 3), 6);
  });

  it('ignores non-finite samples', () => {
    const avg = new Ewma();
    expect(avg.next(Number.NaN)).toBe(0);
    avg.next(500);
    expect(avg.next(Number.POSITIVE_INFINITY)).toBe(500);
  });
});

describe('bufferHealth', () => {
  it('uses the green, yellow, and red thresholds', () => {
    expect(bufferHealth(12, false)).toBe('good');
    expect(bufferHealth(10, false)).toBe('good');
    expect(bufferHealth(5, false)).toBe('low');
    expect(bufferHealth(2.9, false)).toBe('stalled');
    expect(bufferHealth(30, true)).toBe('stalled');
    expect(bufferHealth(null, false)).toBe('unknown');
  });
});

describe('bufferedSeconds', () => {
  it('prefers the backend value and otherwise derives it from the average byte rate', () => {
    expect(bufferedSeconds(7, 0, 0, Number.NaN)).toBe(7);
    // 600 MB over 600 s is 1 MB/s, so 5 MB buffered is 5 s.
    expect(bufferedSeconds(null, 5_000_000, 600_000_000, 600)).toBe(5);
    expect(bufferedSeconds(null, 5_000_000, 600_000_000, Number.NaN)).toBeNull();
  });
});

describe('ETA', () => {
  it('estimates remaining time and refuses to guess at tiny rates', () => {
    expect(etaSeconds(10_000_000, 1_000_000)).toBe(10);
    expect(etaSeconds(0, 0)).toBe(0);
    expect(etaSeconds(10_000_000, 10)).toBeNull();
  });

  it('formats compactly', () => {
    expect(formatEta(null)).toBe('—');
    expect(formatEta(44.2)).toBe('45s');
    expect(formatEta(200)).toBe('3m 20s');
    expect(formatEta(3900)).toBe('1h 05m');
  });
});
