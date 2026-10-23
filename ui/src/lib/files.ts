// SPDX-License-Identifier: MIT OR Apache-2.0

/** Helpers for presenting a torrent's file list. */

import type { FileSummary } from './bindings/FileSummary';

/** The last path component, for display. */
export function fileName(path: string): string {
  const parts = path.split('/');
  return parts[parts.length - 1] ?? path;
}

/** The folder part of a path, or an empty string for top-level files. */
export function folderOf(path: string): string {
  const at = path.lastIndexOf('/');
  return at < 0 ? '' : path.slice(0, at);
}

/** Whether the player can play a file: video or audio. */
export function isPlayable(file: Pick<FileSummary, 'kind'>): boolean {
  return file.kind === 'video' || file.kind === 'audio';
}

/** What to call a torrent's playable files: tracks when they are all audio, else videos. */
export function mediaNoun(files: readonly Pick<FileSummary, 'kind'>[], count: number): string {
  const playable = files.filter(isPlayable);
  const audio = playable.length > 0 && playable.every((f) => f.kind === 'audio');
  const noun = audio ? 'track' : 'video';
  return count === 1 ? noun : `${noun}s`;
}

/** Playable files (video and audio) first, then everything else; each in natural path order. */
export function sortFiles(files: readonly FileSummary[]): FileSummary[] {
  const collator = new Intl.Collator(undefined, { numeric: true, sensitivity: 'base' });
  return [...files].sort((a, b) => {
    const av = isPlayable(a) ? 0 : 1;
    const bv = isPlayable(b) ? 0 : 1;
    return av - bv || collator.compare(a.path, b.path);
  });
}

/**
 * Video containers the built-in (webview) player cannot play. Chromium-based webviews play MP4,
 * M4V, WebM, and usually MOV and MKV with web codecs; Theora (OGV) support was removed, and
 * AVI, WMV, FLV, MPEG-TS, and MPEG-PS were never supported. Another player (VLC, mpv)
 * plays them once downloaded.
 */
const NEEDS_FULL_PLAYER = new Set([
  'ogv',
  'avi',
  'wmv',
  'flv',
  'ts',
  'm2ts',
  'mts',
  'mpg',
  'mpeg',
  // Audio the webview cannot decode.
  'wma',
  'ape',
  'aif',
  'aiff',
  'mka',
]);

/** Whether the built-in player can be expected to play this file. */
// Whether file lists show every file or only the videos; kept for the session.
let allFiles = false;

/** Whether file lists show every file (the user chose "Show all files" this session). */
export function showsAllFiles(): boolean {
  return allFiles;
}

/** Remembers for the session whether file lists show every file. */
export function setShowAllFiles(show: boolean): void {
  allFiles = show;
}

/**
 * The files a torrent's file list shows: only what plays (video and audio), unless `all` is
 * set or nothing plays (then there is nothing else to show).
 */
export function visibleFiles(files: readonly FileSummary[], all: boolean): FileSummary[] {
  if (all || !files.some(isPlayable)) {
    return [...files];
  }
  return files.filter(isPlayable);
}

export function playableInWebview(path: string): boolean {
  const ext = path.toLowerCase().split('.').pop() ?? '';
  return !NEEDS_FULL_PLAYER.has(ext);
}

/** A file's format for display: `a/b.avi` → `AVI`, or null without an extension. */
export function formatOf(path: string): string | null {
  const name = fileName(path);
  const at = name.lastIndexOf('.');
  return at <= 0 || at === name.length - 1 ? null : name.slice(at + 1).toUpperCase();
}

/** The warning for a file the built-in player probably cannot play: `May not play here (format: AVI)`. */
export function mayNotPlay(format: string | null): string {
  return format === null || format === ''
    ? 'May not play here'
    : `May not play here (format: ${format})`;
}

/** Whether a dropped or chosen file looks like a `.torrent` file. */
export function isTorrentFile(name: string): boolean {
  return name.toLowerCase().endsWith('.torrent');
}

/** Reads a chosen or dropped `.torrent` file. Rejects anything else. */
export async function readTorrentFile(
  file: Pick<File, 'name' | 'arrayBuffer'>,
): Promise<Uint8Array> {
  if (!isTorrentFile(file.name)) {
    throw new Error('That is not a .torrent file.');
  }
  return new Uint8Array(await file.arrayBuffer());
}

/**
 * The file a search result's Play button starts: the preloaded file if the backend chose one,
 * otherwise the largest video the built-in player can open, otherwise the largest video. Null
 * if the torrent has no video, so the file list is shown instead.
 */
export function playTarget(
  torrent: { files: readonly FileSummary[] },
  preloaded: number | null,
): FileSummary | null {
  const chosen = torrent.files.find((f) => f.index === preloaded);
  if (chosen) {
    return chosen;
  }
  const largest = (files: FileSummary[]): FileSummary | null =>
    files.reduce<FileSummary | null>(
      (best, f) => (best && best.length >= f.length ? best : f),
      null,
    );
  const videos = torrent.files.filter((f) => f.kind === 'video');
  if (videos.length === 0) {
    // An album, podcast, or audiobook: its first track.
    const natural = new Intl.Collator(undefined, { numeric: true, sensitivity: 'base' });
    const tracks = torrent.files
      .filter((f) => f.kind === 'audio')
      .sort((a, b) => natural.compare(a.path, b.path));
    return tracks.find((f) => playableInWebview(f.path)) ?? tracks[0] ?? null;
  }
  return largest(videos.filter((f) => playableInWebview(f.path))) ?? largest(videos);
}
