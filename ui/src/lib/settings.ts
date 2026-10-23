// SPDX-License-Identifier: MIT OR Apache-2.0

/**
 * Helpers for the Settings page.
 *
 * Validation here mirrors the Rust rules in `torrentplayer-settings` so messages appear while the user
 * types; Rust re-validates every update and stays authoritative. Also: building patches from
 * dotted paths, bandwidth units, which settings apply now or later, and a debounced saver.
 */

import { isIpcError, type Settings, type SettingsView } from './ipc';

/** A partial update: any subset of the settings, nested like {@link Settings}. */
export type SettingsPatch = Record<string, unknown>;

/** The Settings page's sections, for opening it at one (for example `network`). */
export type SettingsSection =
  'playback' | 'storage' | 'bandwidth' | 'search' | 'network' | 'advanced' | 'about';

// --- Validation ---------------------------------------------------------------------------

/** Inclusive ranges, matching `torrentplayer-settings`. */
export const LIMITS = {
  speed: [0.5, 3],
  rateKib: [0, 1024 * 1024],
  peerLimit: [5, 500],
  maxDownloads: [0, 100],
  cacheLimitGb: [1, 100_000],
  cleanupDays: [1, 3650],
  results: [10, 500],
  port: [1, 65_535],
  listenPort: [1024, 65_535],
} as const;

const MAX_TEXT = 1024;

type Check = (value: unknown) => string | null;

function inRange(value: unknown, [min, max]: readonly [number, number], integer: boolean): boolean {
  return (
    typeof value === 'number' &&
    Number.isFinite(value) &&
    (!integer || Number.isInteger(value)) &&
    value >= min &&
    value <= max
  );
}

function range(limits: readonly [number, number], message: string, integer = true): Check {
  return (value) => (inRange(value, limits, integer) ? null : message);
}

function optional(check: Check): Check {
  return (value) => (value === null ? null : check(value));
}

// eslint-disable-next-line no-control-regex
const CONTROL = /[\u0000-\u001f\u007f-\u009f]/;

/** Non-empty, at most 1024 characters, and no control characters. */
export function isLabel(text: string): boolean {
  return text.trim().length > 0 && text.length <= MAX_TEXT && !CONTROL.test(text);
}

const IPV4 = /^(25[0-5]|2[0-4]\d|1\d\d|[1-9]?\d)(\.(25[0-5]|2[0-4]\d|1\d\d|[1-9]?\d)){3}$/;
const HOST_LABEL = /^[A-Za-z0-9](?:[A-Za-z0-9-]{0,61}[A-Za-z0-9])?$/;

/** A host name or IP address, without a scheme, port, or brackets. */
export function isHost(host: string): boolean {
  if (host.length === 0 || host.length > 253) {
    return false;
  }
  if (IPV4.test(host) || (host.includes(':') && /^[0-9A-Fa-f:.]+$/.test(host))) {
    return true;
  }
  return host.split('.').every((label) => HOST_LABEL.test(label));
}

/** Why a download folder path is unusable, or null if it looks fine. */
export function downloadDirProblem(dir: string): string | null {
  if (dir.length === 0 || dir.length > MAX_TEXT || CONTROL.test(dir)) {
    return 'Enter a folder path.';
  }
  if (dir.startsWith('\\\\?\\') || dir.startsWith('\\\\.\\')) {
    return 'Device paths are not supported; choose a normal folder.';
  }
  if (/^[\\/]{2}/.test(dir)) {
    return 'Network folders are not supported; choose a folder on this computer.';
  }
  const windows = /^[A-Za-z]:[\\/]/.test(dir);
  if (!windows && !dir.startsWith('/')) {
    return 'Enter a full path (for example D:\\Lectures or /home/me/Lectures).';
  }
  const parts = dir.split(/[\\/]+/).slice(windows ? 1 : 0);
  if (parts.some((p) => p === '.' || p === '..')) {
    return 'The path cannot contain "." or "..".';
  }
  if (!parts.some((p) => p.length > 0)) {
    return 'Choose a folder, not the root of a drive.';
  }
  return null;
}

