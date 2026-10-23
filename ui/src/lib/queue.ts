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
import { torrentLoadKeys } from './torrent-loads';

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

/** The parts of an item that say which torrent it belongs to. */
type TorrentRef = Pick<QueueItem, 'source' | 'infoHash'>;
/** The parts of an item that say which file of which torrent it plays. */
type EntryRef = Pick<QueueItem, 'source' | 'infoHash' | 'fileIndex'>;

/**
 * What a `null` file index (a search result's "main video") plays in an item's torrent: the
 * index of the file it resolves to when the torrent's files are known, otherwise null.
 */
export type MainFile = (torrent: TorrentRef) => number | null;

/** For when no torrent's files are known. */
const UNKNOWN_MAIN: MainFile = () => null;

// Item keys, worked out once per item object (the queue compares every pair when merging).
const keyCache = new WeakMap<object, string[]>();

function keysOf(item: TorrentRef): string[] {
  let keys = keyCache.get(item);
  if (keys === undefined) {
    keys = torrentLoadKeys(item.source, item.infoHash);
    keyCache.set(item, keys);
  }
  return keys;
}

/**
 * Whether two items belong to the same torrent: by info hash when both know it, otherwise by
 * the hash in a magnet link, or by the link itself.
 */
export function sameTorrent(a: TorrentRef, b: TorrentRef): boolean {
  if (a.infoHash !== null && b.infoHash !== null) {
    return a.infoHash.toLowerCase() === b.infoHash.toLowerCase();
  }
  const keys = keysOf(a);
  return keysOf(b).some((key) => keys.includes(key));
}

/**
 * Whether two items play the same file of the same torrent. A `null` file index plays the
 * torrent's main video, so it matches that file's index once `mainFile` knows it (and only
 * then: in a season pack, episode 3 is not the main video).
 */
export function sameEntry(a: EntryRef, b: EntryRef, mainFile: MainFile = UNKNOWN_MAIN): boolean {
  if (!sameTorrent(a, b)) {
    return false;
  }
  if (a.fileIndex === b.fileIndex) {
    return true;
  }
  const main = mainFile(a) ?? mainFile(b);
  const first = a.fileIndex ?? main;
  return first !== null && first === (b.fileIndex ?? main);
}

/** Index of the cursor, or -1 if there is none (or it was removed). */
export function currentIndex(queue: Queue): number {
  return queue.current === null ? -1 : queue.items.findIndex((i) => i.id === queue.current);
}

/** Whether an item reached the end during playback. */
export function isPlayed(queue: Queue, id: string): boolean {
  return queue.items.find((i) => i.id === id)?.played === true;
}

/** Number of items after the cursor, still to play. */
export function upcomingCount(queue: Queue): number {
  return queue.items.slice(currentIndex(queue) + 1).filter((i) => !i.skipped).length;
}

/** The item to play after the current one (skipped items are passed over), or null at the end. */
export function nextItem(queue: Queue): QueueItem | null {
  return queue.items.slice(currentIndex(queue) + 1).find((i) => !i.skipped) ?? null;
}

/**
 * Which torrent an item belongs to: its info hash, the hash in its magnet link, or its link
 * when the hash is unknown.
 */
export function groupKey(item: TorrentRef): string {
  if (item.infoHash !== null) {
    return item.infoHash.toLowerCase();
  }
  const hash = keysOf(item).find((key) => key.startsWith('hash:'));
  return hash === undefined ? `link:${item.source}` : hash.slice('hash:'.length);
}

/**
 * Adds items, skipping any already queued (and anything beyond {@link MAX_QUEUE_ITEMS}).
 * An item of a torrent already in the queue joins its group: after the group's last item, or,
 * with `order`, before the first item of the group that `order` puts after it (for example in
 * lecture order). Other items go at the end. Returns the new queue and how many were added.
 * `mainFile` tells which file a `null` file index plays (see {@link sameEntry}).
 */
