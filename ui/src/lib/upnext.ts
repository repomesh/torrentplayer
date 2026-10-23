// SPDX-License-Identifier: MIT OR Apache-2.0

/**
 * When to preload the next queued video. It only downloads: nothing plays it until the user
 * does.
 *
 * Preloading the next video shares bandwidth with the current one, so it waits until the
 * current file cannot starve: it is fully downloaded, or playback is nearly over and the rest
 * of the file will arrive well before the end.
 */

import { nextItem, type Queue, type QueueItem } from './queue';

/**
 * The queue item to preload: its next item ({@link nextItem}, what Shift+N plays), unless the
 * user paused it, since preloading downloads.
 */
export function preloadTarget(queue: Queue): QueueItem | null {
  const next = nextItem(queue);
  return next !== null && !next.paused ? next : null;
}

/** Playback time left at which the next video may start preloading. */
export const PRELOAD_NEXT_LEAD_SECS = 120;
/** The rest of the file must arrive this many times faster than it plays. */
const SAFETY_FACTOR = 2;

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
