// SPDX-License-Identifier: MIT OR Apache-2.0

/**
 * Typed wrappers around Tauri IPC commands.
 *
 * Every call to the Rust backend goes through this module so the IPC contract
 * (documented in docs/ipc.md) lives in one place. Payload types are generated
 * from Rust by ts-rs into ./bindings and must not be edited by hand.
 */

import { invoke } from '@tauri-apps/api/core';
import type { AppInfo } from './bindings/AppInfo';
import type { ConvertedStream } from './bindings/ConvertedStream';
import type { DevAutoplay } from './bindings/DevAutoplay';
import type { IpcError } from './bindings/IpcError';
import type { PlayerMetricKind } from './bindings/PlayerMetricKind';
import type { LibraryItem } from './bindings/LibraryItem';
import type { StreamAudio } from './bindings/StreamAudio';
import type { StreamInfo } from './bindings/StreamInfo';
import type { StreamStats } from './bindings/StreamStats';
import type { TorrentSummary } from './bindings/TorrentSummary';

export type {
  AppInfo,
  DevAutoplay,
  IpcError,
  LibraryItem,
  PlayerMetricKind,
  StreamAudio,
  StreamInfo,
  StreamStats,
  TorrentSummary,
};

/** Returns name, version, and license information for the About screen. */
export function getAppInfo(): Promise<AppInfo> {
  return invoke<AppInfo>('app_info');
}

/** Adds a torrent from a magnet link or an http(s) link to a `.torrent` file. */
export function addTorrentLink(link: string): Promise<TorrentSummary> {
  return invoke<TorrentSummary>('add_torrent_link', { link });
}

/** Adds a torrent from the contents of a `.torrent` file (sent as the raw request body). */
export function addTorrentFile(bytes: Uint8Array): Promise<TorrentSummary> {
  return invoke<TorrentSummary>('add_torrent_file', bytes);
}

/** Starts streaming a video file. The returned URL contains a secret token: never log it. */
export function startStream(torrentId: number, fileIndex: number): Promise<StreamInfo> {
  return invoke<StreamInfo>('start_stream', { torrentId, fileIndex });
}

/** Reports the player's position (call on seeks and about once a second). */
export function reportPlayback(
  streamId: number,
  positionSecs: number,
  durationSecs: number | null,
): Promise<void> {
  return invoke<undefined>('report_playback', { streamId, positionSecs, durationSecs });
}

/** Records a decoder-level timing measured by the player. */
export function recordPlayerMetric(
  streamId: number,
  kind: PlayerMetricKind,
  millis: number,
): Promise<void> {
  return invoke<undefined>('record_player_metric', { streamId, kind, millis });
}

/** Returns live statistics for a stream. */
export function streamStats(streamId: number): Promise<StreamStats> {
  return invoke<StreamStats>('stream_stats', { streamId });
}

/**
 * The stream's audio tracks and codecs, read from its container header. Resolves once the
 * header has been read (usually within a second of starting), or after 45 s at most.
 */
export function streamAudio(streamId: number): Promise<StreamAudio> {
  return invoke<StreamAudio>('stream_audio', { streamId });
}

/**
 * Starts converting a stream's audio (Dolby or DTS, when {@link streamAudio} says it is
 * `convertible`) from `start` seconds, stopping its previous conversion. See converted.ts.
 */
export function convertAudio(streamId: number, start: number): Promise<ConvertedStream> {
  return invoke<ConvertedStream>('convert_audio', { streamId, start });
}

// --- Subtitles ----------------------------------------------------------------------------

import type { StreamSubtitles } from './bindings/StreamSubtitles';
import type { SubtitleCues } from './bindings/SubtitleCues';

export type { StreamSubtitles, SubtitleCues };

/**
 * The stream's subtitles: subtitle files in its torrent (those matching the video first) and
 * embedded tracks (`null` until the container header has been read; ask again after
 * {@link streamAudio} resolves).
 */
export function streamSubtitles(streamId: number): Promise<StreamSubtitles> {
  return invoke<StreamSubtitles>('stream_subtitles', { streamId });
}

/** Downloads one subtitle file of the stream's torrent in the background and parses it. */
export function loadSubtitles(streamId: number, fileIndex: number): Promise<SubtitleCues> {
  return invoke<SubtitleCues>('load_subtitles', { streamId, fileIndex });
}

