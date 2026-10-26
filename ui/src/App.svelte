<!-- SPDX-License-Identifier: MIT OR Apache-2.0 -->
<script lang="ts">
  /*
   * The app shell: the navigation rail, the views, the queue, and what is playing.
   *
   * Views are reached from the rail or with Back/Forward (Alt+←/→, the mouse's side buttons).
   * The search page stays mounted, so its results survive any trip elsewhere. Leaving the
   * player keeps the stream open (it keeps downloading) and coming back resumes where the
   * video was; playing something else, or Stop, closes it.
   */
  import { onMount, tick, untrack } from 'svelte';
  import AddTorrent from './lib/components/AddTorrent.svelte';
  import FileChooser from './lib/components/FileChooser.svelte';
  import FileExplorer from './lib/components/FileExplorer.svelte';
  import Library from './lib/components/Library.svelte';
  import NavRail from './lib/components/NavRail.svelte';
  import NetworkIndicator from './lib/components/NetworkIndicator.svelte';
  import TransferRates from './lib/components/TransferRates.svelte';
  import OpenTorrent from './lib/components/OpenTorrent.svelte';
  import Player from './lib/components/Player.svelte';
  import PlaylistPanel from './lib/components/PlaylistPanel.svelte';
  import VpnHint from './lib/components/VpnHint.svelte';
  import SettingsPage from './lib/components/settings/SettingsPage.svelte';
  import type { FileSummary } from './lib/bindings/FileSummary';
  import { fileName, isPlayable, playTarget, readTorrentFile } from './lib/files';
  import { dismissVpnHint, followSeedingStarted, followVpnHint, showVpnHint } from './lib/vpnHint';
  import { explorerTorrents, torrentKeys, type ExplorerTorrent } from './lib/explorer';
  import { byId } from './lib/downloads';
  import {
    addTorrentFile,
    addTorrentLink,
    devAutoplay,
    errorMessage,
    loadQueue,
    preloadNext,
    preloadStatus,
    queueDownloads,
    queueReadiness,
    saveQueue,
    startStream,
    stopStream,
    streamStats,
    type DownloadsSnapshot,
    type LibraryItem,
    type PreloadStatusDto,
    type SearchResultDto,
    type StreamInfo,
    type StreamStats,
    type TorrentSummary,
    type ReadinessState,
  } from './lib/ipc';
  import { followLibrary, libraryKey, resumeStart } from './lib/library';
  import {
    back,
    canGoBack,
    canGoForward,
    forward,
    initialHistory,
    keyNavigation,
    lastSearchId,
    mouseNavigation,
    prune,
    push,
    replace,
    sameView,
    viewKey,
    type NavHistory,
    type View,
    type ViewName,
  } from './lib/nav';
  import {
    QueueSaver,
    clipTitle,
    emptyQueue,
    enqueue,
    findEntry,
    groupKey,
    moveGroup,
    newItemId,
    nextItem,
    playbackEnded,
    restoreOrder,
    queueForPlayback,
    queueEntryState,
    removeItem,
    restoreRemoved,
    resolveTorrent,
    sanitizeQueue,
    setCurrent,
    setPaused,
    sortQueue,
    SORT_KEYS,
    type SortKey,
    upcomingCount,
    type MainFile,
    type Queue,
    type QueueItem,
    type Removed,
  } from './lib/queue';
  import { preloadActive, preloadLabel } from './lib/search';
  import type { SettingsSection } from './lib/settings';
  import { preloadTarget, shouldPreloadNext } from './lib/upnext';
  import { TorrentLoads, torrentLoadKeys } from './lib/torrent-loads';
  import { Toasts, type Toast } from './lib/toasts';

  /** The video open in the player (it may be hidden while the user browses). */
  interface NowPlaying {
    torrent: TorrentSummary;
    file: FileSummary;
    stream: StreamInfo;
    /** `performance.now()` when the player was last shown, for its start-up timer. */
    startedAt: number;
    autoSeeks: number[];
    /** Where to continue when the player is shown again. */
    resumeAt: number;
    /** `Continuing from 12:34`, shown over the video when it continued from a saved position. */
    resumeChip: string | null;
    /** The queue item playing, if it came from the queue. */
    queueId: string | null;
  }

  const HOME: View = { name: 'search', search: null };
  const PLAYLIST_KEY = 'torrentplayer.playlistOpen';
  /** How often the next queued video's preload conditions are checked while playing. */
  const NEXT_CHECK_MS = 2000;

  let history = $state.raw(initialHistory(HOME));
  const view = $derived(history.current);
  const shownSearch = $derived(lastSearchId(history));

  let torrents = $state.raw<TorrentSummary[]>([]);
  /** Links that add each torrent again (for queue entries), by torrent id. */
  const sources: Record<number, string> = {};
  let lastTorrentId = $state<number | null>(null);
  let nowPlaying = $state.raw<NowPlaying | null>(null);
  let player: { togglePlayback: () => void; checkpoint: () => Promise<void> } | undefined =
    $state();
  let playbackPaused = $state(true);
  /** The player's latest position, updated many times a second, so not reactive. */
  const playback = { position: 0, duration: Number.NaN };

  let queue = $state.raw<Queue>(emptyQueue());
  let library = $state.raw<LibraryItem[]>([]);
  const watchProgress = $derived(new Map(library.map((item) => [libraryKey(item), item] as const)));
  /** The queue as last loaded or saved, so loading it does not save it again. */
  let persisted: Queue | null = null;
  const saver = new QueueSaver(saveQueue, (reason) => {
    showToast(`The queue could not be saved: ${errorMessage(reason)}`, { error: true });
  });

  let loadingTorrents = $state.raw<ReadonlySet<string>>(new Set());
  const torrentLoads = new TorrentLoads<TorrentSummary>((keys) => {
    loadingTorrents = keys;
  });
  // A newer selection or navigation supersedes older loads' player/view changes.
  let selection = $state(0);
  let pendingPlayback = $state.raw<{
    request: number;
    source: string;
    infoHash: string | null;
    queueId?: string;
  } | null>(null);
  let addError = $state<string | null>(null);
  let playError = $state<string | null>(null);
  let starting = $state.raw<{ request: number; torrentId: number; fileIndex: number } | null>(null);
  // Stream starts change download priorities, so only that short stage is serialized.
  let streamStart: Promise<void> = Promise.resolve();
  let streamStop: Promise<void> = Promise.resolve();
  let queueLoaded: Promise<void> = Promise.resolve();
  let playlistOpen = $state(readPlaylistOpen());
  let toasts = $state.raw<readonly Toast[]>([]);
  const toaster = new Toasts((list) => {
    toasts = list;
  });
  let announcement = $state('');
  // The VPN note, once per run, when the first torrent starts uploading in Direct mode.
  // It disappears as soon as it no longer applies (dismissed anywhere, or another mode chosen).
  let vpnHintApplies = $state(false);
  let seedingHintOpen = $state(false);
  let seedingHintShown = false;
  let seedingHintError = $state<string | null>(null);
  const seedingHint = $derived(seedingHintOpen && vpnHintApplies);

  // The next item's preload: asked once per (stream, item); its status shown in the queue.
  let nextPreloadKey: string | null = null;
  let nextPreload = $state<PreloadStatusDto | null>(null);
  let nextPreloadTimer: ReturnType<typeof setTimeout> | undefined;

  // Bumped to put the cursor in the search box (Ctrl+F, or arriving at an empty search).
  let focusSearch = $state(0);
  // The stream the player was last shown for, so showing it again resumes instead of restarting.
  let shownStream: number | null = null;
  let searchEl: HTMLDivElement | undefined = $state();
  let pageEl: HTMLDivElement | undefined = $state();
  // Shown fullscreen instead of the player, so fullscreen survives the next video replacing it.
  let stageEl: HTMLDivElement | undefined = $state();
  const scrolls: Record<string, number> = {};
  const focused: Record<string, HTMLElement | undefined> = {};

  const shownTorrent = $derived(
    view.name === 'torrent' ? (torrents.find((t) => t.id === view.torrentId) ?? null) : null,
  );
  const next = $derived(nextItem(queue));
  const toPreload = $derived(preloadTarget(queue));
  const nextStatus = $derived(next ? preloadLabel(nextPreload, next.infoHash ?? '') : null);
  const playingId = $derived(nowPlaying?.queueId ?? null);

  // --- Queue groups and readiness --------------------------------------------------------

  /** How often readiness is asked for while the queue is shown. */
  const READINESS_MS = 4000;
  const lectures = new Intl.Collator(undefined, { numeric: true, sensitivity: 'base' });

  /** How ready queued items are to play, by item id (items in torrents not added: absent). */
  let readiness = $state.raw<ReadonlyMap<string, ReadinessState>>(new Map());
  /** The torrent whose videos "Choose files…" shows. */
  let chooser = $state.raw<TorrentSummary | null>(null);
  const queueShown = $derived(view.name === 'player' && (playlistOpen || nowPlaying === null));

  /** The opened torrent a queue group (by `groupKey`) belongs to, if it was opened. */
  function torrentOf(key: string): TorrentSummary | null {
    return (
      torrents.find(
        (t) =>
          t.infoHash.toLowerCase() === key ||
          groupKey({ infoHash: null, source: sourceOf(t) }) === key,
      ) ?? null
    );
  }

  /** An item's info hash, or its torrent's if it was queued by a download link. */
  function hashOf(item: QueueItem): string | null {
    return item.infoHash ?? torrentOf(groupKey(item))?.infoHash ?? null;
  }

  async function refreshReadiness(): Promise<void> {
    const items = queue.items.map((item) => ({
      id: item.id,
      infoHash: hashOf(item),
      fileIndex: item.fileIndex,
    }));
    try {
      const answers = await queueReadiness(items);
      readiness = new Map(answers.map((a) => [a.id, a.state]));
    } catch {
      return;
    }
  }

  /** How often background downloads are read while the queue is shown. */
  const DOWNLOADS_MS = 1000;
  /** What each queued item downloads in the background, and how the link is shared. */
  let downloads = $state.raw<DownloadsSnapshot | null>(null);

  /** Whether the window is on screen (not minimized): the queue's polls stop while it is not. */
  let pageVisible = $state(document.visibilityState === 'visible');
  $effect(() => {
    const follow = (): void => {
      pageVisible = document.visibilityState === 'visible';
    };
    document.addEventListener('visibilitychange', follow);
    return () => {
      document.removeEventListener('visibilitychange', follow);
    };
  });

  // Background downloads are read every second while the queue is on screen. A snapshot equal
  // to the last one is dropped, so idle rows (paused, finished) don't re-render every second.
  $effect(() => {
    if (!queueShown || !pageVisible) {
      return;
    }
    let last = '';
    const refresh = (): void => {
      queueDownloads().then(
        (snapshot) => {
          const json = JSON.stringify(snapshot);
          if (json !== last) {
            last = json;
            downloads = snapshot;
          }
        },
        () => undefined,
      );
    };
    refresh();
    const timer = setInterval(refresh, DOWNLOADS_MS);
    return () => {
      clearInterval(timer);
    };
  });

  // Readiness is read every few seconds while the queue is on screen.
  $effect(() => {
    if (!queueShown || !pageVisible) {
      return;
    }
    untrack(() => void refreshReadiness());
    const timer = setInterval(() => void refreshReadiness(), READINESS_MS);
    return () => {
      clearInterval(timer);
    };
  });

  /** "Sort by…": reorders the queue once, with Undo. Nothing re-sorts it later. */
  function sortBy(key: SortKey): void {
    const before = queue;
    const sorted = sortQueue(queue, key, byId(downloads));
    const label = SORT_KEYS.find((k) => k.key === key)?.label ?? key;
    if (sorted === before) {
      showToast(`Already in that order: ${label}`);
      return;
    }
    queue = sorted;
    showToast(`Sorted by ${label}`, {
      undo: () => {
        queue = restoreOrder(queue, before);
      },
    });
  }

  function removeGroup(key: string): void {
    const items = queue.items.filter((i) => groupKey(i) === key);
    removeQueueItems(
      items.map((i) => i.id),
      `Removed ${String(items.length)} ${items.length === 1 ? 'video' : 'videos'}`,
    );
  }

  /** "Choose files…": shows the torrent's videos, adding it first if it is not open yet. */
  async function chooseFiles(key: string): Promise<void> {
    const request = ++selection;
    const torrent = await loadTorrent(key);
    if (torrent && request === selection) {
      chooser = torrent;
    }
  }

  /**
   * The torrent of a queue group, adding it first if it is not open yet (its details are
   * fetched; nothing is downloaded), so its tree can list all its videos.
   */
  async function loadTorrent(key: string): Promise<TorrentSummary | null> {
    const known = torrentOf(key);
    if (known) {
      return known;
    }
    const item = queue.items.find((i) => groupKey(i) === key);
    if (!item) {
      return null;
    }
    try {
      return await fetchTorrent(item.source, item.infoHash);
    } catch (reason: unknown) {
      showToast(`Could not list its files: ${errorMessage(reason)}`, { error: true });
      return null;
    }
  }

  /** The torrent and file playing, for the queue tree's "Playing" mark. */
  const playingFile = $derived(
    nowPlaying
      ? {
          keys: torrentKeys(nowPlaying.torrent, sourceOf(nowPlaying.torrent)),
          index: nowPlaying.file.index,
        }
      : null,
  );

  function chooseFile(torrent: TorrentSummary, file: FileSummary, queued: boolean): void {
    const entry = findEntry(
      queue,
      {
        source: sourceOf(torrent),
        infoHash: torrent.infoHash,
        fileIndex: file.index,
      },
      mainFile,
    );
    if (queued && !entry) {
      queue = enqueue(queue, itemsFor(torrent, [file]), lectureOrder(torrent)).queue;
    } else if (!queued && entry) {
      removeFromQueue(entry.id);
    }
  }

  // --- Navigation ---------------------------------------------------------------------

  function navigate(to: View): void {
    selection += 1;
    if (sameView(history.current, to)) {
      if (to.name === 'settings' && history.current.name === 'settings') {
        history = replace(history, to);
      }
      return;
    }
    go(push(history, to));
  }

  function goBack(): void {
    // Esc in the player closes what is over the video first.
    if (chooser) {
      chooser = null;
      return;
    }
    if (canGoBack(history)) {
      go(back(history));
    }
  }

  function goForward(): void {
    if (canGoForward(history)) {
      go(forward(history));
    }
  }

  /** Moves to another place in the history. */
  function go(next: NavHistory): void {
    selection += 1;
    const fromPlayer = view.name === 'player';
    leaving();
    history = next;
    if (!fromPlayer && next.current.name === 'player' && nowPlaying) {
      if (nowPlaying.stream.streamId === shownStream) {
        // Shown again: the player remounts and continues where the video was.
        nowPlaying = {
          ...nowPlaying,
          startedAt: performance.now(),
          resumeAt: playback.position,
          resumeChip: null,
        };
      }
      shownStream = nowPlaying.stream.streamId;
    }
    void arrived();
  }

  /** The element that scrolls for a view. */
  function scroller(v: View): HTMLElement | undefined {
    return v.name === 'search' ? searchEl : pageEl;
  }

  /** Remembers the scroll position and focus of the view being left. */
  function leaving(): void {
    const key = viewKey(view);
    const el = scroller(view);
    if (el) {
      scrolls[key] = el.scrollTop;
      const active = document.activeElement;
      if (active instanceof HTMLElement && el.contains(active)) {
        focused[key] = active;
      }
    }
  }

  /** Restores the scroll position and focus of the view arrived at, and announces it. */
  async function arrived(): Promise<void> {
    await tick();
    const v = view;
    const key = viewKey(v);
    const el = scroller(v);
    if (el && v.name !== 'player') {
      el.scrollTop = scrolls[key] ?? 0;
    }
    announcement = describe(v);
    if (v.name === 'player') {
      // The player handles the keyboard itself; a focused rail button would catch Space too.
      if (document.activeElement instanceof HTMLElement) {
        document.activeElement.blur();
      }
      return;
    }
    const remembered = focused[key];
    if (remembered?.isConnected === true && el?.contains(remembered) === true) {
      remembered.focus({ preventScroll: true });
    } else if (v.name === 'search') {
      if (v.search === null) {
        focusSearch += 1;
      }
    } else {
      el?.querySelector<HTMLElement>('[tabindex="-1"]')?.focus({ preventScroll: true });
    }
  }

  function describe(v: View): string {
    switch (v.name) {
      case 'search':
        return 'Search';
      case 'open':
        return 'Open a torrent';
      case 'torrent':
        return `Files of ${torrents.find((t) => t.id === v.torrentId)?.name ?? 'the torrent'}`;
      case 'player':
        return nowPlaying ? `Playing ${nowPlaying.stream.fileName}` : 'Queue';
      case 'library':
        return 'Library';
      case 'settings':
        return 'Settings';
    }
  }

  function onRail(name: ViewName): void {
    switch (name) {
      case 'search':
        navigate({ name: 'search', search: shownSearch });
        break;
      case 'torrent':
        if (lastTorrentId !== null) {
          navigate({ name: 'torrent', torrentId: lastTorrentId });
        } else {
          const first = explored[0];
          if (first) {
            void explore(first, false);
          }
        }
        break;
      case 'player':
        showPlayer();
        break;
      case 'library':
        navigate({ name: 'library' });
        break;
      case 'settings':
        openSettings();
        break;
      case 'open':
        navigate({ name: 'open' });
        break;
    }
  }

  /** Goes to the search page with the cursor in the search box (Ctrl+F). */
  function showSearch(): void {
    focused[viewKey({ name: 'search', search: shownSearch })] = undefined;
    navigate({ name: 'search', search: shownSearch });
    focusSearch += 1;
  }

  function openSettings(section: SettingsSection = 'playback'): void {
    navigate({ name: 'settings', section });
  }

  /** The Playing view: the video (if any) and the queue beside it. */
  function showPlayer(): void {
    navigate({ name: 'player' });
  }

  /** Every torrent the explorer lists: the queue's, then the others opened this session. */
  const explored = $derived(explorerTorrents(queue, torrents, (t) => torrentKeys(t, sourceOf(t))));

  /**
   * Shows a torrent in the explorer, adding it first if it is only in the queue (its files
   * are fetched; nothing is downloaded). `replaceView` swaps the explorer's torrent in place,
   * so Back does not step through every torrent looked at.
   */
  async function explore(entry: ExplorerTorrent, replaceView: boolean): Promise<void> {
    const request = ++selection;
    let torrent = entry.torrent;
    if (torrent === null) {
      if (entry.source === null) {
        return;
      }
      try {
        torrent = await fetchTorrent(
          entry.source,
          entry.key.startsWith('link:') ? null : entry.key,
        );
      } catch (reason: unknown) {
        showToast(`Could not list its files: ${errorMessage(reason)}`, { error: true });
        return;
      }
    }
    if (request !== selection) {
      return;
    }
    lastTorrentId = torrent.id;
    playError = null;
    const to: View = { name: 'torrent', torrentId: torrent.id };
    if (replaceView && view.name === 'torrent') {
      history = replace(history, to);
    } else {
      navigate(to);
    }
  }

  // --- Torrents and playback ----------------------------------------------------------

  /** Records an opened torrent (most recent first) and the link that adds it again. */
  function remember(torrent: TorrentSummary, source: string | null): void {
    torrents = [torrent, ...torrents.filter((t) => t.id !== torrent.id)];
    if (source !== null || !(torrent.id in sources)) {
      // A torrent opened from a file is saved as a magnet link with its trackers.
      sources[torrent.id] = source ?? torrent.magnet;
    }
    // Its queued items learn its hash, and entries that turn out to play the same file merge.
    const resolved = resolveTorrent(
      queue,
      { source: sourceOf(torrent), infoHash: torrent.infoHash },
      mainFile,
    );
    queue = resolved.queue;
    const playingId = nowPlaying?.queueId;
    const mergedInto = playingId == null ? undefined : resolved.merged.get(playingId);
    if (nowPlaying && mergedInto !== undefined) {
      nowPlaying = { ...nowPlaying, queueId: mergedInto };
    }
    const pendingId = pendingPlayback?.queueId;
    const pendingMerged = pendingId === undefined ? undefined : resolved.merged.get(pendingId);
    if (pendingPlayback && pendingMerged !== undefined) {
      pendingPlayback = { ...pendingPlayback, queueId: pendingMerged };
    }
  }

  /**
   * The file a queued `null` file index (a search result's main video) plays in an opened
   * torrent, so the queue can tell it is the same entry as that file; null while the torrent's
   * files are unknown.
   */
  const mainFile: MainFile = (item) => {
    const keys = torrentLoadKeys(item.source, item.infoHash);
    const torrent = torrents.find((t) =>
      torrentLoadKeys(sourceOf(t), t.infoHash).some((key) => keys.includes(key)),
    );
    return torrent === undefined ? null : (playTarget(torrent, null)?.index ?? null);
  };

  function sourceOf(torrent: TorrentSummary): string {
    return sources[torrent.id] ?? torrent.magnet;
  }

  /** Pending work only blocks controls for the torrent it belongs to. */
  function isTorrentBusy(source: string, infoHash: string | null = null): boolean {
    const keys = torrentLoadKeys(source, infoHash);
    if (keys.some((key) => loadingTorrents.has(key))) {
      return true;
    }
    const playback = pendingPlayback;
    if (playback?.request === selection) {
      if (torrentLoadKeys(playback.source, playback.infoHash).some((key) => keys.includes(key))) {
        return true;
      }
    }
    return (
      starting?.request === selection &&
      torrents.some(
        (t) =>
          t.id === starting?.torrentId &&
          torrentLoadKeys(sourceOf(t), t.infoHash).some((key) => keys.includes(key)),
      )
    );
  }

  function isGroupBusy(key: string): boolean {
    const item = queue.items.find((i) => groupKey(i) === key);
    const torrent = torrentOf(key);
    return item
      ? isTorrentBusy(item.source, item.infoHash ?? torrent?.infoHash ?? null)
      : torrent !== null && isTorrentBusy(sourceOf(torrent), torrent.infoHash);
  }

  /** Reuses opened torrents and shares an in-flight metadata fetch across all views. */
  function fetchTorrent(source: string, infoHash: string | null = null): Promise<TorrentSummary> {
    const keys = torrentLoadKeys(source, infoHash);
    const known = torrents.find((t) =>
      torrentLoadKeys(sourceOf(t), t.infoHash).some((key) => keys.includes(key)),
    );
    if (known) {
      return Promise.resolve(known);
    }
    return torrentLoads.run(keys, async () => {
      const torrent = await addTorrentLink(source);
      remember(torrent, source);
      return torrent;
    });
  }

  async function add(load: () => Promise<TorrentSummary>): Promise<void> {
    const request = ++selection;
    addError = null;
    try {
      const torrent = await load();
      if (request !== selection) {
        return;
      }
      playError = null;
      lastTorrentId = torrent.id;
      navigate({ name: 'torrent', torrentId: torrent.id });
    } catch (reason: unknown) {
      if (request === selection) {
        addError = errorMessage(reason);
      } else {
        showToast(`Could not open torrent: ${errorMessage(reason)}`, { error: true });
      }
    }
  }

  function addLink(link: string, infoHash: string | null = null): void {
    void add(() => fetchTorrent(link, infoHash));
  }

  function addFile(bytes: Uint8Array): void {
    void add(async () => {
      const digest = await crypto.subtle.digest('SHA-256', bytes.slice().buffer);
      const key = Array.from(new Uint8Array(digest), (b) => b.toString(16).padStart(2, '0')).join(
        '',
      );
      return torrentLoads.run([`file:${key}`], async () => {
        const torrent = await addTorrentFile(bytes);
        remember(torrent, null);
        return torrent;
      });
    });
  }

  /** Closes the open stream. `forget` also drops the player from the history. */
  async function closeStream(forget: boolean): Promise<void> {
    const streamId = nowPlaying?.stream.streamId;
    const checkpoint = streamId === undefined ? Promise.resolve() : player?.checkpoint();
    // Attach the handler before unmounting the player; the final report precedes stop_stream.
    const saved = Promise.resolve(checkpoint).catch(() => undefined);
    if (nowPlaying) {
      nowPlaying = null;
    }
    nextPreloadKey = null;
    if (forget) {
      history = prune(history, (v) => v.name !== 'player', { name: 'search', search: shownSearch });
      void arrived();
    }
    if (streamId !== undefined) {
      const previous = streamStop;
      streamStop = (async () => {
        await previous;
        await saved;
        await stopStream(streamId).catch(() => undefined);
      })();
    }
    await streamStop;
  }

  function stopPlayback(): void {
    selection += 1;
    pendingPlayback = null;
    void closeStream(false);
  }

  /**
   * The queue entry of a file that starts playing. Played from search, a torrent's files or the
   * file chooser (`first`), it is added at the front. Existing entries keep their place.
   * A new file selected inside the queue (`group`) joins its torrent in lecture order.
   */
  function enterQueue(
    torrent: TorrentSummary,
    file: FileSummary,
    where: 'first' | 'group' | 'existing' | undefined,
  ): string | null {
    const [item] = itemsFor(torrent, [file]);
    if (item === undefined) {
      return null;
    }
    const placed = queueForPlayback(queue, item, where, mainFile, lectureOrder(torrent));
    queue = placed.queue;
    return placed.id;
  }

  async function play(
    torrent: TorrentSummary,
    file: FileSummary,
    options: {
      fromQueue?: boolean;
      queue?: 'first' | 'group';
      autoSeeks?: number[];
      request?: number;
    } = {},
  ): Promise<boolean> {
    const request = options.request ?? ++selection;
    await queueLoaded;
    if (request !== selection) {
      return false;
    }
    lastTorrentId = torrent.id;
    if (nowPlaying?.torrent.id === torrent.id && nowPlaying.file.index === file.index) {
      const queueId = enterQueue(torrent, file, options.fromQueue ? 'existing' : options.queue);
      if (queueId !== null) {
        queue = setCurrent(queue, queueId);
        nowPlaying = { ...nowPlaying, queueId };
      }
      showPlayer();
      return true;
    }
    const startedAt = performance.now();
    starting = { request, torrentId: torrent.id, fileIndex: file.index };
    playError = null;
    const previousStart = streamStart;
    let finishStart: () => void = () => undefined;
    streamStart = new Promise<void>((resolve) => {
      finishStart = resolve;
    });
    try {
      await previousStart;
      if (request !== selection) {
        return false;
      }
      const queueId = enterQueue(torrent, file, options.fromQueue ? 'existing' : options.queue);
      if (queueId === null && options.fromQueue) {
        return false;
      }
      if (queueId === null || !queue.items.some((item) => item.id === queueId)) {
        throw new Error('The queue is full. Remove an item before playing another file.');
      }
      // Close the old stream before starting another on the same torrent: its stop clears
      // priorities. A superseded start is also stopped before the next one begins.
      await closeStream(false);
      if (request !== selection) {
        return false;
      }
      const stream = await startStream(torrent.id, file.index);
      if (request !== selection || !queue.items.some((item) => item.id === queueId)) {
        await stopStream(stream.streamId).catch(() => undefined);
        return false;
      }
      // No question: with "Resume where I left off" on it continues (the chip offers to start
      // over); with it off, the backend sends no position.
      const { startAt: resumeAt, chip: resumeChip } = resumeStart(stream.resumePositionSecs);
      playback.position = resumeAt;
      playback.duration = Number.NaN;
      // The file becomes the queue's current item, wherever it was played from.
      playbackPaused = true;
      nowPlaying = {
        torrent,
        file,
        stream,
        startedAt,
        autoSeeks: options.autoSeeks ?? [],
        resumeAt,
        resumeChip,
        queueId,
      };
      queue = setCurrent(queue, queueId);
      navigate({ name: 'player' });
      // Already on the player (the next video of the queue): it has been shown now.
      shownStream = stream.streamId;
      return true;
    } catch (reason: unknown) {
      if (request !== selection) {
        return false;
      }
      playError = errorMessage(reason);
      if (nowPlaying) {
        showToast(playError, { error: true });
        return false;
      }
      // Nothing is playing now, so the player is no longer a place to go back to.
      const onPlayer = view.name === 'player';
      history = prune(history, (v) => v.name !== 'player', {
        name: 'torrent',
        torrentId: torrent.id,
      });
      if (onPlayer) {
        void arrived();
      }
      return false;
    } finally {
      finishStart();
      if (starting.request === request) {
        starting = null;
      }
    }
  }

  /**
   * A search result's Play button: adds the torrent (a preloaded one is reused) and plays its
   * preloaded file, or its best video, without showing the file list. The file list appears
   * only if there is no video or playback cannot start.
   */
  async function playNow(
    magnet: string,
    fileIndex: number | null,
    infoHash: string | null = null,
  ): Promise<void> {
    const request = ++selection;
    pendingPlayback = { request, source: magnet, infoHash };
    addError = null;
    playError = null;
    try {
      const torrent = await fetchTorrent(magnet, infoHash);
      if (request !== selection) {
        return;
      }
      lastTorrentId = torrent.id;
      const file = playTarget(torrent, fileIndex);
      const played = file ? await play(torrent, file, { request, queue: 'first' }) : false;
      if (!played && request === selection) {
        navigate({ name: 'torrent', torrentId: torrent.id });
      }
    } catch (reason: unknown) {
      if (request === selection) {
        addError = errorMessage(reason);
      }
    } finally {
      finishPlayback(request);
    }
  }

  /** Reopens an exact file from playback history. */
  async function playLibraryItem(item: LibraryItem): Promise<void> {
    const request = ++selection;
    pendingPlayback = { request, source: item.source, infoHash: item.infoHash };
    playError = null;
    try {
      const torrent = await fetchTorrent(item.source, item.infoHash);
      if (request !== selection) {
        return;
      }
      lastTorrentId = torrent.id;
      const file = torrent.files.find((candidate) => candidate.index === item.fileIndex);
      if (!file || !isPlayable(file)) {
        throw new Error('This file is no longer available in the torrent.');
      }
      await play(torrent, file, { request, queue: 'first' });
    } catch (reason: unknown) {
      if (request === selection) {
        showToast(`Could not open library item: ${errorMessage(reason)}`, { error: true });
      }
    } finally {
      finishPlayback(request);
    }
  }

  /** An older request must not clear the newer selection's playback indicator. */
  function finishPlayback(request: number): void {
    if (pendingPlayback?.request === request) {
      pendingPlayback = null;
    }
  }

  // --- Queue --------------------------------------------------------------------------

  function addToQueue(
    items: QueueItem[],
    what: string,
    order?: (a: QueueItem, b: QueueItem) => number,
  ): void {
    const { queue: after, added } = enqueue(queue, items, order, mainFile);
    if (added === 0) {
      showToast(items.length === 1 ? 'Already in the queue' : 'Already queued');
      return;
    }
    queue = after;
    showToast(added === 1 ? `Queued ${what}` : `Queued ${String(added)} videos`, {
      undo: () => {
        removeQueueItems(items.map((item) => item.id));
      },
    });
  }

  function queueResult(result: SearchResultDto, fileIndex: number | null): void {
    addToQueue(
      [
        {
          id: newItemId(),
          source: result.link,
          infoHash: result.infoHash,
          fileIndex,
          title: clipTitle(result.title),
          torrentName: null,
          sizeBytes: result.sizeBytes,
          paused: false,
          played: false,
          addedAt: Date.now(),
        },
      ],
      clipTitle(result.title),
    );
  }

  function itemsFor(torrent: TorrentSummary, files: readonly FileSummary[]): QueueItem[] {
    const source = sourceOf(torrent);
    return files.map((file) => ({
      id: newItemId(),
      source,
      infoHash: torrent.infoHash,
      fileIndex: file.index,
      title: clipTitle(fileName(file.path)),
      torrentName: clipTitle(torrent.name),
      sizeBytes: file.length,
      paused: false,
      played: false,
      addedAt: Date.now(),
    }));
  }

  /** Files of one torrent join its group in lecture order (2 before 10). */
  function lectureOrder(torrent: TorrentSummary): (a: QueueItem, b: QueueItem) => number {
    const path = (item: QueueItem): string =>
      torrent.files.find((f) => f.index === item.fileIndex)?.path ?? item.title;
    return (a, b) => lectures.compare(path(a), path(b));
  }

  function queueFiles(torrent: TorrentSummary, files: readonly FileSummary[]): void {
    addToQueue(
      itemsFor(torrent, files),
      clipTitle(fileName(files[0]?.path ?? '')),
      lectureOrder(torrent),
    );
  }

  function isQueued(torrent: TorrentSummary, file: FileSummary): boolean {
    return (
      findEntry(
        queue,
        {
          source: sourceOf(torrent),
          infoHash: torrent.infoHash,
          fileIndex: file.index,
        },
        mainFile,
      ) !== null
    );
  }

  function removeFromQueue(id: string): void {
    const item = queue.items.find((i) => i.id === id);
    if (item) {
      removeQueueItems([id], `Removed ${item.title}`);
    }
  }

  /** Every removal path stops an affected stream; Undo only restores queue membership. */
  function removeQueueItems(ids: readonly string[], message?: string): void {
    if (pendingPlayback?.queueId && ids.includes(pendingPlayback.queueId)) {
      selection += 1;
      pendingPlayback = null;
    }
    if (nowPlaying?.queueId && ids.includes(nowPlaying.queueId)) {
      stopPlayback();
    }
    const removed: Removed[] = [];
    for (const id of ids) {
      const result = removeItem(queue, id);
      queue = result.queue;
      if (result.removed) {
        removed.push(result.removed);
      }
    }
    if (removed.length === 0 || message === undefined) {
      return;
    }
    showToast(message, {
      undo: () => {
        for (const item of [...removed].reverse()) {
          queue = restoreRemoved(queue, item, mainFile);
        }
      },
    });
  }

  /** Plays a queued item: adds its torrent again if needed (a preloaded one is reused). */
  async function playQueueItem(item: QueueItem): Promise<void> {
    const request = ++selection;
    pendingPlayback = { request, source: item.source, infoHash: item.infoHash, queueId: item.id };
    try {
      const torrent = await fetchTorrent(item.source, item.infoHash);
      if (request !== selection) {
        return;
      }
      lastTorrentId = torrent.id;
      const preloaded = nextPreload?.infoHash === torrent.infoHash ? nextPreload.fileIndex : null;
      const file =
        item.fileIndex === null
          ? playTarget(torrent, preloaded)
          : (torrent.files.find((f) => f.index === item.fileIndex && isPlayable(f)) ?? null);
      if (file === null) {
        playError = 'That video is not in the torrent any more.';
        navigate({ name: 'torrent', torrentId: torrent.id });
        return;
      }
      const played = await play(torrent, file, { fromQueue: true, request });
      if (!played && request === selection) {
        navigate({ name: 'torrent', torrentId: torrent.id });
      }
    } catch (reason: unknown) {
      if (request === selection) {
        showToast(`Could not play ${item.title}: ${errorMessage(reason)}`, { error: true });
      }
    } finally {
      finishPlayback(request);
    }
  }

  function playNext(): void {
    if (next) {
      void playQueueItem(next);
    }
  }

  // Saves the queue whenever it changes.
  $effect(() => {
    const q = queue;
    if (persisted !== null && q !== persisted) {
      persisted = q;
      saver.schedule(q);
    }
  });

  // --- End of a video -------------------------------------------------------------------

  /** The video stops on its last frame; the user picks what plays next. */
  function onEnded(): void {
    queue = playbackEnded(queue, nowPlaying?.queueId ?? null);
  }

  function onTime(currentTime: number, duration: number): void {
    // A player being unmounted resets its video to 0; that is not where the user left off.
    if (view.name !== 'player') {
      return;
    }
    playback.position = currentTime;
    playback.duration = duration;
  }

  /**
   * Preloads the next queued video once the current one cannot starve: it is fully
   * downloaded, or nearly over with the rest arriving in time (see `shouldPreloadNext`).
   */
  $effect(() => {
    const playing = nowPlaying;
    const item = toPreload;
    if (!playing || !item) {
      return;
    }
    const key = `${String(playing.stream.streamId)}:${item.id}`;
    if (nextPreloadKey === key) {
      return;
    }
    const timer = setInterval(() => {
      void checkNextPreload(playing.stream.streamId, item, key);
    }, NEXT_CHECK_MS);
    return () => {
      clearInterval(timer);
    };
  });

  async function checkNextPreload(streamId: number, item: QueueItem, key: string): Promise<void> {
    if (nextPreloadKey === key) {
      return;
    }
    let stats: StreamStats;
    try {
      stats = await streamStats(streamId);
    } catch {
      return;
    }
    if (nextPreloadKey === key || nowPlaying?.stream.streamId !== streamId) {
      return;
    }
    const ready = shouldPreloadNext({
      fileDownloaded: stats.fileDownloaded,
      fileLength: stats.fileLength,
      downloadBps: stats.downloadBps,
      position: playback.position,
      duration: playback.duration,
    });
    if (!ready) {
      return;
    }
    nextPreloadKey = key;
    try {
      // A torrent already open (usually the course playing) is prefetched inside it.
      const added = torrents.find(
        (t) =>
          sourceOf(t) === item.source || (item.infoHash !== null && t.infoHash === item.infoHash),
      );
      await preloadNext(streamId, {
        link: item.source,
        fileIndex: item.fileIndex,
        infoHash: item.infoHash,
        torrentId: added?.id ?? null,
      });
      if (!added) {
        followNextPreload(item);
      }
    } catch {
      // Nothing preloaded; the item still plays, just not instantly.
    }
  }

  /** Follows the next item's preload for the queue's "Ready to play instantly". */
  function followNextPreload(item: QueueItem, checks = 0): void {
    clearTimeout(nextPreloadTimer);
    preloadStatus()
      .then((status) => {
        const ours = status !== null && status.infoHash === item.infoHash;
        nextPreload = ours ? status : null;
        if (ours ? preloadActive(status) : checks < 5) {
          nextPreloadTimer = setTimeout(() => {
            followNextPreload(item, checks + 1);
          }, 1500);
        }
      })
      .catch(() => undefined);
  }

  // --- Toasts, keys, and the window --------------------------------------------------

  function showToast(message: string, options: { error?: boolean; undo?: () => void } = {}): void {
    toaster.show(message, options);
  }

  /**
   * Keeps the toasts visible in fullscreen: they move into the fullscreen element while there
   * is one, and back afterwards.
   */
  function followFullscreen(node: HTMLElement): () => void {
    const home = node.parentElement;
    const anchor = document.createComment('toasts');
    node.before(anchor);
    const move = (): void => {
      const host = document.fullscreenElement;
      if (host !== null) {
        host.append(node);
      } else if (node.parentElement !== home) {
        anchor.after(node);
      }
    };
    move();
    document.addEventListener('fullscreenchange', move);
    return () => {
      document.removeEventListener('fullscreenchange', move);
      anchor.remove();
    };
  }

  function readPlaylistOpen(): boolean {
    try {
      return localStorage.getItem(PLAYLIST_KEY) !== 'false';
    } catch {
      return true;
    }
  }

  function togglePlaylist(): void {
    playlistOpen = !playlistOpen;
    try {
      localStorage.setItem(PLAYLIST_KEY, String(playlistOpen));
    } catch {
      // Not remembered; harmless.
    }
  }

  function isTextField(target: EventTarget | null): boolean {
    return (
      target instanceof HTMLElement &&
      (target.isContentEditable || ['INPUT', 'TEXTAREA', 'SELECT'].includes(target.tagName))
    );
  }

  function onKeydown(event: KeyboardEvent): void {
    // The "Choose files" dialog handles its own keys (Esc closes it).
    if (event.defaultPrevented || chooser) {
      return;
    }
    const step = keyNavigation(event);
    if (step) {
      event.preventDefault();
      if (step === 'back') {
        goBack();
      } else {
        goForward();
      }
      return;
    }
    const mod = (event.ctrlKey || event.metaKey) && !event.altKey && !event.shiftKey;
    if (mod && event.key === ',') {
      // Ctrl+, (Cmd+, on macOS) opens Settings, as in most desktop apps.
      event.preventDefault();
      openSettings();
      return;
    }
    if (mod && (event.key === 'f' || event.key === 'F')) {
      event.preventDefault();
      showSearch();
      return;
    }
    if (mod && (event.key === 'o' || event.key === 'O')) {
      event.preventDefault();
      navigate({ name: 'open' });
      return;
    }
    if (event.ctrlKey || event.metaKey || event.altKey || isTextField(event.target)) {
      return;
    }
    if (view.name === 'player') {
      if (event.key === 'q' || event.key === 'Q') {
        event.preventDefault();
        togglePlaylist();
      } else if (event.key === 'N' && event.shiftKey) {
        event.preventDefault();
        playNext();
      }
      return;
    }
    if (event.key === 'Escape' && ['torrent', 'open'].includes(view.name)) {
      event.preventDefault();
      goBack();
    }
  }

  /** The mouse's back and forward buttons. */
  function onMouseUp(event: MouseEvent): void {
    const step = mouseNavigation(event.button);
    if (step) {
      event.preventDefault();
      if (step === 'back') {
        goBack();
      } else {
        goForward();
      }
    }
  }

  function onMouseDown(event: MouseEvent): void {
    // Keeps the webview from navigating on its own.
    if (mouseNavigation(event.button)) {
      event.preventDefault();
    }
  }

  function onDragOver(event: DragEvent): void {
    if (view.name !== 'player' && event.dataTransfer?.types.includes('Files')) {
      event.preventDefault();
      event.dataTransfer.dropEffect = 'copy';
    }
  }

  function onDrop(event: DragEvent): void {
    const file = event.dataTransfer?.files[0];
    if (view.name !== 'player' && file) {
      event.preventDefault();
      void readTorrentFile(file)
        .then(addFile)
        .catch((reason: unknown) => {
          addError = errorMessage(reason);
        });
    }
  }

  onMount(() => {
    const stopLibrary = followLibrary((items) => {
      library = items;
    });
    const stopVpnHint = followVpnHint((network) => {
      vpnHintApplies = showVpnHint(network.mode, network.vpnHintDismissed, network.hold !== null);
    });
    const stopSeeding = followSeedingStarted(() => {
      // Used up only once it is actually shown.
      if (!seedingHintShown && vpnHintApplies) {
        seedingHintShown = true;
        seedingHintOpen = true;
      }
    });
    queueLoaded = loadQueue()
      .then((saved) => {
        const loaded = sanitizeQueue(saved);
        persisted = loaded;
        // Anything queued while loading goes after the saved items (and is saved).
        queue = enqueue(loaded, queue.items, undefined, mainFile).queue;
      })
      .catch(() => {
        persisted = queue;
      });

    // Development builds can be told to add and play a torrent automatically, to measure
    // real decoder timings. Release builds always return null.
    devAutoplay()
      .then((auto) => {
        if (!auto) {
          return;
        }
        remember(auto.torrent, null);
        lastTorrentId = auto.torrent.id;
        navigate({ name: 'torrent', torrentId: auto.torrent.id });
        const file = auto.torrent.files.find((f) => f.index === auto.fileIndex);
        if (file) {
          void play(auto.torrent, file, { autoSeeks: auto.seeks });
        }
      })
      .catch(() => undefined);

    return () => {
      toaster.clear();
      clearTimeout(nextPreloadTimer);
      stopLibrary();
      stopVpnHint();
      stopSeeding();
      void saver.flush();
    };
  });
