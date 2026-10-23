// SPDX-License-Identifier: MIT OR Apache-2.0

import { describe, expect, it } from 'vitest';
import type { FileSummary } from './bindings/FileSummary';
import {
  LISTED_VIDEOS,
  formatDay,
  formatPieceSize,
  healthLabel,
  playsVerdict,
  soundVerdict,
  summarizeFiles,
  toggled,
  trackerSummary,
} from './details';

const file = (index: number, path: string, kind: FileSummary['kind'], length = 10) => ({
  index,
  path,
  kind,
  length,
});

describe('summarizeFiles', () => {
  it('lists videos in lecture order, subtitles, and other files by type', () => {
    const files = [
      file(0, 'Course/Lecture 10.mp4', 'video', 300),
      file(1, 'Course/Lecture 2.mp4', 'video', 500),
      file(2, 'Course/Lecture 2.srt', 'subtitle'),
      file(3, 'Course/notes.pdf', 'document'),
      file(4, 'Course/slides.pdf', 'document'),
      file(5, 'Course/readme.txt', 'document'),
      file(6, 'Course/LICENSE', 'other'),
    ];
    const s = summarizeFiles(files);
    expect(s.videoCount).toBe(2);
    expect(s.largestVideo).toBe(500);
    expect(s.videos.map((v) => v.path)).toEqual(['Course/Lecture 2.mp4', 'Course/Lecture 10.mp4']);
    expect(s.moreVideos).toBe(0);
    expect(s.subtitles).toHaveLength(1);
    expect(s.others).toBe('2 PDF · 1 TXT · 1 other');
  });

  it('cuts long lists and copes with no videos', () => {
    const many = Array.from({ length: 25 }, (_, i) => file(i, `L${String(i)}.mp4`, 'video'));
    const s = summarizeFiles(many);
    expect(s.videos).toHaveLength(LISTED_VIDEOS);
    expect(s.moreVideos).toBe(15);
    const none = summarizeFiles([file(0, 'book.pdf', 'document')]);
    expect(none.largestVideo).toBeNull();
    expect(none.others).toBe('1 PDF');
  });
});

describe('trackerSummary', () => {
  it('counts trackers and says when Direct mode is needed', () => {
    expect(trackerSummary(2, 1, 0, true)).toEqual({
      text: '2 UDP trackers · 1 HTTP tracker',
      needsDirect: false,
    });
    expect(trackerSummary(3, 0, 0, false).needsDirect).toBe(true);
    expect(trackerSummary(3, 0, 0, true).needsDirect).toBe(false);
    // A web seed works anyway, and an unknown mode is not a reason to warn.
    expect(trackerSummary(3, 0, 1, false).needsDirect).toBe(false);
    expect(trackerSummary(3, 0, 0, null).needsDirect).toBe(false);
    expect(trackerSummary(0, 0, 0, true).text).toBe('No trackers (peers from the DHT only)');
  });
});

describe('verdicts', () => {
  it('says whether it plays, and why', () => {
    expect(playsVerdict({ playable: 'plays', container: 'MP4', codec: 'x264' })).toBe(
      'Plays in the built-in player (MP4, x264)',
    );
    expect(playsVerdict({ playable: 'needsFullPlayer', container: 'AVI/TS', codec: null })).toBe(
      'Needs the full player (AVI/TS)',
    );
    expect(playsVerdict({ playable: 'unknown', container: null, codec: null })).toBe(
      'The title does not say whether it plays here',
    );
  });

  it('says whether there will be sound', () => {
    expect(soundVerdict({ audio: null, audioPlays: null, audioConverted: false })).toBeNull();
    expect(soundVerdict({ audio: 'AAC', audioPlays: true, audioConverted: false })).toBe(
      'AAC: plays with sound here',
    );
    expect(soundVerdict({ audio: 'DTS', audioPlays: false, audioConverted: false })).toBe(
      'DTS: no sound in the built-in player',
    );
  });

  it('says converted Dolby and DTS audio plays with sound', () => {
    expect(soundVerdict({ audio: 'DTS', audioPlays: true, audioConverted: true })).toBe(
      'DTS: plays with sound here, converted to stereo',
    );
  });

  it('labels health and leaves out the unknown', () => {
    expect(healthLabel('good')).toContain('Healthy');
    expect(healthLabel('dead')).toBe('No seeders');
    expect(healthLabel('unknown')).toBeNull();
    expect(healthLabel(null)).toBeNull();
  });
});

describe('formatting', () => {
  it('formats days and piece sizes', () => {
    expect(formatDay(Date.UTC(2026, 9, 18))).toBe('2026-10-18');
    expect(formatDay(null)).toBe('');
    expect(formatDay(-5)).toBe('');
    expect(formatDay(Number.NaN)).toBe('');
    expect(formatPieceSize(262_144)).toBe('256 KiB');
    expect(formatPieceSize(4 * 1024 * 1024)).toBe('4 MiB');
  });
});

describe('toggled', () => {
  it('opens and closes rows', () => {
    const one = toggled(new Set(), 'a');
    expect([...one]).toEqual(['a']);
    expect([...toggled(one, 'b')]).toEqual(['a', 'b']);
    expect([...toggled(one, 'a')]).toEqual([]);
  });
});
