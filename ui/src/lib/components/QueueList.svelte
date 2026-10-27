<!-- SPDX-License-Identifier: MIT OR Apache-2.0 -->
<script lang="ts">
  /*
   * The queue as a tree: each torrent with several videos is a node (name, files, size, how
   * many are ready) whose videos sit beneath it in lecture order, queued or not. A tick adds a
   * video to the queue or removes it; the name plays it now. A torrent with one video is a
   * plain row. Torrents move as a whole: drag the torrent row, or Alt+↑/↓ on it.
   *
   * Keyboard (ARIA treeview): ↑/↓ move, → expands or enters a torrent, ← collapses or goes
   * up, Enter plays, Space ticks, P pauses or resumes, Delete removes, Alt+↑/↓ moves a torrent.
   * The tree is one Tab stop (roving tabindex); the focused row's or torrent's own buttons
   * (pause, choose files, all videos, play, remove) follow it in the Tab order, and Escape on one
   * of them returns to its row.
   *
   * Queued videos download in the background: each row shows its speed, peers, progress, and
   * time left, and can be paused (a torrent row pauses all of its queued videos). Nothing is
   * paused or removed automatically: a pause stops downloading and uploading until the user
   * resumes it, and unticking removes the video (with Undo). Only the video playing can hold downloads back ("Waiting for
   * playback"), which never changes the user's pauses.
   */
  import { tick, untrack } from 'svelte';
  import type { FileSummary } from '../bindings/FileSummary';
  import type { LibraryItem } from '../bindings/LibraryItem';
  import type { ReadinessState } from '../bindings/ReadinessState';
  import { fileName, isPlayable, mediaNoun } from '../files';
  import {
    barTitle,
    canPause,
    withPause,
    describeDownload,
    isMoving,
    progressOf,
    toneOf,
    type ItemDownload,
  } from '../downloads';
  import { formatBytes, formatRate } from '../format';
  import { libraryKey, watchedLabel } from '../library';
  import {
    nextItem,
    queueRowLabel,
    groupQueue,
    groupTree,
    type Queue,
    type QueueGroup,
    type QueueItem,
    type TreeRow,
  } from '../queue';
  import DownloadedChunks from './DownloadedChunks.svelte';

  interface Props {
    queue: Queue;
    /** The queue item playing now, if the video playing came from the queue. */
    playingId: string | null;
    playbackPaused?: boolean;
    onTogglePlayback?: (() => void) | undefined;
    onStopPlayback?: (() => void) | undefined;
    /** The torrent and file playing now, by group key and file index. */
    playingFile?: { keys: readonly string[]; index: number } | null;
    /** What the next item's preload is doing (`Ready to play instantly`), if anything. */
    nextStatus?: string | null;
    /** How ready items are to play, by id; items not listed are unknown. */
    readiness?: ReadonlyMap<string, ReadinessState>;
    /** A torrent's files (by group key), if it was opened; else only its queued items show. */
    filesOf?: (key: string) => readonly FileSummary[] | null;
    /** Narrow layout for the panel beside the player. */
    compact?: boolean;
    /** Loading or starting playback only blocks its own torrent. */
    isBusy?: (key: string) => boolean;
    onPlay: (item: QueueItem) => void;
    /** Plays a video of a torrent that is not queued. */
    onPlayFile: (key: string, file: FileSummary) => void;
    /** Ticks (queues) or unticks (removes) a video of a torrent. */
    onToggleFile: (key: string, file: FileSummary, queued: boolean) => void;
    onRemove: (id: string) => void;
    onMoveGroup: (key: string, index: number) => void;
    /** Removes every queued file of a torrent. */
    onRemoveGroup?: (key: string) => void;
    /** Opens "Choose files…" for a torrent (bulk changes). */
    onChooseFiles?: (key: string) => void;
    /** Fetches a torrent's file list, to show all its videos. */
    onLoadFiles?: (key: string) => void;
    /** Background downloads by item id; items not listed are unknown. */
    downloads?: ReadonlyMap<string, ItemDownload>;
    /** Saved watch progress, keyed by torrent info hash and file index. */
    watchProgress?: ReadonlyMap<string, LibraryItem>;
    /** Pauses or resumes background downloading of items. */
    onTogglePause?: ((ids: readonly string[], paused: boolean) => void) | undefined;
    /** The limit of downloads at once, when known (for the waiting rows' wording). */
    maxActive?: number | null;
  }

  let {
    queue,
    playingId,
    playbackPaused = false,
    onTogglePlayback,
    onStopPlayback,
    playingFile = null,
    nextStatus = null,
    readiness = new Map(),
    filesOf,
    compact = false,
    isBusy = () => false,
    onPlay,
    onPlayFile,
    onToggleFile,
    onRemove,
    onMoveGroup,
    onRemoveGroup,
    onChooseFiles,
    onLoadFiles,
    downloads = new Map(),
    watchProgress = new Map(),
    onTogglePause,
    maxActive = null,
  }: Props = $props();

  /** Pause transfers: the pause bars beside an up and down arrow (unlike the video's pause). */
  const PAUSE_TRANSFERS = 'M6 6v10M10 6v10M17 5v14M14.5 7.5 17 5l2.5 2.5M14.5 16.5 17 19l2.5-2.5';
  const PLAYING_PAUSE_TITLE = "Playing now: use the player's controls";

  type Node =
    | { kind: 'group'; id: string; group: QueueGroup; g: number }
    | {
        kind: 'row';
        id: string;
        group: QueueGroup;
        g: number;
        row: TreeRow;
        parent: string | null;
      };

  const next = $derived(nextItem(queue));
  const groups = $derived(groupQueue(queue));
  let collapsed = $state.raw<readonly string[]>([]);
  let focusId = $state<string | null>(null);
  let dragKey = $state<string | null>(null);
  let dropIndex = $state<number | null>(null);
  let announcement = $state('');
  let root: HTMLUListElement | undefined = $state();

  // Reveal a newly selected file even if its torrent was collapsed. An explicit collapse
  // afterwards is respected until the user chooses a different playing item.
  $effect(() => {
    const id = playingId;
    untrack(() => {
      const group = groups.find((g) => g.items.some((item) => item.id === id));
      if (group) {
        collapsed = collapsed.filter((key) => key !== group.key);
      }
    });
  });

  function videos(group: QueueGroup): readonly FileSummary[] | null {
    return filesOf?.(group.key) ?? null;
  }

  /** Whether a group shows as a tree node (otherwise its one video is a plain row). */
  function chrome(group: QueueGroup): boolean {
    const files = videos(group);
    const count = files === null ? 1 : files.filter(isPlayable).length;
    return group.items.length > 1 || count > 1;
  }

  function rowId(group: QueueGroup, row: TreeRow): string {
    return row.item ? `i:${row.item.id}` : `f:${group.key}:${String(row.file?.index ?? -1)}`;
  }

  /** Every node in display order, with collapsed groups' rows left out. */
  const nodes = $derived.by((): Node[] => {
    const list: Node[] = [];
    groups.forEach((group, g) => {
      if (!chrome(group)) {
        for (const item of group.items) {
          list.push({
            kind: 'row',
            id: `i:${item.id}`,
            group,
            g,
            row: { file: null, item },
            parent: null,
          });
        }
        return;
      }
      const id = `g:${group.key}`;
      list.push({ kind: 'group', id, group, g });
      if (!collapsed.includes(group.key)) {
        for (const row of groupTree(group, videos(group))) {
          list.push({ kind: 'row', id: rowId(group, row), group, g, row, parent: id });
        }
      }
    });
    return list;
  });
  // The node that takes Tab: the focused one, else the playing one, else the first.
  const tabId = $derived(
    nodes.find((n) => n.id === focusId)?.id ??
      nodes.find((n) => n.kind === 'row' && n.row.item?.id === playingId)?.id ??
      nodes[0]?.id ??
      null,
  );

  function isReady(item: QueueItem | null): boolean {
    const state = item ? readiness.get(item.id) : undefined;
    return state === 'ready' || state === 'complete';
  }

  function groupName(group: QueueGroup): string {
    return (
      group.items.find((i) => i.torrentName !== null)?.torrentName ?? group.items[0]?.title ?? ''
    );
  }

  function groupMeta(group: QueueGroup): string {
    const files = videos(group);
    const total = files?.filter(isPlayable).length;
    const queued = group.items.length;
    const bytes = group.items.reduce((sum, i) => sum + (i.sizeBytes ?? 0), 0);
    const ready = group.items.filter(isReady).length;
    return [
      total === undefined || total === queued
        ? `${String(queued)} ${mediaNoun(files ?? [], queued)}`
        : `${String(queued)} of ${String(total)} queued`,
      bytes > 0 ? formatBytes(bytes) : null,
      ready > 0 ? `${String(ready)} ready` : null,
      groupRate(group),
    ]
      .filter((part): part is string => part !== null)
      .join(' · ');
  }

  function isPlaying(group: QueueGroup, row: TreeRow): boolean {
    if (row.item !== null && row.item.id === playingId) {
      return true;
    }
    return (
      row.file !== null &&
      playingFile?.keys.includes(group.key) === true &&
      playingFile.index === row.file.index
    );
  }

  function savedWatch(item: QueueItem | null): string | null {
    if (item === null || item.infoHash === null || item.fileIndex === null) {
      return null;
    }
    return watchedLabel(
      watchProgress.get(libraryKey({ infoHash: item.infoHash, fileIndex: item.fileIndex })),
    );
  }

  function rowLabel(group: QueueGroup, row: TreeRow): string | null {
    if (isPlaying(group, row) && playbackPaused) {
      return 'Paused';
    }
    return queueRowLabel({
      playing: isPlaying(group, row),
      busy: isBusy(group.key),
      item: row.item,
      // The same item Shift+N plays.
      next: row.item !== null && row.item.id === next?.id,
      nextStatus,
      watched: savedWatch(row.item),
      ready: row.item !== null && isReady(row.item),
    });
  }

  /** Its torrent's download rate (its items share the torrent), while any of them moves. */
  function groupRate(group: QueueGroup): string | null {
    const rates = group.items
      .map((i) => downloads.get(i.id))
      .filter((d): d is ItemDownload => d !== undefined && isMoving(d))
      .map((d) => d.downloadBps);
    const bps = Math.max(0, ...rates);
    return bps > 0 ? `↓ ${formatRate(bps)}` : null;
  }

  /** Whether every queued video of a torrent is paused. */
  function groupPaused(group: QueueGroup): boolean {
    return group.items.every((i) => i.paused);
  }

  /** Removes every queued video of a torrent (the toast offers Undo). */
  function removeWholeGroup(group: QueueGroup): void {
    if (!onRemoveGroup) {
      return;
    }
    onRemoveGroup(group.key);
    announcement = `${groupName(group)} removed from the queue`;
  }

  /** An item's download as shown, with its pause applied at once. */
  function shownDownload(item: QueueItem): ItemDownload | undefined {
    return withPause(item.paused, downloads.get(item.id));
  }

  /** Pauses or resumes a queued video, and says so. */
  function togglePauseItem(item: QueueItem, title: string): void {
    if (item.id === playingId && onTogglePlayback) {
      onTogglePlayback();
      announcement = `${title} ${playbackPaused ? 'resumed' : 'paused'}`;
      return;
    }
    if (!onTogglePause) {
      return;
    }
    if (!canPause(shownDownload(item))) {
      announcement = PLAYING_PAUSE_TITLE;
      return;
    }
    onTogglePause([item.id], !item.paused);
    announcement = `${title} ${item.paused ? 'resumed' : 'paused'}`;
  }

  /** Pauses or resumes all of a torrent's queued videos, and says so. */
  function togglePauseGroup(group: QueueGroup): void {
    if (!onTogglePause) {
      return;
    }
    const paused = groupPaused(group);
    onTogglePause(
      group.items.map((i) => i.id),
      !paused,
    );
    announcement = `${groupName(group)} ${paused ? 'resumed' : 'paused'}`;
  }

  /** The Tab index of a button inside a tree item: reachable only while the item has focus. */
  function innerTab(id: string): number {
    return focusId === id ? 0 : -1;
  }

  function rowTitle(row: TreeRow): string {
    return row.file ? fileName(row.file.path) : (row.item?.title ?? '');
  }

  function rowSize(row: TreeRow): number | null {
    return row.file?.length ?? row.item?.sizeBytes ?? null;
  }

  function play(node: Node): void {
    if (node.kind !== 'row' || isBusy(node.group.key)) {
      return;
    }
    if (node.row.item) {
      onPlay(node.row.item);
    } else if (node.row.file) {
      onPlayFile(node.group.key, node.row.file);
    }
  }

  function toggleRow(node: Node): void {
    if (node.kind !== 'row') {
      return;
    }
    const { row, group } = node;
    // Unticking removes the item (with Undo); a torrent's file stays listed, to tick again.
    if (row.item) {
      onRemove(row.item.id);
      announcement = `${rowTitle(row)} removed from the queue`;
    } else if (row.file) {
      onToggleFile(group.key, row.file, true);
      announcement = `${rowTitle(row)} queued`;
    }
  }

  function setCollapsed(key: string, value: boolean): void {
    const now = collapsed.includes(key);
    if (now !== value) {
      collapsed = value ? [...collapsed, key] : collapsed.filter((k) => k !== key);
    }
  }

  function moveGroupTo(group: QueueGroup, index: number): void {
    const target = Math.min(Math.max(index, 0), groups.length - 1);
    if (target === groups.indexOf(group)) {
      return;
    }
    onMoveGroup(group.key, target);
    announcement = `Moved ${groupName(group)} to position ${String(target + 1)} of ${String(groups.length)}`;
  }

  async function focusNode(id: string | null | undefined): Promise<void> {
    if (id === null || id === undefined) {
      return;
    }
    focusId = id;
    await tick();
    root?.querySelector<HTMLElement>(`[data-node="${CSS.escape(id)}"]`)?.focus();
  }

  function onTreeKey(event: KeyboardEvent): void {
    const at = nodes.findIndex((n) => n.id === (focusId ?? tabId));
    const node = nodes[at];
    if (!node || event.ctrlKey || event.metaKey) {
      return;
    }
    const handled = (): void => {
      event.preventDefault();
      event.stopPropagation();
    };
    // A button or tick inside the item: Enter and Space activate it, Escape returns to the item.
    const target = event.target;
    if (target instanceof HTMLElement && target.getAttribute('role') !== 'treeitem') {
      if (event.key === 'Escape') {
        handled();
        void focusNode(node.id);
        return;
      }
      if (event.key === 'Enter' || event.key === ' ') {
        return;
      }
    }
    if (event.altKey) {
      if (event.key === 'ArrowUp' || event.key === 'ArrowDown') {
        handled();
        moveGroupTo(node.group, node.g + (event.key === 'ArrowUp' ? -1 : 1));
        void focusNode(node.kind === 'group' ? node.id : node.id);
      }
      return;
    }
    switch (event.key) {
      case 'ArrowDown':
        handled();
        void focusNode(nodes[at + 1]?.id);
        break;
      case 'ArrowUp':
        handled();
        void focusNode(nodes[at - 1]?.id);
        break;
      case 'Home':
        handled();
        void focusNode(nodes[0]?.id);
        break;
      case 'End':
        handled();
        void focusNode(nodes.at(-1)?.id);
        break;
      case 'ArrowRight':
        if (node.kind === 'group') {
          handled();
          if (collapsed.includes(node.group.key)) {
            setCollapsed(node.group.key, false);
          } else {
            void focusNode(nodes[at + 1]?.id);
          }
        }
        break;
      case 'ArrowLeft':
        if (node.kind === 'group' && !collapsed.includes(node.group.key)) {
          handled();
          setCollapsed(node.group.key, true);
        } else if (node.kind === 'row' && node.parent !== null) {
          handled();
          void focusNode(node.parent);
        }
        break;
      case 'Enter':
        handled();
        if (node.kind === 'group') {
          setCollapsed(node.group.key, !collapsed.includes(node.group.key));
        } else {
          play(node);
        }
        break;
      case ' ':
        if (node.kind === 'row') {
          handled();
          toggleRow(node);
        }
        break;
      case 'Delete':
        handled();
        if (node.kind === 'group') {
          removeWholeGroup(node.group);
        } else if (node.row.item) {
          onRemove(node.row.item.id);
        }
        break;
      case 'p':
      case 'P':
        if (onTogglePause && !event.shiftKey) {
          handled();
          if (node.kind === 'group') {
            togglePauseGroup(node.group);
          } else if (node.row.item) {
            togglePauseItem(node.row.item, rowTitle(node.row));
          }
        }
        break;
      default:
        break;
    }
  }

  function dragOver(event: DragEvent, g: number): void {
    if (dragKey === null) {
      return;
    }
    event.preventDefault();
    if (event.dataTransfer) {
      event.dataTransfer.dropEffect = 'move';
    }
    const box = (event.currentTarget as HTMLElement).getBoundingClientRect();
    dropIndex = event.clientY < box.top + box.height / 2 ? g : g + 1;
  }

  function drop(event: DragEvent): void {
    event.preventDefault();
    const group = groups.find((x) => x.key === dragKey);
    const target = dropIndex;
    dragEnd();
    if (group && target !== null) {
      const from = groups.indexOf(group);
      moveGroupTo(group, target > from ? target - 1 : target);
    }
  }

  function dragEnd(): void {
    dragKey = null;
    dropIndex = null;
  }

  function dragProps(group: QueueGroup, g: number): Record<string, unknown> {
    return {
      draggable: 'true',
      ondragstart: (event: DragEvent) => {
        dragKey = group.key;
        event.dataTransfer?.setData('text/plain', groupName(group));
        if (event.dataTransfer) {
          event.dataTransfer.effectAllowed = 'move';
        }
      },
      ondragover: (event: DragEvent) => {
        dragOver(event, g);
      },
      ondrop: drop,
      ondragend: dragEnd,
    };
  }
