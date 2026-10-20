// SPDX-License-Identifier: MIT OR Apache-2.0

import { afterEach, describe, expect, it, vi } from 'vitest';
import type { FileSummary } from './bindings/FileSummary';
import {
  MAX_QUEUE_ITEMS,
  QueueSaver,
  clearPlayed,
  clipTitle,
  currentIndex,
  emptyQueue,
  enqueue,
  findEntry,
  groupKey,
  groupQueue,
  groupTree,
  keepGroupsTogether,
  moveGroup,
  optimizeQueue,
  putFirst,
  restoreOrder,
  isPlayed,
  move,
  moveBy,
  nextItem,
  remove,
  sameEntry,
  sanitizeQueue,
  setCurrent,
  setOrder,
  setPaused,
  setSkipped,
  upcomingCount,
  type Queue,
  type QueueItem,
} from './queue';

const HASH = 'dd8255ecdc7ca55fb0bbf81323d87062db1f6d1c';

function item(
  id: string,
  fileIndex: number | null = null,
  infoHash: string | null = HASH,
): QueueItem {
  return {
    id,
    source: `magnet:?xt=urn:btih:${HASH}`,
    infoHash,
    fileIndex,
    title: `Lecture ${id}`,
    torrentName: 'Course',
    sizeBytes: 1000,
    paused: false,
    skipped: false,
  };
}

function queueOf(...ids: string[]): Queue {
  return enqueue(
    emptyQueue(),
    ids.map((id, i) => item(id, i)),
  ).queue;
}

const ids = (q: Queue): string[] => q.items.map((i) => i.id);

describe('adding', () => {
  it('appends new items and skips ones already queued', () => {
    const q = queueOf('a', 'b');
    const again = enqueue(q, [item('c', 0), item('d', 9)]);
    expect(again.added).toBe(1);
    expect(ids(again.queue)).toEqual(['a', 'b', 'd']);
    expect(enqueue(q, [item('x', 1)]).queue).toBe(q);
  });

  it('matches entries by info hash and file, or by link when the hash is unknown', () => {
    expect(sameEntry(item('a', 1), { ...item('b', 1), source: 'https://other/x.torrent' })).toBe(
      true,
    );
    expect(sameEntry(item('a', 1), item('b', 2))).toBe(false);
    expect(sameEntry(item('a', null, null), item('b', null, null))).toBe(true);
    expect(
      sameEntry(item('a', null, null), { ...item('b', null, null), source: 'https://x/y' }),
    ).toBe(false);
    expect(findEntry(queueOf('a', 'b'), item('z', 1))?.id).toBe('b');
  });

  it('stops at the item limit', () => {
    const many = Array.from({ length: MAX_QUEUE_ITEMS + 5 }, (_, i) => item(`i${String(i)}`, i));
    const { queue, added } = enqueue(emptyQueue(), many);
    expect(added).toBe(MAX_QUEUE_ITEMS);
    expect(queue.items).toHaveLength(MAX_QUEUE_ITEMS);
  });
});

describe('the cursor', () => {
  it('starts at the first item and moves along as items play', () => {
    let q = queueOf('a', 'b', 'c');
    expect(currentIndex(q)).toBe(-1);
    expect(nextItem(q)?.id).toBe('a');
    expect(upcomingCount(q)).toBe(3);
    q = setCurrent(q, 'b');
    expect(nextItem(q)?.id).toBe('c');
    expect(upcomingCount(q)).toBe(1);
    expect(isPlayed(q, 'a')).toBe(true);
    expect(isPlayed(q, 'b')).toBe(false);
    q = setCurrent(q, 'c');
    expect(nextItem(q)).toBeNull();
    expect(setCurrent(q, 'nope')).toBe(q);
  });

  it('keeps the next item next when the current one is removed', () => {
    let q = setCurrent(queueOf('a', 'b', 'c'), 'b');
    q = remove(q, 'b');
    expect(q.current).toBe('a');
    expect(nextItem(q)?.id).toBe('c');
    q = remove(setCurrent(q, 'a'), 'a');
    expect(q.current).toBeNull();
    expect(nextItem(q)?.id).toBe('c');
    expect(remove(q, 'missing')).toBe(q);
  });

  it('clears the played items before the cursor', () => {
    const q = setCurrent(queueOf('a', 'b', 'c'), 'b');
    expect(ids(clearPlayed(q))).toEqual(['b', 'c']);
    expect(clearPlayed(queueOf('a'))).toEqual(queueOf('a'));
  });
});