export function enqueue(
  queue: Queue,
  items: readonly QueueItem[],
  order?: (a: QueueItem, b: QueueItem) => number,
  mainFile: MainFile = UNKNOWN_MAIN,
): { queue: Queue; added: number } {
  const next = [...queue.items];
  let added = 0;
  for (const item of items) {
    if (next.length >= MAX_QUEUE_ITEMS || next.some((i) => sameEntry(i, item, mainFile))) {
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
  entry: EntryRef,
  mainFile: MainFile = UNKNOWN_MAIN,
): QueueItem | null {
  return queue.items.find((i) => sameEntry(i, entry, mainFile)) ?? null;
}

/** How an entry already in the queue relates to playback. */
export type QueueEntryState = 'playing' | 'upNext' | 'queued';

/** The visible queue state for an entry, or `null` when it is not queued. */
export function queueEntryState(
  queue: Queue,
  entry: EntryRef,
  playingId: string | null,
  mainFile: MainFile = UNKNOWN_MAIN,
): QueueEntryState | null {
  const item = findEntry(queue, entry, mainFile);
  if (item === null) {
    return null;
  }
  if (item.id === playingId) {
    return 'playing';
  }
  return nextItem(queue)?.id === item.id ? 'upNext' : 'queued';
}

/**
 * Puts the entry for `item`'s file first in the queue, adding `item` if that file is not queued:
 * its torrent's other items follow it, then everything else in order. Returns the entry's id,
 * or null (and the same queue) when the queue is full.
 */
export function putFirst(
  queue: Queue,
  item: QueueItem,
  mainFile: MainFile = UNKNOWN_MAIN,
): { queue: Queue; id: string | null } {
  const existing = findEntry(queue, item, mainFile);
  if (existing === null && queue.items.length >= MAX_QUEUE_ITEMS) {
    return { queue, id: null };
  }
  const entry = existing ?? item;
  const key = groupKey(entry);
  const others = queue.items.filter((i) => i !== entry);
  const items = [
    entry,
    ...others.filter((i) => groupKey(i) === key),
    ...others.filter((i) => groupKey(i) !== key),
  ];
  const unchanged =
    items.every((i, n) => i === queue.items[n]) && items.length === queue.items.length;
  return { queue: unchanged ? queue : { ...queue, items }, id: entry.id };
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

/**
 * Puts the cursor on an item (it is playing now), or clears it with `null`. Items jumped over
 * are marked skipped rather than played, and directly playing a skipped item includes it again.
 */
export function setCurrent(queue: Queue, id: string | null): Queue {
  if (id !== null && !queue.items.some((i) => i.id === id)) {
    return queue;
  }
  if (id === null) {
    return queue.current === null ? queue : { ...queue, current: null };
  }
  const from = currentIndex(queue);
  const to = queue.items.findIndex((i) => i.id === id);
  let changed = queue.current !== id;
  const items = queue.items.map((item, index) => {
    if (item.id === id && item.skipped) {
      changed = true;
      return { ...item, skipped: false };
    }
    if (from < index && index < to && !item.played && !item.skipped) {
      changed = true;
      return { ...item, skipped: true };
    }
    return item;
  });
  return changed ? { ...queue, items, current: id } : queue;
}

/** Records that a video reached its end, and includes it again if it had been skipped. */
export function markPlayed(queue: Queue, id: string): Queue {
  const item = queue.items.find((i) => i.id === id);
  if (item === undefined || (item.played && !item.skipped)) {
    return queue;
  }
  return {
    ...queue,
    items: queue.items.map((i) => (i.id === id ? { ...i, played: true, skipped: false } : i)),
  };
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

/** Removes only items that actually reached the end; passed-over skipped items stay. */
export function clearPlayed(queue: Queue): Queue {
  return queue.items
    .filter((item) => item.played)
    .reduce((next, item) => remove(next, item.id), queue);
}

/**
 * Merges items that play the same file of the same torrent (see {@link sameEntry}) into the
 * first of them, which keeps its place and takes the other's file index, info hash, and
 * progress (played if either was; skipped or paused only if both were). The cursor follows a
 * merged item. Returns the queue (the same object if nothing merged) and, for each removed
 * item, the id of the item it was merged into.
 */
export function mergeDuplicates(
  queue: Queue,
  mainFile: MainFile = UNKNOWN_MAIN,
): { queue: Queue; merged: Map<string, string> } {
  const kept: QueueItem[] = [];
  const merged = new Map<string, string>();
  for (const item of queue.items) {
    const at = kept.findIndex((k) => sameEntry(k, item, mainFile));
    const twin = kept[at];
    if (twin === undefined) {
      kept.push(item);
      continue;
    }
    kept[at] = {
      ...twin,
      fileIndex: twin.fileIndex ?? item.fileIndex,
      infoHash: twin.infoHash ?? item.infoHash,
      played: twin.played || item.played,
      skipped: twin.skipped && item.skipped,
      paused: twin.paused && item.paused,
    };
    merged.set(item.id, twin.id);
  }
  if (merged.size === 0) {
    return { queue, merged };
  }
  const current = queue.current === null ? null : (merged.get(queue.current) ?? queue.current);
  return { queue: { ...queue, items: kept, current }, merged };
}

/**
 * Once a torrent's details are known (`torrent`, with the link it was opened from): fills in
 * the info hash of its items that lacked one (a result offered only as a download link), and
 * merges the items that turn out to play the same file. See {@link mergeDuplicates}.
 */
export function resolveTorrent(
  queue: Queue,
  torrent: { source: string; infoHash: string },
  mainFile: MainFile,
): { queue: Queue; merged: Map<string, string> } {
  const infoHash = torrent.infoHash.toLowerCase();
  const items = queue.items.map((item) =>
    item.infoHash === null && sameTorrent(item, { source: torrent.source, infoHash: null })
      ? { ...item, infoHash }
      : item,
  );
  const filled = items.some((item, i) => item !== queue.items[i]);
  const result = mergeDuplicates(filled ? { ...queue, items } : queue, mainFile);
  return { queue: result.queue, merged: result.merged };
}

/**
 * A queue read back from the backend, checked again on this side: items without the fields
 * the queue needs are dropped, duplicates merged, and a dangling cursor cleared.
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
    played: (i.played as boolean | undefined) ?? false,
  }));
  const saved = 'current' in value && typeof value.current === 'string' ? value.current : null;
  // Older versions could queue one file twice (a search result's main video, then the same
  // file by its index; or one link with and one without the hash).
  const merged = mergeDuplicates({ ...emptyQueue(), items: valid, current: saved }).queue;
  const { queue } = enqueue(emptyQueue(), merged.items);
  const order: DownloadOrder =
    'order' in value && value.order === 'fastest' ? 'fastest' : 'inOrder';
  return setCurrent({ ...queue, order }, merged.current);
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
    (v.skipped === undefined || typeof v.skipped === 'boolean') &&
    (v.played === undefined || typeof v.played === 'boolean')
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
