// SPDX-License-Identifier: MIT OR Apache-2.0

/**
 * When to preload the next queued video, and the countdown before it plays.
 *
 * Preloading the next video shares bandwidth with the current one, so it waits until the
 * current file cannot starve: it is fully downloaded, or playback is nearly over and the rest
 * of the file will arrive well before the end.
 */

/** Playback time left at which the next video may start preloading. */
export const PRELOAD_NEXT_LEAD_SECS = 120;
/** The rest of the file must arrive this many times faster than it plays. */
const SAFETY_FACTOR = 2;
/** Seconds the "Up next" countdown runs before the next video plays. */
export const AUTOPLAY_COUNTDOWN_SECS = 5;

export interface NextPreloadInput {
  /** Downloaded and verified bytes of the current file. */
  fileDownloaded: number;
  fileLength: number;
  /** Current download rate, bytes per second. */
  downloadBps: number;
  /** Playback position and duration in seconds (duration NaN until known). */
  position: number;
  duration: number;
}

/** Whether the next queued video can be preloaded without slowing the current one. */
export function shouldPreloadNext(input: NextPreloadInput): boolean {
  const { fileDownloaded, fileLength, downloadBps, position, duration } = input;
  if (fileLength > 0 && fileDownloaded >= fileLength) {
    return true;
  }
  if (!Number.isFinite(duration) || duration <= 0 || !Number.isFinite(position)) {
    return false;
  }
  const remaining = duration - position;
  if (remaining > PRELOAD_NEXT_LEAD_SECS || downloadBps <= 0) {
    return false;
  }
  const missing = Math.max(0, fileLength - fileDownloaded);
  return (missing / downloadBps) * SAFETY_FACTOR < remaining;
}

/** A cancellable countdown that ticks once a second, then calls `onDone`. */
export class Countdown {
  #timer: ReturnType<typeof setInterval> | undefined;
  #left = 0;
  readonly #onTick: (secondsLeft: number) => void;
  readonly #onDone: () => void;

  constructor(onTick: (secondsLeft: number) => void, onDone: () => void) {
    this.#onTick = onTick;
    this.#onDone = onDone;
  }

  /** Seconds left, or 0 when not running. */
  get secondsLeft(): number {
    return this.#left;
  }

  get running(): boolean {
    return this.#timer !== undefined;
  }

  /** Starts (or restarts) the countdown from `seconds`. */
  start(seconds: number = AUTOPLAY_COUNTDOWN_SECS): void {
    this.cancel();
    this.#left = Math.max(1, Math.round(seconds));
    this.#onTick(this.#left);
    this.#timer = setInterval(() => {
      this.#left -= 1;
      if (this.#left > 0) {
        this.#onTick(this.#left);
        return;
      }
      this.cancel();
      this.#onDone();
    }, 1000);
  }

  /** Stops without calling `onDone`. */
  cancel(): void {
    clearInterval(this.#timer);
    this.#timer = undefined;
    this.#left = 0;
  }
}
