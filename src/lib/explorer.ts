// SPDX-License-Identifier: MIT OR Apache-2.0

/**
 * The file explorer's logic: which torrents it lists, how a torrent's files are split
 * (videos above, everything else below by folder), and which file is selected.
 */

import type { FileSummary } from './bindings/FileSummary';
import type { TorrentSummary } from './bindings/TorrentSummary';
import { fileName, folderOf, isPlayable } from './files';
import { groupKey, groupQueue, type Queue } from './queue';

const natural = new Intl.Collator(undefined, { numeric: true, sensitivity: 'base' });

/** A torrent's files: videos in lecture order, and the other files by folder. */
export interface SplitFiles {
  videos: FileSummary[];
  others: { folder: string; files: FileSummary[] }[];
}

/** Splits a torrent's files for the explorer (natural order: lecture 2 before 10). */
export function splitFiles(files: readonly FileSummary[]): SplitFiles {
  const byPath = (a: FileSummary, b: FileSummary): number => natural.compare(a.path, b.path);
  const videos = files.filter(isPlayable).sort(byPath);
  const folders = new Map<string, FileSummary[]>();
  for (const file of files.filter((f) => !isPlayable(f)).sort(byPath)) {
    const folder = folderOf(file.path);
    const list = folders.get(folder);
    if (list) {
      list.push(file);
    } else {
      folders.set(folder, [file]);
    }
  }
  const others = [...folders]
    .sort(([a], [b]) => natural.compare(a, b))
    .map(([folder, list]) => ({ folder, files: list }));
  return { videos, others };
}

/** A torrent in the explorer's list. */
export interface ExplorerTorrent {
  /** The queue's group key for it (`groupKey`). */
  key: string;
  name: string;
  /** Its details, once opened this session (otherwise opening it fetches them). */
  torrent: TorrentSummary | null;
  /** How many of its files are queued. */
  queued: number;
  /** A link that adds it, for torrents only in the queue. */
  source: string | null;
}

/**
 * The torrents to explore: those in the queue (in queue order), then the others opened this
 * session (most recent first). `keyOf` gives an opened torrent's group key.
 */
export function explorerTorrents(
  queue: Queue,
  opened: readonly TorrentSummary[],
  keyOf: (torrent: TorrentSummary) => readonly string[],
): ExplorerTorrent[] {
  const find = (key: string): TorrentSummary | null =>
    opened.find((t) => keyOf(t).includes(key)) ?? null;
  const list: ExplorerTorrent[] = [];
  for (const group of groupQueue(queue)) {
    const torrent = find(group.key);
    const known = list.find((e) => e.key === group.key || (torrent && e.torrent === torrent));
    if (known) {
      known.queued += group.items.length;
      continue;
    }
    const first = group.items[0];
    list.push({
      key: group.key,
      name:
        torrent?.name ??
        group.items.find((i) => i.torrentName !== null)?.torrentName ??
        first?.title ??
        '',
      torrent,
      queued: group.items.length,
      source: first?.source ?? null,
    });
  }
  for (const torrent of opened) {
    if (!list.some((e) => e.torrent === torrent)) {
      list.push({
        key: keyOf(torrent)[0] ?? torrent.infoHash,
        name: torrent.name,
        torrent,
        queued: 0,
        source: null,
      });
    }
  }
  return list;
}

/** The queue group keys an opened torrent may have: by info hash, and by its link. */
export function torrentKeys(torrent: TorrentSummary, source: string): string[] {
  return [torrent.infoHash.toLowerCase(), groupKey({ infoHash: null, source })];
}

/**
 * The file to select when a torrent is shown: the one already selected if it is still there,
 * else the file playing, else the first video, else the first other file.
 */
export function defaultSelection(
  split: SplitFiles,
  current: number | null,
  playing: number | null,
): number | null {
  const all = [...split.videos, ...split.others.flatMap((g) => g.files)];
  for (const wanted of [current, playing]) {
    if (wanted !== null && all.some((f) => f.index === wanted)) {
      return wanted;
    }
  }
  return all[0]?.index ?? null;
}

/** The kind of preview to expect, for the pane's placeholder while it loads. */
export function previewHint(file: Pick<FileSummary, 'path' | 'kind'>): string {
  const ext = fileName(file.path).split('.').pop()?.toLowerCase() ?? '';
  if (file.kind === 'video') {
    return 'video';
  }
  if (['jpg', 'jpeg', 'png', 'webp'].includes(ext)) {
    return 'image';
  }
  if (['txt', 'nfo', 'md', 'srt', 'vtt', 'ass', 'ssa', 'sub', 'log'].includes(ext)) {
    return 'text';
  }
  return 'none';
}