const RULES: Record<string, Check> = {
  'playback.defaultSpeed': range(LIMITS.speed, 'Choose a speed between 0.5× and 3×.', false),
  'storage.downloadDir': optional((v) =>
    typeof v === 'string' ? downloadDirProblem(v) : 'Enter a folder path.',
  ),
  'advanced.ffmpegDir': optional((v) =>
    typeof v === 'string' ? downloadDirProblem(v) : 'Enter a folder path.',
  ),
  'storage.cacheLimitGb': optional(
    range(LIMITS.cacheLimitGb, 'Choose a limit between 1 GB and 100 000 GB, or unlimited.'),
  ),
  'storage.cleanupAfterDays': optional(
    range(LIMITS.cleanupDays, 'Choose between 1 and 3650 days, or never.'),
  ),
  'bandwidth.downloadLimitKib': range(LIMITS.rateKib, 'The largest limit is 1 GiB/s.'),
  'bandwidth.uploadLimitKib': range(LIMITS.rateKib, 'The largest limit is 1 GiB/s.'),
  'bandwidth.peerLimit': range(LIMITS.peerLimit, 'Choose between 5 and 500 peers.'),
  'bandwidth.maxDownloads': range(
    LIMITS.maxDownloads,
    'Choose up to 100 downloads, or 0 for no limit.',
  ),
  'search.resultsPerSearch': range(LIMITS.results, 'Choose between 10 and 500 results.'),
  'network.socks5.host': (v) =>
    typeof v === 'string' && (v === '' || isHost(v))
      ? null
      : 'Enter a host name or IP address without a scheme or port (for example 10.0.0.1).',
  'network.socks5.port': range(LIMITS.port, 'Choose a port between 1 and 65535.'),
  'network.incomingPeers.port': (v) =>
    v === 0 || inRange(v, LIMITS.listenPort, true)
      ? null
      : 'Choose a port between 1024 and 65535, or 0 for a random one.',
  'network.socks5.username': optional((v) =>
    typeof v === 'string' && isLabel(v) && new TextEncoder().encode(v).length <= 255
      ? null
      : 'User names are 1–255 bytes without control characters.',
  ),
};

/** The problem with a new value for the setting at `path`, or null if it is valid. */
export function validate(path: string, value: unknown): string | null {
  return RULES[path]?.(value) ?? null;
}

// --- Patches ------------------------------------------------------------------------------

function isPlainObject(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null && !Array.isArray(value);
}

/** The patch that sets one setting, e.g. `patchAt('bandwidth.peerLimit', 80)`. */
export function patchAt(path: string, value: unknown): SettingsPatch {
  const keys = path.split('.');
  let patch: unknown = value;
  for (let i = keys.length - 1; i >= 0; i -= 1) {
    patch = { [keys[i] ?? '']: patch };
  }
  return patch as SettingsPatch;
}

/** Deep-merges `b` over `a`. Arrays and `null` replace; neither input is modified. */
export function mergePatch(a: SettingsPatch, b: SettingsPatch): SettingsPatch {
  const out: SettingsPatch = { ...a };
  for (const [key, value] of Object.entries(b)) {
    const current = out[key];
    out[key] = isPlainObject(current) && isPlainObject(value) ? mergePatch(current, value) : value;
  }
  return out;
}

/** `settings` with `patch` applied, for showing a change before the backend confirms it. */
export function withPatch(settings: Settings, patch: SettingsPatch): Settings {
  const copy = JSON.parse(JSON.stringify(settings)) as SettingsPatch;
  return mergePatch(copy, patch) as unknown as Settings;
}

/** The setting named at the start of a validation error from `update_settings`, if any. */
export function fieldError(reason: unknown): { field: string; message: string } | null {
  if (!isIpcError(reason) || reason.kind !== 'invalid-argument') {
    return null;
  }
  const match = /^([a-z][A-Za-z0-9]*(?:\.[A-Za-z0-9]+)+): (.+)$/s.exec(reason.message);
  if (!match?.[1] || !match[2]) {
    return null;
  }
  return { field: match[1], message: match[2] };
}

// --- Bandwidth units ----------------------------------------------------------------------

export type RateUnit = 'KB/s' | 'MB/s';

/** A limit in KiB/s as the most readable unit (MB/s when it is a round tenth of one). */
export function splitRate(kib: number): { value: number; unit: RateUnit } {
  if (kib >= 1024 && Number.isInteger((kib * 10) / 1024)) {
    return { value: kib / 1024, unit: 'MB/s' };
  }
  return { value: kib, unit: 'KB/s' };
}

/** Converts a limit typed in `unit` to whole KiB/s. */
export function toKib(value: number, unit: RateUnit): number {
  return Math.round(unit === 'MB/s' ? value * 1024 : value);
}

