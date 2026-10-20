// SPDX-License-Identifier: MIT OR Apache-2.0

import { describe, expect, it } from 'vitest';
import { SeekGate } from './seekgate';

describe('SeekGate', () => {
  it('pauses a playing video on seek and resumes after the first new frame', () => {
    const gate = new SeekGate();
    expect(gate.seeking(false)).toBe(true);
    expect(gate.holding).toBe(true);
    expect(gate.framePainted()).toBe(false); // a frame from the old position
    gate.seeked();
    expect(gate.framePainted()).toBe(true);
    expect(gate.holding).toBe(false);
    expect(gate.framePainted()).toBe(false); // later frames do nothing
  });

  it('does not start a video that was paused before the seek', () => {
    const gate = new SeekGate();
    expect(gate.seeking(true)).toBe(false);
    gate.seeked();
    expect(gate.framePainted()).toBe(false);
    expect(gate.holding).toBe(false);
  });

  it('keeps the original intent across repeated seeks', () => {
    const gate = new SeekGate();
    gate.seeking(false); // playing; the gate pauses it
    gate.seeking(true); // second seek while held: the video is paused by us
    gate.seeked();
    expect(gate.framePainted()).toBe(true);
  });

  it('resumes when the new position is painted before seeked (as WebView2 does)', () => {
    const gate = new SeekGate();
    gate.seeking(false, 300);
    expect(gate.framePainted(12.5)).toBe(false); // the old position
    expect(gate.framePainted(299.96)).toBe(false); // the target's frame, before `seeked`
    expect(gate.holding).toBe(true);
    expect(gate.seeked()).toBe(true); // no further frame comes while paused
    expect(gate.holding).toBe(false);
  });

  it('does not count old-position frames before seeked', () => {
    const gate = new SeekGate();
    gate.seeking(false, 300);
    gate.framePainted(12.5);
    expect(gate.seeked()).toBe(false);
    expect(gate.framePainted(300)).toBe(true);
  });

  it('a later seek forgets the earlier target', () => {
    const gate = new SeekGate();
    gate.seeking(false, 300);
    gate.framePainted(300);
    gate.seeking(true, 100); // held, so the video is paused by us
    expect(gate.seeked()).toBe(false);
    expect(gate.framePainted(100)).toBe(true);
  });

  it('expire releases a hold when no frame comes after seeked', () => {
    const gate = new SeekGate();
    gate.seeking(false, 300);
    expect(gate.expire()).toBe(false); // not before `seeked`
    gate.seeked();
    expect(gate.expire()).toBe(true);
    expect(gate.holding).toBe(false);
    expect(gate.expire()).toBe(false);
  });

  it('lets the user override while holding', () => {
    const gate = new SeekGate();
    gate.seeking(false);
    gate.cancel();
    gate.seeked();
    expect(gate.holding).toBe(false);
    expect(gate.framePainted()).toBe(false);
  });
});
