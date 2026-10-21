// SPDX-License-Identifier: MIT OR Apache-2.0

import { describe, expect, it } from 'vitest';
import { formatBytes, formatMillis, formatPercent, formatRate, loadingMessage } from './format';

describe('formatBytes', () => {
  it('uses decimal units with sensible precision', () => {
    expect(formatBytes(0)).toBe('0 B');
    expect(formatBytes(999)).toBe('999 B');
    expect(formatBytes(1_500)).toBe('1.5 KB');
    expect(formatBytes(206_400_000)).toBe('206 MB');
    expect(formatBytes(5_960_800_000)).toBe('6.0 GB');
    expect(formatBytes(3e15)).toBe('3000 TB');
  });

  it('handles nonsense input', () => {
    expect(formatBytes(-1)).toBe('–');
    expect(formatBytes(Number.NaN)).toBe('–');
  });
});

describe('formatRate, formatPercent, formatMillis', () => {
  it('formats rates', () => {
    expect(formatRate(2_400_000)).toBe('2.4 MB/s');
  });

  it('clamps percentages', () => {
    expect(formatPercent(50, 200)).toBe('25%');
    expect(formatPercent(300, 200)).toBe('100%');
    expect(formatPercent(1, 0)).toBe('0%');
  });

  it('formats milliseconds and switches to seconds for long waits', () => {
    expect(formatMillis(null)).toBe('–');
    expect(formatMillis(undefined)).toBe('–');
    expect(formatMillis(372.6)).toBe('373 ms');
    expect(formatMillis(12_340)).toBe('12.3 s');
  });
});

describe('loadingMessage', () => {
  const base = { firstFrameSeen: false, stalled: false, hasData: false, bufferedSecs: null };

  it('says peers are being found until data arrives', () => {
    expect(loadingMessage(base)).toBe('Finding peers…');
  });

  it('says buffering once data arrives, with seconds when known', () => {
    expect(loadingMessage({ ...base, hasData: true })).toBe('Buffering…');
    expect(loadingMessage({ ...base, hasData: true, bufferedSecs: 4.7 })).toBe(
      'Buffering… 4s ahead',
    );
    expect(loadingMessage({ ...base, hasData: true, bufferedSecs: 0.4 })).toBe('Buffering…');
  });

  it('is silent while playing and speaks again when stalled', () => {
    const playing = { ...base, firstFrameSeen: true, hasData: true };
    expect(loadingMessage(playing)).toBeNull();
    expect(loadingMessage({ ...playing, stalled: true, bufferedSecs: 2 })).toBe(
      'Buffering… 2s ahead',
    );
  });
});
