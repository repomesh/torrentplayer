<!-- SPDX-License-Identifier: MIT OR Apache-2.0 -->
<script lang="ts">
  /*
   * The search page. It stays mounted while other views are shown, so a search, its results,
   * and the filters are exactly as they were when the user comes back. Each search is kept in
   * a small cache, so Back and Forward move between recent searches without searching again.
   */
  import { onMount, untrack } from 'svelte';
  import {
    cancelSearch,
    errorMessage,
    fetchThumbnails,
    isIpcError,
    prefetchTorrents,
    preloadStatus,
    previewFrame,
    searchTorrentsProgressive,
    type PreloadStatusDto,
    type PreviewFrameDto,
    type SearchResponseDto,
    type SearchResultDto,
  } from '../ipc';
  import { readTorrentFile } from '../files';
  import {
    DEFAULT_FILTERS,
    applyFilters,
    bestPickHidden,
    categoryChoices,
    categoryIds,
    facets,
    type SearchFilters,
  } from '../filters';
  import { evictSearches, normalizeQuery, rememberQuery } from '../recent';
  import {
    looksLikeLink,
    pendingSourcesLabel,
    framePossible,
    prefetchRequests,
    preloadActive,
    searchDelay,
    thumbnailRequests,
    withRecent,
    withRecentEntry,
  } from '../search';
  import SearchFiltersBar from './SearchFilters.svelte';
  import SearchResults from './SearchResults.svelte';
  import type { QueueEntryState } from '../queue';

  interface Props {
    isTorrentBusy: (source: string, infoHash?: string | null) => boolean;
    error: string | null;
    /** The search to show, from the navigation history; null shows the empty search page. */
    searchId: number | null;
    onLink: (link: string, infoHash?: string | null) => void;
    onFile: (bytes: Uint8Array) => void;
    /** Adds a search result and plays it at once: its preloaded file, if known. */
    onPlay?: (magnet: string, fileIndex: number | null, infoHash: string | null) => void;
    /** Adds a search result to the queue: its preloaded file, if known. */
    onQueue?: (result: SearchResultDto, fileIndex: number | null) => void;
    /** Whether a search result's selected video is already in the queue. */
    queuedState?: (result: SearchResultDto, fileIndex: number | null) => QueueEntryState | null;
    /** A new search started (`id`), or the results were cleared (null): a new history entry. */
    onSearch: (id: number | null) => void;
    /** Searches dropped from the cache, so history entries for them can be removed. */
    onForget: (ids: number[]) => void;
    /** Each change puts the cursor in the search box, with its text selected. */
    focusRequest?: number;
  }

  let {
    isTorrentBusy,
    error,
    searchId,
    onLink,
    onFile,
    onPlay,
    onQueue,
    queuedState,
    onSearch,
    onForget,
    focusRequest = 0,
  }: Props = $props();

  interface Search {
    id: number;
    query: string;
    response: SearchResponseDto | null;
    progress: { completed: number; total: number } | null;
    searching: boolean;
    /** Stopped by the user: later progress is ignored. */
    stopped: boolean;
    error: string | null;
    /** Torznab category ids the search service was asked for (empty: every category). */
    categories: readonly number[];
  }

  let link = $state('');
  const busy = $derived(looksLikeLink(link.trim()) && isTorrentBusy(link.trim()));
  let fileInput: HTMLInputElement | undefined = $state();
  let queryInput: HTMLInputElement | undefined = $state();
  let localError = $state<string | null>(null);
  let searches = $state.raw<Search[]>([]);
  let recent = $state.raw<string[]>([]);
  let filters = $state<SearchFilters>({ ...DEFAULT_FILTERS });
  let preload = $state<PreloadStatusDto | null>(null);
  let preloadTimer: ReturnType<typeof setTimeout> | undefined;
  let nextId = 1;

  // How often the preload of the best pick is checked, and how many checks may pass after the
  // search ends before the backend reports it (it starts once the pick has held for 2 s, or
  // when the search finishes).
  const PRELOAD_POLL_MS = 700;
  const PRELOAD_START_CHECKS = 5;
  // Searches submitted within this long of the last one sent collapse into the last.
  const SEARCH_GAP_MS = 300;
  // The torrent details of the first results shown are fetched in the background, once the
  // list has held still for a moment, so that Play on any of them starts sooner.
  const PREFETCH_COUNT = 12;
  const PREFETCH_WAIT_MS = 1500;
  // Results (by key) remembered as prefetched (the backend keeps 64 torrents).
  const PREFETCH_REMEMBER = 64;
  let lastSentAt: number | null = null;
  let prefetched = $state.raw<ReadonlySet<string>>(new Set());
  // The info hash of each prefetched result, by key (download-only results learn theirs then).
  let prefetchedHashes = $state.raw<ReadonlyMap<string, string>>(new Map());
  let prefetchKey = '';
  // Thumbnails: a picture from inside each torrent (by key), asked for once its metadata is
  // known, and a frame of the preloaded best pick. Results tried (with or without a picture)
  // are not asked for again.
  const THUMBNAIL_COUNT = 12;
  const THUMBNAIL_WAIT_MS = 800;
  const THUMBNAIL_REMEMBER = 200;
  let thumbnails = $state.raw<ReadonlyMap<string, string>>(new Map());
  let thumbnailsTried: ReadonlySet<string> = new Set();
  let thumbnailKey = '';
  let frame = $state<PreviewFrameDto | null>(null);

  const active = $derived(searches.find((s) => s.id === searchId) ?? null);
  const searching = $derived(active?.searching ?? false);
  const all = $derived(active?.response?.results ?? []);
  const menuIds = $derived(categoryIds(filters.category));
  // A search made in the chosen category was narrowed by the service, whose category labels
  // vary (`TV/Anime`, `5040`); the category is filtered here only for other searches.
  const narrowed = $derived(
    active !== null && menuIds.length > 0 && sameIds(active.categories, menuIds),
  );
  const shown = $derived(applyFilters(all, narrowed ? { ...filters, category: null } : filters));
  const shownResponse = $derived(active?.response ? { ...active.response, results: shown } : null);
  const pickHidden = $derived(bestPickHidden(all, shown));
  // Searching by category: the menu in the search box narrows the results to one category.
  const categories = $derived(categoryChoices(facets(all).categories, filters.category));
  const hasResults = $derived(active !== null && (active.response !== null || active.searching));
  // The best pick so far: the backend preloads it once it has settled, often while the search
  // is still running, so its progress is shown from then on.
  const bestHash = $derived(active?.response?.results.find((r) => r.bestPick)?.infoHash ?? null);
  const pending = $derived(
    searching
      ? pendingSourcesLabel(active?.progress?.completed ?? 0, active?.progress?.total ?? 0)
      : null,
  );

  // Choosing another category searches again in it (or shows that search, if it is cached).
  let shownCategory: string | null = DEFAULT_FILTERS.category;
  $effect(() => {
    const category = filters.category;
    if (category === shownCategory) {
      return;
    }
    shownCategory = category;
    untrack(() => {
      if (active !== null && !sameIds(active.categories, categoryIds(category))) {
        searchAgain(active.query);
      }
    });
  });

  // Showing another search (Back, Forward, or a recent search) puts its words in the box.
  let shownId: number | null = null;
  $effect.pre(() => {
    if (searchId !== shownId) {
      shownId = searchId;
      const s = searches.find((x) => x.id === searchId);
      link = s?.query ?? '';
      localError = null;
    }
  });

  // Follows the best pick's preload; starts over when the search ends, since the final pick
  // may only be preloaded then.
  $effect(() => {
    const hash = bestHash;
    const running = searching;
    untrack(() => {
      if (preload?.infoHash !== hash) {
        preload = null;
      }
    });
    if (hash !== null) {
      void followPreload(hash, running);
    }
    return stopPreloadPolling;
  });

  $effect(() => {
    const requests = prefetchRequests(
      shown,
      PREFETCH_COUNT,
      untrack(() => prefetched),
    );
    const key = requests.map((r) => r.key).join(' ');
    if (key === '' || key === prefetchKey) {
      return;
    }
    // The backend replaces its previous request with this one.
    const timer = setTimeout(
      () => {
        prefetchKey = key;
        void prefetchTorrents(requests, (ready) => {
          prefetched = withRecent(prefetched, ready.key, PREFETCH_REMEMBER);
          prefetchedHashes = withRecentEntry(
            prefetchedHashes,
            ready.key,
            ready.infoHash,
            PREFETCH_REMEMBER,
          );
        }).catch(() => undefined);
      },
      searching ? PREFETCH_WAIT_MS : 0,
    );
    return () => {
      clearTimeout(timer);
    };
  });

  $effect(() => {
    const best = shown.find((r) => r.bestPick);
    const bestReady =
      best !== undefined &&
      preload !== null &&
      preload.infoHash === best.infoHash &&
      preload.phase !== 'resolving';
    const requests = thumbnailRequests(
      shown,
      THUMBNAIL_COUNT,
      prefetchedHashes,
      untrack(() => thumbnailsTried),
      bestReady,
    );
    const key = requests.map((r) => r.key).join(' ');
    if (key === '' || key === thumbnailKey) {
      return;
    }
    // The backend replaces its previous request; results it had not finished come back here.
    const timer = setTimeout(() => {
      thumbnailKey = key;
      void fetchThumbnails(requests, (thumbnail) => {
        thumbnailsTried = withRecent(thumbnailsTried, thumbnail.key, THUMBNAIL_REMEMBER);
        if (thumbnail.dataUrl !== null) {
          thumbnails = withRecentEntry(
            thumbnails,
            thumbnail.key,
            thumbnail.dataUrl,
            THUMBNAIL_REMEMBER,
          );
        }
      }).catch(() => undefined);
    }, THUMBNAIL_WAIT_MS);
    return () => {
      clearTimeout(timer);
    };
  });

  // A frame of the best pick, once its preload (the start of the video) is ready.
  $effect(() => {
    const best = shown.find((r) => r.bestPick);
    const ready =
      best !== undefined &&
      best.infoHash !== null &&
      framePossible(best.quality) &&
      preload?.infoHash === best.infoHash &&
      preload.phase === 'ready';
    if (!ready || untrack(() => frame?.infoHash) === best.infoHash) {
      return;
    }
    void previewFrame()
      .then((preview) => {
        if (preview !== null && preview.infoHash === best.infoHash) {
          frame = preview;
        }
      })
      .catch(() => undefined);
  });

  // Ready to type as soon as the app opens.
  onMount(() => {
    queryInput?.focus();
    return stopPreloadPolling;
  });

  $effect(() => {
    if (focusRequest > 0) {
      queryInput?.focus();
      queryInput?.select();
    }
  });

  function stopPreloadPolling(): void {
    clearTimeout(preloadTimer);
    preloadTimer = undefined;
  }

  /**
   * Follows the backend's preload of the best pick until it finishes; while the search runs
   * (`running`), keeps checking until the preload starts.
   */
  async function followPreload(infoHash: string, running: boolean, checks = 0): Promise<void> {
    let status: PreloadStatusDto | null;
    try {
      status = await preloadStatus();
    } catch {
      return;
    }
    if (bestHash !== infoHash) {
      return;
    }
    const ours = status !== null && status.infoHash === infoHash;
    preload = ours ? status : null;
    if (ours ? preloadActive(status) : running || checks < PRELOAD_START_CHECKS) {
      preloadTimer = setTimeout(() => {
        void followPreload(infoHash, running, checks + 1);
      }, PRELOAD_POLL_MS);
    }
  }

  function preloadedFile(result: SearchResultDto): number | null {
    return result.infoHash !== null && preload?.infoHash === result.infoHash
      ? preload.fileIndex
      : null;
  }

  function submit(event: SubmitEvent): void {
    event.preventDefault();
    const trimmed = link.trim();
    if (trimmed.length === 0 || busy) {
      return;
    }
    localError = null;
    if (looksLikeLink(trimmed)) {
      onLink(trimmed);
    } else {
      void search(trimmed);
    }
  }

  function update(id: number, change: Partial<Search>): void {
    searches = searches.map((s) => (s.id === id ? { ...s, ...change } : s));
  }

  function isLive(id: number): boolean {
    return searches.some((s) => s.id === id && !s.stopped);
  }

  async function search(words: string): Promise<void> {
    const query = normalizeQuery(words);
    const categories = categoryIds(filters.category);
    // The same words again while they are being searched: nothing to do.
    if (active?.searching && active.query === query && sameIds(active.categories, categories)) {
      return;
    }
    // One search runs at a time: the backend stops the previous one when this one starts.
    if (searches.some((s) => s.searching)) {
      searches = searches.map((s) => (s.searching ? { ...s, searching: false, stopped: true } : s));
    }
    const id = nextId++;
    recent = rememberQuery(recent, query);
    const { kept, dropped } = evictSearches(
      [
        ...searches,
        {
          id,
          query,
          response: null,
          progress: null,
          searching: true,
          stopped: false,
          error: null,
          categories,
        },
      ],
      id,
    );
    searches = kept;
    if (dropped.length > 0) {
      onForget(dropped);
    }
    onSearch(id);
    const wait = searchDelay(lastSentAt, performance.now(), SEARCH_GAP_MS);
    if (wait > 0) {
      await new Promise((resolve) => setTimeout(resolve, wait));
      if (!isLive(id)) {
        return;
      }
    }
    lastSentAt = performance.now();
    try {
      const response = await searchTorrentsProgressive(
        query,
        (progress) => {
          if (isLive(id)) {
            update(id, {
              response: progress.response,
              progress: { completed: progress.completed, total: progress.total },
            });
          }
        },
        categories,
      );
      if (isLive(id)) {
        update(id, { response, searching: false });
      }
    } catch (reason: unknown) {
      // A newer search, or Stop, cancels this one in the backend: its results so far stay.
      const cancelled = isIpcError(reason) && reason.kind === 'cancelled';
      if (cancelled) {
        update(id, { searching: false, stopped: true });
      } else if (isLive(id)) {
        update(id, { error: errorMessage(reason), searching: false });
      }
    }
  }

  /**
   * A recent search, or the same words in another category: shows cached results if that search
   * finished, otherwise searches again.
   */
  function searchAgain(query: string): void {
    const ids = categoryIds(filters.category);
    const cached = [...searches]
      .reverse()
      .find(
        (s) =>
          s.query.toLowerCase() === query.toLowerCase() &&
          sameIds(s.categories, ids) &&
          !s.searching &&
          s.error === null &&
          s.response !== null,
      );
    if (cached) {
      recent = rememberQuery(recent, query);
      onSearch(cached.id);
    } else {
      void search(query);
    }
  }

  function sameIds(a: readonly number[], b: readonly number[]): boolean {
    return a.length === b.length && a.every((id, i) => id === b[i]);
  }

  function stop(): void {
    if (active?.searching === true) {
      update(active.id, { searching: false, stopped: true });
      cancelSearch().catch(() => undefined);
    }
  }

  function clearSearch(): void {
    stop();
    localError = null;
    onSearch(null);
    queryInput?.focus();
  }

  async function readFile(file: File): Promise<void> {
    try {
      const bytes = await readTorrentFile(file);
      localError = null;
      onFile(bytes);
    } catch (reason: unknown) {
      localError = errorMessage(reason);
    }
  }

  function chosen(event: Event): void {
    const input = event.currentTarget as HTMLInputElement;
    const file = input.files?.[0];
    input.value = '';
    if (file) {
      void readFile(file);
    }
  }