/** Parses a subtitle file the user chose (sent as the raw request body, at most 5 MiB). */
export function parseSubtitles(bytes: Uint8Array): Promise<SubtitleCues> {
  return invoke<SubtitleCues>('parse_subtitles', bytes);
}

/** Stops a stream. Downloaded data is kept. */
export function stopStream(streamId: number): Promise<void> {
  return invoke<undefined>('stop_stream', { streamId });
}

// --- Library ------------------------------------------------------------------------------

/** Event carrying one item whose playback history changed. */
export const LIBRARY_CHANGED_EVENT = 'library-changed';

/** Saved playback history, most recently used first. */
export function libraryItems(): Promise<LibraryItem[]> {
  return invoke<LibraryItem[]>('library_items');
}

/** Development builds only: a torrent to add and play automatically, if configured. */
export function devAutoplay(): Promise<DevAutoplay | null> {
  return invoke<DevAutoplay | null>('dev_autoplay');
}

/** Whether a rejection value is an {@link IpcError} from the backend. */
export function isIpcError(value: unknown): value is IpcError {
  return (
    typeof value === 'object' &&
    value !== null &&
    'kind' in value &&
    'message' in value &&
    typeof value.message === 'string'
  );
}

/** What to add to network errors so they say what happened and what to do. */
const ERROR_HINTS: Partial<Record<IpcError['kind'], string>> = {
  'kill-switch':
    'Nothing was sent. Traffic resumes by itself when the VPN or proxy works again (see Settings → Network & VPN).',
  'network-unavailable':
    'Check your internet connection, or the VPN or proxy in Settings → Network & VPN.',
};

/** A user-facing message for any rejection from an IPC call. */
export function errorMessage(reason: unknown): string {
  if (isIpcError(reason)) {
    const hint = ERROR_HINTS[reason.kind];
    return hint === undefined ? reason.message : `${reason.message} ${hint}`;
  }
  if (reason instanceof Error) {
    return reason.message;
  }
  return typeof reason === 'string' ? reason : 'Something went wrong.';
}

// --- Search -------------------------------------------------------------------------------

import type { IndexerStatusDto } from './bindings/IndexerStatusDto';
import type { SearchResponseDto } from './bindings/SearchResponseDto';
import type { SearchResultDto } from './bindings/SearchResultDto';

export type { IndexerStatusDto, SearchResponseDto, SearchResultDto };

/**
 * Searches public torrent indexers (search.emberglow.tech), in all categories or only
 * `categories` (Torznab ids such as 2000 Movies, 5000 TV, 5070 Anime; at most 4, never adult
 * ones). Uncached searches take about 6 s. Results are not license-checked.
 */
export function searchTorrents(
  query: string,
  categories: readonly number[] = [],
): Promise<SearchResponseDto> {
  return invoke<SearchResponseDto>('search_torrents', { query, categories });
}

// --- Settings -----------------------------------------------------------------------------

import type { CacheUsage } from './bindings/CacheUsage';
import type { ClearCacheOutcome } from './bindings/ClearCacheOutcome';
import type { SecretKey } from './bindings/SecretKey';
import type { SecretStatus } from './bindings/SecretStatus';
import type { Settings } from './bindings/Settings';
import type { SettingsView } from './bindings/SettingsView';

export type { CacheUsage, ClearCacheOutcome, SecretKey, SecretStatus, Settings, SettingsView };

/** Returns the saved settings and what the Settings page needs to explain them. */
export function getSettings(): Promise<SettingsView> {
  return invoke<SettingsView>('get_settings');
}

/** The backend event carrying the new {@link Settings} after every saved change. */
export const SETTINGS_CHANGED_EVENT = 'settings-changed';
/** Event (no payload) sent when a torrent starts seeding after its playback ended. */
export const SEEDING_STARTED_EVENT = 'seeding-started';

/**
 * Applies a partial update (only the changed fields, nested like {@link Settings}) and returns
 * the saved settings. Rejects with an `invalid-argument` error, whose message starts with the
 * field's dotted path, if any value is invalid; nothing is saved then.
 */
export function updateSettings(patch: object): Promise<SettingsView> {
  return invoke<SettingsView>('update_settings', { patch });
}

