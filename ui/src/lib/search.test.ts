// SPDX-License-Identifier: MIT OR Apache-2.0

import { beforeEach, describe, expect, it, vi } from 'vitest';
import type { QualityDto } from './bindings/QualityDto';
import {
  failedIndexerCount,
  formatPeers,
  formatPublished,
  holdOrder,
  looksLikeLink,
  pendingSourcesLabel,
  prefetchRequests,
  preloadActive,
  preloadLabel,
  qualityNotes,
  searchDelay,
  framePossible,
  thumbnailRequests,
  withRecent,
  withRecentEntry,
} from './search';

const invoke = vi.fn();
/** Stands in for Tauri's IPC channel: tests call `onmessage` directly. */
class FakeChannel {
  onmessage: (message: unknown) => void = () => undefined;
}
vi.mock('@tauri-apps/api/core', () => ({ invoke, Channel: FakeChannel }));

const { cancelSearch, prefetchTorrents, preloadStatus, searchTorrents, searchTorrentsProgressive } =
  await import('./ipc');

describe('looksLikeLink', () => {
  it('treats magnet and http(s) links as links and everything else as a search', () => {
    expect(looksLikeLink(' magnet:?xt=urn:btih:abc')).toBe(true);
    expect(looksLikeLink('HTTPS://academictorrents.com/download/x.torrent')).toBe(true);
    expect(looksLikeLink('http://example.org/a.torrent')).toBe(true);
    expect(looksLikeLink('mit 6.006')).toBe(false);
    expect(looksLikeLink('magnetism lecture')).toBe(false);
    expect(looksLikeLink('')).toBe(false);
  });
});

describe('formatting', () => {
  it('formats peers and dates', () => {
    expect(formatPeers(412, 430)).toBe('↑412 ↓430');
    expect(formatPeers(null, 5)).toBe('↑? ↓5');
    expect(formatPublished('2026-08-29T00:00:00Z')).toBe('2026-08-29');
    expect(formatPublished('yesterday')).toBe('');
    expect(formatPublished(null)).toBe('');
  });

  it('counts failed indexers', () => {
    expect(
      failedIndexerCount([
        { name: 'A', results: 1, error: null },
        { name: 'B', results: 0, error: 'timed out after 20s' },
      ]),
    ).toBe(1);
  });
});

describe('searchTorrents', () => {
  beforeEach(() => {
    invoke.mockReset();
  });

  it('invokes search_torrents with the query', async () => {
    const response = { results: [], indexers: [], excludedAdult: 0 };
    invoke.mockResolvedValue(response);
    await expect(searchTorrents('mit 6.006')).resolves.toEqual(response);
    expect(invoke).toHaveBeenCalledWith('search_torrents', { query: 'mit 6.006', categories: [] });
    await searchTorrents('sintel', [2000, 5000]);
    expect(invoke).toHaveBeenLastCalledWith('search_torrents', {
      query: 'sintel',
      categories: [2000, 5000],
    });
  });
});

describe('searchTorrentsProgressive', () => {
  beforeEach(() => {
    invoke.mockReset();
  });

  it('passes a channel whose messages reach the callback', async () => {
    const final = { results: [], indexers: [], excludedAdult: 0 };
    invoke.mockResolvedValue(final);
    const seen: number[] = [];
    await expect(
      searchTorrentsProgressive('sintel', (p) => {
        seen.push(p.completed);
      }),
    ).resolves.toEqual(final);
    const [command, args] = invoke.mock.calls[0] as [
      string,
      { query: string; onProgress: FakeChannel },
    ];
    expect(command).toBe('search_torrents_progressive');
    expect(args.query).toBe('sintel');
    args.onProgress.onmessage({ completed: 3 });
    expect(seen).toEqual([3]);
  });

  it('cancels the running search', async () => {
    invoke.mockResolvedValue(undefined);
    await expect(cancelSearch()).resolves.toBeUndefined();
    expect(invoke).toHaveBeenCalledWith('cancel_search');
  });

  it('prefetches through a channel', async () => {
    invoke.mockResolvedValue(undefined);
    const ready: string[] = [];
    const request = { key: 'ab', magnet: 'magnet:?xt=urn:btih:ab', download: null, infoHash: 'ab' };
    await prefetchTorrents([request], (r) => {
      ready.push(r.key);
    });
    const [command, args] = invoke.mock.calls[0] as [
      string,
      { results: unknown[]; onReady: FakeChannel },
    ];
    expect(command).toBe('prefetch_torrents');
    expect(args.results).toEqual([request]);
    args.onReady.onmessage({ key: 'ab', infoHash: 'ab' });
    expect(ready).toEqual(['ab']);
  });

  it('reads the preload status', async () => {
    invoke.mockResolvedValue(null);
    await expect(preloadStatus()).resolves.toBeNull();
    expect(invoke).toHaveBeenCalledWith('preload_status');
  });
});

