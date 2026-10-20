<!-- SPDX-License-Identifier: MIT OR Apache-2.0 -->
<script lang="ts">
  import type { Snippet } from 'svelte';
  import { flip } from 'svelte/animate';
  import { fade, slide } from 'svelte/transition';
  import { toggled } from '../details';
  import ResultDetails from './ResultDetails.svelte';
  import { formatBytes } from '../format';
  import type { PreloadStatusDto, SearchResponseDto, SearchResultDto } from '../ipc';
  import {
    failedIndexerCount,
    formatPeers,
    formatPublished,
    holdOrder,
    preloadLabel,
    framePossible,
    qualityNotes,
  } from '../search';
  import type { PreviewFrameDto } from '../ipc';

  interface Props {
    response: SearchResponseDto;
    isTorrentBusy: (source: string, infoHash?: string | null) => boolean;
    /** The backend's preload of the best pick, if any. */
    preload?: PreloadStatusDto | null;
    /** While the search runs, what it is still doing, for example `searching 12 more sources…`. */
    searching?: string | null;
    /** Keys of results whose torrent details are already fetched, so playing them starts sooner. */
    prefetched?: ReadonlySet<string>;
    /** Shown between the counts and the list, for example search filters. */
    filters?: Snippet;
    /** Pictures from inside the torrents, as `data:` URLs, by result key. */
    thumbnails?: ReadonlyMap<string, string>;
    /** The preloaded best pick's video, for a frame of its start. */
    frame?: PreviewFrameDto | null;
    onAdd: (magnet: string, infoHash?: string | null) => void;
    /** Adds a result and plays its video at once, skipping the file list. */
    onPlay?: ((result: SearchResultDto) => void) | undefined;
    /** Adds a result to the queue. */
    onQueue?: ((result: SearchResultDto) => void) | undefined;
  }

  let {
    response,
    isTorrentBusy,
    preload = null,
    searching = null,
    prefetched = new Set<string>(),
    filters,
    thumbnails = new Map<string, string>(),
    frame = null,
    onAdd,
    onPlay,
    onQueue,
  }: Props = $props();

  // Rows slide to their new places as results arrive; less motion if the system asks for it.
  const reduceMotion =
    typeof window !== 'undefined' &&
    typeof window.matchMedia === 'function' &&
    window.matchMedia('(prefers-reduced-motion: reduce)').matches;
  const MOVE_MS = reduceMotion ? 0 : 200;
  const FADE_MS = reduceMotion ? 0 : 150;

  // A frame a few seconds into the best pick's video (its start is preloaded), unless the
  // webview cannot decode it.
  let frameFailed = $state<string | null>(null);

  function showFrame(event: Event): void {
    const video = event.currentTarget as HTMLVideoElement;
    const at = Number.isFinite(video.duration) ? Math.min(5, video.duration / 10) : 5;
    video.currentTime = at;
  }

  // While the pointer or keyboard focus is in the list, rows keep their places and new results
  // go to the end, so what the user is about to click does not move. The ranked order returns
  // when they leave.
  let held = $state.raw<SearchResultDto[] | null>(null);
  // Rows whose details are open (several may be); they open in place, below the row.
  let expanded = $state.raw<ReadonlySet<string>>(new Set());

  function toggle(key: string): void {
    expanded = toggled(expanded, key);
  }

  // Unique per result and safe as an element id.
  const detailsId = (index: number): string => `result-details-${String(index)}`;
  let pointerIn = false;
  let focusIn = false;
  const rows = $derived(held === null ? response.results : holdOrder(held, response.results));

  function updateHold(): void {
    if (pointerIn || focusIn) {
      held ??= response.results;
    } else {
      held = null;
    }
  }

  const failed = $derived(failedIndexerCount(response.indexers));

  function meta(result: SearchResultDto): string {
    return [
      ...(result.bestPick ? [] : qualityNotes(result.quality)),
      result.sizeBytes === null ? null : formatBytes(result.sizeBytes),
      formatPeers(result.seeders, result.peers),
      result.category,
      formatPublished(result.published) || null,
    ]
      .filter((part): part is string => part !== null && part !== '')
      .join(' · ');
  }
</script>