describe('reordering', () => {
  it('moves items, clamped to the ends, keeping the cursor on its item', () => {
    const q = setCurrent(queueOf('a', 'b', 'c', 'd'), 'b');
    expect(ids(move(q, 'd', 0))).toEqual(['d', 'a', 'b', 'c']);
    expect(ids(move(q, 'a', 99))).toEqual(['b', 'c', 'd', 'a']);
    expect(ids(moveBy(q, 'c', -1))).toEqual(['a', 'c', 'b', 'd']);
    expect(ids(moveBy(q, 'a', -1))).toEqual(ids(q));
    expect(move(q, 'b', 1)).toBe(q);
    const moved = move(q, 'd', 0);
    expect(moved.current).toBe('b');
    expect(nextItem(moved)?.id).toBe('c');
  });
});

describe('skipping', () => {
  it('keeps skipped items but passes over them for up next', () => {
    const q = queueFrom([item('a', 1), item('b', 2), item('c', 3)], 'a');
    const skipped = setSkipped(q, ['b'], true);
    expect(skipped.items.map((i) => i.id)).toEqual(['a', 'b', 'c']);
    expect(nextItem(skipped)?.id).toBe('c');
    expect(upcomingCount(skipped)).toBe(1);
    expect(setSkipped(skipped, ['b'], true)).toBe(skipped);
    expect(nextItem(setSkipped(skipped, ['b'], false))?.id).toBe('b');
    expect(nextItem(setSkipped(q, ['b', 'c'], true))).toBeNull();
  });

  it('reads the flag back, defaulting for older queues', () => {
    const saved = setSkipped(queueFrom([item('a', 1), item('b', 2)]), ['b'], true);
    const back = sanitizeQueue(JSON.parse(JSON.stringify(saved)));
    expect(back.items.map((i) => i.skipped)).toEqual([false, true]);
    const old = { items: [{ ...item('a'), skipped: undefined }], current: null };
    expect(sanitizeQueue(JSON.parse(JSON.stringify(old))).items[0]?.skipped).toBe(false);
  });
});

describe('background downloads', () => {
  it('pauses and resumes items, and returns the same queue when nothing changes', () => {
    const q = queueFrom([item('a'), item('b'), item('c')], 'a');
    const paused = setPaused(q, ['a', 'c'], true);
    expect(paused.items.map((i) => i.paused)).toEqual([true, false, true]);
    expect(paused.current).toBe('a');
    expect(setPaused(paused, ['a'], true)).toBe(paused);
    expect(setPaused(paused, ['a', 'c'], false).items.every((i) => !i.paused)).toBe(true);
    expect(setPaused(q, ['gone'], true)).toBe(q);
  });

  it('sets the download order, kept by every queue operation', () => {
    const q = setOrder(queueFrom([item('a'), item('b')]), 'fastest');
    expect(q.order).toBe('fastest');
    expect(setOrder(q, 'fastest')).toBe(q);
    expect(remove(q, 'a').order).toBe('fastest');
    expect(moveBy(q, 'a', 1).order).toBe('fastest');
  });

  it('reads pauses and the order back, defaulting for older queues', () => {
    const saved = {
      ...setPaused(queueFrom([item('a', 1), item('b', 2)]), ['b'], true),
      order: 'fastest',
    };
    const back = sanitizeQueue(JSON.parse(JSON.stringify(saved)));
    expect(back.items.map((i) => i.paused)).toEqual([false, true]);
    expect(back.order).toBe('fastest');
    const old = { items: [{ ...item('a'), paused: undefined }], current: null };
    const fromOld = sanitizeQueue(JSON.parse(JSON.stringify(old)));
    expect(fromOld.items[0]?.paused).toBe(false);
    expect(fromOld.order).toBe('inOrder');
    expect(sanitizeQueue({ items: [{ ...item('a'), paused: 'yes' }] }).items).toEqual([]);
  });
});

