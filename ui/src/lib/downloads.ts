// SPDX-License-Identifier: MIT OR Apache-2.0

/**
 * Background downloading of the queue, as the backend reports it (`queue_downloads`): what
 * each item is doing, and how the connection is shared with the video being watched.
 * Pure wording and arithmetic, so the queue rows stay simple.
 */

import type { DownloadsSnapshot } from './bindings/DownloadsSnapshot';
import type { ItemDownload } from './bindings/ItemDownload';
import type { LinkUse } from './bindings/LinkUse';
import { formatRate } from './format';

export type { DownloadsSnapshot, ItemDownload, LinkUse };

/** States in which an item is moving data. */
const MOVING = new Set<ItemDownload['state']>(['downloading', 'connecting', 'watching']);

/** "45 s", "4 min", "1 h 20 min". */
export function formatEta(secs: number | null): string | null {
  if (secs === null || !Number.isFinite(secs) || secs < 0) {
    return null;
  }
  if (secs < 60) {
    return `${String(Math.max(1, Math.round(secs)))} s`;
  }
  const mins = Math.round(secs / 60);
  if (mins < 60) {
    return `${String(mins)} min`;
  }
  const h = Math.floor(mins / 60);
  const m = mins % 60;
  return m === 0 ? `${String(h)} h` : `${String(h)} h ${String(m)} min`;
}

/** The share downloaded, 0–1, or null while the length is unknown. */
export function progressOf(d: ItemDownload): number | null {
  if (d.length === null || d.length <= 0) {
    return null;
  }
  return Math.min(1, Math.max(0, d.downloaded / d.length));
}

function percent(d: ItemDownload): string | null {
  const p = progressOf(d);
  return p === null ? null : `${String(Math.floor(p * 100))}%`;
}

function peers(n: number): string {
  return `${String(n)} ${n === 1 ? 'peer' : 'peers'}`;
}

/**
 * One line about an item's download: `↓ 2.4 MB/s · 14 peers · 37% · 4 min left`, or what it
 * waits for. Null when there is nothing to say (an unknown, untouched item).
 */
export function describeDownload(d: ItemDownload): string | null {
  const parts: string[] = [];
  const pct = percent(d);
  switch (d.state) {
    case 'downloading':
    case 'watching': {
      parts.push(`↓ ${formatRate(d.downloadBps)}`, peers(d.peersLive));
      if (pct !== null) {
        parts.push(pct);
      }
      const eta = formatEta(d.etaSecs);
      if (eta !== null) {
        parts.push(`${eta} left`);
      }
      if (d.limited && d.state === 'downloading') {
        parts.push('slowed for playback');
      }
      break;
    }
    case 'complete':
      // Finished torrents keep sharing until the user pauses or removes them.
      parts.push(d.uploading ? 'Downloaded · seeding' : 'Downloaded');
      break;
    case 'connecting':
      parts.push(
        d.peersSeen > 0 ? `Connecting to peers (${String(d.peersSeen)} found)` : 'Finding peers…',
      );
      if (pct !== null && d.downloaded > 0) {
        parts.push(pct);
      }
      break;
    case 'starting':
      parts.push('Starting…');
      break;
    case 'queued':
      parts.push(
        d.waitReason === 'playback'
          ? 'Waiting for playback: the video playing needs the bandwidth'
          : d.waitReason === 'limit'
            ? 'Waiting: your limit of downloads at once'
            : 'Waiting',
      );
      if (pct !== null && d.downloaded > 0) {
        parts.push(pct);
      }
      break;
    case 'skipped':
      // The row already says Skipped; say only what is kept.
      if (pct !== null && d.downloaded > 0) {
        parts.push(`${pct} downloaded`);
      }
      break;
    case 'paused':
      // Only the user pauses; nothing resumes it but the user.
      parts.push('Paused');
      if (pct !== null && d.downloaded > 0) {
        parts.push(pct);
      }
      break;
    case 'failed':
      parts.push(
        d.error === null ? 'Could not start; trying again soon' : `${d.error} Trying again soon.`,
      );
      break;
  }
  // Sharing with other peers, whatever else it is doing.
  if (d.uploadBps > 0) {
    parts.push(`↑ ${formatRate(d.uploadBps)}`);
  }
  return parts.length === 0 ? null : parts.join(' · ');
}

/** The app's total transfer rates: `↓ 2.4 MB/s · ↑ 120 KB/s`. */
export function describeRates(link: LinkUse): string {
  return `↓ ${formatRate(link.usedBps)} · ↑ ${formatRate(link.uploadBps)}`;
}

/**
 * The colour of an item's status: `playable` (green: its start and end are downloaded, or all
 * of it, or it is playing), `downloading` (yellow: connected to peers and downloading, not
 * playable yet), or `waiting` (grey: no peer yet, queued, or paused).
 */
export type Tone = 'playable' | 'downloading' | 'waiting';

export function toneOf(d: ItemDownload): Tone {
  if (d.startable || d.state === 'complete' || d.state === 'watching') {
    return 'playable';
  }
  if (d.state === 'downloading' && d.peersLive > 0) {
    return 'downloading';
  }
  return 'waiting';
}

/** Whether an item is moving data now (for a live progress bar). */
export function isMoving(d: ItemDownload): boolean {
  return MOVING.has(d.state);
}

/**
 * Whether the pause button applies: anything but the video being watched. Pausing stops both
 * downloading and sharing, so it applies to finished and skipped items too.
 */
export function canPause(d: ItemDownload | undefined): boolean {
  return d?.state !== 'watching';
}

/**
 * The item downloading fastest, when at least two download and it is clearly ahead (half as
 * fast again as the next): a hint that it would play smoothest now.
 */
export function fastestItem(items: readonly ItemDownload[]): string | null {
  const moving = items
    .filter((d) => d.state === 'downloading' && d.downloadBps > 0)
    .sort((a, b) => b.downloadBps - a.downloadBps);
  const [first, second] = moving;
  if (first === undefined || second === undefined) {
    return null;
  }
  return first.downloadBps >= second.downloadBps * 1.5 ? first.id : null;
}

/**
 * A summary of the connection: `Connection ≈ 12 MB/s · 9.8 MB/s in use · 3 downloading`, plus
 * how playback is doing when a video plays.
 */
export function describeLink(link: LinkUse): string | null {
  if (link.capacityBps === 0 && link.usedBps === 0 && link.active === 0) {
    return null;
  }
  const parts: string[] = [];
  if (link.capacityBps > 0) {
    parts.push(`Connection ≈ ${formatRate(link.capacityBps)}`);
  }
  parts.push(`${formatRate(link.usedBps)} in use`);
  if (link.active > 0) {
    parts.push(
      link.maxActive === null
        ? `${String(link.active)} downloading`
        : `${String(link.active)} of max ${String(link.maxActive)} downloading`,
    );
  }
  if (link.playbackNeedBps !== null) {
    parts.push(
      link.pressure === 'starving'
        ? 'queue waiting for playback'
        : link.pressure === 'guarded'
          ? 'playback first'
          : 'playback has enough',
    );
  }
  return parts.join(' · ');
}

/** The snapshot's items by id. */
export function byId(snapshot: DownloadsSnapshot | null): ReadonlyMap<string, ItemDownload> {
  return new Map((snapshot?.items ?? []).map((d) => [d.id, d]));
}