</script>

<svelte:window
  ondragover={onDragOver}
  ondrop={onDrop}
  onkeydown={onKeydown}
  onmouseup={onMouseUp}
  onmousedown={onMouseDown}
/>

<div class="app">
  <NavRail
    current={view.name}
    canBack={canGoBack(history)}
    canForward={canGoForward(history)}
    hasTorrent={lastTorrentId !== null || explored.length > 0}
    playing={nowPlaying !== null}
    upcoming={upcomingCount(queue)}
    onBack={goBack}
    onForward={goForward}
    onNavigate={onRail}
  />

  <main class="content" aria-label={describe(view)}>
    <div class="view" hidden={view.name !== 'search'} bind:this={searchEl}>
      <AddTorrent
        focusRequest={focusSearch}
        searchId={shownSearch}
        {isTorrentBusy}
        error={addError}
        onLink={addLink}
        onFile={addFile}
        onPlay={(magnet: string, fileIndex: number | null, infoHash: string | null) =>
          void playNow(magnet, fileIndex, infoHash)}
        onQueue={queueResult}
        queuedState={(result: SearchResultDto, fileIndex: number | null) =>
          queueEntryState(
            queue,
            { source: result.link, infoHash: result.infoHash, fileIndex },
            playingId,
            mainFile,
          )}
        onSearch={(id: number | null) => {
          addError = null;
          navigate({ name: 'search', search: id });
        }}
        onForget={(ids: number[]) => {
          history = prune(
            history,
            (v) => v.name !== 'search' || v.search === null || !ids.includes(v.search),
            HOME,
          );
        }}
      />
    </div>

    {#if view.name !== 'search'}
      <div class="view" class:flush={view.name === 'player'} bind:this={pageEl}>
        {#if view.name === 'open'}
          <OpenTorrent
            {isTorrentBusy}
            error={addError}
            {torrents}
            onLink={addLink}
            onFile={addFile}
            onShow={(torrent: TorrentSummary) => {
              lastTorrentId = torrent.id;
              navigate({ name: 'torrent', torrentId: torrent.id });
            }}
          />
        {:else if view.name === 'library'}
          <Library
            items={library}
            isBusy={(item: LibraryItem) => isTorrentBusy(item.source, item.infoHash)}
            onPlay={(item: LibraryItem) => void playLibraryItem(item)}
          />
        {:else if view.name === 'torrent' && shownTorrent}
          {@const torrent = shownTorrent}
          <FileExplorer
            torrents={explored}
            {torrent}
            busy={isTorrentBusy(sourceOf(torrent), torrent.infoHash)}
            isLoading={(entry: ExplorerTorrent) => isGroupBusy(entry.key)}
            starting={starting?.request === selection && starting.torrentId === torrent.id
              ? starting.fileIndex
              : null}
            error={playError}
            playingIndex={nowPlaying?.torrent.id === torrent.id ? nowPlaying.file.index : null}
            isQueued={(file: FileSummary) => isQueued(torrent, file)}
            onSelectTorrent={(entry: ExplorerTorrent) => void explore(entry, true)}
            onPlay={(file: FileSummary) => void play(torrent, file, { queue: 'first' })}
            onQueue={(file: FileSummary) => {
              queueFiles(torrent, [file]);
            }}
            onRemove={(file: FileSummary) => {
              chooseFile(torrent, file, false);
            }}
            onQueueAll={(files: FileSummary[]) => {
              queueFiles(torrent, files);
            }}
          />
        {:else if view.name === 'settings'}
          <SettingsPage section={view.section} onBack={goBack} />
        {:else if view.name === 'player'}
          <div class="watch">
            <PlaylistPanel
              {queue}
              collapsed={!playlistOpen && nowPlaying !== null}
              {playingId}
              {playbackPaused}
              onTogglePlayback={() => player?.togglePlayback()}
              onStopPlayback={stopPlayback}
              playingTitle={nowPlaying && nowPlaying.queueId === null
                ? nowPlaying.stream.fileName
                : null}
              {nextStatus}
              isBusy={isGroupBusy}
              onToggle={togglePlaylist}
              onPlay={(item: QueueItem) => void playQueueItem(item)}
              onRemove={removeFromQueue}
              onMoveGroup={(key: string, index: number) => {
                queue = moveGroup(queue, key, index);
              }}
              onRemoveGroup={removeGroup}
              onChooseFiles={(key: string) => void chooseFiles(key)}
              {readiness}
              {downloads}
              {watchProgress}
              onTogglePause={(ids: readonly string[], paused: boolean) => {
                queue = setPaused(queue, ids, paused);
              }}
              onSort={sortBy}
              onNetworkSettings={() => {
                openSettings('network');
              }}
              filesOf={(key: string) => torrentOf(key)?.files ?? null}
              {playingFile}
              onPlayFile={(key: string, file: FileSummary) => {
                const torrent = torrentOf(key);
                if (torrent) {
                  void play(torrent, file, { queue: 'group' });
                }
              }}
              onToggleFile={(key: string, file: FileSummary, queued: boolean) => {
                const torrent = torrentOf(key);
                if (torrent) {
                  chooseFile(torrent, file, queued);
                }
              }}
              onLoadFiles={(key: string) => void loadTorrent(key)}
              onClear={() => {
                removeQueueItems(
                  queue.items.map((i) => i.id),
                  'Cleared the queue',
                );
              }}
              onClearPlayed={() => {
                const ids = queue.items
                  .filter((item) => item.played && item.id !== playingId)
                  .map((item) => item.id);
                removeQueueItems(
                  ids,
                  `Cleared ${String(ids.length)} played ${ids.length === 1 ? 'video' : 'videos'}`,
                );
              }}
            />
            <div class="stage" bind:this={stageEl}>
              {#if nowPlaying}
                {@const playing = nowPlaying}
                {#key playing.stream.streamId}
                  <Player
                    bind:this={player}
                    stream={playing.stream}
                    startedAt={playing.startedAt}
                    autoSeeks={playing.autoSeeks}
                    startAt={playing.resumeAt}
                    resumeChip={playing.resumeChip}
                    fullscreenTarget={stageEl}
                    audio={playing.file.kind === 'audio'
                      ? { title: fileName(playing.file.path), album: playing.torrent.name }
                      : null}
                    onBack={goBack}
                    {onEnded}
                    {onTime}
                    onPauseChange={(paused: boolean) => {
                      if (nowPlaying?.stream.streamId === playing.stream.streamId) {
                        playbackPaused = paused;
                      }
                    }}
                    nextTitle={next?.title ?? null}
                    onPlayNext={playNext}
                  />
                {/key}
              {:else}
                <!-- Between two videos of the queue, or after playback failed. -->
                <p class="between" aria-live="polite">
                  {pendingPlayback?.request === selection || starting?.request === selection
                    ? 'Starting the video…'
                    : 'Pick something from the queue or search.'}
                </p>
              {/if}
            </div>
          </div>
        {:else}
          <p class="gone">
            This is no longer available.
            <button type="button" class="link" onclick={showSearch}>Go to search</button>
          </p>
        {/if}
      </div>
    {/if}

    {#if view.name !== 'player'}
      <!-- On the player screen the indicator lives in the auto-hiding controls instead. -->
      <footer>
        <TransferRates />
        <NetworkIndicator
          onOpen={() => {
            openSettings('network');
          }}
        />
      </footer>
    {/if}

    {#if chooser}
      {@const torrent = chooser}
      <FileChooser
        {torrent}
        isQueued={(file: FileSummary) => isQueued(torrent, file)}
        playingIndex={nowPlaying?.torrent.id === torrent.id ? nowPlaying.file.index : null}
        onToggle={(file: FileSummary, queued: boolean) => {
          chooseFile(torrent, file, queued);
        }}
        onToggleAll={(queued: boolean) => {
          for (const file of torrent.files.filter(isPlayable)) {
            chooseFile(torrent, file, queued);
          }
        }}
        onPlay={(file: FileSummary) => {
          // `torrent` is derived from `chooser`: read it before clearing `chooser`, after which
          // it reads null.
          const chosen = torrent;
          chooser = null;
          void play(chosen, file, { queue: 'first' });
        }}
        onClose={() => {
          // Closing the dialog on unmount fires `close` late: after Play it must not cancel the
          // playback Play just started.
          if (chooser === null) {
            return;
          }
          selection += 1;
          chooser = null;
        }}
      />
    {/if}

    {#if seedingHint}
      <div class="seeding-hint" role="status">
        <strong>Sharing started</strong>
        <VpnHint
          onDismiss={() => {
            seedingHintError = null;
            dismissVpnHint()
              .then(() => {
                seedingHintOpen = false;
              })
              .catch((reason: unknown) => {
                seedingHintError = `The setting could not be saved: ${errorMessage(reason)}`;
              });
          }}
          onClose={() => {
            seedingHintOpen = false;
          }}
          error={seedingHintError}
        />
      </div>
    {/if}

    <!-- Always rendered, so screen readers know the live regions before a toast arrives. -->
    <div class="toasts" class:raised={view.name === 'player'} {@attach followFullscreen}>
      <div class="stack" role="alert">
        {#each toasts.filter((t) => t.error) as t (t.id)}
          <!-- eslint-disable-next-line @typescript-eslint/no-confusing-void-expression -- a local snippet is typed as returning void -->
          {@render toastView(t)}
        {/each}
      </div>
      <div class="stack" role="status">
        {#each toasts.filter((t) => !t.error) as t (t.id)}
          <!-- eslint-disable-next-line @typescript-eslint/no-confusing-void-expression -- a local snippet is typed as returning void -->
          {@render toastView(t)}
        {/each}
      </div>
    </div>
  </main>
</div>
<p class="sr-only" aria-live="polite">{announcement}</p>

{#snippet toastView(t: Toast)}
  <div
    class="toast"
    class:error={t.error}
    role="group"
    aria-label="Notification"
    onmouseenter={() => {
      toaster.hold(t.id);
    }}
    onmouseleave={() => {
      toaster.release(t.id);
    }}
    onfocusin={() => {
      toaster.hold(t.id);
    }}
    onfocusout={() => {
      toaster.release(t.id);
    }}
  >
    <span title={t.message}>{t.message}</span>
    {#if t.undo}
      <button
        type="button"
        class="link"
        onclick={() => {
          toaster.undo(t.id);
        }}>Undo</button
      >
    {/if}
    <button
      type="button"
      class="close"
      aria-label="Close"
      title="Close"
      onclick={() => {
        toaster.dismiss(t.id);
      }}
    >
      <svg viewBox="0 0 24 24" aria-hidden="true"><path d="M7 7l10 10M17 7 7 17" /></svg>
    </button>
  </div>
{/snippet}

<style>
  .app {
    display: flex;
    height: 100%;
  }

  .content {
    position: relative;
    display: flex;
    flex: 1;
    flex-direction: column;
    min-width: 0;
  }

  .view {
    flex: 1;
    min-height: 0;
    overflow-y: auto;
  }

  .view.flush {
    overflow: hidden;
  }

  .watch {
    position: relative;
    display: flex;
    height: 100%;
  }

  .stage {
    flex: 1;
    min-width: 0;
    height: 100%;
  }

  .stage:fullscreen {
    background: #000;
  }

  footer {
    display: flex;
    gap: 0.75rem;
    align-items: center;
    justify-content: flex-end;
    padding: 0.3rem 1rem;
    border-top: 1px solid rgb(255 255 255 / 5%);
    color: var(--text-muted);
    font-size: 0.72rem;
  }

  .toasts {
    position: absolute;
    z-index: 10;
    bottom: 2.75rem;
    left: 50%;
    display: flex;
    flex-direction: column;
    align-items: center;
    gap: 0.4rem;
    width: min(32rem, calc(100% - 2rem));
    pointer-events: none;
    transform: translateX(-50%);
  }

  /* Over the video, above its controls (also in fullscreen). */
  .toasts.raised {
    bottom: 6rem;
  }

  .stack {
    display: flex;
    flex-direction: column;
    align-items: center;
    gap: 0.4rem;
    width: 100%;
  }

  .toast {
    display: flex;
    align-items: center;
    gap: 0.75rem;
    max-width: 100%;
    padding: 0.4rem 0.4rem 0.4rem 0.9rem;
    pointer-events: auto;
    border: 1px solid rgb(255 255 255 / 8%);
    border-radius: 0.6rem;
    background: rgb(22 22 28 / 96%);
    box-shadow: 0 0.5rem 1.5rem rgb(0 0 0 / 40%);
    color: var(--text);
    font-size: 0.875rem;
  }

  .toast .close {
    display: inline-flex;
    padding: 0.25rem;
    border: none;
    border-radius: 0.35rem;
    background: none;
    color: var(--text-muted);
    cursor: pointer;
  }

  .toast .close:hover {
    color: var(--text);
    background: rgb(255 255 255 / 6%);
  }

  .toast .close:focus-visible {
    outline: 2px solid var(--accent);
    outline-offset: -2px;
  }

  .toast .close svg {
    width: 0.85rem;
    height: 0.85rem;
    fill: none;
    stroke: currentcolor;
    stroke-width: 1.8;
    stroke-linecap: round;
  }

  .seeding-hint {
    position: absolute;
    z-index: 10;
    right: 1rem;
    bottom: 2.75rem;
    width: min(24rem, calc(100% - 2rem));
    padding: 0.75rem 0.75rem 0.25rem;
    border: 1px solid rgb(255 255 255 / 8%);
    border-radius: 0.6rem;
    background: rgb(22 22 28 / 96%);
    box-shadow: 0 0.5rem 1.5rem rgb(0 0 0 / 40%);
    font-size: 0.875rem;
  }

  .seeding-hint strong {
    display: block;
    margin: 0 0.25rem 0.4rem;
    font-weight: 500;
  }

  .toast span {
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  .toast.error {
    border-color: #6b3a30;
    color: #e8907a;
  }

  .link {
    padding: 0;
    border: none;
    background: none;
    color: var(--accent);
    font: inherit;
    font-weight: 500;
    cursor: pointer;
  }

  .link:hover {
    text-decoration: underline;
  }

  .link:focus-visible {
    outline: 2px solid var(--accent);
    outline-offset: 2px;
  }

  .gone {
    padding: 2.5rem;
    color: var(--text-muted);
  }

  .between {
    display: grid;
    place-items: center;
    height: 100%;
    margin: 0;
    background: #000;
    color: var(--text-muted);
  }

  .sr-only {
    position: absolute;
    width: 1px;
    height: 1px;
    margin: 0;
    overflow: hidden;
    clip-path: inset(50%);
    white-space: nowrap;
  }
</style>
