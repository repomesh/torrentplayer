// SPDX-License-Identifier: MIT OR Apache-2.0

/** Display formatting helpers. Pure functions, so they are easy to test. */

const UNITS = ['B', 'KB', 'MB', 'GB', 'TB'] as const;

/** Formats a byte count with decimal units, e.g. `1.5 GB`. */
export function formatBytes(bytes: number): string {
  if (!Number.isFinite(bytes) || bytes < 0) {
    return '–';
  }
  let value = bytes;
  let unit = 0;
  while (value >= 1000 && unit < UNITS.length - 1) {
    value /= 1000;
    unit += 1;
  }
  const digits = unit === 0 || value >= 100 ? 0 : 1;
  return `${value.toFixed(digits)} ${UNITS[unit] ?? 'B'}`;
}

/** Formats a transfer rate in bytes per second, e.g. `2.4 MB/s`. */
export function formatRate(bytesPerSecond: number): string {
  return `${formatBytes(bytesPerSecond)}/s`;
}

/** Formats a fraction as a whole percentage, clamped to 0–100. */
export function formatPercent(part: number, whole: number): string {
  if (!(whole > 0)) {
    return '0%';
  }
  const pct = Math.min(100, Math.max(0, (part / whole) * 100));
  return `${Math.floor(pct).toString()}%`;
}

/** Formats milliseconds for the debug overlay, or `–` when unknown. */
export function formatMillis(ms: number | null | undefined): string {
  if (ms === null || ms === undefined || !Number.isFinite(ms)) {
    return '–';
  }
  return ms >= 10_000 ? `${(ms / 1000).toFixed(1)} s` : `${Math.round(ms).toString()} ms`;
}

/** A playback position as a clock: `4:05`, `1:02:09`. */
export function formatClock(seconds: number): string {
  const total = Math.max(0, Math.floor(seconds));
  const hours = Math.floor(total / 3600);
  const minutes = Math.floor((total % 3600) / 60);
  const rest = (total % 60).toString().padStart(2, '0');
  return hours > 0
    ? `${String(hours)}:${minutes.toString().padStart(2, '0')}:${rest}`
    : `${String(minutes)}:${rest}`;
}

/** The status line shown while waiting for video. */
export function loadingMessage(state: {
  firstFrameSeen: boolean;
  stalled: boolean;
  hasData: boolean;
  bufferedSecs: number | null;
}): string | null {
  if (!state.firstFrameSeen && !state.hasData) {
    return 'Finding peers…';
  }
  if (!state.firstFrameSeen || state.stalled) {
    const secs = state.bufferedSecs;
    return secs !== null && Number.isFinite(secs) && secs >= 1
      ? `Buffering… ${Math.floor(secs).toString()}s ahead`
      : 'Buffering…';
  }
  return null;
}
