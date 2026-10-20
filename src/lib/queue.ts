// SPDX-License-Identifier: MIT OR Apache-2.0

/**
 * The play queue: videos lined up to watch next, persisted across restarts.
 *
 * The queue is a playlist with a cursor. `current` is the item playing or last played; the
 * next item is the one after it (or the first, if there is no cursor). Played items stay
 * until removed, so the list reads like a playlist. Every operation returns a new queue.
 *
 * Items store only what is needed to play them again: the torrent's link, the file index,
 * a title, and a size. Never stream URLs (they carry a session token).
 */

import type { DownloadOrder } from './bindings/DownloadOrder';
import type { Queue } from './bindings/Queue';
import type { FileSummary } from './bindings/FileSummary';
import { isPlayable } from './files';
import type { QueueItem } from './bindings/QueueItem';

export type { DownloadOrder, Queue, QueueItem };

/** The most items a queue holds (the backend enforces the same limit). */
export const MAX_QUEUE_ITEMS = 500;

export function emptyQueue(): Queue {
  return { items: [], current: null, order: 'inOrder' };
}

/** A new, unique item id. */
export function newItemId(): string {
  return globalThis.crypto.randomUUID();
}

/** Whether two items play the same file of the same torrent. */
export function sameEntry(
  a: Pick<QueueItem, 'source' | 'infoHash' | 'fileIndex'>,
  b: Pick<QueueItem, 'source' | 'infoHash' | 'fileIndex'>,
): boolean {
  const sameTorrent =
    a.infoHash !== null && b.infoHash !== null
      ? a.infoHash.toLowerCase() === b.infoHash.toLowerCase()
      : a.source === b.source;
  return sameTorrent && a.fileIndex === b.fileIndex;
}

/** Index of the cursor, or -1 if there is none (or it was removed). */
export function currentIndex(queue: Queue): number {
  return queue.current === null ? -1 : queue.items.findIndex((i) => i.id === queue.current);
}

/** Whether an item comes before the cursor (it has been played). */
export function isPlayed(queue: Queue, id: string): boolean {
  const at = queue.items.findIndex((i) => i.id === id);
  return at >= 0 && at < currentIndex(queue);
}

/** Number of items after the cursor, still to play. */
export function upcomingCount(queue: Queue): number {
  return queue.items.slice(currentIndex(queue) + 1).filter((i) => !i.skipped).length;
}

/** The item to play after the current one (skipped items are passed over), or null at the end. */
export function nextItem(queue: Queue): QueueItem | null {
  return queue.items.slice(currentIndex(queue) + 1).find((i) => !i.skipped) ?? null;
}

/** Which torrent an item belongs to: its info hash, or its link when the hash is unknown. */
export function groupKey(item: Pick<QueueItem, 'infoHash' | 'source'>): string {
  return item.infoHash === null ? `link:${item.source}` : item.infoHash.toLowerCase();
}

/**
 * Adds items, skipping any already queued (and anything beyond {@link MAX_QUEUE_ITEMS}).
 * An item of a torrent already in the queue joins its group: after the group's last item, or,
 * with `order`, before the first item of the group that `order` puts after it (for example in
 * lecture order). Other items go at the end. Returns the new queue and how many were added.
 */
export function enqueue(
  queue: Queue,
  items: readonly QueueItem[],
  order?: (a: QueueItem, b: QueueItem) => number,
): { queue: Queue; added: number } {
  const next = [...queue.items];
  let added = 0;
  for (const item of items) {
    if (next.length >= MAX_QUEUE_ITEMS || next.some((i) => sameEntry(i, item))) {
      continue;
    }
    const key = groupKey(item);
    const last = lastIndexOf(next, (i) => groupKey(i) === key);
    if (last < 0) {
      next.push(item);
    } else {
      const first = next.findIndex((i) => groupKey(i) === key);
      const before = order ? next.slice(first, last + 1).findIndex((i) => order(item, i) < 0) : -1;
      next.splice(before < 0 ? last + 1 : first + before, 0, item);
    }
    added += 1;
  }
  return { queue: added === 0 ? queue : { ...queue, items: next }, added };
}

/** A run of queued items from one torrent. */
export interface QueueGroup {
  /** {@link groupKey} of its items. */
  key: string;
  items: QueueItem[];
  /** Index of its first item in the queue. */
  start: number;
}