/** "Unlimited", "512 KB/s", or "2.5 MB/s". */
export function describeRate(kib: number): string {
  if (kib === 0) {
    return 'Unlimited';
  }
  const { value, unit } = splitRate(kib);
  return `${String(value)} ${unit}`;
}

// --- What applies when --------------------------------------------------------------------

/**
 * When a setting takes effect: `live` immediately, `restart` the next time TorrentPlayer starts,
 * `later` once the feature it controls ships (it is saved now).
 */
export type Availability = 'live' | 'restart' | 'later';

const RESTART = new Set(['storage.downloadDir', 'advanced.engine', 'bandwidth.peerLimit']);

const LATER = new Set([
  // Player features not built yet (the default speed, resume positions, the debug overlay key,
  // and the queue's ordering apply now).
  'playback.hardwareDecoding',
  // Search sources and filters not built yet (the adult filter and preloading apply now).
  'search.sources.emberglow',
  'search.sources.internetArchive',
  'search.sources.academicTorrents',
  'search.openLicenseOnly',
  'search.resultsPerSearch',
  // Neither engine implements protocol encryption.
  'network.encryption',
]);

/** When the setting at `path` takes effect. */
export function availability(path: string): Availability {
  if (RESTART.has(path)) {
    return 'restart';
  }
  return LATER.has(path) ? 'later' : 'live';
}

// --- Debounced saving ---------------------------------------------------------------------

export interface Timers {
  set: (fn: () => void, ms: number) => unknown;
  clear: (handle: unknown) => void;
}

const browserTimers: Timers = {
  set: (fn, ms) => setTimeout(fn, ms),
  clear: (handle) => {
    clearTimeout(handle as ReturnType<typeof setTimeout>);
  },
};

export interface SaverOptions {
  /** Sends one merged patch. */
  save: (patch: SettingsPatch) => Promise<SettingsView>;
  /** Called after each successful save; `more` is true if newer changes are waiting. */
  onSaved: (view: SettingsView, more: boolean) => void;
  /** Called when a save fails, with the patch that was rejected. */
  onError: (reason: unknown, patch: SettingsPatch) => void;
  delayMs?: number;
  timers?: Timers;
}

export interface Saver {
  /** Adds a change; it is saved once no further change arrives for the delay. */
  queue: (patch: SettingsPatch) => void;
  /** Saves waiting changes now; resolves when every save so far has finished. */
  flush: () => Promise<void>;
  /** Changes not yet sent, if any. */
  pending: () => SettingsPatch | null;
}

/** Merges rapid changes into one save, and never runs two saves at once. */
export function createSaver(options: SaverOptions): Saver {
  const timers = options.timers ?? browserTimers;
  const delay = options.delayMs ?? 400;
  let waiting: SettingsPatch | null = null;
  let handle: unknown = null;
  let chain: Promise<void> = Promise.resolve();

  function flush(): Promise<void> {
    if (handle !== null) {
      timers.clear(handle);
      handle = null;
    }
    const patch = waiting;
    waiting = null;
    if (patch !== null) {
      chain = chain.then(async () => {
        try {
          const view = await options.save(patch);
          options.onSaved(view, waiting !== null);
        } catch (reason: unknown) {
          options.onError(reason, patch);
        }
      });
    }
    return chain;
  }

  return {
    queue(patch) {
      waiting = waiting === null ? patch : mergePatch(waiting, patch);
      if (handle !== null) {
        timers.clear(handle);
      }
      handle = timers.set(() => {
        handle = null;
        void flush();
      }, delay);
    },
    flush,
    pending: () => waiting,
  };
}

// --- About --------------------------------------------------------------------------------

/** The per-license crate counts from THIRD_PARTY_NOTICES.md ("- MIT License: 494" lines). */
export function licenseSummary(notices: string): { license: string; count: number }[] {
  const start = notices.indexOf('### Number of crates per license');
  if (start < 0) {
    return [];
  }
  const out: { license: string; count: number }[] = [];
  for (const line of notices.slice(start).split('\n').slice(1)) {
    if (line.startsWith('#')) {
      break;
    }
    const match = /^- (.+): (\d+)\s*$/.exec(line);
    if (match?.[1] && match[2]) {
      out.push({ license: match[1], count: Number(match[2]) });
    }
  }
  return out;
}
