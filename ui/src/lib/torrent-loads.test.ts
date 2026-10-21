// SPDX-License-Identifier: MIT OR Apache-2.0

import { describe, expect, it, vi } from 'vitest';
import { TorrentLoads, torrentLoadKeys } from './torrent-loads';

const HASH = 'dd8255ecdc7ca55fb0bbf81323d87062db1f6d1c';
const OTHER_HASH = '1234567890123456789012345678901234567890';
const magnet = (hash: string): string => `magnet:?xt=urn:btih:${hash}`;

function deferred<T>(): {
  promise: Promise<T>;
  resolve: (value: T) => void;
  reject: (reason: Error) => void;
} {
  let resolve!: (value: T) => void;
  let reject!: (reason: Error) => void;
  const promise = new Promise<T>((res, rej) => {
    resolve = res;
    reject = rej;
  });
  return { promise, resolve, reject };
}

describe('per-torrent metadata loads', () => {
  it('allows another torrent to finish while the first is still waiting for peers', async () => {
    let pending: ReadonlySet<string> = new Set();
    const loads = new TorrentLoads<string>((keys) => {
      pending = keys;
    });
    const slow = deferred<string>();
    const fast = deferred<string>();
    const slowKey = torrentLoadKeys(magnet(HASH));
    const fastKey = torrentLoadKeys(magnet(OTHER_HASH));
    const first = loads.run(slowKey, () => slow.promise);
    const second = loads.run(fastKey, () => fast.promise);

    fast.resolve('fast torrent');
    await expect(second).resolves.toBe('fast torrent');
    expect(slowKey.every((key) => pending.has(key))).toBe(true);
    expect(fastKey.some((key) => pending.has(key))).toBe(false);

    slow.resolve('slow torrent');
    await first;
    expect(pending.size).toBe(0);
  });

  it('shares requests for the same hash, including magnets with different trackers', async () => {
    let pending: ReadonlySet<string> = new Set();
    const loads = new TorrentLoads<string>((keys) => {
      pending = keys;
    });
    const metadata = deferred<string>();
    const fetch = vi.fn(() => metadata.promise);
    const first = loads.run(torrentLoadKeys(magnet(HASH)), fetch);
    const otherLink = `${magnet(HASH.toUpperCase())}&tr=https://tracker.example/announce`;
    const second = loads.run(torrentLoadKeys(otherLink), fetch);

    expect(second).toBe(first);
    expect(pending.has(`link:${otherLink}`)).toBe(true);
    await Promise.resolve();
    expect(fetch).toHaveBeenCalledTimes(1);

    metadata.resolve('torrent');
    await Promise.all([first, second]);
    expect(pending.size).toBe(0);
  });

  it('shares a download-link request when another view supplies its hash later', async () => {
    const loads = new TorrentLoads<string>(() => undefined);
    const metadata = deferred<string>();
    const fetch = vi.fn(() => metadata.promise);
    const link = 'https://indexer.example/download/1';
    const first = loads.run(torrentLoadKeys(link), fetch);
    const second = loads.run(torrentLoadKeys(link, HASH), fetch);
    const third = loads.run(torrentLoadKeys(magnet(HASH)), fetch);
    expect(second).toBe(first);
    expect(third).toBe(first);

    metadata.resolve('torrent');
    await third;
    expect(fetch).toHaveBeenCalledTimes(1);
  });

  it('clears a failed torrent without clearing another load and permits retry', async () => {
    let pending: ReadonlySet<string> = new Set();
    const loads = new TorrentLoads<string>((keys) => {
      pending = keys;
    });
    const metadata = deferred<string>();
    const other = deferred<string>();
    const keys = torrentLoadKeys(magnet(HASH));
    const first = loads.run(keys, () => metadata.promise);
    const second = loads.run(torrentLoadKeys(magnet(OTHER_HASH)), () => other.promise);
    const rejected = expect(first).rejects.toThrow('No peers');
    metadata.reject(new Error('No peers'));
    await rejected;
    expect(keys.some((key) => pending.has(key))).toBe(false);
    expect(pending.has(`hash:${OTHER_HASH}`)).toBe(true);

    const retry = loads.run(keys, () => Promise.resolve('retried'));
    await expect(retry).resolves.toBe('retried');
    expect(pending.has(`hash:${OTHER_HASH}`)).toBe(true);
    other.resolve('other');
    await second;
    expect(pending.size).toBe(0);
  });

  it('cleans up if starting a request throws synchronously', async () => {
    let pending: ReadonlySet<string> = new Set();
    const loads = new TorrentLoads<string>((keys) => {
      pending = keys;
    });
    await expect(
      loads.run(torrentLoadKeys(magnet(HASH)), () => {
        throw new Error('Could not start');
      }),
    ).rejects.toThrow('Could not start');
    expect(pending.size).toBe(0);
  });
});

describe('torrent aliases', () => {
  it('matches encoded magnet hashes and ignores display names and trackers', () => {
    const keys = torrentLoadKeys(`  magnet:?dn=Lecture&xt=urn%3Abtih%3A${HASH.toUpperCase()}  `);
    expect(keys).toContain(`hash:${HASH}`);
  });

  it('keeps unknown download links distinct', () => {
    const first = torrentLoadKeys('https://indexer.example/1');
    const second = torrentLoadKeys('https://indexer.example/2');
    expect(first.some((key) => second.includes(key))).toBe(false);
  });
});
