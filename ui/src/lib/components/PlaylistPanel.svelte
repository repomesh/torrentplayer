<!-- SPDX-License-Identifier: MIT OR Apache-2.0 -->
<script lang="ts">
  /*
   * The playlist beside the player: what is playing, and the queue to pick from. It
   * collapses to a slim strip (Q toggles it), and is hidden in fullscreen.
   */
  import type { FileSummary } from '../bindings/FileSummary';
  import type { LibraryItem } from '../bindings/LibraryItem';
  import type { ReadinessState } from '../bindings/ReadinessState';
  import { byId, describeLink, type DownloadsSnapshot } from '../downloads';
  import type { NetworkStatus } from '../ipc';
  import { followNetwork, queueBlockedNotice } from '../network';
  import {
    currentIndex,
    groupKey,
    SORT_KEYS,
    upcomingCount,
    type Queue,
    type QueueItem,
    type SortKey,
  } from '../queue';
  import QueueList from './QueueList.svelte';

  interface Props {
    queue: Queue;
    collapsed: boolean;
    /** The queue item playing now, if the video playing came from the queue. */
    playingId: string | null;
    /** The file playing, when it did not come from the queue. */
    playingTitle: string | null;
    nextStatus: string | null;
    isBusy: (key: string) => boolean;
    onToggle: () => void;
    onPlay: (item: QueueItem) => void;
    onRemove: (id: string) => void;
    onMoveGroup: (key: string, index: number) => void;
    onRemoveGroup: (key: string) => void;
    onChooseFiles: (key: string) => void;
    onClearPlayed: () => void;
    readiness: ReadonlyMap<string, ReadinessState>;
    /** A torrent's files by group key, if opened (see `QueueList`). */
    filesOf: (key: string) => readonly FileSummary[] | null;
    playingFile: { keys: readonly string[]; index: number } | null;
    onPlayFile: (key: string, file: FileSummary) => void;
    onToggleFile: (key: string, file: FileSummary, queued: boolean) => void;
    onLoadFiles: (key: string) => void;
    /** Empties the queue (with Undo). */
    onClear: () => void;
    /** Background downloads of the queue (`queue_downloads`), once known. */
    downloads?: DownloadsSnapshot | null;
    /** Saved watch progress, keyed by torrent info hash and file index. */
    watchProgress?: ReadonlyMap<string, LibraryItem>;
    /** Pauses or resumes background downloading of items. */
    onTogglePause?: (ids: readonly string[], paused: boolean) => void;
    /** Reorders the queue once by `key` ("Sort by…"); it never re-sorts on its own. */
    onSort?: (key: SortKey) => void;
    /** Opens Settings → Network & VPN (from the banner shown while traffic is blocked). */
    onNetworkSettings?: () => void;
  }

  let {
    queue,
    collapsed,
    playingId,
    playingTitle,
    nextStatus,
    isBusy,
    onToggle,
    onPlay,
    onRemove,
    onMoveGroup,
    onRemoveGroup,
    onChooseFiles,
    onClearPlayed,
    readiness,
    filesOf,
    playingFile,
    onPlayFile,
    onToggleFile,
    onLoadFiles,
    onClear,
    downloads = null,
    watchProgress = new Map(),
    onTogglePause,
    onSort,
    onNetworkSettings,
  }: Props = $props();

  // The kill switch: while it blocks traffic, rows would say "Finding peers…" with no reason.
  let network = $state<NetworkStatus | null>(null);
  $effect(() =>
    followNetwork((next) => {
      network = next;
    }),
  );
  const blockedNotice = $derived(queueBlockedNotice(network));

  const linkLine = $derived(downloads ? describeLink(downloads.link) : null);
  const byItem = $derived(byId(downloads));
  const anyRunning = $derived(queue.items.some((i) => !i.paused));

  const upcoming = $derived(upcomingCount(queue));
  let menuOpen = $state(false);
  let confirmClear = $state(false);
  let menuButton: HTMLButtonElement | undefined = $state();

  function closeMenu(): void {
    menuOpen = false;
    confirmClear = false;
  }

  // A click outside the menu closes it.
  $effect(() => {
    if (!menuOpen) {
      return;
    }
    const outside = (event: PointerEvent): void => {
      const target = event.target;
      if (target instanceof Element && !target.closest('.menu, .more')) {
        closeMenu();
      }
    };
    window.addEventListener('pointerdown', outside);
    return () => {
      window.removeEventListener('pointerdown', outside);
    };
  });