/** The queue as groups: consecutive items from the same torrent. */
export function groupQueue(queue: Queue): QueueGroup[] {
  const groups: QueueGroup[] = [];
  queue.items.forEach((item, index) => {
    const key = groupKey(item);
    const last = groups.at(-1);
    if (last?.key === key) {
      last.items.push(item);
    } else {
      groups.push({ key, items: [item], start: index });
    }
  });
  return groups;
}

/**
 * Puts each torrent's items together, at the place of its first item, keeping every other
 * order (a queue saved by an older version may have them apart).
 */
export function keepGroupsTogether(items: readonly QueueItem[]): QueueItem[] {
  const byKey = new Map<string, QueueItem[]>();
  for (const item of items) {
    const key = groupKey(item);
    const group = byKey.get(key);
    if (group) {
      group.push(item);
    } else {
      byKey.set(key, [item]);
    }
  }
  return [...byKey.values()].flat();
}

/** The queued item for this file of this torrent, if any. */
export function findEntry(
  queue: Queue,
  entry: Pick<QueueItem, 'source' | 'infoHash' | 'fileIndex'>,
): QueueItem | null {
  return queue.items.find((i) => sameEntry(i, entry)) ?? null;
}

/**
 * Removes an item. Removing the cursor moves it to the previous item, so the item that
 * followed is still next.
 */
export function remove(queue: Queue, id: string): Queue {
  const at = queue.items.findIndex((i) => i.id === id);
  if (at < 0) {
    return queue;
  }
  const items = queue.items.filter((i) => i.id !== id);
  const current = queue.current === id ? (queue.items[at - 1]?.id ?? null) : queue.current;
  return { ...queue, items, current };
}

/**
 * Moves an item to `index`, clamped to its torrent's group (groups move as a whole with
 * {@link moveGroup}), keeping the cursor on the same item.
 */
export function move(queue: Queue, id: string, index: number): Queue {
  const from = queue.items.findIndex((i) => i.id === id);
  const item = queue.items[from];
  if (item === undefined) {
    return queue;
  }
  const group = groupQueue(queue).find((g) => g.start <= from && from < g.start + g.items.length);
  const [low, high] = group
    ? [group.start, group.start + group.items.length - 1]
    : [0, queue.items.length - 1];
  const to = Math.min(Math.max(Math.trunc(index), low), high);
  if (to === from) {
    return queue;
  }
  const items = queue.items.filter((i) => i.id !== id);
  items.splice(to, 0, item);
  return { ...queue, items };
}

/** Moves a whole group (by {@link groupKey}) to position `index` among the groups (clamped). */
export function moveGroup(queue: Queue, key: string, index: number): Queue {
  const groups = groupQueue(queue);
  const from = groups.findIndex((g) => g.key === key);
  const group = groups[from];
  if (group === undefined) {
    return queue;
  }
  const to = Math.min(Math.max(Math.trunc(index), 0), groups.length - 1);
  if (to === from) {
    return queue;
  }
  const others = groups.filter((g) => g !== group);
  others.splice(to, 0, group);
  return { ...queue, items: others.flatMap((g) => g.items) };
}

/** Moves an item up (negative) or down (positive) by `delta` places. */
export function moveBy(queue: Queue, id: string, delta: number): Queue {
  const from = queue.items.findIndex((i) => i.id === id);
  return from < 0 ? queue : move(queue, id, from + delta);
}

/** Puts the cursor on an item (it is playing now), or clears it with null. */
export function setCurrent(queue: Queue, id: string | null): Queue {
  if (id !== null && !queue.items.some((i) => i.id === id)) {
    return queue;
  }
  return queue.current === id ? queue : { ...queue, current: id };
}

/**
 * Pauses or resumes background downloading of the items in `ids` (playing them still works).
 * Returns the same queue if nothing changed.
 */
export function setPaused(queue: Queue, ids: readonly string[], paused: boolean): Queue {
  const wanted = new Set(ids);
  if (!queue.items.some((i) => wanted.has(i.id) && i.paused !== paused)) {
    return queue;
  }
  return {
    ...queue,
    items: queue.items.map((i) => (wanted.has(i.id) && i.paused !== paused ? { ...i, paused } : i)),
  };
}

/**
 * Skips (unticks) or restores the items in `ids`: skipped items stay in the queue but are not
 * played next or downloaded in the background. Returns the same queue if nothing changed.
 */