describe('progress and preload labels', () => {
  const hash = 'ab'.repeat(20);

  it('says how many sources are left', () => {
    expect(pendingSourcesLabel(3, 0)).toBe('searching more sources…');
    expect(pendingSourcesLabel(40, 53)).toBe('searching 13 more sources…');
    expect(pendingSourcesLabel(52, 53)).toBe('searching 1 more source…');
    expect(pendingSourcesLabel(53, 53)).toBe('finishing…');
    expect(pendingSourcesLabel(60, 53)).toBe('finishing…');
  });

  it('labels the preloaded result only', () => {
    expect(preloadLabel(null, hash)).toBeNull();
    expect(preloadLabel({ infoHash: hash, phase: 'resolving', progress: 0 }, hash)).toBe(
      'Preloading…',
    );
    expect(preloadLabel({ infoHash: hash, phase: 'buffering', progress: 0.406 }, hash)).toBe(
      'Preloading… 40%',
    );
    expect(preloadLabel({ infoHash: hash, phase: 'ready', progress: 1 }, hash)).toBe(
      'Ready to play instantly',
    );
    expect(preloadLabel({ infoHash: hash, phase: 'failed', progress: 0 }, hash)).toBeNull();
    expect(
      preloadLabel({ infoHash: 'cd'.repeat(20), phase: 'ready', progress: 1 }, hash),
    ).toBeNull();
    expect(
      preloadLabel({ infoHash: '', phase: 'ready', progress: 1 }, null),
      'a result offered only as a download link',
    ).toBeNull();
  });

  it('polls only while a preload works', () => {
    expect(preloadActive(null)).toBe(false);
    expect(preloadActive({ phase: 'resolving' })).toBe(true);
    expect(preloadActive({ phase: 'buffering' })).toBe(true);
    expect(preloadActive({ phase: 'ready' })).toBe(false);
    expect(preloadActive({ phase: 'cancelled' })).toBe(false);
  });
});

describe('qualityNotes', () => {
  const base: QualityDto = {
    resolution: '1080p',
    hdr: false,
    codec: 'x264',
    source: 'WEB-DL',
    container: 'MP4',
    camera: false,
    suspectSize: false,
    playable: 'plays',
    label: '1080p WEB-DL',
    kind: 'video',
    audio: null,
    audioPlays: null,
  };

  it('shows the label, with warnings first', () => {
    expect(qualityNotes(base)).toEqual(['1080p WEB-DL']);
    expect(qualityNotes({ ...base, camera: true, source: 'CAM', label: '1080p CAM' })).toEqual([
      'Camera copy',
    ]);
    expect(
      qualityNotes({ ...base, resolution: '2160p', suspectSize: true, label: '2160p' }),
    ).toEqual(['Too small for 4K', '2160p']);
    expect(qualityNotes({ ...base, playable: 'needsFullPlayer', label: '' })).toEqual([
      'Needs the full player',
    ]);
  });
});

describe('streaming results', () => {
  const row = (key: string, seeders = 1) => ({ key, seeders });

  it('keeps shown rows in place and adds new ones at the end', () => {
    const held = [row('a'), row('b'), row('c')];
    const next = [row('d'), row('c', 9), row('a')];
    expect(holdOrder(held, next)).toEqual([row('a'), row('c', 9), row('d')]);
    expect(holdOrder([], next)).toEqual(next);
  });

  it('collapses searches sent in quick succession', () => {
    expect(searchDelay(null, 1000, 300)).toBe(0);
    expect(searchDelay(1000, 1100, 300)).toBe(200);
    expect(searchDelay(1000, 1400, 300)).toBe(0);
  });

  it('prefetches the first results, without the best pick or ones already done', () => {
    const r = (key: string, bestPick = false) => ({
      key,
      magnet: `magnet:?xt=urn:btih:${key}`,
      download: null,
      infoHash: key,
      bestPick,
    });
    const download = {
      key: 'download:https://search.example/download/x',
      magnet: null,
      download: 'https://search.example/download/x',
      infoHash: null,
      bestPick: false,
    };
    const results = [r('a', true), r('b'), r('c'), download, r('d')];
    expect(prefetchRequests(results, 4, new Set(['c']))).toEqual([
      { key: 'b', magnet: 'magnet:?xt=urn:btih:b', download: null, infoHash: 'b' },
      {
        key: download.key,
        magnet: null,
        download: 'https://search.example/download/x',
        infoHash: null,
      },
    ]);
    expect(prefetchRequests([], 3, new Set())).toEqual([]);
  });

  it('remembers a bounded number of recent items', () => {
    let set: Set<string> = new Set();
    for (const item of ['a', 'b', 'c', 'a', 'd']) {
      set = withRecent(set, item, 3);
    }
    expect([...set]).toEqual(['c', 'a', 'd']);
  });
});

describe('thumbnails', () => {
  const r = (key: string, infoHash: string | null, bestPick = false) => ({
    key,
    infoHash,
    bestPick,
  });

  it('asks only for results whose metadata is known, once', () => {
    const results = [r('a', 'a', true), r('b', 'b'), r('download:x', null), r('c', 'c')];
    const hashes = new Map([
      ['b', 'b'],
      ['download:x', 'ff'],
    ]);
    expect(thumbnailRequests(results, 3, hashes, new Set(), false)).toEqual([
      { key: 'b', infoHash: 'b' },
      { key: 'download:x', infoHash: 'ff' },
    ]);
    // The best pick once its preload has the metadata; nothing already tried; only the first.
    expect(thumbnailRequests(results, 3, hashes, new Set(['b']), true)).toEqual([
      { key: 'a', infoHash: 'a' },
      { key: 'download:x', infoHash: 'ff' },
    ]);
    expect(thumbnailRequests(results, 1, new Map(), new Set(), false)).toEqual([]);
  });

  it('shows frames only of videos the webview decodes', () => {
    expect(framePossible({ playable: 'plays' })).toBe(true);
    expect(framePossible({ playable: 'likely' })).toBe(true);
    expect(framePossible({ playable: 'unlikely' })).toBe(false);
    expect(framePossible({ playable: 'needsFullPlayer' })).toBe(false);
    expect(framePossible({ playable: 'unknown' })).toBe(false);
  });

  it('keeps a bounded number of pictures', () => {
    let map: Map<string, number> = new Map();
    for (const [i, key] of ['a', 'b', 'c', 'a', 'd'].entries()) {
      map = withRecentEntry(map, key, i, 3);
    }
    expect([...map.entries()]).toEqual([
      ['c', 2],
      ['a', 3],
      ['d', 4],
    ]);
  });
});