<section class="results" aria-label="Search results">
  <p class="counts">
    {response.results.length} result{response.results.length === 1 ? '' : 's'}
    {#if response.excludedAdult > 0}
      · {response.excludedAdult} adult result{response.excludedAdult === 1 ? '' : 's'} hidden
    {/if}
    {#if searching}
      · <span class="pending"><span class="dot" aria-hidden="true"></span>{searching}</span>
    {:else if failed > 0}
      · {failed} of {response.indexers.length} indexers did not respond
    {/if}
  </p>

  {@render filters?.()}

  {#if response.results.length === 0}
    {#if !searching}
      <p class="empty">Nothing found. Try different or fewer words.</p>
    {/if}
  {:else}
    <ul
      onpointerenter={() => {
        pointerIn = true;
        updateHold();
      }}
      onpointerleave={() => {
        pointerIn = false;
        updateHold();
      }}
      onfocusin={() => {
        focusIn = true;
        updateHold();
      }}
      onfocusout={(event) => {
        focusIn = event.currentTarget.contains(event.relatedTarget as Node | null);
        updateHold();
      }}
    >
      {#each rows as result, index (result.key)}
        {@const preloaded = preloadLabel(preload, result.infoHash)}
        {@const picture = thumbnails.get(result.key)}
        {@const open = expanded.has(result.key)}
        <li
          class:best={result.bestPick}
          animate:flip={{ duration: MOVE_MS }}
          in:fade={{ duration: FADE_MS }}
        >
          <div class="row">
            <!-- Space is kept even before (or without) a picture, so rows never jump. -->
            <div class="thumb" class:large={result.bestPick} aria-hidden="true">
              {#if result.bestPick && frame && frame.infoHash === result.infoHash && frameFailed !== frame.url && framePossible(result.quality)}
                <video
                  src={frame.url}
                  muted
                  playsinline
                  preload="metadata"
                  tabindex="-1"
                  disablepictureinpicture
                  onloadedmetadata={showFrame}
                  onerror={() => {
                    frameFailed = frame.url;
                  }}
                ></video>
              {:else if picture}
                <img src={picture} alt="" loading="lazy" decoding="async" />
              {/if}
            </div>
            <!-- Clicking the text opens the details too; the Details button is the keyboard way. -->
            <div
              class="text"
              role="button"
              tabindex="-1"
              aria-expanded={open}
              aria-controls={detailsId(index)}
              onclick={() => {
                toggle(result.key);
              }}
              onkeydown={(event) => {
                if (event.key === 'Enter' || event.key === ' ') {
                  event.preventDefault();
                  toggle(result.key);
                }
              }}
            >
              <span class="title">
                {#if result.bestPick}<span class="pick">Best pick</span>{/if}
                {result.title}
              </span>
              {#if result.bestPick}
                <span class="reason">
                  {result.reason}
                  {#if preloaded}<span class="preload" aria-live="polite">· {preloaded}</span>{/if}
                </span>
              {/if}
              <span class="meta">{meta(result)}</span>
              <span class="source">
                {result.indexers.join(', ')}
                {#if !preloaded && prefetched.has(result.key)}
                  <span class="ready" title="Torrent details fetched: playing it starts sooner"
                    >· details ready</span
                  >
                {/if}
              </span>
            </div>
            <div class="actions">
              <button
                type="button"
                class="toggle"
                aria-expanded={open}
                aria-controls={detailsId(index)}
                title={open ? 'Hide the details' : 'Show everything known about it'}
                onclick={() => {
                  toggle(result.key);
                }}>Details {open ? '▴' : '▾'}</button
              >
              {#if onPlay}
                <button
                  type="button"
                  disabled={isTorrentBusy(result.link, result.infoHash)}
                  title="Add it and play the video now"
                  onclick={() => {
                    onPlay(result);
                  }}>{isTorrentBusy(result.link, result.infoHash) ? 'Loading…' : 'Play'}</button
                >
              {/if}
              {#if onQueue}
                <button
                  type="button"
                  class="secondary"
                  title="Play it after the current video"
                  onclick={() => {
                    onQueue(result);
                  }}>Queue</button
                >
              {/if}
              <button
                type="button"
                class:secondary={onPlay !== undefined}
                disabled={isTorrentBusy(result.link, result.infoHash)}
                title="Add it and choose a file"
                onclick={() => {
                  onAdd(result.link, result.infoHash);
                }}>{isTorrentBusy(result.link, result.infoHash) ? 'Loading…' : 'Add'}</button
              >
            </div>
          </div>
          {#if open}
            <div transition:slide={{ duration: MOVE_MS }}>
              <ResultDetails {result} id={detailsId(index)} />
            </div>
          {/if}
        </li>
      {/each}
    </ul>
  {/if}
</section>

<style>
  .results {
    container-type: inline-size;
    width: 100%;
    min-width: 0;
    text-align: left;
  }

  .counts {
    margin: 0 0 0.75rem;
    color: var(--text-muted);
    font-size: 0.8125rem;
    font-variant-numeric: tabular-nums;
  }

  .pending {
    white-space: nowrap;
  }

  .dot {
    display: inline-block;
    width: 0.4rem;
    height: 0.4rem;
    margin-right: 0.35rem;
    border-radius: 50%;
    background: var(--accent);
    vertical-align: 0.1em;
    animation: pulse 1.4s ease-in-out infinite;
  }

  @keyframes pulse {
    50% {
      opacity: 0.25;
    }
  }

  @media (prefers-reduced-motion: reduce) {
    .dot {
      animation: none;
    }
  }

  .empty {
    color: var(--text-muted);
  }

  ul {
    display: grid;
    gap: 0.4rem;
    margin: 0;
    padding: 0;
    list-style: none;
  }

  li {
    min-width: 0;
    border: 1px solid #24221e;
    border-radius: 0.6rem;
    background: var(--surface);
  }

  .row {
    display: grid;
    grid-template-columns: auto minmax(0, 1fr) auto;
    align-items: start;
    gap: 1rem;
    padding: 0.7rem 0.9rem;
  }

  .text {
    cursor: pointer;
  }

  .text:focus-visible,
  .toggle:focus-visible {
    outline: 2px solid var(--accent);
    outline-offset: 1px;
  }

  button.toggle {
    padding: 0.5rem 0.4rem;
    background: transparent;
    color: var(--text-muted);
    font-weight: 500;
  }

  button.toggle:hover {
    color: var(--text);
  }

  /* A fixed box, so a picture arriving later never moves anything. */
  .thumb {
    flex: none;
    width: 4.5rem;
    aspect-ratio: 16 / 10;
    overflow: hidden;
    border-radius: 0.35rem;
    background: #1b1a17;
  }

  .thumb.large {
    width: 8rem;
  }

  .thumb img,
  .thumb video {
    display: block;
    width: 100%;
    height: 100%;
    object-fit: cover;
    pointer-events: none;
  }

  @container (width <= 52rem) {
    .row {
      grid-template-columns: auto minmax(0, 1fr);
    }

    .actions {
      grid-column: 2 / -1;
      justify-self: start;
    }

    .thumb {
      grid-row: 1 / span 2;
    }
  }

  @container (width <= 30rem) {
    .thumb {
      grid-row: auto;
    }

    .actions {
      grid-column: 1 / -1;
    }

    .thumb {
      width: 3.25rem;
    }

    .thumb.large {
      width: 5rem;
    }
  }

  li.best {
    border-color: #4a3f2c;
  }

  .pick {
    margin-right: 0.45rem;
    padding: 0.05rem 0.45rem;
    border-radius: 1rem;
    background: rgb(242 180 90 / 15%);
    color: var(--accent);
    font-size: 0.75rem;
    font-weight: 600;
    vertical-align: 0.1em;
  }

  .reason {
    color: var(--text);
    font-size: 0.8125rem;
    overflow-wrap: anywhere;
    opacity: 0.85;
  }

  .preload {
    color: var(--accent);
  }

  .ready {
    margin-left: 0.25rem;
    opacity: 0.8;
  }

  .text {
    display: grid;
    flex: 1;
    gap: 0.25rem;
    min-width: 0;
    line-height: 1.45;
  }

  .title {
    color: var(--text);
    overflow-wrap: anywhere;
  }

  .meta,
  .source {
    color: var(--text-muted);
    font-size: 0.8125rem;
    overflow-wrap: anywhere;
    font-variant-numeric: tabular-nums;
  }

  button {
    padding: 0.5rem 1rem;
    border: none;
    border-radius: 0.5rem;
    background: var(--accent);
    color: #1a1408;
    font: inherit;
    font-weight: 600;
    white-space: nowrap;
    cursor: pointer;
  }

  button:disabled {
    opacity: 0.45;
    cursor: default;
  }

  button.secondary {
    border: 1px solid #3a3731;
    background: transparent;
    color: var(--text);
    font-weight: 500;
  }

  /* Play stands out on the best pick; on the other rows it is quieter. */
  li:not(.best) .actions button:not(.secondary):not(.toggle) {
    background: rgb(242 180 90 / 85%);
  }

  .actions {
    display: flex;
    flex: none;
    flex-wrap: wrap;
    justify-content: flex-end;
    gap: 0.4rem;
    max-width: 100%;
  }
</style>
