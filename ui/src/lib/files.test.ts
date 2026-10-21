// SPDX-License-Identifier: MIT OR Apache-2.0

import { describe, expect, it } from 'vitest';
import type { FileSummary } from './bindings/FileSummary';
import {
  fileName,
  folderOf,
  isPlayable,
  isTorrentFile,
  mediaNoun,
  playableInWebview,
  playTarget,
  readTorrentFile,
  setShowAllFiles,
  showsAllFiles,
  sortFiles,
  visibleFiles,
} from './files';

const file = (index: number, path: string, kind: FileSummary['kind']): FileSummary => ({
  index,
  path,
  length: 1,
  kind,
});

describe('file names', () => {
  it('splits paths', () => {
    expect(fileName('MIT6.006/Lecture 1.mp4')).toBe('Lecture 1.mp4');
    expect(fileName('a.mp4')).toBe('a.mp4');
    expect(folderOf('MIT6.006/sub/Lecture 1.mp4')).toBe('MIT6.006/sub');
    expect(folderOf('a.mp4')).toBe('');
  });
});

describe('sortFiles', () => {
  it('puts videos first in natural order', () => {
    const sorted = sortFiles([
      file(0, 'notes.pdf', 'document'),
      file(1, 'Lecture 10.mp4', 'video'),
      file(2, 'Lecture 2.mp4', 'video'),
      file(3, 'Lecture 2.srt', 'subtitle'),
    ]);
    expect(sorted.map((f) => f.index)).toEqual([2, 1, 3, 0]);
  });
});

describe('torrent files', () => {
  it('recognizes .torrent names case-insensitively', () => {
    expect(isTorrentFile('course.TORRENT')).toBe(true);
    expect(isTorrentFile('course.torrent.exe')).toBe(false);
  });

  it('reads .torrent files and refuses other files', async () => {
    const bytes = new Uint8Array([100, 101]);
    const torrent = { name: 'a.torrent', arrayBuffer: () => Promise.resolve(bytes.buffer) };
    await expect(readTorrentFile(torrent)).resolves.toEqual(bytes);
    const other = { name: 'a.mp4', arrayBuffer: () => Promise.resolve(bytes.buffer) };
    await expect(readTorrentFile(other)).rejects.toThrow('not a .torrent');
  });
});

describe('playableInWebview', () => {
  it('accepts web formats and flags ones that need the full player', () => {
    for (const ok of ['a.mp4', 'b/C.M4V', 'c.webm', 'd.mkv', 'e.mov']) {
      expect(playableInWebview(ok)).toBe(true);
    }
    for (const no of ['lec05.ogv', 'a.AVI', 'b.wmv', 'c.flv', 'd.ts', 'e.mpg']) {
      expect(playableInWebview(no)).toBe(false);
    }
  });
});

describe('playTarget', () => {
  const sized = (index: number, path: string, length: number): FileSummary => ({
    ...file(index, path, path.endsWith('.srt') ? 'subtitle' : 'video'),
    length,
  });
  const torrent = {
    files: [
      sized(0, 'Course/01.mp4', 500),
      sized(1, 'Course/02.avi', 900),
      sized(2, 'Course/03.mkv', 700),
      sized(3, 'Course/03.srt', 1),
    ],
  };

  it('prefers the preloaded file', () => {
    expect(playTarget(torrent, 0)?.index).toBe(0);
  });

  it('otherwise picks the largest video the built-in player can open', () => {
    expect(playTarget(torrent, null)?.index).toBe(2);
    expect(playTarget(torrent, 42)?.index).toBe(2);
    expect(playTarget({ files: [sized(5, 'a.avi', 9)] }, null)?.index).toBe(5);
  });

  it('gives up without a video', () => {
    expect(playTarget({ files: [sized(3, 'notes.srt', 1)] }, null)).toBeNull();
  });

  it('starts an album or audiobook at its first track', () => {
    const album = {
      files: [
        file(0, 'Album/10 - Finale.flac', 'audio'),
        file(1, 'Album/2 - Second.mp3', 'audio'),
        file(2, 'Album/1 - Opening.mp3', 'audio'),
        file(3, 'Album/cover.jpg', 'other'),
      ],
    };
    expect(playTarget(album, null)?.index).toBe(2);
    expect(playTarget({ files: [file(0, 'a.wma', 'audio')] }, null)?.index).toBe(0);
  });
});

describe('audio', () => {
  it('counts video and audio as playable', () => {
    expect(isPlayable({ kind: 'video' })).toBe(true);
    expect(isPlayable({ kind: 'audio' })).toBe(true);
    expect(isPlayable({ kind: 'subtitle' })).toBe(false);
  });

  it('calls the files of an all-audio torrent tracks', () => {
    const album = [file(0, '1.mp3', 'audio'), file(1, 'cover.jpg', 'other')];
    expect(mediaNoun(album, 1)).toBe('track');
    expect(mediaNoun(album, 12)).toBe('tracks');
    expect(mediaNoun([file(0, 'a.mp4', 'video'), file(1, 'b.mp3', 'audio')], 2)).toBe('videos');
    expect(mediaNoun([], 3)).toBe('videos');
  });

  it('shows audio in file lists and knows what the webview cannot play', () => {
    const files = [file(0, 'a.mp3', 'audio'), file(1, 'notes.pdf', 'document')];
    expect(visibleFiles(files, false).map((f) => f.index)).toEqual([0]);
    expect(playableInWebview('song.flac')).toBe(true);
    expect(playableInWebview('song.WMA')).toBe(false);
  });
});

describe('visibleFiles', () => {
  const file = (index: number, kind: FileSummary['kind']): FileSummary => ({
    index,
    path: `f${String(index)}`,
    length: 1,
    kind,
  });

  it('lists only the videos unless every file is asked for', () => {
    const files = [file(0, 'video'), file(1, 'document'), file(2, 'subtitle'), file(3, 'video')];
    expect(visibleFiles(files, false).map((f) => f.index)).toEqual([0, 3]);
    expect(visibleFiles(files, true)).toHaveLength(4);
    // A torrent without videos shows what it has.
    expect(visibleFiles([file(0, 'document')], false)).toHaveLength(1);
  });

  it('remembers the choice for the session', () => {
    expect(showsAllFiles()).toBe(false);
    setShowAllFiles(true);
    expect(showsAllFiles()).toBe(true);
    setShowAllFiles(false);
  });
});