describe('persistence', () => {
  afterEach(() => {
    vi.useRealTimers();
  });

  it('drops invalid items read back and a dangling cursor', () => {
    const raw = {
      items: [item('a', 0), { id: 'b' }, item('a', 0), 'junk', item('c', 1)],
      current: 'b',
    };
    const q = sanitizeQueue(raw);
    expect(ids(q)).toEqual(['a', 'c']);
    expect(q.current).toBeNull();
    expect(sanitizeQueue(null)).toEqual(emptyQueue());
    expect(sanitizeQueue({ items: 'x' })).toEqual(emptyQueue());
    expect(sanitizeQueue({ items: [item('a', 0)], current: 'a' }).current).toBe('a');
  });

  it('saves the latest queue once changes settle, one save at a time', async () => {
    vi.useFakeTimers();
    const saved: string[][] = [];
    let release: (() => void) | undefined;
    const save = vi.fn(
      (q: Queue) =>
        new Promise<void>((resolve) => {
          saved.push(ids(q));
          release = resolve;
        }),
    );
    const saver = new QueueSaver(save, () => undefined, 100);
    saver.schedule(queueOf('a'));
    saver.schedule(queueOf('a', 'b'));
    await vi.advanceTimersByTimeAsync(100);
    expect(saved).toEqual([['a', 'b']]);
    saver.schedule(queueOf('c'));
    await vi.advanceTimersByTimeAsync(100);
    expect(save).toHaveBeenCalledTimes(1);
    release?.();
    await vi.advanceTimersByTimeAsync(0);
    expect(saved).toEqual([['a', 'b'], ['c']]);
    release?.();
    await saver.flush();
    expect(save).toHaveBeenCalledTimes(2);
  });

  it('reports failed saves', async () => {
    const onError = vi.fn();
    const saver = new QueueSaver(() => Promise.reject(new Error('disk full')), onError);
    saver.schedule(queueOf('a'));
    await saver.flush();
    expect(onError).toHaveBeenCalledWith(new Error('disk full'));
  });

  it('makes titles the backend accepts', () => {
    expect(clipTitle('Lecture\t1\nIntro')).toBe('Lecture 1 Intro');
    expect(clipTitle('  \u0007 ')).toBe('Untitled');
    const long = clipTitle('é'.repeat(2000));
    expect(Array.from(long)).toHaveLength(500);
    expect(long.endsWith('…')).toBe(true);
  });
});

/** An item of torrent `torrent` (a letter): ids like `a1`, `b2`. */
function of(torrent: string, file: number): QueueItem {
  return { ...item(`${torrent}${String(file)}`, file, torrent.repeat(40)), torrentName: torrent };
}

function queueFrom(items: QueueItem[], current: string | null = null): Queue {
  return { items, current, order: 'inOrder' };
}