/** Stores a secret in the OS credential store. Secrets can never be read back. */
export function setSecret(key: SecretKey, value: string): Promise<SecretStatus> {
  return invoke<SecretStatus>('set_secret', { key, value });
}

/** Removes a secret from the OS credential store. */
export function clearSecret(key: SecretKey): Promise<SecretStatus> {
  return invoke<SecretStatus>('clear_secret', { key });
}

/** Which secrets are set (never their values). */
export function secretStatus(): Promise<SecretStatus> {
  return invoke<SecretStatus>('secret_status');
}

/** How much space the download cache uses. */
export function cacheUsage(): Promise<CacheUsage> {
  return invoke<CacheUsage>('cache_usage');
}

/** Deletes the download cache now, or at the next start if the engine is using it. */
export function clearCache(): Promise<ClearCacheOutcome> {
  return invoke<ClearCacheOutcome>('clear_cache');
}

/**
 * Opens the system folder picker for the download folder and saves the choice. Resolves with the
 * saved settings, or null if the user cancelled. This is the only way to choose the folder:
 * {@link updateSettings} refuses a new one.
 */
export function chooseDownloadDir(): Promise<SettingsView | null> {
  return invoke<SettingsView | null>('choose_download_dir');
}

// --- Network ------------------------------------------------------------------------------

import type { NetworkStatus } from './bindings/NetworkStatus';
import type { PublicIp } from './bindings/PublicIp';
import type { WireGuardImport } from './bindings/WireGuardImport';
import type { WireGuardSummary } from './bindings/WireGuardSummary';

export type { NetworkStatus, PublicIp, WireGuardImport, WireGuardSummary };

/** The backend event carrying a new {@link NetworkStatus} whenever the network state changes. */
export const NETWORK_STATUS_EVENT = 'network-status';
/** Event with the status bar's public addresses (`PublicIp`, or null while unknown or off). */
export const PUBLIC_IP_EVENT = 'network-public-ip';

/** The network mode, connection state, kill switch, and recent events. */
export function networkStatus(): Promise<NetworkStatus> {
  return invoke<NetworkStatus>('network_status');
}

/** Checks the proxy or WireGuard tunnel again now ("Try again"); resolves with the new status. */
export function recheckNetwork(): Promise<NetworkStatus> {
  return invoke<NetworkStatus>('recheck_network');
}

/**
 * Asks icanhazip.com which IPv4 and IPv6 addresses it sees, through the current mode. Rejects
 * with a `kill-switch` error, without sending anything, while traffic is paused.
 */
export function checkConnection(): Promise<PublicIp> {
  return invoke<PublicIp>('check_connection');
}

/**
 * The status bar's latest public addresses (null while unknown, blocked, or turned off by
 * `network.showPublicIp`). Updates arrive as {@link PUBLIC_IP_EVENT} events.
 */
export function publicIp(): Promise<PublicIp | null> {
  return invoke<PublicIp | null>('public_ip');
}

/** Checks a WireGuard configuration and describes it without its keys. Nothing is saved. */
export function validateWireGuardConfig(text: string): Promise<WireGuardSummary> {
  return invoke<WireGuardSummary>('validate_wireguard_config', { text });
}

/**
 * Imports a WireGuard configuration: the backend stores its keys in the OS credential store
 * and the rest in `network.wireguard`. `name` (for example the file name) labels it.
 */
export function importWireGuardConfig(text: string, name: string | null): Promise<WireGuardImport> {
  return invoke<WireGuardImport>('import_wireguard_config', { text, name });
}

// --- Progressive search and preloading ----------------------------------------------------

import { Channel } from '@tauri-apps/api/core';
import type { PlayabilityDto } from './bindings/PlayabilityDto';
import type { PrefetchedDto } from './bindings/PrefetchedDto';
import type { PrefetchRequest } from './bindings/PrefetchRequest';
import type { PreviewFrameDto } from './bindings/PreviewFrameDto';
import type { ThumbnailDto } from './bindings/ThumbnailDto';
import type { ThumbnailRequest } from './bindings/ThumbnailRequest';
import type { TorrentDetailsDto } from './bindings/TorrentDetailsDto';
import type { PreloadPhaseDto } from './bindings/PreloadPhaseDto';
import type { PreloadStatusDto } from './bindings/PreloadStatusDto';
import type { QualityDto } from './bindings/QualityDto';
import type { SearchProgressDto } from './bindings/SearchProgressDto';

