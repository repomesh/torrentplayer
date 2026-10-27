// SPDX-License-Identifier: MIT OR Apache-2.0

import { beforeEach, describe, expect, it, vi } from 'vitest';
import type { LibraryItem } from './bindings/LibraryItem';

const invoke = vi.fn();
const listen = vi.fn();
vi.mock('@tauri-apps/api/core', () => ({ invoke }));
vi.mock('@tauri-apps/api/event', () => ({ listen }));

const {
  followLibrary,
  groupLibrary,
  libraryKey,
  mergeLibrary,
  queueWithoutLibraryTorrent,
  resumePosition,
  resumeStart,
  watchedFraction,
  watchedLabel,
  withoutLibraryTorrent,
} = await import('./library');

const HASH = 'dd8255ecdc7ca55fb0bbf81323d87062db1f6d1c';

function item(overrides: Partial<LibraryItem> = {}): LibraryItem {
  return {
    infoHash: HASH,
    fileIndex: 2,
    source: `magnet:?xt=urn:btih:${HASH}`,
    title: 'Lecture 2',
    torrentName: 'Course',
    sizeBytes: 100,
    positionSecs: 25,
    durationSecs: 100,
    completed: false,
    updatedAtSecs: 10,
    ...overrides,
  };
}

beforeEach(() => {
  invoke.mockReset();
  listen.mockReset();
});

describe('library progress', () => {
  it('keys files by normalized torrent hash and file index', () => {
    expect(libraryKey(item({ infoHash: HASH.toUpperCase() }))).toBe(`${HASH}:2`);
  });

  it('labels partial and completed playback', () => {
    expect(watchedFraction(item())).toBe(0.25);
    expect(watchedLabel(item())).toBe('25% watched');
    expect(watchedLabel(item({ durationSecs: null }))).toBe('Started');
    expect(watchedLabel(item({ positionSecs: 9 }))).toBeNull();
    expect(watchedFraction(item({ completed: true }))).toBe(1);
    expect(watchedLabel(item({ completed: true }))).toBe('Watched');
  });

  it('offers resume only after the opening and before the final ten seconds', () => {
    expect(resumePosition(item({ positionSecs: 9.9 }))).toBeNull();
    expect(resumePosition(item({ positionSecs: 10 }))).toBe(10);
    expect(resumePosition(item({ positionSecs: 89.9 }))).toBe(89.9);
    expect(resumePosition(item({ positionSecs: 90 }))).toBeNull();
    expect(resumePosition(item({ completed: true }))).toBeNull();
  });

  it('replaces changed files, sorts newest first, and groups courses', () => {
    const old = item();
    const other = item({ fileIndex: 3, title: 'Lecture 3', updatedAtSecs: 20 });
    const changed = item({ positionSecs: 50, updatedAtSecs: 30 });
    const merged = mergeLibrary([other, old], changed);
    expect(merged.map((entry) => entry.fileIndex)).toEqual([2, 3]);
    expect(merged[0]?.positionSecs).toBe(50);
    expect(groupLibrary(merged)).toMatchObject([
      { key: HASH, name: 'Course', items: [{ fileIndex: 2 }, { fileIndex: 3 }] },
    ]);
  });
});

