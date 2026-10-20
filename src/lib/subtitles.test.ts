// SPDX-License-Identifier: MIT OR Apache-2.0

import { beforeEach, describe, expect, it } from 'vitest';
import type { SubtitleFileDto } from './bindings/SubtitleFileDto';
import {
  MAX_SUBTITLE_BYTES,
  addCues,
  checkSubtitleFile,
  cueText,
  forgetSubtitleChoice,
  initialSubtitle,
  rememberSubtitle,
  rememberedSubtitle,
  subtitleGroups,
  subtitleMenuNote,
  subtitleOptions,
  toggledSubtitle,
  type LocalSubtitle,
  type StreamSubtitles,
} from './subtitles';

function file(
  index: number,
  label: string,
  language: string | null,
  matched = true,
): SubtitleFileDto {
  return {
    fileIndex: index,
    label,
    language,
    fileName: `Movie.${language ?? 'x'}.srt`,
    matched,
    forced: label.includes('forced'),
    sdh: false,
    length: 1000,
  };
}

const list: StreamSubtitles = {
  files: [
    file(1, 'English', 'en'),
    file(2, 'English (forced)', 'en'),
    file(3, 'French', 'fr'),
    file(9, 'Spanish', 'es', false),
    { ...file(10, 'Huge', 'de'), length: MAX_SUBTITLE_BYTES + 1 },
  ],
  embedded: [
    { label: 'Japanese', language: 'ja', codec: 'ass', text: true, default: true, forced: false },
    { label: 'PGS', language: null, codec: 'image', text: false, default: false, forced: false },
  ],
};

const local: LocalSubtitle = {
  id: 'local:1',
  name: 'mine.srt',
  cues: { format: 'srt', cues: [], truncated: false },
};

beforeEach(() => {
  forgetSubtitleChoice();
});

describe('subtitleOptions', () => {
  it('orders the video, local, other, and embedded subtitles', () => {
    const options = subtitleOptions(list, [local]);
    expect(options.map((o) => o.id)).toEqual([
      'file:1',
      'file:2',
      'file:3',
      'file:10',
      'local:1',
      'file:9',
      'embedded:0',
      'embedded:1',
    ]);
    const byId = (id: string) => options.find((o) => o.id === id);
    expect(byId('file:10')).toMatchObject({ enabled: false, detail: 'too large to load' });
    expect(byId('file:1')).toMatchObject({ detail: 'Movie.en.srt', enabled: true });
    expect(byId('embedded:0')).toMatchObject({
      enabled: false,
      detail: 'embedded, not supported yet',
    });
    expect(byId('embedded:1')?.detail).toBe('picture subtitles, not supported');
    expect(subtitleOptions(null, [])).toEqual([]);
  });

  it('groups the menu, leaving out empty sections', () => {
    const groups = subtitleGroups(subtitleOptions(list, []));
    expect(groups.map((g) => [g.title, g.options.length])).toEqual([
      [null, 4],
      ['Other subtitles in this torrent', 1],
      ['Embedded in the video', 2],
    ]);
    expect(subtitleGroups(subtitleOptions({ files: [], embedded: null }, []))).toEqual([]);
    expect(subtitleMenuNote(subtitleOptions({ files: [], embedded: [] }, []))).toBe(
      'No subtitles in this torrent',
    );
    expect(subtitleMenuNote(subtitleOptions(list, []))).toBeNull();
    expect(subtitleMenuNote(subtitleOptions(null, [local]))).toBeNull();
  });
});

describe('initialSubtitle', () => {
  const options = subtitleOptions(list, []);

  it('shows nothing unless asked to', () => {
    expect(initialSubtitle(options, null, false, ['en-US'])).toBeNull();
  });

  it('picks the viewer language from the video subtitles, avoiding forced ones', () => {
    expect(initialSubtitle(options, null, true, ['en-US', 'fr'])).toBe('file:1');
    expect(initialSubtitle(options, null, true, ['fr-CA'])).toBe('file:3');
    // Spanish exists only as another episode's file, and German is too large.
    expect(initialSubtitle(options, null, true, ['es', 'de'])).toBeNull();
  });

  it('follows the session choice to the next video', () => {
    rememberSubtitle(options.find((o) => o.id === 'file:2') ?? null);
    expect(rememberedSubtitle()).toEqual({ on: true, language: 'en', label: 'English (forced)' });
    expect(initialSubtitle(options, rememberedSubtitle(), false, [])).toBe('file:2');
    rememberSubtitle(null);
    expect(initialSubtitle(options, rememberedSubtitle(), true, ['en'])).toBeNull();
  });

  it('falls back to the setting when the remembered language is missing', () => {
    const french = options.find((o) => o.id === 'file:3') ?? null;
    rememberSubtitle(french);
    const englishOnly = subtitleOptions({ files: [file(1, 'English', 'en')], embedded: [] }, []);
    expect(initialSubtitle(englishOnly, rememberedSubtitle(), false, ['en'])).toBeNull();
    expect(initialSubtitle(englishOnly, rememberedSubtitle(), true, ['en'])).toBe('file:1');
  });
});

describe('toggledSubtitle', () => {
  const options = subtitleOptions(list, [local]);

  it('turns subtitles off, then back to the last track', () => {
    expect(toggledSubtitle(options, 'file:3', 'file:3', ['en'])).toBeNull();
    expect(toggledSubtitle(options, null, 'file:3', ['en'])).toBe('file:3');
  });

  it('without a last track, picks the viewer language or the first usable track', () => {
    expect(toggledSubtitle(options, null, null, ['fr'])).toBe('file:3');
    expect(toggledSubtitle(options, null, null, ['ko'])).toBe('file:1');
    expect(toggledSubtitle(options, null, 'embedded:0', ['ko'])).toBe('file:1');
    expect(toggledSubtitle([], null, null, ['en'])).toBeNull();
  });
});

describe('cues', () => {
  it('escapes cue text so it is never markup', () => {
    expect(cueText('<b>x</b> & <script>')).toBe('&lt;b&gt;x&lt;/b&gt; &amp; &lt;script&gt;');
    expect(cueText('two\nlines')).toBe('two\nlines');
  });

  it('adds cues in seconds', () => {
    const added: { start: number; end: number; text: string }[] = [];
    const track = {
      addCue: (cue: TextTrackCue) => {
        added.push(cue as unknown as { start: number; end: number; text: string });
      },
    };
    const make = (start: number, end: number, text: string) =>
      ({ start, end, text }) as unknown as TextTrackCue;
    const n = addCues(track, [{ startMs: 1500, endMs: 3000, text: 'a < b' }], make);
    expect(n).toBe(1);
    expect(added).toEqual([{ start: 1.5, end: 3, text: 'a &lt; b' }]);
  });

  it('checks a chosen file before reading it', () => {
    expect(checkSubtitleFile({ name: 'Movie.EN.SRT', size: 10 })).toBeNull();
    expect(checkSubtitleFile({ name: 'a.vtt', size: MAX_SUBTITLE_BYTES + 1 })).toMatch(/5 MiB/);
    expect(checkSubtitleFile({ name: 'a.sub', size: 10 })).toMatch(/SRT, WebVTT/);
  });
});
