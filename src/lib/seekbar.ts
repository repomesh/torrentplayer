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
