// SPDX-License-Identifier: MIT OR Apache-2.0

/** Playback-library helpers and its event-driven subscription. */

import { listen } from '@tauri-apps/api/event';
import { formatClock } from './format';
import { LIBRARY_CHANGED_EVENT, libraryItems, type LibraryItem } from './ipc';

/** Stable key for a playable file. */
export function libraryKey(item: Pick<LibraryItem, 'infoHash' | 'fileIndex'>): string {
  return `${item.infoHash.toLowerCase()}:${String(item.fileIndex)}`;
}

/** Adds or replaces one item and keeps newest-first order. */
export function mergeLibrary(items: readonly LibraryItem[], changed: LibraryItem): LibraryItem[] {
  return [...items.filter((item) => libraryKey(item) !== libraryKey(changed)), changed].sort(
    (a, b) => b.updatedAtSecs - a.updatedAtSecs || libraryKey(a).localeCompare(libraryKey(b)),
  );
}

/** Watched fraction, clamped to 0–1 (zero until duration is known). */
export function watchedFraction(item: LibraryItem): number {
  if (item.completed) {
    return 1;
  }
  if (!(item.durationSecs !== null && item.durationSecs > 0)) {
    return 0;
  }
  return Math.min(1, Math.max(0, item.positionSecs / item.durationSecs));
}

/** Short queue label for saved watch progress. */
export function watchedLabel(item: LibraryItem | null | undefined): string | null {
  if (item?.completed === true) {
    return 'Watched';
  }
  if (item === null || item === undefined || item.positionSecs < 10) {
    return null;
  }
  const percent = Math.floor(watchedFraction(item) * 100);
  return item.durationSecs === null ? 'Started' : `${String(percent)}% watched`;
}

/** Position worth offering to resume, matching the backend's safety thresholds. */
export function resumePosition(item: LibraryItem): number | null {
  if (item.completed || !Number.isFinite(item.positionSecs) || item.positionSecs < 10) {
    return null;
  }
  if (
    item.durationSecs !== null &&
    (!Number.isFinite(item.durationSecs) ||
      item.durationSecs <= 0 ||
      item.positionSecs >= item.durationSecs - 10)
  ) {
    return null;
  }
  return item.positionSecs;
}

/**
 * Where a new stream starts, and the chip over the video that says so. The backend offers a
 * saved position only while "Resume where I left off" is on: then playback continues there
 * without asking, and the chip offers to start over. Otherwise it starts at the beginning.
 */
export function resumeStart(saved: number | null): { startAt: number; chip: string | null } {
  if (saved === null || !Number.isFinite(saved) || saved <= 0) {
    return { startAt: 0, chip: null };
  }
  return { startAt: saved, chip: `Continuing from ${formatClock(saved)}` };
}

export interface LibraryCourse {
  key: string;
  name: string;
  items: LibraryItem[];
}

/** Groups recent items into courses/torrents while preserving newest-first order. */
export function groupLibrary(items: readonly LibraryItem[]): LibraryCourse[] {
  const groups = new Map<string, LibraryCourse>();
  for (const item of items) {
    const key = item.infoHash.toLowerCase();
    const existing = groups.get(key);
    if (existing) {
      existing.items.push(item);
    } else {
      groups.set(key, { key, name: item.torrentName, items: [item] });
    }
  }
  return [...groups.values()];
}

/**
 * Reads the current library and follows individual changes until the returned function is
 * called. Changes that race the initial read are overlaid on its result. A failed read is tried
 * once more; if that fails too, the library starts empty and still follows changes.
 */
export function followLibrary(onItems: (items: LibraryItem[]) => void): () => void {
  let stopped = false;
  let current: LibraryItem[] | null = null;
  let pending: LibraryItem[] = [];
  let unlisten: (() => void) | null = null;
  let loading = false;
  let retried = false;
  const loadInitial = (): void => {
    if (stopped || loading) {
      return;
    }
    loading = true;
    libraryItems()
      .then((items) => {
        if (stopped) {
          return;
        }
        current = pending.reduce(mergeLibrary, items);
        pending = [];
        onItems(current);
      })
      .catch(() => {
        if (stopped) {
          return;
        }
        if (!retried) {
          retried = true;
          loading = false;
          loadInitial();
          return;
        }
        // Event delivery can still keep the view useful if the initial read alone failed;
        // without a base list, changes would wait in `pending` forever.
        current = pending.reduce(mergeLibrary, []);
        pending = [];
        onItems(current);
      });
  };
  listen<LibraryItem>(LIBRARY_CHANGED_EVENT, (event) => {
    if (stopped) {
      return;
    }
    if (current === null) {
      pending.push(event.payload);
      return;
    }
    current = mergeLibrary(current, event.payload);
    onItems(current);
  })
    .then((stop) => {
      if (stopped) {
        stop();
      } else {
        unlisten = stop;
        loadInitial();
      }
    })
    .catch(() => {
      loadInitial();
    })
    .catch(() => undefined);
  return () => {
    stopped = true;
    pending = [];
    unlisten?.();
  };
}