export type {
  PlayabilityDto,
  PrefetchedDto,
  PrefetchRequest,
  PreloadPhaseDto,
  PreviewFrameDto,
  ThumbnailDto,
  ThumbnailRequest,
  TorrentDetailsDto,
  PreloadStatusDto,
  QualityDto,
  SearchProgressDto,
};

/**
 * Searches like {@link searchTorrents}, but calls `onProgress` with the merged, ranked results
 * as indexers answer, at most every 150 ms (the first results usually arrive in under a
 * second; slow indexers keep adding results for up to 20 s). Resolves with the final results.
 * Starting another search, or {@link cancelSearch}, stops this one: it rejects with a
 * `cancelled` error. Once the best pick has settled, the backend preloads it if the
 * `search.preloadTopResult` setting is on; follow it with {@link preloadStatus}.
 */
export function searchTorrentsProgressive(
  query: string,
  onProgress: (progress: SearchProgressDto) => void,
  categories: readonly number[] = [],
): Promise<SearchResponseDto> {
  const channel = new Channel<SearchProgressDto>();
  channel.onmessage = onProgress;
  return invoke<SearchResponseDto>('search_torrents_progressive', {
    query,
    categories,
    onProgress: channel,
  });
}

/** Stops the running progressive search, if any. */
export function cancelSearch(): Promise<void> {
  return invoke<undefined>('cancel_search');
}

/**
 * Fetches the torrent details (metadata only) of these results in the background, top first
 * (the backend takes the first 20), calling `onReady` with each one that is ready, so that
 * playing it starts sooner. A result's download link is tried before its magnet link (only
 * the first few download links are followed). Replaces the previous request; a new search also
 * stops it. Does nothing while the `search.preloadTopResult` setting is off. Failures are not
 * reported: playing a result fetches its details itself.
 */
export function prefetchTorrents(
  results: PrefetchRequest[],
  onReady: (ready: PrefetchedDto) => void,
): Promise<void> {
  const channel = new Channel<PrefetchedDto>();
  channel.onmessage = onReady;
  return invoke<undefined>('prefetch_torrents', { results, onReady: channel });
}

/**
 * Finds thumbnails for these results (top first; the backend takes the first 12): a picture
 * from inside each torrent, whose metadata must be known (prefetched). Calls `onReady` with
 * each result's outcome (`dataUrl` null when it has no suitable picture). Replaces the
 * previous request; a new search also stops it.
 */
export function fetchThumbnails(
  results: ThumbnailRequest[],
  onReady: (thumbnail: ThumbnailDto) => void,
): Promise<void> {
  const channel = new Channel<ThumbnailDto>();
  channel.onmessage = onReady;
  return invoke<undefined>('fetch_thumbnails', { results, onReady: channel });
}

/**
 * The preloaded best pick's video, to show a frame from its start (already downloaded), or
 * null unless its preload is ready. The URL carries a session token: never log it.
 */
export function previewFrame(): Promise<PreviewFrameDto | null> {
  return invoke<PreviewFrameDto | null>('preview_frame');
}

/**
 * A result's details from its torrent's metadata, or null if it is not known. With `load`, the
 * metadata is fetched first (metadata only, through the download link, else the magnet link).
 */
export function torrentDetails(
  result: Pick<SearchResultDto, 'infoHash' | 'magnet' | 'download'>,
  load: boolean,
): Promise<TorrentDetailsDto | null> {
  return invoke<TorrentDetailsDto | null>('torrent_details', {
    infoHash: result.infoHash,
    magnet: result.magnet,
    download: result.download,
    load,
  });
}

/** Opens an http(s) link (an indexer's page) in the system browser, after checking it. */
export function openLink(link: string): Promise<void> {
  return invoke<undefined>('open_link', { link });
}

/** The current preload of a search's best pick, or null if there is none. */
export function preloadStatus(): Promise<PreloadStatusDto | null> {
  return invoke<PreloadStatusDto | null>('preload_status');
}

// --- Queue --------------------------------------------------------------------------------

import type { ItemReadiness } from './bindings/ItemReadiness';
import type { NextVideo } from './bindings/NextVideo';
import type { Queue } from './bindings/Queue';
import type { QueueItem } from './bindings/QueueItem';
import type { ReadinessQuery } from './bindings/ReadinessQuery';
import type { ReadinessState } from './bindings/ReadinessState';

