// SPDX-License-Identifier: MIT OR Apache-2.0

import { describe, expect, it } from 'vitest';
import {
  AudioDecodeWatch,
  CONFIRM_AFTER_SECS,
  SILENT_AFTER_SECS,
  audioNotice,
  hasDecodeCounters,
  silenceThreshold,
  type AudioTrackDto,
  type StreamAudio,
} from './audio';

const ac3: AudioTrackDto = { codec: 'ac3', label: 'Dolby Digital (AC-3)', decodes: false };
const aac: AudioTrackDto = { codec: 'aac', label: 'AAC', decodes: true };
const mp2: AudioTrackDto = { codec: 'mp2', label: 'MPEG Layer II', decodes: null };

function probed(
  primary: AudioTrackDto | null,
  alternative: AudioTrackDto | null = null,
): StreamAudio {
  return {
    probed: true,
    trackCount: primary === null ? 0 : 1,
    primary,
    alternative,
    convertible: false,
  };
}

const unknown: StreamAudio = {
  probed: false,
  trackCount: 0,
  primary: null,
  alternative: null,
  convertible: false,
};

/** Feeds the watch samples 0.5 s apart; video bytes grow while `playing`. */
function run(watch: AudioDecodeWatch, samples: number, audioBytes = 0, playing = true): void {
  for (let i = 0; i < samples; i++) {
    watch.sample(
      {
        webkitAudioDecodedByteCount: audioBytes,
        webkitVideoDecodedByteCount: playing ? 1000 * (i + 1) : 1000,
      },
      playing,
      500 * i,
    );
  }
}

describe('AudioDecodeWatch', () => {
  it('reports silence only after enough playback with video but no audio', () => {
    const watch = new AudioDecodeWatch();
    run(watch, 4); // 1.5 s
    expect(watch.state(SILENT_AFTER_SECS)).toBe('unknown');
    expect(watch.state(CONFIRM_AFTER_SECS)).toBe('silent');
    run(watch, 8);
    expect(watch.state(SILENT_AFTER_SECS)).toBe('silent');
  });

  it('reports decoding as soon as audio bytes appear', () => {
    const watch = new AudioDecodeWatch();
    run(watch, 10);
    watch.sample(
      { webkitAudioDecodedByteCount: 4096, webkitVideoDecodedByteCount: 99_999 },
      true,
      6000,
    );
    expect(watch.state(CONFIRM_AFTER_SECS)).toBe('decoding');
  });

  it('does not count pauses, stalls, or a frozen video decoder', () => {
    const watch = new AudioDecodeWatch();
    run(watch, 20, 0, false); // paused
    expect(watch.state(CONFIRM_AFTER_SECS)).toBe('unknown');
    // Playing, but no video decoded either (stalled waiting for data).
    for (let i = 0; i < 20; i++) {
      watch.sample(
        { webkitAudioDecodedByteCount: 0, webkitVideoDecodedByteCount: 5 },
        true,
        500 * i,
      );
    }
    expect(watch.state(CONFIRM_AFTER_SECS)).toBe('unknown');
  });

  it('caps long gaps between samples', () => {
    const watch = new AudioDecodeWatch();
    watch.sample({ webkitAudioDecodedByteCount: 0, webkitVideoDecodedByteCount: 1 }, true, 0);
    watch.sample({ webkitAudioDecodedByteCount: 0, webkitVideoDecodedByteCount: 2 }, true, 60_000);
    expect(watch.state(SILENT_AFTER_SECS)).toBe('unknown');
  });

  it('does nothing without counters', () => {
    const watch = new AudioDecodeWatch();
    for (let i = 0; i < 20; i++) {
      watch.sample({}, true, 500 * i);
    }
    expect(watch.state(0.1)).toBe('unknown');
    expect(hasDecodeCounters({})).toBe(false);
    expect(
      hasDecodeCounters({ webkitAudioDecodedByteCount: 0, webkitVideoDecodedByteCount: 0 }),
    ).toBe(true);
  });
});

describe('audioNotice', () => {
  it('names an undecodable codec once the counters confirm silence', () => {
    const info = probed(ac3);
    expect(audioNotice(info, 'unknown', true)).toBeNull();
    expect(audioNotice(info, 'silent', true)).toBe(
      "This video's audio is Dolby Digital (AC-3), which the built-in player can't play yet.",
    );
    expect(silenceThreshold(info)).toBe(CONFIRM_AFTER_SECS);
  });

  it('trusts the probe alone when there are no counters', () => {
    expect(audioNotice(probed(ac3), 'unknown', false)).toContain('Dolby Digital (AC-3)');
    expect(audioNotice(unknown, 'unknown', false)).toBeNull();
  });

  it('mentions a playable alternative track', () => {
    expect(audioNotice(probed(ac3, aac), 'silent', true)).toBe(
      "This video's audio is Dolby Digital (AC-3), which the built-in player can't play yet. Another audio track (AAC) would play, but switching tracks isn't supported yet.",
    );
  });

  it('never shows a notice while audio is decoding', () => {
    expect(audioNotice(probed(ac3, aac), 'decoding', true)).toBeNull();
    expect(audioNotice(probed(ac3), 'decoding', false)).toBeNull();
  });

  it('is quiet for videos without audio and for playable codecs', () => {
    expect(audioNotice(probed(null), 'silent', true)).toBeNull();
    expect(audioNotice(probed(aac), 'silent', true)).toBeNull();
    expect(silenceThreshold(probed(aac))).toBe(SILENT_AFTER_SECS);
  });

  it('falls back to the counters when the probe cannot tell', () => {
    expect(audioNotice(null, 'unknown', true)).toBeNull();
    expect(audioNotice(null, 'silent', true)).toMatch(
      /^No sound is being decoded\. This video may have no audio/,
    );
    expect(audioNotice(unknown, 'silent', true)).toMatch(/may have no audio/);
    expect(audioNotice(probed(mp2), 'silent', true)).toBe(
      "No sound is being decoded. This video's audio is MPEG Layer II, which the built-in player may not support.",
    );
    expect(silenceThreshold(null)).toBe(SILENT_AFTER_SECS);
  });
});