export function setSkipped(queue: Queue, ids: readonly string[], skipped: boolean): Queue {
  const wanted = new Set(ids);
  if (!queue.items.some((i) => wanted.has(i.id) && i.skipped !== skipped)) {
    return queue;
  }
  return {
    ...queue,
    items: queue.items.map((i) =>
      wanted.has(i.id) && i.skipped !== skipped ? { ...i, skipped } : i,
    ),
  };
}

/** Sets how spare bandwidth is shared between queued items. */
export function setOrder(queue: Queue, order: DownloadOrder): Queue {
  return queue.order === order ? queue : { ...queue, order };
}

/** Removes the played items before the cursor. */
export function clearPlayed(queue: Queue): Queue {
  const at = currentIndex(queue);
  return at <= 0 ? queue : { ...queue, items: queue.items.slice(at) };
}

/**
 * A queue read back from the backend, checked again on this side: items without the fields
 * the queue needs are dropped, duplicates removed, and a dangling cursor cleared.
 */
export function sanitizeQueue(value: unknown): Queue {
  if (typeof value !== 'object' || value === null || !('items' in value)) {
    return emptyQueue();
  }
  const raw = Array.isArray(value.items) ? (value.items as unknown[]) : [];
  // Items saved before pausing existed have no flag.
  const valid = raw.filter(isQueueItem).map((i) => ({
    ...i,
    paused: (i.paused as boolean | undefined) ?? false,
    skipped: (i.skipped as boolean | undefined) ?? false,
  }));
  const { queue } = enqueue(emptyQueue(), valid);
  const current = 'current' in value && typeof value.current === 'string' ? value.current : null;
  const order: DownloadOrder =
    'order' in value && value.order === 'fastest' ? 'fastest' : 'inOrder';
  return setCurrent({ ...queue, order }, current);
}

function isQueueItem(value: unknown): value is QueueItem {
  if (typeof value !== 'object' || value === null) {
    return false;
  }
  const v = value as Record<string, unknown>;
  const optional = (x: unknown, type: 'string' | 'number'): boolean =>
    x === null || typeof x === type;
  return (
    typeof v.id === 'string' &&
    v.id !== '' &&
    typeof v.source === 'string' &&
    v.source !== '' &&
    typeof v.title === 'string' &&
    optional(v.infoHash, 'string') &&
    optional(v.fileIndex, 'number') &&
    optional(v.torrentName, 'string') &&
    optional(v.sizeBytes, 'number') &&
    (v.paused === undefined || typeof v.paused === 'boolean') &&
    (v.skipped === undefined || typeof v.skipped === 'boolean')
  );
}

/** Longest title kept, in characters (the backend accepts up to 1024). */
const TITLE_CHARS = 500;

/**
 * A title the backend accepts: control characters (tabs, line breaks) become spaces, and
 * very long names are shortened. Torrent names are untrusted text.
 */
export function clipTitle(text: string): string {
  // eslint-disable-next-line no-control-regex -- removing control characters is the point
  const clean = text.replace(/[\u0000-\u001f\u007f-\u009f]+/g, ' ').trim();
  const chars = Array.from(clean);
  const clipped =
    chars.length > TITLE_CHARS ? `${chars.slice(0, TITLE_CHARS - 1).join('')}…` : clean;
  return clipped === '' ? 'Untitled' : clipped;
}

/**
 * Saves the queue after changes settle, one save at a time, always ending with the latest
 * queue (a save that fails is reported and the next change tries again).
 */
export class QueueSaver {
  readonly #save: (queue: Queue) => Promise<void>;
  readonly #onError: (reason: unknown) => void;
  readonly #delayMs: number;
  #pending: Queue | null = null;
  #timer: ReturnType<typeof setTimeout> | undefined;
  #chain: Promise<void> = Promise.resolve();

  constructor(
    save: (queue: Queue) => Promise<void>,
    onError: (reason: unknown) => void,
    delayMs = 300,
  ) {
    this.#save = save;
    this.#onError = onError;
    this.#delayMs = delayMs;
  }

  /** Saves `queue` soon, replacing any queue not saved yet. */
  schedule(queue: Queue): void {
    this.#pending = queue;
    clearTimeout(this.#timer);
    this.#timer = setTimeout(() => void this.flush(), this.#delayMs);
  }

  /** Saves the pending queue now; resolves when every save started so far has finished. */
  flush(): Promise<void> {
    clearTimeout(this.#timer);
    this.#timer = undefined;
    this.#chain = this.#chain.then(async () => {
      const queue = this.#pending;
      this.#pending = null;
      if (queue !== null) {
        try {
          await this.#save(queue);
        } catch (reason: unknown) {
          this.#onError(reason);
        }
      }
    });
    return this.#chain;
  }
}