export type { ItemReadiness, NextVideo, Queue, QueueItem, ReadinessQuery, ReadinessState };

/** The saved play queue (empty if there is none). */
export function loadQueue(): Promise<Queue> {
  return invoke<Queue>('load_queue');
}

/**
 * Saves the whole play queue. Rejects with an `invalid-argument` error, saving nothing, if an
 * item is invalid (for example a link to this computer: stream URLs are never stored).
 */
export function saveQueue(queue: Queue): Promise<void> {
  return invoke<undefined>('save_queue', { queue });
}

/**
 * Starts preloading the queue's next video while `streamId` plays (see `shouldPreloadNext`
 * for when). A torrent not added yet is preloaded like a search's best pick (follow it with
 * {@link preloadStatus}); in one that is (`torrentId`, for example the course playing), the
 * file's start and end are downloaded in the background.
 */
export function preloadNext(streamId: number, next: NextVideo): Promise<void> {
  return invoke<undefined>('preload_next', { streamId, next });
}

/**
 * How ready queued videos are to play: `complete`, `ready` (start and end downloaded), or
 * `waiting`. Only items in torrents already added are answered. Reads nothing but which
 * pieces are present, and never starts the engine; ask every few seconds at most.
 */
export function queueReadiness(items: ReadinessQuery[]): Promise<ItemReadiness[]> {
  return invoke<ItemReadiness[]>('queue_readiness', { items });
}

import type { DownloadsSnapshot } from './bindings/DownloadsSnapshot';

export type { DownloadsSnapshot };

/**
 * What each queued item is downloading in the background (speed, peers, progress, time
 * left) and how the connection is shared with the video being watched. The backend updates
 * it about once a second and the call reads only that; poll it about as often. Pause an item
 * by saving the queue with its `paused` flag.
 */
export function queueDownloads(): Promise<DownloadsSnapshot> {
  return invoke<DownloadsSnapshot>('queue_downloads');
}

// --- File explorer ------------------------------------------------------------------------

import type { FilePreviewDto } from './bindings/FilePreviewDto';
import type { PreviewKind } from './bindings/PreviewKind';

export type { FilePreviewDto, PreviewKind };

/**
 * A preview of one file of an added torrent: a picture (`data:` URL), plain text (at most
 * 64 KiB; render as text only), a still-frame stream URL for a video whose start is downloaded
 * (never log it), or a `note`. A newer call stops the previous one's wait.
 */
export function previewFile(torrentId: number, fileIndex: number): Promise<FilePreviewDto> {
  return invoke<FilePreviewDto>('preview_file', { torrentId, fileIndex });
}

/**
 * Ends the file explorer's preview when it closes: stops a preview still loading and withdraws
 * the preview video's stream, so its torrent's data is no longer kept from cache clean-up.
 */
export function stopPreview(): Promise<void> {
  return invoke<undefined>('stop_preview');
}

// --- FFmpeg (optional) --------------------------------------------------------------------

import type { MediaToolDto } from './bindings/MediaToolDto';
import type { MediaToolsDto } from './bindings/MediaToolsDto';

export type { MediaToolDto, MediaToolsDto };

/**
 * A frame of the playing video near `positionSecs` (of `durationSecs`) for the seek bar, as a
 * `data:image/jpeg` URL, or null when there is none: the setting is off, FFmpeg is not
 * installed, that part is not downloaded yet, or a newer call replaced this one.
 */
export function seekThumbnail(
  streamId: number,
  positionSecs: number,
  durationSecs: number,
): Promise<string | null> {
  return invoke<string | null>('seek_thumbnail', { streamId, positionSecs, durationSecs });
}

/** Looks for ffprobe and ffmpeg again (in `advanced.ffmpegDir`, or on PATH). */
export function mediaTools(): Promise<MediaToolsDto> {
  return invoke<MediaToolsDto>('media_tools');
}

/**
 * Opens the system folder picker for the FFmpeg folder and saves the choice. Resolves with the
 * saved settings, or null if cancelled. This is the only way to choose the folder.
 */
export function chooseFfmpegDir(): Promise<SettingsView | null> {
  return invoke<SettingsView | null>('choose_ffmpeg_dir');
}