describe('followLibrary', () => {
  it('overlays an event that races the initial read and then follows changes', async () => {
    let handler: ((event: { payload: LibraryItem }) => void) | undefined;
    const unlisten = vi.fn();
    listen.mockImplementation((name: string, callback: typeof handler) => {
      if (name === 'library-changed') {
        handler = callback;
      }
      return Promise.resolve(unlisten);
    });
    let resolveInitial: ((items: LibraryItem[]) => void) | undefined;
    invoke.mockReturnValue(
      new Promise<LibraryItem[]>((resolve) => {
        resolveInitial = resolve;
      }),
    );
    const seen: LibraryItem[][] = [];
    const stop = followLibrary((items) => seen.push(items));
    handler?.({ payload: item({ positionSecs: 40, updatedAtSecs: 12 }) });
    await vi.waitFor(() => {
      expect(invoke).toHaveBeenCalledWith('library_items');
    });
    resolveInitial?.([item()]);
    await vi.waitFor(() => {
      expect(seen.at(-1)?.[0]?.positionSecs).toBe(40);
    });
    handler?.({ payload: item({ positionSecs: 60, updatedAtSecs: 13 }) });
    expect(seen.at(-1)?.[0]?.positionSecs).toBe(60);
    stop();
    expect(unlisten).toHaveBeenCalled();
    handler?.({ payload: item({ positionSecs: 70 }) });
    expect(seen.at(-1)?.[0]?.positionSecs).toBe(60);
  });

  it('tries a failed initial read once more', async () => {
    listen.mockResolvedValue(vi.fn());
    invoke.mockRejectedValueOnce(new Error('busy')).mockResolvedValueOnce([item()]);
    const seen: LibraryItem[][] = [];
    const stop = followLibrary((items) => seen.push(items));
    await vi.waitFor(() => {
      expect(seen).toEqual([[item()]]);
    });
    expect(invoke).toHaveBeenCalledTimes(2);
    stop();
  });

  it('starts empty and follows changes when the initial read keeps failing', async () => {
    let handler: ((event: { payload: LibraryItem }) => void) | undefined;
    listen.mockImplementation((name: string, callback: typeof handler) => {
      if (name === 'library-changed') {
        handler = callback;
      }
      return Promise.resolve(vi.fn());
    });
    invoke.mockRejectedValue(new Error('gone'));
    const seen: LibraryItem[][] = [];
    const stop = followLibrary((items) => seen.push(items));
    await vi.waitFor(() => {
      expect(seen).toEqual([[]]);
    });
    expect(invoke).toHaveBeenCalledTimes(2);
    handler?.({ payload: item() });
    expect(seen.at(-1)).toEqual([item()]);
    stop();
  });
});

describe('library removal', () => {
  it('removes only one torrent from history and queue, including unresolved source entries', () => {
    const other = item({ infoHash: 'a'.repeat(40) });
    expect(
      withoutLibraryTorrent([item(), item({ fileIndex: 3 }), other], HASH.toUpperCase()),
    ).toEqual([other]);
    const queued = (id: string, infoHash: string | null, source: string) => ({
      id,
      infoHash,
      source,
      fileIndex: 2,
      title: id,
      torrentName: null,
      sizeBytes: null,
      paused: false,
      played: false,
    });
    const queue = {
      current: 'removed',
      items: [
        queued('removed', HASH, item().source),
        queued('unresolved', null, 'https://example.org/course.torrent'),
        queued('magnet-alias', null, `magnet:?xt=urn:btih:${HASH}&dn=Another+name`),
        queued('kept', other.infoHash, other.source),
      ],
    };
    const after = queueWithoutLibraryTorrent(
      queue,
      HASH.toUpperCase(),
      new Set(['https://example.org/course.torrent', item().source]),
    );
    expect(after.items.map((entry) => entry.id)).toEqual(['kept']);
    expect(after.current).toBeNull();
    expect(queueWithoutLibraryTorrent({ ...queue, current: 'kept' }, HASH, new Set()).current).toBe(
      'kept',
    );
  });

  it('applies removals that race the first read and follows later removals', async () => {
    let removed: ((event: { payload: string }) => void) | undefined;
    const stops = [vi.fn(), vi.fn()];
    listen.mockImplementation((name: string, callback: typeof removed) => {
      if (name === 'library-removed') {
        removed = callback;
      }
      return Promise.resolve(name === 'library-removed' ? stops[1] : stops[0]);
    });
    let resolveInitial: ((items: LibraryItem[]) => void) | undefined;
    invoke.mockReturnValue(
      new Promise<LibraryItem[]>((resolve) => {
        resolveInitial = resolve;
      }),
    );
    const seen: LibraryItem[][] = [];
    const stop = followLibrary((items) => seen.push(items));
    removed?.({ payload: HASH.toUpperCase() });
    await vi.waitFor(() => {
      expect(invoke).toHaveBeenCalledWith('library_items');
    });
    const other = item({ infoHash: 'a'.repeat(40) });
    resolveInitial?.([item(), other]);
    await vi.waitFor(() => {
      expect(seen.at(-1)).toEqual([other]);
    });
    removed?.({ payload: other.infoHash });
    expect(seen.at(-1)).toEqual([]);
    stop();
    stops.forEach((unlisten) => {
      expect(unlisten).toHaveBeenCalled();
    });
  });
});

describe('resumeStart', () => {
  it('continues from a saved position without asking, and says so', () => {
    expect(resumeStart(754)).toEqual({ startAt: 754, chip: 'Continuing from 12:34' });
  });

  it('starts at the beginning when nothing was saved (or the setting is off)', () => {
    expect(resumeStart(null)).toEqual({ startAt: 0, chip: null });
    expect(resumeStart(Number.NaN)).toEqual({ startAt: 0, chip: null });
  });
});