</script>

<section class="add" class:has-results={hasResults} aria-label="Search">
  <h1>TorrentPlayer</h1>
  <p class="tagline">Stream at the speed of the swarm.</p>

  <form onsubmit={submit} role="search">
    <select
      class="category"
      class:set={filters.category !== null}
      bind:value={filters.category}
      aria-label="Category"
      title="Search in one category"
    >
      <option value={null}>All categories</option>
      {#each categories as choice (choice.value)}
        <option value={choice.value}>
          {choice.value}{all.length > 0 ? ` (${String(choice.count)})` : ''}
        </option>
      {/each}
    </select>
    <input
      bind:this={queryInput}
      type="text"
      bind:value={link}
      placeholder="Search, or paste a magnet / .torrent link"
      aria-label="Search, or paste a magnet or torrent link"
      autocomplete="off"
      spellcheck="false"
      list={recent.length > 0 ? 'recent-searches' : undefined}
    />
    <button type="submit" disabled={busy || link.trim().length === 0}>
      {looksLikeLink(link) ? 'Open' : 'Search'}
    </button>
    <datalist id="recent-searches">
      {#each recent as query (query)}
        <option value={query}></option>
      {/each}
    </datalist>
  </form>

  {#if !hasResults}
    <div class="or">
      <button type="button" class="secondary" onclick={() => fileInput?.click()}>
        Open .torrent file
      </button>
      <span class="hint">or drop one anywhere on this window</span>
    </div>
    {#if recent.length > 0}
      <div class="recent" role="group" aria-label="Recent searches">
        <span class="hint">Recent:</span>
        {#each recent as query (query)}
          <button
            type="button"
            class="chip"
            onclick={() => {
              searchAgain(query);
            }}>{query}</button
          >
        {/each}
      </div>
    {/if}
  {/if}
  <input
    bind:this={fileInput}
    type="file"
    accept=".torrent,application/x-bittorrent"
    hidden
    onchange={chosen}
  />

  <p class="status" aria-live="polite">
    {#if searching}
      <!-- How many sources are left is shown with the results, out of this live region. -->
      Searching public indexers…
      <button type="button" class="link" onclick={stop}>Stop</button>
    {:else if localError ?? error ?? active?.error}
      <span role="alert">{localError ?? error ?? active?.error}</span>
    {:else if busy}
      Fetching torrent details…
    {:else if active?.response}
      {#if active.stopped}Search stopped ·{/if}
      <button type="button" class="link" onclick={clearSearch}>Clear results</button>
    {/if}
  </p>

  {#if active?.response && all.length > 0}
    <SearchFiltersBar bind:filters results={all} shown={shown.length} bestPickHidden={pickHidden} />
  {/if}

  {#if shownResponse && (shown.length > 0 || all.length === 0)}
    <SearchResults
      response={shownResponse}
      {isTorrentBusy}
      {preload}
      searching={pending}
      {prefetched}
      {thumbnails}
      {frame}
      onAdd={onLink}
      onPlay={onPlay
        ? (result: SearchResultDto) => {
            onPlay(result.link, preloadedFile(result), result.infoHash);
          }
        : undefined}
      onQueue={onQueue
        ? (result: SearchResultDto) => {
            onQueue(result, preloadedFile(result));
          }
        : undefined}
      queuedState={queuedState
        ? (result: SearchResultDto) => queuedState(result, preloadedFile(result))
        : undefined}
    />
  {:else if shownResponse}
    <p class="none">
      None of the {all.length} results match these filters.
      <button
        type="button"
        class="link"
        onclick={() => {
          filters = { ...DEFAULT_FILTERS, sort: filters.sort };
        }}>Reset filters</button
      >
    </p>
  {/if}
</section>

<style>
  .add {
    display: grid;
    grid-template-columns: minmax(0, 1fr);
    place-content: center stretch;
    justify-items: center;
    gap: 1.25rem;
    width: 100%;
    min-width: 0;
    min-height: 100%;
    box-sizing: border-box;
    padding: 0 1.5rem;
    text-align: center;
    background: radial-gradient(ellipse at 50% 35%, #1d1a16 0%, var(--bg) 70%);
  }

  h1 {
    margin: 0;
    font-family: var(--font-display);
    font-size: clamp(3rem, 8vw, 5.5rem);
    font-weight: 400;
    letter-spacing: 0.04em;
    color: var(--accent);
    text-shadow: 0 0 40px rgb(242 180 90 / 25%);
  }

  .tagline {
    margin: -0.75rem 0 1.5rem;
    letter-spacing: 0.08em;
  }

  form {
    display: flex;
    gap: 0.5rem;
    width: min(40rem, 100%);
  }

  input[type='text'] {
    flex: 1;
    min-width: 0;
    padding: 0.8rem 1rem;
    border: 1px solid #2c2a26;
    border-radius: 0.6rem;
    background: var(--surface);
    color: var(--text);
    font: inherit;
  }

  input[type='text']:focus,
  select.category:focus-visible {
    outline: 2px solid rgb(242 180 90 / 50%);
    outline-offset: 1px;
  }

  select.category {
    flex: none;
    max-width: 9.5rem;
    padding: 0 0.6rem;
    border: 1px solid #2c2a26;
    border-radius: 0.6rem;
    background: var(--surface);
    color: var(--text-muted);
    font: inherit;
    font-size: 0.9rem;
    cursor: pointer;
  }

  select.category.set {
    border-color: #4a3f2c;
    color: var(--accent);
  }

  button {
    padding: 0.8rem 1.4rem;
    border: none;
    border-radius: 0.6rem;
    background: var(--accent);
    color: #1a1408;
    font: inherit;
    font-weight: 600;
    cursor: pointer;
  }

  button:disabled {
    opacity: 0.45;
    cursor: default;
  }

  button.secondary {
    background: transparent;
    color: var(--text);
    border: 1px solid #3a3731;
    font-weight: 500;
  }

  .or,
  .recent {
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    justify-content: center;
    gap: 0.75rem;
  }

  .recent {
    gap: 0.4rem;
    max-width: min(40rem, 90vw);
  }

  button.chip {
    padding: 0.25rem 0.7rem;
    border: 1px solid #2c2a26;
    border-radius: 1rem;
    background: transparent;
    color: var(--text-muted);
    font-size: 0.8125rem;
    font-weight: 400;
  }

  button.chip:hover:not(:disabled) {
    border-color: #4a3f2c;
    color: var(--text);
  }

  .hint {
    color: var(--text-muted);
    font-size: 0.875rem;
  }

  .status {
    min-height: 1.5em;
    margin: 0;
    color: var(--text-muted);
  }

  .status [role='alert'] {
    color: #e8907a;
  }

  .none {
    margin: 0;
    color: var(--text-muted);
  }

  /* With results, the page scrolls from the top instead of centering. */
  .add.has-results {
    place-content: start stretch;
    padding-top: 2.5rem;
    padding-bottom: 2.5rem;
  }

  .add.has-results form {
    width: min(64rem, 100%);
  }

  .add.has-results h1 {
    font-size: clamp(2rem, 5vw, 3rem);
  }

  button.link {
    padding: 0 0.25rem;
    background: transparent;
    color: var(--accent);
    font-weight: 500;
    text-decoration: underline;
  }

  button:focus-visible {
    outline: 2px solid var(--accent);
    outline-offset: 1px;
  }
</style>
