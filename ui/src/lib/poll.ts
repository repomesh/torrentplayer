// SPDX-License-Identifier: MIT OR Apache-2.0

/** What `pollWhileVisible` needs of `document`. */
export interface Visibility {
  readonly visibilityState: DocumentVisibilityState;
  addEventListener(type: 'visibilitychange', listener: () => void): void;
  removeEventListener(type: 'visibilitychange', listener: () => void): void;
}

/**
 * Calls `poll` at once and then every `intervalMs` while the window is on screen (not
 * minimized), and again as soon as it shows. One call at a time: a tick while the last call is
 * still waiting is skipped, so an older answer never lands after a newer one. Returns a function
 * that stops it.
 */
export function pollWhileVisible(
  poll: () => Promise<unknown>,
  intervalMs: number,
  doc: Visibility = document,
): () => void {
  let inFlight = false;
  let stopped = false;
  const tick = (): void => {
    if (inFlight || stopped || doc.visibilityState !== 'visible') {
      return;
    }
    inFlight = true;
    poll()
      .catch(() => undefined)
      .finally(() => {
        inFlight = false;
      });
  };
  const timer = setInterval(tick, intervalMs);
  doc.addEventListener('visibilitychange', tick);
  tick();
  return () => {
    stopped = true;
    clearInterval(timer);
    doc.removeEventListener('visibilitychange', tick);
  };
}

/** The last playback position sent to the backend, and when. */
export interface ReportedPosition {
  position: number;
  duration: number | null;
  at: number;
}

/**
 * Whether the periodic progress report should be sent: whenever the position or duration
 * changed, and while they don't (paused, minimized while paused) only every `repeatMs`, so the
 * backend still saves a paused position on its own schedule without a call every second.
 */
export function reportDue(
  last: ReportedPosition | null,
  position: number,
  duration: number | null,
  now: number,
  repeatMs: number,
): boolean {
  return (
    last === null ||
    last.position !== position ||
    last.duration !== duration ||
    now - last.at >= repeatMs
  );
}
