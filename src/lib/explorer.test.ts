// SPDX-License-Identifier: MIT OR Apache-2.0

import { describe, expect, it } from 'vitest';
import type { FileSummary } from './bindings/FileSummary';
import type { TorrentSummary } from './bindings/TorrentSummary';
import {
  defaultSelection,
  explorerTorrents,
  previewHint,
  splitFiles,
  torrentKeys,
} from './explorer';
import type { QueueItem } from './queue';

const file = (index: number, path: string, kind: FileSummary['kind'] = 'other'): FileSummary => ({
  index,
  path,
  length: 1000,
  kind,
});

const files = [
  file(0, 'Course/Lecture 10.mp4', 'video'),
  file(1, 'Course/notes/Week 2.pdf', 'document'),
  file(2, 'Course/Lecture 2.mp4', 'video'),
  file(3, 'Course/cover.jpg'),
  file(4, 'Course/subs/Lecture 2.srt', 'subtitle'),
  file(5, 'Course/notes/Week 10.pdf', 'document'),
  file(6, 'Course/Lecture 1.mkv', 'video'),
];

function torrent(hash: string, name: string, list: FileSummary[] = files): TorrentSummary {
  return {
    id: hash.charCodeAt(0),
    name,
    infoHash: hash.repeat(40),
    totalLength: 1,
    files: list,
    magnet: `magnet:?xt=urn:btih:${hash.repeat(40)}`,
  };
}

function queued(hash: string | null, id: string, source = 'magnet:x'): QueueItem {
  return {
    id,
    source,
    infoHash: hash === null ? null : hash.repeat(40),
    fileIndex: 0,
    title: `Video ${id}`,
    torrentName: hash === null ? null : `Torrent ${hash}`,
    paused: false,
    skipped: false,
    sizeBytes: 1,
  };
}

describe('splitFiles', () => {
  it('lists videos in lecture order above, other files by folder below', () => {
    const split = splitFiles(files);
    expect(split.videos.map((f) => f.path)).toEqual([
      'Course/Lecture 1.mkv',
      'Course/Lecture 2.mp4',
      'Course/Lecture 10.mp4',
    ]);
    expect(split.others.map((g) => [g.folder, g.files.map((f) => f.index)])).toEqual([
      ['Course', [3]],
      ['Course/notes', [1, 5]],
      ['Course/subs', [4]],
    ]);
    expect(splitFiles([])).toEqual({ videos: [], others: [] });
  });
});

describe('explorerTorrents', () => {
  const keyOf = (t: TorrentSummary): string[] => torrentKeys(t, t.magnet);

  it('lists queued torrents in queue order, then the others opened', () => {
    const opened = [torrent('c', 'Opened only'), torrent('a', 'Course A')];
    const queue = {
      items: [
        queued('b', 'b1'),
        queued('b', 'b2'),
        queued('a', 'a1'),
        queued(null, 'x', 'https://dl/x'),
      ],
      current: null,
      order: 'inOrder' as const,
    };
    const list = explorerTorrents(queue, opened, keyOf);
    expect(list.map((e) => [e.name, e.queued, e.torrent?.name ?? null])).toEqual([
      ['Torrent b', 2, null],
      ['Course A', 1, 'Course A'],
      ['Video x', 1, null],
      ['Opened only', 0, 'Opened only'],
    ]);
    expect(list[0]?.source).toBe('magnet:x');
    expect(list[2]?.key).toBe('link:https://dl/x');
  });

  it('finds a torrent queued by its link once it is opened', () => {
    const opened = torrent('d', 'From a link');
    const queue = {
      items: [queued(null, 'x', opened.magnet)],
      current: null,
      order: 'inOrder' as const,
    };
    const list = explorerTorrents(queue, [opened], keyOf);
    expect(list).toHaveLength(1);
    expect(list[0]?.torrent).toBe(opened);
  });
});

describe('selection', () => {
  const split = splitFiles(files);

  it('keeps the selection, else picks the file playing, else the first video', () => {
    expect(defaultSelection(split, 5, 2)).toBe(5);
    expect(defaultSelection(split, 99, 2)).toBe(2);
    expect(defaultSelection(split, null, null), 'Lecture 1, the first video').toBe(6);
    expect(defaultSelection(splitFiles([file(9, 'readme.txt')]), null, null)).toBe(9);
    expect(defaultSelection(splitFiles([]), null, null)).toBeNull();
  });

  it('knows which preview to expect', () => {
    expect(previewHint(file(0, 'a.MP4', 'video'))).toBe('video');
    expect(previewHint(file(0, 'cover.JPG'))).toBe('image');
    expect(previewHint(file(0, 'subs/en.srt', 'subtitle'))).toBe('text');
    expect(previewHint(file(0, 'slides.pdf', 'document'))).toBe('none');
    expect(previewHint(file(0, 'index.html'))).toBe('none');
  });
});
