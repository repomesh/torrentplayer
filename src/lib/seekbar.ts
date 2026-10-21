// SPDX-License-Identifier: MIT OR Apache-2.0

/** Geometry for the seek bar. Pure, so it can be tested without a DOM. */

/** A horizontal span of the seek bar, as fractions of its width. */
export interface Run {
  start: number;
  end: number;
  /** 1 for fully downloaded cells; less for partially downloaded ones. */
  opacity: number;
}

/**
 * Turns the piece map (cells of 0–255 spanning the whole file) into as few drawable spans as
 * possible: neighbouring fully downloaded cells merge into one solid span, neighbouring partial
 * cells with the same value merge into one translucent span, and empty cells are skipped.
 */
export function downloadedRuns(cells: readonly number[]): Run[] {
  const n = cells.length;
  const runs: Run[] = [];
  let i = 0;
  while (i < n) {
    const value = clampCell(cells[i] ?? 0);
    if (value === 0) {
      i += 1;
      continue;
    }
    let j = i + 1;
    while (j < n && clampCell(cells[j] ?? 0) === value) {
      j += 1;
    }
    runs.push({ start: i / n, end: j / n, opacity: value / 255 });
    i = j;
  }
  return runs;
}

function clampCell(v: number): number {
  return Number.isFinite(v) ? Math.min(255, Math.max(0, Math.round(v))) : 0;
}

/** The fraction (0–1) of the bar at a pointer's x position. */
export function fractionAt(clientX: number, left: number, width: number): number {
  if (!(width > 0)) {
    return 0;
  }
  return Math.min(1, Math.max(0, (clientX - left) / width));
}

/** Played fraction, safe for unknown or zero durations. */
export function playedFraction(currentTime: number, duration: number): number {
  if (!Number.isFinite(duration) || duration <= 0 || !Number.isFinite(currentTime)) {
    return 0;
  }
  return Math.min(1, Math.max(0, currentTime / duration));
}

/** Formats a playback time as `m:ss` or `h:mm:ss`. */
export function formatTime(secs: number): string {
  if (!Number.isFinite(secs) || secs < 0) {
    return '--:--';
  }
  const total = Math.floor(secs);
  const h = Math.floor(total / 3600);
  const m = Math.floor((total % 3600) / 60);
  const s = total % 60;
  const ss = s.toString().padStart(2, '0');
  return h > 0 ? `${h.toString()}:${m.toString().padStart(2, '0')}:${ss}` : `${m.toString()}:${ss}`;
}

/** Seek-bar thumbnails per video, at most; nearby positions share one. */
export const THUMBNAIL_STEPS = 120;
/** Shortest time between two thumbnails, in seconds. */
export const MIN_THUMBNAIL_STEP_SECS = 2;
/** Most encoded preview URLs retained for one video. */
export const THUMBNAIL_CACHE_SIZE = 48;

/**
 * The time whose thumbnail stands for `fraction` of the video: the start of its step, so a
 * pointer moving a little asks for nothing new. Null while the duration is unknown.
 */
export function thumbnailTime(fraction: number, duration: number): number | null {
  if (!Number.isFinite(duration) || duration <= 0 || !Number.isFinite(fraction)) {
    return null;
  }
  const step = Math.max(MIN_THUMBNAIL_STEP_SECS, duration / THUMBNAIL_STEPS);
  const at = Math.min(1, Math.max(0, fraction)) * duration;
  // The exact duration has no frame. Keep the right edge in the final non-empty bucket.
  const bucketCount = Math.max(1, Math.ceil(duration / step));
  return Math.min(Math.floor(at / step), bucketCount - 1) * step;
}

/** Thumbnails of one video, remembered once fetched. */
export interface ThumbnailSource {
  /** The picture for `secs`, if already fetched. */
  peek: (secs: number) => string | undefined;
  /** Fetches the picture for `secs` (null: none to show now). */
  get: (secs: number) => Promise<string | null>;
}

/**
 * Remembers the pictures `fetch` returns, up to `limit` (the oldest is forgotten first). A null
 * answer is not remembered: that part may be downloaded later.
 */
export function thumbnailSource(
  fetch: (secs: number) => Promise<string | null>,
  limit: number = THUMBNAIL_CACHE_SIZE,
): ThumbnailSource {
  const pictures = new Map<number, string>();
  return {
    peek: (secs) => pictures.get(secs),
    get: async (secs) => {
      const known = pictures.get(secs);
      if (known !== undefined) {
        return known;
      }
      const picture = await fetch(secs);
      if (picture !== null) {
        if (pictures.size >= limit) {
          const oldest = pictures.keys().next();
          if (oldest.done !== true) {
            pictures.delete(oldest.value);
          }
        }
        pictures.set(secs, picture);
      }
      return picture;
    },
  };
}