function lastIndexOf<T>(items: readonly T[], test: (item: T) => boolean): number {
  for (let i = items.length - 1; i >= 0; i -= 1) {
    const item = items[i];
    if (item !== undefined && test(item)) {
      return i;
    }
  }
  return -1;
}

/** The items passing `test` first, then the rest; each part keeps its order. */
function readyFirst<T>(items: readonly T[], test: (item: T) => boolean): T[] {
  return [...items.filter(test), ...items.filter((i) => !test(i))];
}

/**
 * "Play ready videos first": moves queued items that are ready to play (`ready`, by item id)
 * ahead of those that are not, among the items still to play. Played items and the cursor
 * (the video playing) never move, and the rest of the cursor's torrent stays right after it.
 * Torrents move as whole groups, by whether their first item is ready; with
 * `keepCourseOrder` off, the ready files of a torrent also move to the front of its group.
 * Every other order is kept. Returns the queue (the same object if nothing moved) and the
 * ids of the ready items that moved up.
 */
export function optimizeQueue(
  queue: Queue,
  ready: ReadonlySet<string>,
  keepCourseOrder: boolean,
): { queue: Queue; moved: string[] } {
  const isReady = (item: QueueItem): boolean => ready.has(item.id);
  const within = (items: QueueItem[]): QueueItem[] =>
    keepCourseOrder ? items : readyFirst(items, isReady);
  const cursor = currentIndex(queue);
  const fixed = queue.items.slice(0, cursor + 1);
  const rest = queue.items.slice(cursor + 1);
  const current = queue.items[cursor];
  const pinnedKey = current === undefined ? null : groupKey(current);
  let pinned = 0;
  for (const item of rest) {
    if (groupKey(item) !== pinnedKey) {
      break;
    }
    pinned += 1;
  }
  const groups = groupQueue({ ...queue, items: rest.slice(pinned), current: null }).map((g) =>
    within(g.items),
  );
  const ordered = readyFirst(groups, (g) => g[0] !== undefined && isReady(g[0]));
  const items = [...fixed, ...within(rest.slice(0, pinned)), ...ordered.flat()];
  if (items.every((item, i) => item === queue.items[i])) {
    return { queue, moved: [] };
  }
  const before = new Map(queue.items.map((item, i) => [item.id, i]));
  const moved = items
    .filter((item, i) => isReady(item) && i < (before.get(item.id) ?? i))
    .map((item) => item.id);
  return { queue: { ...queue, items }, moved };
}

/**
 * Undo for a reordering: puts the items back in `before`'s order. Items added since keep
 * their group (or go at the end), items removed since stay removed, and the cursor stays.
 */
export function restoreOrder(queue: Queue, before: Queue): Queue {
  const now = new Map(queue.items.map((item) => [item.id, item]));
  const kept = before.items.filter((item) => now.has(item.id)).map((item) => now.get(item.id));
  const restored = kept.filter((item): item is QueueItem => item !== undefined);
  const known = new Set(restored.map((item) => item.id));
  const added = queue.items.filter((item) => !known.has(item.id));
  return { ...queue, items: keepGroupsTogether([...restored, ...added]) };
}

/** One row under a torrent in the queue's tree: a video, and its queue item if it is queued. */
export interface TreeRow {
  /** The torrent's file, if its files are known. */
  file: FileSummary | null;
  /** The queue item playing this file, if it is queued. */
  item: QueueItem | null;
}

const lectureNames = new Intl.Collator(undefined, { numeric: true, sensitivity: 'base' });

/**
 * The rows under a queue group: every video of its torrent in lecture order (2 before 10),
 * each with its queue item if queued, when the torrent's files are known (`files`); otherwise
 * only the queued items. Queued items that are not one of those videos (a search result's
 * "main video") come first.
 */
export function groupTree(group: QueueGroup, files: readonly FileSummary[] | null): TreeRow[] {
  if (files === null) {
    return group.items.map((item) => ({ file: null, item }));
  }
  const videos = files.filter(isPlayable).sort((a, b) => lectureNames.compare(a.path, b.path));
  const rows = videos.map((file) => ({
    file,
    item: group.items.find((i) => i.fileIndex === file.index) ?? null,
  }));
  const loose = group.items
    .filter((i) => !videos.some((f) => f.index === i.fileIndex))
    .map((item) => ({ file: null, item }));
  return [...loose, ...rows];
}
