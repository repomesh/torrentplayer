// SPDX-License-Identifier: MIT OR Apache-2.0

import { describe, expect, it } from 'vitest';
import { PlaybackTimer } from './metrics';

/** A manual clock. */
function clock(): { now: () => number; advance: (ms: number) => void } {
  let t = 1000;
  return {
    now: () => t,
    advance: (ms) => {
      t += ms;
    },
  };
}

describe('PlaybackTimer', () => {
  it('measures time to first frame from the click', () => {
    const c = clock();
    const timer = new PlaybackTimer(c.now);
    const clickedAt = c.now();
    c.advance(200); // start_stream round trip
    timer.play(clickedAt);
    c.advance(1300);
    expect(timer.firstFrameSeen).toBe(false);
    expect(timer.frame()).toEqual({ kind: 'firstFrame', millis: 1500 });
    expect(timer.firstFrameSeen).toBe(true);
    expect(timer.frame()).toBeNull();
  });

  it('measures a seek from seeking to the first frame after seeked', () => {
    const c = clock();
    const timer = new PlaybackTimer(c.now);
    timer.play();
    timer.frame();

    timer.seeking();
    c.advance(400);
    // Frames presented before `seeked` do not count.
    expect(timer.frame()).toBeNull();
    timer.seeked();
    c.advance(100);
    expect(timer.frame()).toEqual({ kind: 'seek', millis: 500 });
    expect(timer.frame()).toBeNull();
  });

  it('says while a seek waits for its frame', () => {
    const timer = new PlaybackTimer(clock().now);
    timer.play();
    timer.frame();
    expect(timer.seekPending).toBe(false);
    timer.seeking();
    expect(timer.seekPending).toBe(true);
    timer.seeked();
    expect(timer.seekPending).toBe(true);
    timer.frame();
    expect(timer.seekPending).toBe(false);
  });

  it('restarts on repeated seeking events (scrubbing)', () => {
    const c = clock();
    const timer = new PlaybackTimer(c.now);
    timer.play();
    timer.frame();
    timer.seeking();
    c.advance(300);
    timer.seeking();
    c.advance(200);
    timer.seeked();
    expect(timer.frame()).toEqual({ kind: 'seek', millis: 200 });
  });

  it('treats seeks before the first frame as part of start-up', () => {
    const c = clock();
    const timer = new PlaybackTimer(c.now);
    timer.play();
    timer.seeking();
    timer.seeked();
    c.advance(50);
    expect(timer.frame()).toEqual({ kind: 'firstFrame', millis: 50 });
    expect(timer.frame()).toBeNull();
  });

  it('ignores seeked without seeking', () => {
    const timer = new PlaybackTimer(clock().now);
    timer.play();
    timer.frame();
    timer.seeked();
    expect(timer.frame()).toBeNull();
  });
});