</script>

{#if collapsed}
  <aside class="strip" aria-label="Queue">
    <button
      type="button"
      class="toggle"
      aria-expanded="false"
      aria-label={`Show the queue${upcoming > 0 ? `, ${String(upcoming)} to play` : ''}`}
      title="Show the queue (Q)"
      onclick={onToggle}
    >
      <svg viewBox="0 0 24 24" aria-hidden="true"
        ><path d="M4 6.5h12M4 11.5h12M4 16.5h6.5M15 14v6l5-3z" /></svg
      >
      {#if upcoming > 0}<span class="count" aria-hidden="true">{upcoming}</span>{/if}
    </button>
  </aside>
{:else}
  <aside class="panel" aria-label="Queue">
    <header>
      <h2>Queue</h2>
      {#if upcoming > 0}
        <span class="sub">{upcoming} to play</span>
      {/if}
      <button
        bind:this={menuButton}
        type="button"
        class="toggle more"
        aria-haspopup="true"
        aria-expanded={menuOpen}
        aria-controls="queue-menu"
        aria-label="Queue options"
        title="Queue options"
        onclick={() => {
          menuOpen = !menuOpen;
          confirmClear = false;
        }}
      >
        <svg viewBox="0 0 24 24" aria-hidden="true"
          ><path d="M6 12h.01M12 12h.01M18 12h.01" stroke-width="3" /></svg
        >
      </button>
      <button
        type="button"
        class="toggle"
        aria-expanded="true"
        aria-label="Hide the queue"
        title="Hide the queue (Q)"
        onclick={onToggle}
      >
        <svg viewBox="0 0 24 24" aria-hidden="true"><path d="M14.5 6l-6 6 6 6" /></svg>
      </button>
    </header>

    {#if menuOpen}
      <!-- A non-modal dialog: the player leaves keys inside dialogs alone. -->
      <dialog
        open
        id="queue-menu"
        class="menu"
        aria-label="Queue options"
        onkeydown={(event: KeyboardEvent) => {
          if (event.key === 'Escape') {
            event.preventDefault();
            closeMenu();
            menuButton?.focus();
          }
        }}
      >
        {#if onSort}
          <div class="sort" role="group" aria-labelledby="queue-sort-label">
            <span id="queue-sort-label" class="menu-label"
              >Sort by… <span class="hint">(once; drag to reorder after)</span></span
            >
            {#each SORT_KEYS as option (option.key)}
              <button
                type="button"
                disabled={queue.items.length < 2}
                onclick={() => {
                  onSort(option.key);
                  closeMenu();
                }}>{option.label}</button
              >
            {/each}
          </div>
          <hr />
        {/if}
        {#if onTogglePause}
          <button
            type="button"
            disabled={queue.items.length === 0}
            title="Stops downloading and uploading in the background until you resume; playing a video still works"
            onclick={() => {
              onTogglePause(
                queue.items.map((i) => i.id),
                anyRunning,
              );
              closeMenu();
            }}>{anyRunning ? 'Pause all (download and upload)' : 'Resume all'}</button
          >
        {/if}
        <button
          type="button"
          disabled={currentIndex(queue) <= 0}
          onclick={() => {
            onClearPlayed();
            closeMenu();
          }}>Clear played</button
        >
        {#if confirmClear}
          <span class="confirm">
            Clear all {queue.items.length}?
            <button
              type="button"
              class="danger"
              onclick={() => {
                onClear();
                closeMenu();
              }}>Clear</button
            >
            <button
              type="button"
              onclick={() => {
                confirmClear = false;
              }}>Keep</button
            >
          </span>
        {:else}
          <button
            type="button"
            disabled={queue.items.length === 0}
            onclick={() => {
              confirmClear = true;
            }}>Clear all…</button
          >
        {/if}
      </dialog>
    {/if}

    {#if blockedNotice}
      <div class="blocked" role="status">
        <p>{blockedNotice}</p>
        {#if onNetworkSettings}
          <button type="button" onclick={onNetworkSettings}>Network settings</button>
        {/if}
      </div>
    {/if}

    {#if queue.items.length > 0 && linkLine}
      <p class="link" title="Measured from what all torrents download together">
        {linkLine}
      </p>
    {/if}

    {#if playingTitle}
      <p class="now">
        <span class="label">Playing</span>
        <span class="title" title={playingTitle}>{playingTitle}</span>
      </p>
    {/if}

    <div class="body">
      {#if queue.items.length === 0}
        <p class="empty">
          Nothing queued yet. Use <strong>Queue</strong> on a search result or a file to line up videos
          to watch.
        </p>
      {:else}
        {#if playingId === null && playingTitle === null}
          {@const first = queue.items[0]}
          {#if first}
            <div class="queue-start">
              <p>Queued videos download in the background while there is bandwidth to spare.</p>
              <button
                type="button"
                disabled={isBusy(groupKey(first))}
                onclick={() => {
                  onPlay(first);
                }}>{isBusy(groupKey(first)) ? 'Loading…' : 'Play first'}</button
              >
            </div>
          {/if}
        {/if}
        <QueueList
          {queue}
          {playingId}
          {nextStatus}
          {isBusy}
          {readiness}
          {filesOf}
          {playingFile}
          {onPlayFile}
          {onToggleFile}
          {onLoadFiles}
          compact
          {onPlay}
          {onRemove}
          {onMoveGroup}
          {onRemoveGroup}
          {onChooseFiles}
          downloads={byItem}
          {watchProgress}
          {onTogglePause}
        />
      {/if}
    </div>
  </aside>
{/if}

<style>
  .queue-start {
    margin: 0.5rem;
    padding: 0.75rem;
    border: 1px solid rgb(255 255 255 / 10%);
    border-radius: 0.5rem;
  }

  .queue-start p {
    margin: 0 0 0.5rem;
    font-size: 0.8rem;
    color: #b3b3c4;
  }

  .panel,
  .strip {
    flex: none;
    box-sizing: border-box;
    height: 100%;
    border-right: 1px solid rgb(255 255 255 / 5%);
    background: #0f0f14;
  }

  .panel {
    position: relative;
    display: flex;
    flex-direction: column;
    width: clamp(14rem, 24vw, 20rem);
  }

  .strip {
    display: flex;
    justify-content: center;
    width: 2.25rem;
    padding-top: 0.5rem;
  }

  header {
    display: flex;
    align-items: baseline;
    gap: 0.5rem;
    padding: 0.75rem 0.5rem 0.5rem 0.9rem;
  }

  h2 {
    margin: 0;
    font-size: 0.95rem;
    font-weight: 600;
  }

  .sub {
    color: var(--text-muted);
    font-size: 0.78rem;
  }

  button {
    border: none;
    background: none;
    color: var(--text-muted);
    font: inherit;
    cursor: pointer;
  }

  button:focus-visible {
    outline: 2px solid var(--accent);
    outline-offset: -2px;
  }

  .toggle {
    position: relative;
    display: inline-flex;
    margin-left: auto;
    padding: 0.3rem;
    border-radius: 0.35rem;
    align-self: center;
  }

  .strip .toggle {
    margin: 0;
    align-self: flex-start;
  }

  .toggle:hover {
    color: var(--text);
    background: rgb(255 255 255 / 6%);
  }

  .count {
    position: absolute;
    top: -0.2rem;
    right: -0.35rem;
    min-width: 0.9rem;
    border-radius: 1rem;
    background: var(--accent);
    color: #1a1408;
    font-size: 0.58rem;
    font-weight: 700;
    line-height: 0.9rem;
    text-align: center;
  }

  svg {
    width: 1rem;
    height: 1rem;
    fill: none;
    stroke: currentcolor;
    stroke-width: 1.7;
    stroke-linecap: round;
    stroke-linejoin: round;
  }

  .blocked {
    margin: 0 0.6rem 0.5rem;
    padding: 0.5rem 0.6rem;
    border: 1px solid #6b3a30;
    border-radius: 0.5rem;
    background: rgb(232 144 122 / 8%);
    font-size: 0.8rem;
    line-height: 1.4;
  }

  .blocked p {
    margin: 0 0 0.4rem;
    color: var(--text);
  }

  .blocked button {
    padding: 0.2rem 0.55rem;
    border: 1px solid #6b3a30;
    border-radius: 0.35rem;
    color: #e8907a;
  }

  .blocked button:hover {
    background: rgb(232 144 122 / 10%);
  }

  .link {
    margin: 0 0.6rem 0.5rem;
    color: var(--text-muted);
    font-size: 0.72rem;
    line-height: 1.35;
  }

  .now {
    display: grid;
    gap: 0.1rem;
    margin: 0 0.6rem 0.5rem;
    padding: 0.45rem 0.6rem;
    border: 1px solid #4a3f2c;
    border-radius: 0.5rem;
    background: rgb(242 180 90 / 7%);
  }

  .now .label {
    color: var(--accent);
    font-size: 0.72rem;
  }

  .now .title {
    overflow: hidden;
    font-size: 0.875rem;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  .body {
    flex: 1;
    min-height: 0;
    padding: 0 0.35rem;
    overflow-y: auto;
  }

  .empty {
    margin: 0.25rem 0.55rem;
    color: var(--text-muted);
    font-size: 0.85rem;
    line-height: 1.45;
  }

  .empty strong {
    color: var(--text);
    font-weight: 500;
  }

  .more + .toggle {
    margin-left: 0;
  }

  .menu {
    position: absolute;
    z-index: 8;
    top: 2.6rem;
    right: 0.5rem;
    left: auto;
    display: grid;
    gap: 0.15rem;
    min-width: 13rem;
    margin: 0;
    padding: 0.4rem;
    border: 1px solid rgb(255 255 255 / 8%);
    border-radius: 0.6rem;
    background: #17171e;
    color: var(--text);
    box-shadow: 0 0.6rem 1.6rem rgb(0 0 0 / 50%);
    font-size: 0.85rem;
  }

  .menu > button,
  .sort > button {
    display: flex;
    align-items: center;
    gap: 0.5rem;
    padding: 0.4rem 0.5rem;
    border-radius: 0.4rem;
    color: var(--text);
    text-align: left;
    cursor: pointer;
  }

  .menu > button:hover:not(:disabled),
  .sort > button:hover:not(:disabled) {
    background: rgb(255 255 255 / 6%);
  }

  .menu > button:disabled,
  .sort > button:disabled {
    cursor: default;
    opacity: 0.4;
  }

  .sort {
    display: grid;
    gap: 0.15rem;
  }

  .menu-label {
    padding: 0.3rem 0.5rem 0.1rem;
    color: var(--text-muted);
    font-size: 0.75rem;
  }

  .menu-label .hint {
    opacity: 0.8;
  }

  .menu hr {
    width: 100%;
    margin: 0.2rem 0;
    border: none;
    border-top: 1px solid rgb(255 255 255 / 6%);
  }

  .confirm {
    display: flex;
    align-items: center;
    gap: 0.4rem;
    padding: 0.35rem 0.5rem;
    color: var(--text-muted);
  }

  .confirm button {
    padding: 0.2rem 0.5rem;
    border: 1px solid #3a3731;
    border-radius: 0.35rem;
    color: var(--text);
  }

  .confirm .danger {
    border-color: #6b3a30;
    color: #e8907a;
  }

  /* Narrow windows: the panel would squeeze the video, so it overlays it instead. */
  @media (max-width: 720px) {
    .panel {
      position: absolute;
      z-index: 6;
      top: 0;
      bottom: 0;
      left: 0;
      width: min(18rem, 80vw);
      box-shadow: 0.5rem 0 1.5rem rgb(0 0 0 / 50%);
    }
  }
</style>
