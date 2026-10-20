// SPDX-License-Identifier: MIT OR Apache-2.0

/**
 * Measures decoder-level playback timings in the webview:
 *
 * - **First frame:** from the moment the user chose a lecture to the first painted frame.
 * - **Seek:** from the `seeking` event to the first frame painted after `seeked`.
 *
 * Pure and clock-injectable so it can be tested without a video element. Feed it events and
 * call {@link PlaybackTimer.frame} whenever a frame is presented (`requestVideoFrameCallback`,
 * or the `playing`/`seeked` events as a fallback).
 */

import type { PlayerMetricKind } from './bindings/PlayerMetricKind';

/** A completed measurement. */
export interface Metric {
  kind: PlayerMetricKind;
  millis: number;
}

export class PlaybackTimer {
  readonly #now: () => number;
  #playAt: number | null = null;
  #firstFrameDone = false;
  #seekAt: number | null = null;
  #seeked = false;

  constructor(now: () => number = () => performance.now()) {
    this.#now = now;
  }

  /** The user chose to play. `at` lets the caller use an earlier timestamp (the click). */
  play(at: number = this.#now()): void {
    this.#playAt = at;
    this.#firstFrameDone = false;
    this.#seekAt = null;
    this.#seeked = false;
  }

  /** A seek started. Repeated `seeking` events (dragging) restart the measurement. */
  seeking(): void {
    if (!this.#firstFrameDone) {
      // Seeks before the first frame are part of start-up.
      return;
    }
    this.#seekAt = this.#now();
    this.#seeked = false;
  }

  /** The seek finished loading; the next frame completes the measurement. */
  seeked(): void {
    if (this.#seekAt !== null) {
      this.#seeked = true;
    }
  }

  /** A frame was presented. Returns a completed measurement, if this frame finishes one. */
  frame(): Metric | null {
    if (this.#playAt !== null && !this.#firstFrameDone) {
      this.#firstFrameDone = true;
      return { kind: 'firstFrame', millis: Math.max(0, this.#now() - this.#playAt) };
    }
    if (this.#seekAt !== null && this.#seeked) {
      const millis = Math.max(0, this.#now() - this.#seekAt);
      this.#seekAt = null;
      this.#seeked = false;
      return { kind: 'seek', millis };
    }
    return null;
  }

  /** Whether the first frame has been painted. */
  get firstFrameSeen(): boolean {
    return this.#firstFrameDone;
  }
}
