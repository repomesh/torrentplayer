// SPDX-License-Identifier: MIT OR Apache-2.0

/**
 * Starting playback without silently muting it.
 *
 * `play()` can reject for several reasons, and only one of them is about sound:
 *
 * - `NotAllowedError`: the autoplay policy refused playback with sound. Muted playback is always
 *   allowed, so it plays muted, and the caller must say so (an "unmute" button).
 * - `AbortError`: a `pause()`, a seek hold, or a new load interrupted the request. Nothing is
 *   wrong with the sound, so it is retried unchanged if playback is still wanted.
 * - Anything else (`NotSupportedError`): the media cannot play; its `error` event says why.
 */

/** The parts of a media element that starting playback uses. */
export interface Playable {
  muted: boolean;
  play(): Promise<void>;
}

/** How {@link startPlayback} ended. */
export type PlayOutcome =
  /** Playing, with the sound as the user left it. */
  | 'playing'
  /** The autoplay policy refused sound: playing muted until the user unmutes. */
  | 'mutedByPolicy'
  /** Refused even muted: the user has to press play. */
  | 'blocked'
  /** Interrupted and no longer wanted (the user paused, or a seek hold will resume it). */
  | 'cancelled'
  /** Failed for another reason, or kept being interrupted. */
  | 'failed';

export interface PlayOptions {
  /** Whether playback is still wanted; checked after every interruption. */
  stillWanted: () => boolean;
  /** Retries after an interruption (`AbortError`). */
  retries?: number;
  /** Waits before a retry (replaced in tests). */
  wait?: (ms: number) => Promise<void>;
}

const RETRY_DELAY_MS = 100;

const sleep = (ms: number): Promise<void> =>
  new Promise((resolve) => {
    setTimeout(resolve, ms);
  });

/** The `name` of a rejection (`AbortError`, `NotAllowedError`, ...), if it has one. */
export function errorName(reason: unknown): string | null {
  return typeof reason === 'object' &&
    reason !== null &&
    'name' in reason &&
    typeof reason.name === 'string'
    ? reason.name
    : null;
}

/**
 * Calls `play()`, retrying interruptions and falling back to muted playback only when the
 * autoplay policy requires it. Never leaves `media` muted unless it returns `mutedByPolicy`.
 */
export async function startPlayback(media: Playable, options: PlayOptions): Promise<PlayOutcome> {
  const { stillWanted, retries = 3, wait = sleep } = options;
  for (let attempt = 0; ; attempt++) {
    try {
      await media.play();
      return 'playing';
    } catch (reason: unknown) {
      const name = errorName(reason);
      if (name === 'NotAllowedError') {
        return playMuted(media, stillWanted);
      }
      if (name !== 'AbortError') {
        return 'failed';
      }
      if (!stillWanted()) {
        return 'cancelled';
      }
      if (attempt >= retries) {
        return 'failed';
      }
      await wait(RETRY_DELAY_MS * (attempt + 1));
      if (!stillWanted()) {
        return 'cancelled';
      }
    }
  }
}

async function playMuted(media: Playable, stillWanted: () => boolean): Promise<PlayOutcome> {
  if (media.muted) {
    // Refused although already muted (by the user): nothing more to try.
    return 'blocked';
  }
  media.muted = true;
  try {
    await media.play();
    return 'mutedByPolicy';
  } catch (reason: unknown) {
    // Not playing after all: restore the sound, so the user's own play starts with it.
    media.muted = false;
    if (errorName(reason) === 'AbortError') {
      return stillWanted() ? 'failed' : 'cancelled';
    }
    return 'blocked';
  }
}