</script>

{#snippet rowContent(node: Extract<Node, { kind: 'row' }>)}
  {@const row = node.row}
  {@const active = isPlaying(node.group, row)}
  {@const label = rowLabel(node.group, row)}
  {@const size = rowSize(row)}
  {@const download = row.item ? shownDownload(row.item) : undefined}
  {@const status = download ? describeDownload(download, maxActive) : null}
  {@const progress = download ? progressOf(download) : null}
  {@const tone = download ? toneOf(download) : 'waiting'}
  <span
    class="line"
    class:with-pause={row.item !== null && onTogglePause !== undefined}
    class:with-remove={row.item !== null}
  >
    <input
      type="checkbox"
      class="tick"
      tabindex={innerTab(node.id)}
      checked={row.item !== null}
      disabled={row.file === null && row.item === null}
      aria-label={active && row.item !== null
        ? `Stop and remove ${rowTitle(row)} from the queue`
        : row.item === null
          ? `Queue ${rowTitle(row)}`
          : `Remove ${rowTitle(row)} from the queue`}
      title={active && row.item !== null
        ? 'Stop playback and remove from the queue (Undo restores the item)'
        : row.item === null
          ? 'Add to the queue'
          : 'In the queue (untick to remove it; Undo puts it back)'}
      onclick={(event: MouseEvent) => {
        event.stopPropagation();
      }}
      onchange={() => {
        toggleRow(node);
      }}
    />
    <button
      type="button"
      class="main"
      tabindex="-1"
      disabled={isBusy(node.group.key)}
      title={`Play now: ${rowTitle(row)}`}
      onclick={(event: MouseEvent) => {
        event.stopPropagation();
        focusId = node.id;
        play(node);
      }}
    >
      <span class="title">
        {#if isPlaying(node.group, row)}<span class="eq" aria-hidden="true">▶</span>{/if}
        {rowTitle(row)}
      </span>
      {#if label || size !== null}
        <span class="meta">
          {#if label}<span
              class="state"
              title={label.startsWith('Next') ? 'Shift+N plays the next video' : undefined}
              >{label}</span
            >{/if}
          {#if label && size !== null}·{/if}
          {size === null ? '' : formatBytes(size)}
        </span>
      {/if}
      {#if status}
        <span class="status {tone}">{status}</span>
      {:else if row.item !== null}
        <!-- Keeps the row's height while the first report is on its way. -->
        <span class="status" aria-hidden="true">&nbsp;</span>
      {/if}
      {#if download}
        <span
          class="bar"
          title={`${barTitle(download, maxActive)} · Green sections are downloaded`}
          role="progressbar"
          aria-label="Downloaded"
          aria-valuemin={0}
          aria-valuemax={100}
          aria-valuenow={Math.floor((progress ?? 0) * 100)}
          ><DownloadedChunks cells={download.pieceMap} /></span
        >
      {:else if row.item !== null}
        <span class="bar placeholder" aria-hidden="true"></span>
      {/if}
    </button>
    {#if active && onTogglePlayback}
      <button
        type="button"
        class="icon pause"
        tabindex={innerTab(node.id)}
        aria-label={`${playbackPaused ? 'Resume' : 'Pause'} playback ${rowTitle(row)}`}
        title={playbackPaused ? 'Resume playback (P)' : 'Pause playback (P)'}
        onclick={(event: MouseEvent) => {
          event.stopPropagation();
          onTogglePlayback();
        }}
      >
        <svg viewBox="0 0 24 24" aria-hidden="true"
          ><path d={playbackPaused ? 'M8 5.5v13l10-6.5z' : 'M8 5v14M16 5v14'} /></svg
        >
      </button>
    {:else if row.item !== null && onTogglePause}
      {@const item = row.item}
      <!-- The only queued video of its torrent pauses the torrent; in a torrent with others
           queued, the torrent keeps sharing for them. -->
      {@const what = node.group.items.length === 1 ? 'downloading and uploading' : 'downloading'}
      <!-- A toggle: one fixed label, its state in aria-pressed. -->
      <button
        type="button"
        class="icon pause"
        tabindex={innerTab(node.id)}
        disabled={!canPause(download)}
        aria-label={`Pause ${what} ${rowTitle(row)}`}
        aria-pressed={item.paused}
        title={!canPause(download)
          ? PLAYING_PAUSE_TITLE
          : item.paused
            ? `Paused by you: press to resume ${what} (P)`
            : `Pause ${what} until you resume; playing it still works (P)`}
        onclick={(event: MouseEvent) => {
          event.stopPropagation();
          togglePauseItem(item, rowTitle(row));
        }}
      >
        <svg viewBox="0 0 24 24" aria-hidden="true"><path d={PAUSE_TRANSFERS} /></svg>
      </button>
    {/if}
    {#if active && onStopPlayback}
      <button
        type="button"
        class="icon stop"
        tabindex={innerTab(node.id)}
        aria-label={`Stop playback ${rowTitle(row)}`}
        title="Stop playback; keep in the queue"
        onclick={(event: MouseEvent) => {
          event.stopPropagation();
          onStopPlayback();
        }}
      >
        <svg viewBox="0 0 24 24" aria-hidden="true"><path d="M6 6h12v12H6z" /></svg>
      </button>
    {:else}
      <button
        type="button"
        class="icon play"
        tabindex={innerTab(node.id)}
        disabled={isBusy(node.group.key)}
        aria-label={`Play ${rowTitle(row)} now`}
        title="Play now"
        onclick={(event: MouseEvent) => {
          event.stopPropagation();
          play(node);
        }}
      >
        <svg viewBox="0 0 24 24" aria-hidden="true"><path d="M8 5.5v13l10-6.5z" /></svg>
      </button>
    {/if}
    {#if row.item !== null}
      {@const id = row.item.id}
      <button
        type="button"
        class="icon remove"
        tabindex={innerTab(node.id)}
        aria-label={active
          ? `Stop and remove ${rowTitle(row)} from the queue`
          : `Remove ${rowTitle(row)} from the queue`}
        title={active ? 'Stop playback and remove from the queue' : 'Remove from the queue'}
        onclick={(event: MouseEvent) => {
          event.stopPropagation();
          onRemove(id);
        }}
      >
        <svg viewBox="0 0 24 24" aria-hidden="true"><path d="M7.5 7.5l9 9M16.5 7.5l-9 9" /></svg>
      </button>
    {/if}
  </span>
{/snippet}

<ul
  bind:this={root}
  class="tree"
  class:compact
  role="tree"
  aria-label="Queue"
  onkeydown={onTreeKey}
>
  {#each groups as group, g (group.key)}
    {#if chrome(group)}
      {@const id = `g:${group.key}`}
      {@const open = !collapsed.includes(group.key)}
      {@const rows = nodes.filter((n) => n.kind === 'row' && n.parent === id)}
      <li
        class="node"
        role="treeitem"
        aria-expanded={open}
        aria-selected={focusId === id}
        tabindex={tabId === id ? 0 : -1}
        data-node={id}
        onfocus={(event: FocusEvent) => {
          if (event.target === event.currentTarget) {
            focusId = id;
          }
        }}
      >
        <div
          class="torrent"
          class:dragging={dragKey === group.key}
          class:drop-before={dragKey !== null && dropIndex === g}
          class:drop-after={dragKey !== null && dropIndex === g + 1 && g === groups.length - 1}
          role="presentation"
          {...dragProps(group, g)}
        >
          <button
            type="button"
            class="caret"
            tabindex="-1"
            aria-label={open ? 'Collapse' : 'Expand'}
            onclick={() => {
              focusId = id;
              setCollapsed(group.key, open);
            }}
          >
            <svg class:open viewBox="0 0 24 24" aria-hidden="true"><path d="M9.5 6l6 6-6 6" /></svg>
          </button>
          <span class="torrent-text" title={groupName(group)}>
            <span class="torrent-name">{groupName(group)}</span>
            <span class="meta">{groupMeta(group)}</span>
          </span>
          <span class="actions">
            {#if videos(group) === null && onLoadFiles}
              <button
                type="button"
                class="text"
                tabindex={innerTab(id)}
                disabled={isBusy(group.key)}
                title="Show all of its videos"
                onclick={() => {
                  onLoadFiles(group.key);
                }}>{isBusy(group.key) ? 'Loading…' : 'All videos'}</button
              >
            {:else if onChooseFiles}
              <button
                type="button"
                class="icon"
                tabindex={innerTab(id)}
                aria-label={`Choose files of ${groupName(group)}`}
                title="Choose files…"
                onclick={() => {
                  onChooseFiles(group.key);
                }}
              >
                <svg viewBox="0 0 24 24" aria-hidden="true"
                  ><path d="M5 7h14M5 12h14M5 17h9M17 15l2 2 3-4" /></svg
                >
              </button>
            {/if}
            {#if onTogglePause}
              {@const paused = groupPaused(group)}
              <!-- A toggle: one fixed label, its state in aria-pressed. -->
              <button
                type="button"
                class="icon pause"
                tabindex={innerTab(id)}
                aria-label={`Pause downloading and uploading ${groupName(group)}`}
                aria-pressed={paused}
                title={paused
                  ? 'Resume background downloading and uploading; current playback continues (P)'
                  : 'Pause background downloading and uploading; current playback continues (P)'}
                onclick={() => {
                  togglePauseGroup(group);
                }}
              >
                <svg viewBox="0 0 24 24" aria-hidden="true"><path d={PAUSE_TRANSFERS} /></svg>
              </button>
            {/if}
            {#if onRemoveGroup}
              <button
                type="button"
                class="icon group-remove"
                tabindex={innerTab(id)}
                aria-label={`${group.items.some((item) => item.id === playingId) ? 'Stop playback and remove' : 'Remove'} all queued videos of ${groupName(group)} from the queue`}
                title={`Remove all ${String(group.items.length)} queued videos${group.items.some((item) => item.id === playingId) ? ' and stop playback' : ''} (Undo puts them back)`}
                onclick={() => {
                  removeWholeGroup(group);
                }}
              >
                <svg viewBox="0 0 24 24" aria-hidden="true"
                  ><path d="M5 7h14M9 7V5h6v2M8 10v8M12 10v8M16 10v8M7 7l1 13h8l1-13" /></svg
                >
                <span class="remove-count" aria-hidden="true">{group.items.length}</span>
              </button>
            {/if}
          </span>
        </div>
        {#if open}
          <ul class="branch" role="group">
            {#each rows as node (node.id)}
              {#if node.kind === 'row'}
                <li
                  class="leaf"
                  class:playing={isPlaying(group, node.row)}
                  class:unqueued={node.row.item === null}
                  class:played={rowLabel(group, node.row) === 'Played'}
                  role="treeitem"
                  aria-selected={focusId === node.id}
                  aria-current={isPlaying(group, node.row) ? 'true' : undefined}
                  tabindex={tabId === node.id ? 0 : -1}
                  data-node={node.id}
                  onfocus={() => {
                    focusId = node.id;
                  }}
                >
                  <!-- eslint-disable-next-line @typescript-eslint/no-confusing-void-expression -- a local snippet is typed as returning void -->
                  {@render rowContent(node)}
                </li>
              {/if}
            {/each}
          </ul>
        {/if}
      </li>
    {:else}
      {#each nodes.filter((n) => n.kind === 'row' && n.group === group) as node (node.id)}
        {#if node.kind === 'row'}
          <li
            class="leaf plain"
            class:playing={isPlaying(group, node.row)}
            class:played={rowLabel(group, node.row) === 'Played'}
            class:dragging={dragKey === group.key}
            class:drop-before={dragKey !== null && dropIndex === g}
            class:drop-after={dragKey !== null && dropIndex === g + 1 && g === groups.length - 1}
            role="treeitem"
            aria-selected={focusId === node.id}
            aria-current={isPlaying(group, node.row) ? 'true' : undefined}
            tabindex={tabId === node.id ? 0 : -1}
            data-node={node.id}
            onfocus={() => {
              focusId = node.id;
            }}
            {...dragProps(group, g)}
          >
            <!-- eslint-disable-next-line @typescript-eslint/no-confusing-void-expression -- a local snippet is typed as returning void -->
            {@render rowContent(node)}
          </li>
        {/if}
      {/each}
    {/if}
  {/each}
</ul>
<p class="sr-only" aria-live="polite">{announcement}</p>

<style>
  .tree,
  .branch {
    display: grid;
    gap: 0.1rem;
    margin: 0;
    padding: 0;
    list-style: none;
  }

  .node:focus-visible > .torrent,
  .leaf:focus-visible {
    outline: 2px solid var(--accent);
    outline-offset: -2px;
  }

  .node:focus,
  .leaf:focus {
    outline: none;
  }

  .torrent {
    position: relative;
    display: grid;
    grid-template-columns: auto minmax(0, 1fr) auto;
    align-items: center;
    gap: 0.25rem;
    padding: 0.2rem 0.3rem 0.2rem 0.1rem;
    border-radius: 0.5rem;
    cursor: grab;
  }

  .torrent:hover,
  .leaf:hover {
    background: rgb(255 255 255 / 3%);
  }

  /* The tree's guide line, and a short connector to each video (├── / └──). */
  .branch {
    margin-left: 0.8rem;
    padding-left: 0.25rem;
    border-left: 1px solid rgb(255 255 255 / 9%);
  }

  .branch > .leaf {
    position: relative;
    padding-left: 0.55rem;
  }

  .branch > .leaf::before {
    position: absolute;
    top: 50%;
    left: -0.25rem;
    width: 0.55rem;
    border-top: 1px solid rgb(255 255 255 / 9%);
    content: '';
  }

  .branch > .leaf:last-child::after {
    position: absolute;
    top: calc(50% + 1px);
    bottom: -0.1rem;
    left: calc(-0.25rem - 1px);
    width: 1px;
    background: #0f0f14;
    content: '';
  }

  .leaf {
    border-radius: 0.45rem;
  }

  .leaf.plain {
    cursor: grab;
  }

  .leaf.playing {
    background: rgb(242 180 90 / 8%);
  }

  .leaf.unqueued .title {
    color: var(--text-muted);
  }

  .leaf.played .title {
    color: var(--text-muted);
  }

  .line {
    display: grid;
    grid-template-columns: auto minmax(0, 1fr) auto;
    align-items: center;
    gap: 0.3rem;
    padding-right: 0.2rem;
  }

  .line.with-remove {
    grid-template-columns: auto minmax(0, 1fr) auto auto;
  }

  .line.with-pause.with-remove {
    grid-template-columns: auto minmax(0, 1fr) auto auto auto;
  }

  .line.with-pause:not(.with-remove) {
    grid-template-columns: auto minmax(0, 1fr) auto auto;
  }

  .remove:hover:not(:disabled) {
    color: #e8907a;
  }

  .status {
    overflow: hidden;
    color: var(--text-muted);
    font-size: 0.72rem;
    text-overflow: ellipsis;
    white-space: nowrap;
    font-variant-numeric: tabular-nums;
  }

  /* Green: playable. Yellow: downloading from peers, not playable yet. Grey: waiting. */
  .status.playable {
    color: #9fd3a8;
  }

  .status.downloading {
    color: #e8c56a;
  }

  .bar {
    position: relative;
    display: block;
    height: 3px;
    margin-top: 0.15rem;
    overflow: hidden;
    border-radius: 1px;
    background: rgb(255 255 255 / 18%);
  }

  .pause[aria-pressed='true'] {
    color: var(--accent);
    background: rgb(242 180 90 / 12%);
  }

  .bar.placeholder {
    visibility: hidden;
  }

  .tick {
    margin: 0 0 0 0.3rem;
    accent-color: var(--accent);
    cursor: pointer;
  }

  .dragging {
    opacity: 0.4;
  }

  .drop-before::before,
  .drop-after::after {
    position: absolute;
    right: 0.4rem;
    left: 0.4rem;
    height: 2px;
    border-radius: 2px;
    background: var(--accent);
    content: '';
  }

  .drop-before::before {
    top: -0.1rem;
  }

  .drop-after::after {
    bottom: -0.1rem;
  }

  button {
    border: none;
    background: none;
    color: inherit;
    font: inherit;
    cursor: pointer;
  }

  button:disabled {
    cursor: default;
  }

  .caret {
    display: inline-flex;
    padding: 0.3rem 0.15rem;
    color: var(--text-muted);
  }

  .caret svg {
    width: 0.8rem;
    height: 0.8rem;
    transition: transform 0.15s ease;
  }

  .caret svg.open {
    transform: rotate(90deg);
  }

  .torrent-text {
    display: grid;
    min-width: 0;
    padding: 0.2rem 0;
  }

  .main {
    display: grid;
    gap: 0.05rem;
    min-width: 0;
    padding: 0.3rem 0.2rem;
    border-radius: 0.4rem;
    text-align: left;
  }

  .title,
  .torrent-name {
    overflow: hidden;
    color: var(--text);
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  .torrent-name {
    font-weight: 500;
  }

  .eq {
    margin-right: 0.3rem;
    color: var(--accent);
    font-size: 0.7rem;
  }

  .meta {
    overflow: hidden;
    color: var(--text-muted);
    font-size: 0.76rem;
    text-overflow: ellipsis;
    white-space: nowrap;
    font-variant-numeric: tabular-nums;
  }

  .state {
    color: var(--accent);
  }

  .leaf.played .state {
    color: var(--text-muted);
  }

  .actions {
    display: flex;
    align-items: center;
    gap: 0.1rem;
  }

  .icon {
    display: inline-flex;
    padding: 0.28rem;
    border-radius: 0.35rem;
    color: var(--text-muted);
  }

  .group-remove {
    margin-left: 0.2rem;
    padding-left: 0.45rem;
    border-left: 1px solid rgb(255 255 255 / 12%);
  }

  .group-remove:hover:not(:disabled) {
    color: #e8907a;
  }

  .remove-count {
    min-width: 1.1rem;
    margin-left: 0.15rem;
    padding: 0 0.2rem;
    border-radius: 0.55rem;
    background: rgb(255 255 255 / 8%);
    font-size: 0.65rem;
    line-height: 1.1rem;
    text-align: center;
    font-variant-numeric: tabular-nums;
  }

  .text {
    padding: 0.2rem 0.4rem;
    border-radius: 0.35rem;
    color: var(--accent);
    font-size: 0.74rem;
    white-space: nowrap;
  }

  .icon:hover:not(:disabled),
  .text:hover:not(:disabled) {
    color: var(--text);
    background: rgb(255 255 255 / 6%);
  }

  .play:hover:not(:disabled) {
    color: var(--accent);
  }

  svg {
    width: 0.9rem;
    height: 0.9rem;
    fill: none;
    stroke: currentcolor;
    stroke-width: 1.8;
    stroke-linecap: round;
    stroke-linejoin: round;
  }

  /* Beside the player, the row buttons appear on hover or focus, unless there is no hover. */
  .compact .actions,
  .compact .play {
    opacity: 0;
  }

  .compact .torrent:hover .actions,
  .compact .node:focus-within > .torrent .actions,
  .compact .leaf:hover .play,
  .compact .leaf:focus-within .play,
  .compact .leaf:focus .play {
    opacity: 1;
  }

  @media (hover: none) {
    .compact .actions,
    .compact .play {
      opacity: 1;
    }
  }

  .compact .title,
  .compact .torrent-name {
    font-size: 0.86rem;
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
