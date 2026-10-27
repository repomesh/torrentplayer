// SPDX-License-Identifier: MIT OR Apache-2.0

import { describe, expect, it } from 'vitest';
import {
  barTitle,
  byId,
  canPause,
  describeDownload,
  describeLink,
  describeRates,
  formatEta,
  progressOf,
  toneOf,
  withPause,
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
    pieceMap: [],
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
    expect(describeDownload(d({ state: 'paused' }))).toBe('Paused by you · 37%');
    expect(describeDownload(d({ state: 'paused', downloaded: 0 }))).toBe('Paused by you');
    expect(describeDownload(d({ state: 'queued', downloaded: 0, waitReason: 'limit' }))).toBe(
      'Waiting: your limit of downloads at once (Settings → Bandwidth)',
    );
    expect(describeDownload(d({ state: 'queued', downloaded: 0, waitReason: 'limit' }), 3)).toBe(
      'Waiting: up to 3 queued videos download at once (Settings → Bandwidth)',
    );
    expect(describeDownload(d({ state: 'queued', waitReason: 'playback' }))).toBe(
      'Waiting for playback: the video playing needs the bandwidth · 37%',
    );
    expect(describeDownload(d({ state: 'connecting', peersSeen: 0 }))).toBe('Finding peers… · 37%');
    expect(describeDownload(d({ state: 'complete' }))).toBe('Downloaded');
    // A finished torrent keeps sharing: it reads as sharing, never as paused.
    expect(describeDownload(d({ state: 'complete', uploading: true }))).toBe(
      'Downloaded · sharing with others',
    );
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
    // Pausing stops sharing too, so finished items can be paused.
    expect(canPause(d({ state: 'complete' }))).toBe(true);
    expect(canPause(d({ state: 'watching' }))).toBe(false);
    expect(canPause(d({ state: 'paused' }))).toBe(true);
  });

  it('finds items by id', () => {
    const items = [d({ id: 'a', downloadBps: 1_000_000 }), d({ id: 'b', downloadBps: 4_000_000 })];
    expect(byId({ items, link }).get('b')?.downloadBps).toBe(4_000_000);
    expect(byId(null).size).toBe(0);
  });

  it('summarizes the connection', () => {
    expect(describeLink(link)).toBe('Speed ≈ 12.0 MB/s · 9.8 MB/s in use · 3 downloading');
    expect(
      describeLink({ ...link, playbackNeedBps: 1_500_000, pressure: 'guarded', active: 1 }),
    ).toBe('Speed ≈ 12.0 MB/s · 9.8 MB/s in use · 1 downloading · the video playing comes first');
    expect(
      describeLink({ ...link, playbackNeedBps: 1_500_000, pressure: 'starving', active: 1 }),
    ).toBe(
      'Speed ≈ 12.0 MB/s · 9.8 MB/s in use · 1 downloading · queue held for the video playing',
    );
    expect(describeLink({ ...link, maxActive: 4 })).toBe(
      'Speed ≈ 12.0 MB/s · 9.8 MB/s in use · 3 of max 4 downloading',
    );
    expect(describeLink({ ...link, capacityBps: 0, usedBps: 0, active: 0 })).toBeNull();
  });
});

describe('progress bar tooltip', () => {
  it('says why the bar is where it is', () => {
    expect(barTitle(d({ state: 'paused' }))).toBe('Paused by you');
    expect(barTitle(d({ state: 'queued', downloaded: 0, waitReason: 'limit' }), 2)).toBe(
      'Waiting: up to 2 queued videos download at once (Settings → Bandwidth)',
    );
    expect(barTitle(d({ startable: true }))).toBe('Ready to play');
    expect(barTitle(d({ startable: false }))).toBe('Downloading, not ready to play yet');
    expect(barTitle(d({ startable: false, state: 'connecting', peersLive: 0 }))).toBe(
      'Waiting for peers',
    );
  });
});

describe('pausing as shown', () => {
  it('shows a pause or resume at once, and leaves the video playing alone', () => {
    expect(withPause(true, d())).toMatchObject({ state: 'paused', downloadBps: 0, uploadBps: 0 });
    expect(withPause(false, d({ state: 'paused' }))?.state).toBe('queued');
    expect(withPause(true, d({ state: 'watching' }))?.state).toBe('watching');
    expect(withPause(true, undefined)).toBeUndefined();
    const reported = d({ state: 'paused' });
    expect(withPause(true, reported)).toBe(reported);
  });

  it('shows no upload on a paused video, even while its torrent shares for others', () => {
    expect(describeDownload(d({ state: 'paused', uploadBps: 51_000 }))).toBe('Paused by you · 37%');
  });
});
