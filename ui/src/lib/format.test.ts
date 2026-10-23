// SPDX-License-Identifier: MIT OR Apache-2.0

import { describe, expect, it } from 'vitest';
import {
  formatBytes,
  formatClock,
  formatMillis,
  formatPercent,
  formatRate,
  loadingMessage,
  NO_SHARERS,
  NO_SHARERS_MS,
} from './format';

describe('formatClock', () => {
  it('shows minutes and seconds, and hours when there are some', () => {
    expect(formatClock(0)).toBe('0:00');
    expect(formatClock(754.9)).toBe('12:34');
    expect(formatClock(3729)).toBe('1:02:09');
    expect(formatClock(-5)).toBe('0:00');
  });
});

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
  const base = {
    firstFrameSeen: false,
    stalled: false,
    hasData: false,
    peersSeen: 0,
    peersLive: 0,
    downloadBps: 0,
    noPeersMs: 0,
  };

  it('says peers are being found until data arrives, with how many were found', () => {
    expect(loadingMessage(base)).toEqual({ text: 'Finding peers…', spoken: 'Finding peers…' });
    expect(loadingMessage({ ...base, peersSeen: 12 })?.text).toBe('Finding peers… (12 found)');
  });

  it('says buffering once data arrives, with the speed and peers, never seconds ahead', () => {
    const buffering = { ...base, hasData: true, peersLive: 3, downloadBps: 2_400_000 };
    expect(loadingMessage(buffering)).toEqual({
      text: 'Buffering · ↓ 2.4 MB/s from 3 peers',
      spoken: 'Buffering…',
    });
    expect(loadingMessage({ ...buffering, peersLive: 1 })?.text).toBe(
      'Buffering · ↓ 2.4 MB/s from 1 peer',
    );
    expect(loadingMessage({ ...base, hasData: true })?.text).toBe('Buffering…');
  });

  it('is silent while playing and speaks again when stalled', () => {
    const playing = { ...base, firstFrameSeen: true, hasData: true, peersLive: 2 };
    expect(loadingMessage(playing)).toBeNull();
    expect(loadingMessage({ ...playing, stalled: true })?.spoken).toBe('Buffering…');
  });

  it('says when nobody shares the file after a while without peers', () => {
    expect(loadingMessage({ ...base, noPeersMs: NO_SHARERS_MS - 1 })?.text).toBe('Finding peers…');
    expect(loadingMessage({ ...base, noPeersMs: NO_SHARERS_MS })?.text).toBe(NO_SHARERS);
    expect(loadingMessage({ ...base, peersLive: 1, noPeersMs: NO_SHARERS_MS })?.text).toBe(
      'Finding peers…',
    );
  });
});
