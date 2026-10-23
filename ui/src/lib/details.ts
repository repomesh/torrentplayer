// SPDX-License-Identifier: MIT OR Apache-2.0

/** Helpers for a search result's details view. Everything here is plain text. */

import type { FileSummary } from './bindings/FileSummary';
import type { QualityDto } from './bindings/QualityDto';
import type { TorrentDetailsDto } from './bindings/TorrentDetailsDto';
import { fileName, isPlayable, sortFiles } from './files';

/** Videos listed before "+N more". */
export const LISTED_VIDEOS = 10;

/** A torrent's files, summed up for the details view. */
export interface FilesSummary {
  videoCount: number;
  /** Size of the largest video, if there is one. */
  largestVideo: number | null;
  /** The first videos in lecture order (natural sort), at most {@link LISTED_VIDEOS}. */
  videos: FileSummary[];
  /** Videos not listed. */
  moreVideos: number;
  subtitles: FileSummary[];
  /** The other files by type, most first: `3 PDF · 2 TXT · 1 other`; empty if none. */
  others: string;
}

/** The type label of a file for the "other files" line: its extension, or `other`. */
function typeOf(path: string): string {
  const name = fileName(path);
  const dot = name.lastIndexOf('.');
  const extension = dot > 0 ? name.slice(dot + 1) : '';
  return /^[a-z0-9]{1,5}$/i.test(extension) ? extension.toUpperCase() : 'other';
}

export function summarizeFiles(files: readonly FileSummary[]): FilesSummary {
  const sorted = sortFiles(files);
  const videos = sorted.filter(isPlayable);
  const counts = new Map<string, number>();
  for (const f of sorted) {
    if (!isPlayable(f) && f.kind !== 'subtitle') {
      const type = typeOf(f.path);
      counts.set(type, (counts.get(type) ?? 0) + 1);
    }
  }
  const others = [...counts.entries()]
    // Most first, and files of no known type last.
    .sort(
      (a, b) =>
        Number(a[0] === 'other') - Number(b[0] === 'other') ||
        b[1] - a[1] ||
        a[0].localeCompare(b[0]),
    )
    .map(([type, n]) => `${String(n)} ${type}`)
    .join(' · ');
  return {
    videoCount: videos.length,
    largestVideo: videos.length === 0 ? null : Math.max(...videos.map((v) => v.length)),
    videos: videos.slice(0, LISTED_VIDEOS),
    moreVideos: Math.max(0, videos.length - LISTED_VIDEOS),
    subtitles: sorted.filter((f) => f.kind === 'subtitle'),
    others,
  };
}

/**
 * How peers are found: `2 UDP trackers · 1 HTTP tracker · 1 web seed`, and whether the
 * torrent needs Direct mode (only UDP trackers, while the network mode blocks UDP).
 */
export function trackerSummary(
  udp: number,
  http: number,
  webSeeds: number,
  udpAllowed: boolean | null,
): { text: string; needsDirect: boolean } {
  const plural = (n: number, word: string): string => `${String(n)} ${word}${n === 1 ? '' : 's'}`;
  const parts = [
    udp > 0 ? plural(udp, 'UDP tracker') : null,
    http > 0 ? plural(http, 'HTTP tracker') : null,
    webSeeds > 0 ? plural(webSeeds, 'web seed') : null,
  ].filter((p): p is string => p !== null);
  return {
    text: parts.length === 0 ? 'No trackers (peers from the DHT only)' : parts.join(' · '),
    needsDirect: udp > 0 && http === 0 && webSeeds === 0 && udpAllowed === false,
  };
}

/** The "plays here" verdict and why. */
export function playsVerdict(
  quality: Pick<QualityDto, 'playable' | 'container' | 'codec'>,
): string {
  const what = [quality.container, quality.codec].filter((p) => p !== null).join(', ');
  const because = what === '' ? '' : ` (${what})`;
  switch (quality.playable) {
    case 'plays':
      return `Plays in the built-in player${because}`;
    case 'likely':
      return `Probably plays in the built-in player${because}`;
    case 'unlikely':
      return `Probably does not play in the built-in player${because}: needs a hardware decoder`;
    case 'needsFullPlayer':
      return `Needs the full player${because}`;
    default:
      return 'The title does not say whether it plays here';
  }
}

/** The sound line: `AAC: plays with sound here`, or null when the title names no audio. */
export function soundVerdict(
  quality: Pick<QualityDto, 'audio' | 'audioPlays' | 'audioConverted'>,
): string | null {
  if (quality.audio === null) {
    return null;
  }
  if (quality.audioPlays === null) {
    return `${quality.audio}: sound here unknown`;
  }
  if (quality.audioConverted) {
    return `${quality.audio}: plays with sound here, converted to stereo`;
  }
  return quality.audioPlays
    ? `${quality.audio}: plays with sound here`
    : `${quality.audio}: no sound in the built-in player`;
}

/** The search service's swarm verdict in words, or null if unknown. */
export function healthLabel(health: string | null): string | null {
  switch (health) {
    case 'good':
      return 'Healthy (10 or more seeders)';
    case 'low':
      return 'Few seeders (1–9)';
    case 'dead':
      return 'No seeders';
    default:
      return null;
  }
}

/** A date (milliseconds since 1970) as `2026-10-18`, or an empty string if it is not sane. */
export function formatDay(ms: number | null): string {
  if (ms === null || !Number.isFinite(ms) || ms <= 0) {
    return '';
  }
  const date = new Date(ms);
  return Number.isNaN(date.getTime()) ? '' : date.toISOString().slice(0, 10);
}

/** A piece size as `256 KiB` or `4 MiB`. */
export function formatPieceSize(bytes: number): string {
  return bytes >= 1024 * 1024 && bytes % (1024 * 1024) === 0
    ? `${String(bytes / (1024 * 1024))} MiB`
    : `${String(Math.round(bytes / 1024))} KiB`;
}

// Details already read, by result key, for the session (a torrent's metadata never changes).
const knownDetails = new Map<string, TorrentDetailsDto>();
const MAX_KNOWN = 200;

/** Details read earlier this session for a result, if any. */
export function rememberedDetails(key: string): TorrentDetailsDto | undefined {
  return knownDetails.get(key);
}

/** Keeps a result's details for the session (the most recent {@link MAX_KNOWN}). */
export function rememberDetails(key: string, details: TorrentDetailsDto): void {
  knownDetails.delete(key);
  knownDetails.set(key, details);
  for (const old of knownDetails.keys()) {
    if (knownDetails.size <= MAX_KNOWN) {
      break;
    }
    knownDetails.delete(old);
  }
}

/** `set` with `key` added, or removed if it was there. */
export function toggled(set: ReadonlySet<string>, key: string): Set<string> {
  const next = new Set(set);
  if (!next.delete(key)) {
    next.add(key);
  }
  return next;
}
