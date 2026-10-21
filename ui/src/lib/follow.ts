// SPDX-License-Identifier: MIT OR Apache-2.0

/** Following a value the backend announces with an event. */

import { listen } from '@tauri-apps/api/event';

/**
 * Calls `onValue` with every payload of `event`, and first with `read()`'s answer if given
 * (unless an event arrived first, which is newer), until the returned function is called.
 */
export function follow<T>(
  event: string,
  onValue: (value: T) => void,
  read?: () => Promise<T>,
): () => void {
  let stopped = false;
  let heard = false;
  let unlisten: (() => void) | null = null;
  listen<T>(event, (e) => {
    if (!stopped) {
      heard = true;
      onValue(e.payload);
    }
  })
    .then((stop) => {
      if (stopped) {
        stop();
      } else {
        unlisten = stop;
      }
    })
    .catch(() => undefined);
  read?.()
    .then((value) => {
      if (!stopped && !heard) {
        onValue(value);
      }
    })
    .catch(() => undefined);
  return () => {
    stopped = true;
    unlisten?.();
  };
}
