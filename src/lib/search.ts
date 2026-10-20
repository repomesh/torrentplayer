// SPDX-License-Identifier: MIT OR Apache-2.0

/** Helpers for the search box and results list. */

import type { IndexerStatusDto } from './bindings/IndexerStatusDto';
import type { PreloadStatusDto } from './bindings/PreloadStatusDto';
import type { QualityDto } from './bindings/QualityDto';
import type { PrefetchRequest } from './bindings/PrefetchRequest';
import type { SearchResultDto } from './bindings/SearchResultDto';
import type { ThumbnailRequest } from './bindings/ThumbnailRequest';

/** Whether input is a link to add directly rather than words to search for. */
export function looksLikeLink(text: string): boolean {
  const t = text.trim().toLowerCase();
  return t.startsWith('magnet:') || t.startsWith('http://') || t.startsWith('https://');
}

/** Number of indexers that failed on this search. */
export function failedIndexerCount(indexers: readonly IndexerStatusDto[]): number {
  return indexers.filter((i) => i.error !== null).length;
}

/** `↑412 ↓430`-style peer counts, with `?` for unknown values. */
export function formatPeers(seeders: number | null, peers: number | null): string {
  const show = (n: number | null): string => (n === null ? '?' : String(n));
  return `↑${show(seeders)} ↓${show(peers)}`;
}

/** The date part of an ISO 8601 timestamp, or an empty string if it is not one. */
export function formatPublished(iso: string | null): string {
  if (iso === null) {
    return '';
  }
  const match = /^(\d{4}-\d{2}-\d{2})/.exec(iso);
  return match?.[1] ?? '';
}

/**
 * What the results say while a search runs: `searching 12 more sources…`, `searching more
 * sources…` while the number is unknown (`total` 0), or `finishing…` once all have answered.
 */
export function pendingSourcesLabel(completed: number, total: number): string {
  if (total <= 0) {
    return 'searching more sources…';
  }
  const left = total - completed;
  if (left <= 0) {
    return 'finishing…';
  }
  return `searching ${String(left)} more source${left === 1 ? '' : 's'}…`;
}

/**
 * `next` in the order of `held` (results already shown keep their places, with their latest
 * details), followed by new results in `next`'s order. Results gone from `next` are dropped.
 */
export function holdOrder<T extends { key: string }>(held: readonly T[], next: readonly T[]): T[] {
  const latest = new Map(next.map((r) => [r.key, r]));
  const kept = held.flatMap((r) => {
    const now = latest.get(r.key);
    return now === undefined ? [] : [now];
  });
  const keptKeys = new Set(kept.map((r) => r.key));
  return [...kept, ...next.filter((r) => !keptKeys.has(r.key))];
}

/**
 * How long to wait before sending a search, so that searches submitted in quick succession
 * (within `gapMs` of the last one sent) collapse into the last.
 */
export function searchDelay(lastSentAt: number | null, now: number, gapMs: number): number {
  if (lastSentAt === null) {
    return 0;
  }
  return Math.max(0, lastSentAt + gapMs - now);
}

/**
 * The results worth prefetching: the first `count` shown, without the best pick (the backend
 * preloads that one anyway) and those already prefetched (by key).
 */
export function prefetchRequests(
  results: readonly Pick<
    SearchResultDto,
    'key' | 'magnet' | 'download' | 'infoHash' | 'bestPick'
  >[],
  count: number,
  done: ReadonlySet<string>,
): PrefetchRequest[] {
  return results
    .slice(0, count)
    .filter((r) => !r.bestPick && !done.has(r.key))
    .map(({ key, magnet, download, infoHash }) => ({ key, magnet, download, infoHash }));
}

/**
 * The results to find thumbnails for: of the first `count` shown, those not tried yet whose
 * metadata is known, by key in `hashes` (prefetched; their info hash), and the best pick once
 * `bestReady` (its preload has the metadata).
 */
export function thumbnailRequests(
  results: readonly Pick<SearchResultDto, 'key' | 'infoHash' | 'bestPick'>[],
  count: number,
  hashes: ReadonlyMap<string, string>,
  tried: ReadonlySet<string>,
  bestReady: boolean,
): ThumbnailRequest[] {
  return results.slice(0, count).flatMap((r) => {
    if (tried.has(r.key)) {
      return [];
    }
    const infoHash = hashes.get(r.key) ?? (r.bestPick && bestReady ? r.infoHash : null);
    return infoHash === null ? [] : [{ key: r.key, infoHash }];
  });
}

/** Whether the webview can decode a result's video, so a frame of it can be shown. */
export function framePossible(quality: Pick<QualityDto, 'playable'>): boolean {
  return quality.playable === 'plays' || quality.playable === 'likely';
}

/** `map` with `key` set to `value`, keeping only the `max` most recently set. */
export function withRecentEntry<V>(
  map: ReadonlyMap<string, V>,
  key: string,
  value: V,
  max: number,
): Map<string, V> {
  const next = new Map(map);
  next.delete(key);
  next.set(key, value);
  for (const old of next.keys()) {
    if (next.size <= max) {
      break;
    }
    next.delete(old);
  }
  return next;
}

/** `set` with `item` added, keeping only the `max` most recently added. */
export function withRecent<T>(set: ReadonlySet<T>, item: T, max: number): Set<T> {
  const next = new Set(set);
  next.delete(item);
  next.add(item);
  for (const old of next) {
    if (next.size <= max) {
      break;
    }
    next.delete(old);
  }
  return next;
}

/**
 * The preload state of a result, for the line next to the best pick: `Preloading… 40%`,
 * `Ready to play instantly`, or null (not preloaded, failed, or another torrent).
 */
export function preloadLabel(
  status: Pick<PreloadStatusDto, 'infoHash' | 'phase' | 'progress'> | null,
  infoHash: string | null,
): string | null {
  if (status === null || infoHash === null || status.infoHash !== infoHash) {
    return null;
  }
  switch (status.phase) {
    case 'resolving':
      return 'Preloading…';
    case 'buffering':
      return `Preloading… ${String(Math.floor(Math.min(1, Math.max(0, status.progress)) * 100))}%`;
    case 'ready':
      return 'Ready to play instantly';
    default:
      return null;
  }
}

/** Whether a preload is still working, so its status is worth polling. */
export function preloadActive(status: Pick<PreloadStatusDto, 'phase'> | null): boolean {
  return status !== null && (status.phase === 'resolving' || status.phase === 'buffering');
}

/**
 * Short quality notes for a result's details line, warnings first: `Camera copy`,
 * `Too small for 4K`, `Needs the full player`, then the quality label (`1080p WEB-DL`).
 */
export function qualityNotes(quality: QualityDto): string[] {
  const notes: string[] = [];
  if (quality.camera) {
    notes.push('Camera copy');
  }
  if (quality.suspectSize) {
    notes.push('Too small for 4K');
  }
  if (quality.playable === 'needsFullPlayer') {
    notes.push('Needs the full player');
  }
  if (!quality.camera && quality.label !== '') {
    notes.push(quality.label);
  }
  return notes;
}
