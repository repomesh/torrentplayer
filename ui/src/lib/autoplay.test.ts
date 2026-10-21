// SPDX-License-Identifier: MIT OR Apache-2.0

import { describe, expect, it } from 'vitest';
import { errorName, startPlayback, type Playable } from './autoplay';

const abort = (): DOMException => new DOMException('interrupted by pause()', 'AbortError');
const notAllowed = (): DOMException => new DOMException('autoplay refused', 'NotAllowedError');

/** A media element whose `play()` calls follow a script: `ok`, or an error to reject with. */
class FakeMedia implements Playable {
  muted = false;
  calls = 0;
  /** `muted` at each call. */
  mutedAtCall: boolean[] = [];

  constructor(private readonly script: ('ok' | Error)[]) {}

  play(): Promise<void> {
    this.mutedAtCall.push(this.muted);
    const step = this.script[this.calls] ?? 'ok';
    this.calls += 1;
    return step === 'ok' ? Promise.resolve() : Promise.reject(step);
  }
}

const noWait = (): Promise<void> => Promise.resolve();
const wanted = { stillWanted: () => true, wait: noWait };

describe('startPlayback', () => {
  it('plays with sound when allowed', async () => {
    const media = new FakeMedia(['ok']);
    expect(await startPlayback(media, wanted)).toBe('playing');
    expect(media.muted).toBe(false);
  });

  it('retries an interrupted start without muting', async () => {
    // The bug this replaces: any rejection muted the video for good.
    const media = new FakeMedia([abort(), abort(), 'ok']);
    expect(await startPlayback(media, wanted)).toBe('playing');
    expect(media.calls).toBe(3);
    expect(media.mutedAtCall).toEqual([false, false, false]);
    expect(media.muted).toBe(false);
  });

  it('does not retry an interruption that is no longer wanted', async () => {
    const media = new FakeMedia([abort()]);
    const outcome = await startPlayback(media, { stillWanted: () => false, wait: noWait });
    expect(outcome).toBe('cancelled');
    expect(media.calls).toBe(1);
    expect(media.muted).toBe(false);
  });

  it('stops retrying when playback stops being wanted during the wait', async () => {
    let wantedNow = true;
    const media = new FakeMedia([abort(), 'ok']);
    const outcome = await startPlayback(media, {
      stillWanted: () => wantedNow,
      wait: () => {
        wantedNow = false; // the user paused meanwhile
        return Promise.resolve();
      },
    });
    expect(outcome).toBe('cancelled');
    expect(media.calls).toBe(1);
  });

  it('gives up after repeated interruptions, still unmuted', async () => {
    const media = new FakeMedia([abort(), abort(), abort(), abort(), abort()]);
    expect(await startPlayback(media, { ...wanted, retries: 2 })).toBe('failed');
    expect(media.calls).toBe(3);
    expect(media.muted).toBe(false);
  });

  it('mutes only when the autoplay policy refuses sound', async () => {
    const media = new FakeMedia([notAllowed(), 'ok']);
    expect(await startPlayback(media, wanted)).toBe('mutedByPolicy');
    expect(media.mutedAtCall).toEqual([false, true]);
    expect(media.muted).toBe(true);
  });

  it('restores the sound if muted playback fails too', async () => {
    const media = new FakeMedia([notAllowed(), notAllowed()]);
    expect(await startPlayback(media, wanted)).toBe('blocked');
    expect(media.muted).toBe(false);

    const interrupted = new FakeMedia([notAllowed(), abort()]);
    expect(await startPlayback(interrupted, { stillWanted: () => false })).toBe('cancelled');
    expect(interrupted.muted).toBe(false);
  });

  it('leaves a video the user muted alone', async () => {
    const media = new FakeMedia([notAllowed()]);
    media.muted = true;
    expect(await startPlayback(media, wanted)).toBe('blocked');
    expect(media.calls).toBe(1);
    expect(media.muted).toBe(true);
  });

  it('reports other failures without muting', async () => {
    const media = new FakeMedia([new DOMException('no source', 'NotSupportedError')]);
    expect(await startPlayback(media, wanted)).toBe('failed');
    expect(media.muted).toBe(false);
    expect(await startPlayback(new FakeMedia([new Error('odd')]), wanted)).toBe('failed');
  });
});

describe('errorName', () => {
  it('reads the name of errors and error-like values', () => {
    expect(errorName(abort())).toBe('AbortError');
    expect(errorName({ name: 'NotAllowedError' })).toBe('NotAllowedError');
    expect(errorName('AbortError')).toBeNull();
    expect(errorName(null)).toBeNull();
    expect(errorName({ name: 3 })).toBeNull();
  });
});
