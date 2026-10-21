// SPDX-License-Identifier: MIT OR Apache-2.0

/**
 * Subtitles in the built-in player: the CC menu, which track to show, and turning cues into
 * text tracks.
 *
 * Cues come from the backend as plain text (it downloads and parses subtitle files, see
 * `stream_subtitles` and `load_subtitles`) and are added with the TextTrack API
 * (`video.addTextTrack` and `VTTCue`), so no URL is involved and nothing is ever rendered as
 * HTML. Embedded subtitle tracks are listed, but Chromium does not show in-band text tracks
 * from a `<video src>`, so they cannot be chosen yet.
 */

import type { CueDto } from './bindings/CueDto';
import type { StreamSubtitles } from './bindings/StreamSubtitles';
import type { SubtitleCues } from './bindings/SubtitleCues';

export type { CueDto, StreamSubtitles, SubtitleCues };

/** Largest subtitle file the backend parses (5 MiB). */
export const MAX_SUBTITLE_BYTES = 5 * 1024 * 1024;
/** File types the "Load subtitle file…" picker offers. */
export const SUBTITLE_ACCEPT = '.srt,.vtt,.ass,.ssa';

/** One entry in the CC menu. */
export interface SubtitleOption {
  /** `file:<index>`, `local:<n>`, or `embedded:<n>`. */
  id: string;
  label: string;
  /** Secondary text: the file name, or why it cannot be shown. */
  detail: string | null;
  /** BCP 47 language tag, or `''` if unknown. */
  language: string;
  /** The video's own files, files the user loaded, other files in the torrent, embedded. */
  group: 'video' | 'local' | 'other' | 'embedded';
  /** Whether it can be shown. */
  enabled: boolean;
  forced: boolean;
}

/** A subtitle file the user loaded from disk, already parsed. */
export interface LocalSubtitle {
  id: string;
  name: string;
  cues: SubtitleCues;
}

/** The CC menu's entries, in menu order. */
export function subtitleOptions(
  list: StreamSubtitles | null,
  locals: readonly LocalSubtitle[],
): SubtitleOption[] {
  const files = (list?.files ?? []).map((f): SubtitleOption => {
    const tooLarge = f.length > MAX_SUBTITLE_BYTES;
    return {
      id: `file:${String(f.fileIndex)}`,
      label: f.label,
      detail: tooLarge ? 'too large to load' : f.label === f.fileName ? null : f.fileName,
      language: f.language ?? '',
      group: f.matched ? 'video' : 'other',
      enabled: !tooLarge,
      forced: f.forced,
    };
  });
  const loaded = locals.map((l): SubtitleOption => ({
    id: l.id,
    label: l.name,
    detail: 'from this computer',
    language: '',
    group: 'local',
    enabled: true,
    forced: false,
  }));
  const embedded = (list?.embedded ?? []).map((e, i): SubtitleOption => ({
    id: `embedded:${String(i)}`,
    label: e.label,
    detail: e.text ? 'embedded, not supported yet' : 'picture subtitles, not supported',
    language: e.language ?? '',
    group: 'embedded',
    enabled: false,
    forced: e.forced,
  }));
  return [
    ...files.filter((o) => o.group === 'video'),
    ...loaded,
    ...files.filter((o) => o.group === 'other'),
    ...embedded,
  ];
}

/** The menu's sections: the video's and loaded subtitles first, without a heading. */
export function subtitleGroups(
  options: readonly SubtitleOption[],
): { title: string | null; options: SubtitleOption[] }[] {
  const of = (...groups: SubtitleOption['group'][]): SubtitleOption[] =>
    options.filter((o) => groups.includes(o.group));
  return [
    { title: null, options: of('video', 'local') },
    { title: 'Other subtitles in this torrent', options: of('other') },
    { title: 'Embedded in the video', options: of('embedded') },
  ].filter((g) => g.options.length > 0);
}

/** The viewer's last choice this session: subtitles off, or a language (and its label). */
export type SubtitleMemory = { on: false } | { on: true; language: string; label: string };

let memory: SubtitleMemory | null = null;

/** Remembers the viewer's choice, for the next video this session. */
export function rememberSubtitle(option: SubtitleOption | null): void {
  memory =
    option === null ? { on: false } : { on: true, language: option.language, label: option.label };
}

