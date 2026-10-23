// SPDX-License-Identifier: MIT OR Apache-2.0

/** Helpers for the player status bar. Pure, so they can be tested without a DOM. */

/**
 * Exponentially weighted moving average with a time constant, so displayed rates do not jitter
 * when samples arrive about once a second.
 */
export class Ewma {
  readonly #tauSecs: number;
  #value: number | null = null;

  constructor(tauSecs = 3) {
    this.#tauSecs = tauSecs;
  }

  /** Folds in a sample taken `dtSecs` after the previous one and returns the smoothed value. */
  next(sample: number, dtSecs = 1): number {
    if (!Number.isFinite(sample)) {
      return this.#value ?? 0;
    }
    if (this.#value === null) {
      this.#value = sample;
    } else {
      const alpha = 1 - Math.exp(-Math.max(dtSecs, 0) / this.#tauSecs);
      this.#value += alpha * (sample - this.#value);
    }
    return this.#value;
  }

  get value(): number | null {
    return this.#value;
  }
}

/** Buffer health shown as a coloured dot. */
export type Health = 'good' | 'low' | 'stalled' | 'unknown';

/** Everything the player status line shows. */
export interface StatusInfo {
  /** Smoothed rates, bytes per second. */
  downloadBps: number;
  uploadBps: number;
  peersLive: number;
  peersConnecting: number;
  peersSeen: number;
  bufferedSecs: number | null;
  health: Health;
  fileDownloaded: number;
  fileLength: number;
  etaSecs: number | null;
}

/** Green at 10 s or more ahead, yellow from 3 to 10 s, red below 3 s or while stalled. */
export function bufferHealth(bufferedSecs: number | null, stalled: boolean): Health {
  if (stalled) {
    return 'stalled';
  }
  if (bufferedSecs === null || !Number.isFinite(bufferedSecs)) {
    return 'unknown';
  }
  if (bufferedSecs >= 10) {
    return 'good';
  }
  return bufferedSecs >= 3 ? 'low' : 'stalled';
}

/**
 * Seconds of media buffered ahead. Uses the backend's figure when it has one, otherwise derives
 * it from the buffered bytes and the file's average byte rate (length / duration).
 */
export function bufferedSeconds(
  backendSecs: number | null,
  bufferedBytes: number,
  fileLength: number,
  durationSecs: number,
): number | null {
  if (backendSecs !== null && Number.isFinite(backendSecs)) {
    return backendSecs;
  }
  if (!Number.isFinite(durationSecs) || durationSecs <= 0 || !(fileLength > 0)) {
    return null;
  }
  return bufferedBytes / (fileLength / durationSecs);
}

/** Seconds until the rest of the file is downloaded, or null when it cannot be estimated. */
export function etaSeconds(remainingBytes: number, bytesPerSec: number): number | null {
  if (remainingBytes <= 0) {
    return 0;
  }
  if (!Number.isFinite(bytesPerSec) || bytesPerSec < 1024) {
    return null;
  }
  return remainingBytes / bytesPerSec;
}
