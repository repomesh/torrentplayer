// SPDX-License-Identifier: MIT OR Apache-2.0

import { describe, expect, it } from 'vitest';
import {
  byId,
  canPause,
  describeDownload,
  describeLink,
  describeRates,
  fastestItem,
  formatEta,
  progressOf,
  toneOf,
  type ItemDownload,
  type LinkUse,
} from './downloads';

function d(over: Partial<ItemDownload> = {}): ItemDownload {
  return {
    id: 'a',
    state: 'downloading',
    downloadBps: 2_400_000,
    uploadBps: 0,
    peersLive: 14,
    peersConnecting: 2,
    peersSeen: 80,
    downloaded: 370,
    length: 1000,
    etaSecs: 240,
    startable: true,
    limited: false,
    uploading: false,
    waitReason: null,
    error: null,
    ...over,
  };
}

const link: LinkUse = {
  capacityBps: 12_000_000,
  usedBps: 9_800_000,
  uploadBps: 120_000,
  playbackNeedBps: null,
  pressure: 'relaxed',
  maxActive: null,
  active: 3,
};

describe('download status', () => {
  it('formats time left', () => {
    expect(formatEta(null)).toBeNull();
    expect(formatEta(0.2)).toBe('1 s');
    expect(formatEta(45)).toBe('45 s');
    expect(formatEta(240)).toBe('4 min');
    expect(formatEta(3600)).toBe('1 h');
    expect(formatEta(4800)).toBe('1 h 20 min');
  });

  it('describes a download in one line', () => {
    expect(describeDownload(d())).toBe('↓ 2.4 MB/s · 14 peers · 37% · 4 min left');
    expect(describeDownload(d({ limited: true }))).toBe(
      '↓ 2.4 MB/s · 14 peers · 37% · 4 min left · slowed for playback',
    );
    expect(describeDownload(d({ state: 'watching', limited: true, etaSecs: null }))).toBe(
      '↓ 2.4 MB/s · 14 peers · 37%',
    );
    expect(describeDownload(d({ state: 'paused' }))).toBe('Paused · 37%');
    expect(describeDownload(d({ state: 'paused', downloaded: 0 }))).toBe('Paused');
    expect(describeDownload(d({ state: 'queued', downloaded: 0, waitReason: 'limit' }))).toBe(
      'Waiting: your limit of downloads at once',
    );
    expect(describeDownload(d({ state: 'queued', waitReason: 'playback' }))).toBe(
      'Waiting for playback: the video playing needs the bandwidth · 37%',
    );
    expect(describeDownload(d({ state: 'connecting', peersSeen: 0 }))).toBe('Finding peers… · 37%');
    expect(describeDownload(d({ state: 'complete' }))).toBe('Downloaded');
    // A finished torrent keeps sharing: it reads as seeding, never as paused.
    expect(describeDownload(d({ state: 'complete', uploading: true }))).toBe(
      'Downloaded · seeding',
    );
    expect(describeDownload(d({ state: 'skipped' }))).toBe('37% downloaded');
    expect(describeDownload(d({ state: 'skipped', downloaded: 0 }))).toBeNull();
    expect(describeDownload(d({ state: 'failed', error: 'No peers.' }))).toBe(
      'No peers. Trying again soon.',
    );
  });

  it('colours an item by whether it plays, downloads, or waits', () => {
    expect(toneOf(d({ startable: true }))).toBe('playable');
    expect(toneOf(d({ startable: false, state: 'complete' }))).toBe('playable');
    expect(toneOf(d({ startable: false, state: 'watching' }))).toBe('playable');
    expect(toneOf(d({ startable: false }))).toBe('downloading');
    expect(toneOf(d({ startable: false, peersLive: 0 }))).toBe('waiting');
    expect(toneOf(d({ startable: false, state: 'connecting', peersLive: 0 }))).toBe('waiting');
    expect(toneOf(d({ startable: false, state: 'queued' }))).toBe('waiting');
    expect(toneOf(d({ startable: false, state: 'paused' }))).toBe('waiting');
  });

  it('shows upload speed whenever a torrent is sharing', () => {
    expect(describeDownload(d({ uploadBps: 51_000 }))).toBe(
      '↓ 2.4 MB/s · 14 peers · 37% · 4 min left · ↑ 51.0 KB/s',
    );
    expect(describeDownload(d({ state: 'complete', uploadBps: 51_000 }))).toBe(
      'Downloaded · ↑ 51.0 KB/s',
    );
    expect(describeRates(link)).toBe('↓ 9.8 MB/s · ↑ 120 KB/s');
  });

  it('knows the progress and when pausing applies', () => {
    expect(progressOf(d())).toBeCloseTo(0.37);
    expect(progressOf(d({ length: null }))).toBeNull();
    expect(canPause(undefined)).toBe(true);
    // Pausing stops sharing too, so finished and skipped items can be paused.
    expect(canPause(d({ state: 'complete' }))).toBe(true);
    expect(canPause(d({ state: 'skipped' }))).toBe(true);
    expect(canPause(d({ state: 'watching' }))).toBe(false);
    expect(canPause(d({ state: 'paused' }))).toBe(true);
  });

  it('points out an item that downloads clearly fastest', () => {
    const items = [
      d({ id: 'a', downloadBps: 1_000_000 }),
      d({ id: 'b', downloadBps: 4_000_000 }),
      d({ id: 'c', state: 'paused', downloadBps: 9_000_000 }),
    ];
    expect(fastestItem(items)).toBe('b');
    expect(fastestItem([d({ id: 'a' }), d({ id: 'b', downloadBps: 2_500_000 })])).toBeNull();
    expect(fastestItem([d()])).toBeNull();
    expect(byId({ items, link }).get('b')?.downloadBps).toBe(4_000_000);
  });

  it('summarizes the connection', () => {
    expect(describeLink(link)).toBe('Connection ≈ 12.0 MB/s · 9.8 MB/s in use · 3 downloading');
    expect(
      describeLink({ ...link, playbackNeedBps: 1_500_000, pressure: 'guarded', active: 1 }),
    ).toBe('Connection ≈ 12.0 MB/s · 9.8 MB/s in use · 1 downloading · playback first');
    expect(describeLink({ ...link, maxActive: 4 })).toBe(
      'Connection ≈ 12.0 MB/s · 9.8 MB/s in use · 3 of max 4 downloading',
    );
    expect(describeLink({ ...link, capacityBps: 0, usedBps: 0, active: 0 })).toBeNull();
  });
});
