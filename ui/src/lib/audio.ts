// SPDX-License-Identifier: MIT OR Apache-2.0

/**
 * Telling the viewer when the built-in player cannot play a video's audio.
 *
 * Two kinds of evidence are combined. The backend probes the container header
 * (`stream_audio`) and names the codec; Chromium-based webviews decode AAC, MP3, Opus, Vorbis,
 * and FLAC, but not Dolby (AC-3, E-AC-3, TrueHD) or DTS. The webview's decoder counters
 * (`webkitAudioDecodedByteCount`) show whether any audio is actually being decoded. Where the
 * counters exist they have the last word: the notice appears only after video has played for a
 * while with no audio decoded at all, so a codec the webview turns out to support never
 * triggers it. Without counters, the probe alone decides.
 */

import type { AudioTrackDto } from './bindings/AudioTrackDto';
import type { StreamAudio } from './bindings/StreamAudio';

export type { AudioTrackDto, StreamAudio };

/** Chromium's non-standard decoder counters on media elements. */
interface DecodeCounters {
  webkitAudioDecodedByteCount?: unknown;
  webkitVideoDecodedByteCount?: unknown;
}

/** What the decoder counters show: audio decoded, none while video played, or not enough yet. */
export type AudioDecoding = 'decoding' | 'silent' | 'unknown';

/** The conversion of audio the player cannot decode (see converted.ts). */
export type Conversion = 'off' | 'converting' | 'failed';

/** What the player says about a conversion, or null when there is none. */
export function conversionNotice(conversion: Conversion, info: StreamAudio | null): string | null {
  switch (conversion) {
    case 'converting': {
      const codec = info?.primary?.label;
      return `Converting ${codec === undefined ? '' : `${codec} `}audio so it can play here. Seeking may take a moment longer.`;
    }
    case 'failed':
      return 'The audio could not be converted, so this video plays without sound.';
    case 'off':
      return null;
  }
}

/** Seconds of playback with video but no audio decoded before the audio counts as missing. */
export const SILENT_AFTER_SECS = 3;
/** The same when the probe already says the codec cannot be decoded: this only confirms it. */
export const CONFIRM_AFTER_SECS = 1;
/** Longest gap between samples that counts (timers are throttled in the background). */
const MAX_STEP_SECS = 1.5;

/** Whether `media` has decoder counters (Chromium-based webviews do). */
export function hasDecodeCounters(media: object): boolean {
  const counters = media as DecodeCounters;
  return (
    typeof counters.webkitAudioDecodedByteCount === 'number' &&
    typeof counters.webkitVideoDecodedByteCount === 'number'
  );
}

/**
 * Watches the decoder counters. Only intervals in which the video decoder made progress count
 * as playback, so pauses, stalls, and seeks never look like missing audio.
 */
export class AudioDecodeWatch {
  private silentSecs = 0;
  private audioSeen = false;
  private last: { at: number; video: number } | null = null;

  /** Call about twice a second with the media element. `playing`: not paused or held. */
  sample(media: object, playing: boolean, nowMs: number): void {
    const counters = media as DecodeCounters;
    const audio = counters.webkitAudioDecodedByteCount;
    const video = counters.webkitVideoDecodedByteCount;
    if (typeof audio !== 'number' || typeof video !== 'number') {
      return;
    }
    if (audio > 0) {
      this.audioSeen = true;
      return;
    }
    const last = this.last;
    this.last = { at: nowMs, video };
    if (playing && last !== null && video > last.video) {
      this.silentSecs += Math.min(Math.max(nowMs - last.at, 0) / 1000, MAX_STEP_SECS);
    }
  }

  /** The verdict, counting the audio missing after `afterSecs` of silent playback. */
  state(afterSecs: number): AudioDecoding {
    if (this.audioSeen) {
      return 'decoding';
    }
    return this.silentSecs >= afterSecs ? 'silent' : 'unknown';
  }
}

/** How long the counters must show silence before the notice, given the probe's answer. */
export function silenceThreshold(info: StreamAudio | null): number {
  return info?.primary?.decodes === false ? CONFIRM_AFTER_SECS : SILENT_AFTER_SECS;
}

function codecSentence(primary: AudioTrackDto, alternative: AudioTrackDto | null): string {
  const main = `This video's audio is ${primary.label}, which the built-in player can't play yet.`;
  return alternative === null
    ? main
    : `${main} Another audio track (${alternative.label}) would play, but switching tracks isn't supported yet.`;
}

/**
 * The notice to show, or null. `info` is the probe's answer (null until it arrives, or if it
 * failed); `decoding` is the counters' verdict, and `hasCounters` whether there are counters.
 */
export function audioNotice(
  info: StreamAudio | null,
  decoding: AudioDecoding,
  hasCounters: boolean,
): string | null {
  if (decoding === 'decoding') {
    return null;
  }
  if (info?.probed === true && info.primary === null) {
    // The video has no audio track: silence is expected.
    return null;
  }
  const primary = info?.primary ?? null;
  if (primary?.decodes === false) {
    return !hasCounters || decoding === 'silent'
      ? codecSentence(primary, info?.alternative ?? null)
      : null;
  }
  if (primary?.decodes === true || decoding !== 'silent') {
    // The probe says it plays, or the counters have not shown silence (yet).
    return null;
  }
  return primary === null
    ? "No sound is being decoded. This video may have no audio, or its audio format isn't supported by the built-in player yet."
    : `No sound is being decoded. This video's audio is ${primary.label}, which the built-in player may not support.`;
}