/** The remembered choice, if the viewer made one this session. */
export function rememberedSubtitle(): SubtitleMemory | null {
  return memory;
}

/** Forgets the remembered choice (tests). */
export function forgetSubtitleChoice(): void {
  memory = null;
}

const primary = (tag: string): string => tag.toLowerCase().split('-')[0] ?? '';

/** The best option in `language` (its primary subtag): the same label if given, not forced. */
function inLanguage(
  options: readonly SubtitleOption[],
  language: string,
  label?: string,
): SubtitleOption | undefined {
  const want = primary(language);
  if (want === '') {
    return label === undefined ? undefined : options.find((o) => o.label === label);
  }
  const same = options.filter((o) => primary(o.language) === want);
  return same.find((o) => o.label === label) ?? same.find((o) => !o.forced) ?? same[0];
}

/**
 * The option to show when a video starts, or null. The session's choice wins: off stays off,
 * and a chosen language is picked again. Otherwise, with `byDefault` (the
 * `playback.subtitlesByDefault` setting), the first of the viewer's `languages`
 * (`navigator.languages`) that the video's own subtitles have.
 */
export function initialSubtitle(
  options: readonly SubtitleOption[],
  remembered: SubtitleMemory | null,
  byDefault: boolean,
  languages: readonly string[],
): string | null {
  const usable = options.filter((o) => o.enabled && (o.group === 'video' || o.group === 'local'));
  if (remembered !== null) {
    if (!remembered.on) {
      return null;
    }
    const same = inLanguage(usable, remembered.language, remembered.label);
    if (same !== undefined) {
      return same.id;
    }
  }
  if (!byDefault) {
    return null;
  }
  for (const language of languages) {
    const found = inLanguage(usable, language);
    if (found !== undefined) {
      return found.id;
    }
  }
  return null;
}

/**
 * What the C key switches to: off if subtitles show; otherwise the last track shown in this
 * video, the viewer's language, or the first track that can be shown.
 */
export function toggledSubtitle(
  options: readonly SubtitleOption[],
  current: string | null,
  last: string | null,
  languages: readonly string[],
): string | null {
  if (current !== null) {
    return null;
  }
  const enabled = options.filter((o) => o.enabled);
  if (last !== null && enabled.some((o) => o.id === last)) {
    return last;
  }
  const remembered = rememberedSubtitle();
  return (
    initialSubtitle(options, remembered?.on === true ? remembered : null, true, languages) ??
    enabled.find((o) => o.group === 'video' || o.group === 'local')?.id ??
    enabled[0]?.id ??
    null
  );
}

/**
 * Escapes plain text for a `VTTCue`, whose text is parsed as WebVTT cue text: `&`, `<`, and
 * `>` become character references, so subtitles always show literally, never as markup.
 */
export function cueText(text: string): string {
  return text.replaceAll('&', '&amp;').replaceAll('<', '&lt;').replaceAll('>', '&gt;');
}

/** The part of a `TextTrack` that adding cues needs. */
export interface CueTarget {
  addCue(cue: TextTrackCue): void;
}

/** Adds cues to a text track; returns how many were added. */
export function addCues(
  track: CueTarget,
  cues: readonly CueDto[],
  make: (start: number, end: number, text: string) => TextTrackCue = (start, end, text) =>
    new VTTCue(start, end, text),
): number {
  for (const cue of cues) {
    track.addCue(make(cue.startMs / 1000, cue.endMs / 1000, cueText(cue.text)));
  }
  return cues.length;
}

/** Checks a subtitle file the user chose before it is read. Returns an error message, or null. */
export function checkSubtitleFile(file: Pick<File, 'name' | 'size'>): string | null {
  const ext = file.name.toLowerCase().split('.').pop() ?? '';
  if (!SUBTITLE_ACCEPT.split(',').includes(`.${ext}`)) {
    return 'Choose an SRT, WebVTT, ASS, or SSA subtitle file.';
  }
  if (file.size > MAX_SUBTITLE_BYTES) {
    return 'That subtitle file is larger than 5 MiB.';
  }
  return null;
}

/**
 * The CC menu's note when there is nothing to choose (it then offers only "Load subtitle
 * file…"), or null.
 */
export function subtitleMenuNote(options: readonly SubtitleOption[]): string | null {
  return options.length === 0 ? 'No subtitles in this torrent' : null;
}