describe('groups', () => {
  it('keeps a torrent’s files together and adds new ones to its group', () => {
    let q = queueFrom([of('a', 1), of('b', 1)]);
    q = enqueue(q, [of('a', 2), of('c', 1), of('b', 2)]).queue;
    expect(ids(q)).toEqual(['a1', 'a2', 'b1', 'b2', 'c1']);
    expect(groupQueue(q).map((g) => [g.key, g.start, g.items.length])).toEqual([
      ['a'.repeat(40), 0, 2],
      ['b'.repeat(40), 2, 2],
      ['c'.repeat(40), 4, 1],
    ]);
  });

  it('adds files in lecture order when asked', () => {
    const lectureOrder = (x: QueueItem, y: QueueItem): number =>
      (x.fileIndex ?? 0) - (y.fileIndex ?? 0);
    let q = queueFrom([of('a', 1), of('a', 3), of('b', 1)]);
    q = enqueue(q, [of('a', 2), of('a', 9)], lectureOrder).queue;
    expect(ids(q)).toEqual(['a1', 'a2', 'a3', 'a9', 'b1']);
  });

  it('puts a file played from elsewhere first, adding it or moving it with its torrent', () => {
    const q = queueFrom([of('a', 1), of('b', 1), of('b', 2), of('c', 1)], 'b1');
    // Not queued: added in front, the rest untouched.
    const added = putFirst(q, of('d', 1));
    expect(added.id).toBe('d1');
    expect(ids(added.queue)).toEqual(['d1', 'a1', 'b1', 'b2', 'c1']);
    expect(added.queue.current).toBe('b1');
    // Queued: moved to the front with its torrent's other files after it.
    const moved = putFirst(q, { ...of('b', 2), id: 'new' });
    expect(moved.id).toBe('b2');
    expect(ids(moved.queue)).toEqual(['b2', 'b1', 'a1', 'c1']);
    // Already first: the same queue.
    const first = queueFrom([of('a', 1), of('b', 1)]);
    expect(putFirst(first, { ...of('a', 1), id: 'other' }).queue).toBe(first);
  });

  it('leaves a full queue as it is when the file is not queued', () => {
    const full = queueFrom(
      Array.from({ length: MAX_QUEUE_ITEMS }, (_, i) => of(String(i).padStart(4, '0'), 1)),
    );
    expect(putFirst(full, of('z', 1))).toEqual({ queue: full, id: null });
  });

  it('groups by link when the info hash is unknown', () => {
    const link = (id: string): QueueItem => ({
      ...item(id, null, null),
      source: 'https://search.example/download/x',
    });
    expect(groupKey(link('x'))).toBe('link:https://search.example/download/x');
    expect(groupKey(of('a', 1))).toBe('a'.repeat(40));
    expect(groupKey({ ...of('a', 1), infoHash: 'A'.repeat(40) })).toBe('a'.repeat(40));
  });

  it('moves files only within their group, and groups as a whole', () => {
    const q = queueFrom([of('a', 1), of('a', 2), of('b', 1), of('b', 2), of('c', 1)], 'b1');
    expect(ids(move(q, 'b2', 0)), 'clamped to the group').toEqual(['a1', 'a2', 'b2', 'b1', 'c1']);
    expect(ids(move(q, 'a1', 99))).toEqual(['a2', 'a1', 'b1', 'b2', 'c1']);
    expect(ids(moveGroup(q, 'c'.repeat(40), 0))).toEqual(['c1', 'a1', 'a2', 'b1', 'b2']);
    expect(ids(moveGroup(q, 'a'.repeat(40), 1))).toEqual(['b1', 'b2', 'a1', 'a2', 'c1']);
    expect(moveGroup(q, 'a'.repeat(40), 0)).toBe(q);
    expect(moveGroup(q, 'nope', 0)).toBe(q);
    expect(moveGroup(q, 'c'.repeat(40), 0).current, 'the cursor stays on its item').toBe('b1');
  });

  it('puts apart files back together when a queue is loaded', () => {
    const apart = [of('a', 1), of('b', 1), of('a', 2), of('c', 1), of('b', 2)];
    expect(keepGroupsTogether(apart).map((i) => i.id)).toEqual(['a1', 'a2', 'b1', 'b2', 'c1']);
    expect(ids(sanitizeQueue({ items: apart, current: null }))).toEqual([
      'a1',
      'a2',
      'b1',
      'b2',
      'c1',
    ]);
  });
});

describe('play ready videos first', () => {
  const ready = (...list: string[]): ReadonlySet<string> => new Set(list);

  it('moves ready torrents up, keeping every other order', () => {
    const q = queueFrom([of('a', 1), of('b', 1), of('c', 1), of('d', 1)]);
    const { queue, moved } = optimizeQueue(q, ready('c1', 'd1'), true);
    expect(ids(queue)).toEqual(['c1', 'd1', 'a1', 'b1']);
    expect(moved).toEqual(['c1', 'd1']);
    expect(optimizeQueue(queue, ready('c1', 'd1'), true).queue, 'stable').toBe(queue);
    expect(optimizeQueue(q, ready(), true).queue).toBe(q);
  });

  it('never moves played items, the cursor, or the rest of the torrent playing', () => {
    const q = queueFrom(
      [of('a', 1), of('b', 1), of('b', 2), of('b', 3), of('c', 1), of('d', 1)],
      'b1',
    );
    const { queue, moved } = optimizeQueue(q, ready('a1', 'd1', 'b3'), true);
    expect(ids(queue)).toEqual(['a1', 'b1', 'b2', 'b3', 'd1', 'c1']);
    expect(moved).toEqual(['d1']);
  });

  it('keeps course order by default, and reorders within a torrent when it is off', () => {
    const q = queueFrom([of('a', 1), of('b', 1), of('b', 2), of('b', 3)]);
    // b3 is ready but b1 is not: with course order kept, b stays behind a.
    expect(ids(optimizeQueue(q, ready('b3'), true).queue)).toEqual(['a1', 'b1', 'b2', 'b3']);
    const loose = optimizeQueue(q, ready('b3'), false);
    expect(ids(loose.queue)).toEqual(['b3', 'b1', 'b2', 'a1']);
    expect(loose.moved).toEqual(['b3']);

    const playing = queueFrom([of('a', 1), of('a', 2), of('a', 3), of('b', 1)], 'a1');
    expect(ids(optimizeQueue(playing, ready('a3'), false).queue)).toEqual(['a1', 'a3', 'a2', 'b1']);
    expect(ids(optimizeQueue(playing, ready('a3'), true).queue)).toEqual(['a1', 'a2', 'a3', 'b1']);
  });

  it('undoes a reordering, keeping what changed since', () => {
    const before = queueFrom([of('a', 1), of('b', 1), of('c', 1)], 'a1');
    const { queue: after } = optimizeQueue(before, ready('c1'), true);
    expect(ids(after)).toEqual(['a1', 'c1', 'b1']);
    expect(restoreOrder(after, before)).toEqual(before);

    // Since then: b1 was removed and a2 and d1 were queued.
    const changed = enqueue(remove(after, 'b1'), [of('a', 2), of('d', 1)]).queue;
    const undone = restoreOrder(changed, before);
    expect(ids(undone)).toEqual(['a1', 'a2', 'c1', 'd1']);
    expect(undone.current).toBe('a1');
  });
});

describe('groupTree', () => {
  const video = (index: number, path: string): FileSummary => ({
    index,
    path,
    length: 1,
    kind: 'video',
  });
  const files: FileSummary[] = [
    video(0, 'C/Lecture 10.mp4'),
    video(1, 'C/Lecture 2.mp4'),
    { index: 2, path: 'C/notes.pdf', length: 1, kind: 'document' },
    video(3, 'C/Lecture 1.mp4'),
  ];

  it('lists every video in lecture order, with the queued ones ticked', () => {
    const group = groupQueue(queueFrom([of('a', 1), of('a', 0)]))[0];
    if (!group) {
      throw new Error('no group');
    }
    const rows = groupTree(group, files);
    expect(rows.map((r) => [r.file?.path, r.item?.id ?? null])).toEqual([
      ['C/Lecture 1.mp4', null],
      ['C/Lecture 2.mp4', 'a1'],
      ['C/Lecture 10.mp4', 'a0'],
    ]);
  });

  it('shows only the queued items while the files are unknown, and loose items first', () => {
    const items = [of('a', 3), { ...of('a', 9), id: 'main', fileIndex: null }];
    const group = groupQueue(queueFrom(items))[0];
    if (!group) {
      throw new Error('no group');
    }
    expect(groupTree(group, null).map((r) => r.item?.id)).toEqual(['a3', 'main']);
    expect(groupTree(group, files).map((r) => r.item?.id ?? r.file?.path)).toEqual([
      'main',
      'a3',
      'C/Lecture 2.mp4',
      'C/Lecture 10.mp4',
    ]);
  });
});
