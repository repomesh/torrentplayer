<!-- SPDX-License-Identifier: MIT OR Apache-2.0 -->
<script lang="ts">
  /*
   * The queue as a tree: each torrent with several videos is a node (name, files, size, how
   * many are ready) whose videos sit beneath it in lecture order, queued or not. A tick adds a
   * video to the queue or removes it; the name plays it now. A torrent with one video is a
   * plain row. Torrents move as a whole: drag the torrent row, or Alt+↑/↓ on it.
   *
   * Keyboard (ARIA treeview): ↑/↓ move, → expands or enters a torrent, ← collapses or goes
   * up, Enter plays, Space ticks, Delete removes, Alt+↑/↓ moves a torrent.
   *
   * Queued videos download in the background: each row shows its speed, peers, progress, and
   * time left, and can be paused (a torrent row pauses all of its queued videos).
   */
  import { tick } from 'svelte';
  import type { FileSummary } from '../bindings/FileSummary';
  import type { ReadinessState } from '../bindings/ReadinessState';
  import { fileName, isPlayable, mediaNoun } from '../files';
  import {
    canPause,
    describeDownload,
    isMoving,
    progressOf,
    toneOf,
    type ItemDownload,
  } from '../downloads';
  import { formatBytes, formatRate } from '../format';
  import {
    currentIndex,
    groupQueue,
    groupTree,
    type Queue,
    type QueueGroup,
    type QueueItem,
    type TreeRow,
  } from '../queue';

  interface Props {
    queue: Queue;
    /** The queue item playing now, if the video playing came from the queue. */
    playingId: string | null;
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
    /** The item downloading clearly fastest, if any (a hint to watch it now). */
    fastest?: string | null;
    /** Pauses or resumes background downloading of items. */
    onTogglePause?: ((ids: readonly string[], paused: boolean) => void) | undefined;
    /**
     * Skips (unticks) or restores items: skipped items stay listed but are not played next or
     * downloaded. Without it, unticking removes the item.
     */
    onToggleSkip?: ((ids: readonly string[], skipped: boolean) => void) | undefined;
  }

  let {
    queue,
    playingId,
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
    fastest = null,
    onTogglePause,
    onToggleSkip,
  }: Props = $props();

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

  const cursor = $derived(currentIndex(queue));
  const groups = $derived(groupQueue(queue));
  let collapsed = $state.raw<readonly string[]>([]);
  let focusId = $state<string | null>(null);
  let dragKey = $state<string | null>(null);
  let dropIndex = $state<number | null>(null);
  let announcement = $state('');
  let root: HTMLUListElement | undefined = $state();

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

  function rowLabel(group: QueueGroup, row: TreeRow): string | null {
    if (isPlaying(group, row)) {
      return 'Playing';
    }
    if (isBusy(group.key)) {
      return 'Loading…';
    }
    const item = row.item;
    if (item === null) {
      return null;
    }
    if (item.skipped) {
      return 'Skipped';
    }
    const index = queue.items.indexOf(item);
    if (index === cursor + 1) {
      if (nextStatus !== null) {
        return `Up next · ${nextStatus}`;
      }
      return isReady(item) ? 'Up next · Ready' : 'Up next';
    }
    if (index <= cursor) {
      return 'Played';
    }
    return isReady(item) ? 'Ready' : null;
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

  /**
   * An item's download as shown: pausing or resuming shows at once, before the backend's next
   * report (about a second later).
   */
  function withPause(item: QueueItem, d: ItemDownload | undefined): ItemDownload | undefined {
    // Skipping and including again show at once too.
    if (d !== undefined && item.skipped && d.state !== 'skipped' && d.state !== 'watching') {
      return { ...d, state: 'skipped', downloadBps: 0, etaSecs: null };
    }
    if (d !== undefined && !item.skipped && d.state === 'skipped') {
      return { ...d, state: 'queued' };
    }
    if (d === undefined || !canPause(d)) {
      return d;
    }
    if (item.paused && d.state !== 'paused') {
      return { ...d, state: 'paused', downloadBps: 0, etaSecs: null };
    }
    if (!item.paused && d.state === 'paused') {
      return { ...d, state: 'queued' };
    }
    return d;
  }

  /** Whether every queued video of a torrent is paused. */
  function groupPaused(group: QueueGroup): boolean {
    return group.items.every((i) => i.paused);
  }

  /** Bulk removal is deliberately different from a row's × and needs confirmation. */
  function removeWholeGroup(group: QueueGroup): void {
    if (!onRemoveGroup) {
      return;
    }
    const count = group.items.length;
    const noun = count === 1 ? 'file' : 'files';
    if (
      globalThis.confirm(
        `Remove all ${String(count)} ${noun} of “${groupName(group)}” from the queue?`,
      )
    ) {
      onRemoveGroup(group.key);
    }
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
    if (row.item && onToggleSkip) {
      // Unticking keeps the item, skipped; ticking brings it back.
      onToggleSkip([row.item.id], !row.item.skipped);
      announcement = `${rowTitle(row)} ${row.item.skipped ? 'included again' : 'skipped'}`;
    } else if (row.file) {
      onToggleFile(group.key, row.file, row.item === null);
      announcement = `${rowTitle(row)} ${row.item === null ? 'queued' : 'removed from the queue'}`;
    } else if (row.item) {
      onRemove(row.item.id);
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
  {@const label = rowLabel(node.group, row)}
  {@const size = rowSize(row)}
  {@const download = row.item ? withPause(row.item, downloads.get(row.item.id)) : undefined}
  {@const status = download ? describeDownload(download) : null}
  {@const progress = download ? progressOf(download) : null}
  {@const tone = download ? toneOf(download) : 'waiting'}
  {@const skipping = row.item !== null && onToggleSkip !== undefined}
  <span
    class="line"
    class:with-pause={row.item !== null && onTogglePause !== undefined}
    class:with-remove={row.item !== null}
  >
    <input
      type="checkbox"
      class="tick"
      tabindex="-1"
      checked={row.item !== null && !row.item.skipped}
      disabled={row.file === null && row.item === null}
      aria-label={row.item === null
        ? `Queue ${rowTitle(row)}`
        : !skipping
          ? `Remove ${rowTitle(row)} from the queue`
          : row.item.skipped
            ? `Include ${rowTitle(row)} again`
            : `Skip ${rowTitle(row)}`}
      title={row.item === null
        ? 'Add to the queue'
        : !skipping
          ? 'In the queue (untick to remove)'
          : row.item.skipped
            ? 'Skipped: not played next or downloaded (tick to include it again)'
            : 'In the queue (untick to skip it; × removes it)'}
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
        {#if label === 'Playing'}<span class="eq" aria-hidden="true">▶</span>{/if}
        {rowTitle(row)}
      </span>
      {#if label || size !== null}
        <span class="meta">
          {#if label}<span class="state">{label}</span>{/if}
          {#if label && size !== null}·{/if}
          {size === null ? '' : formatBytes(size)}
          {#if row.item !== null && row.item.id === fastest}
            <span class="fastest" title="Downloading fastest of the queue: it plays smoothest now"
              >Fastest</span
            >
          {/if}
        </span>
      {/if}
      {#if status}
        <span class="status {tone}">{status}</span>
      {/if}
      {#if download}
        <span
          class="bar {tone}"
          title={tone === 'playable'
            ? 'Ready to play'
            : tone === 'downloading'
              ? 'Downloading, not ready to play yet'
              : 'Waiting for peers'}
          role="progressbar"
          aria-label="Downloaded"
          aria-valuemin={0}
          aria-valuemax={100}
          aria-valuenow={Math.floor((progress ?? 0) * 100)}
          ><span style:width={`${String((progress ?? 0) * 100)}%`}></span></span
        >
      {/if}
    </button>
    {#if row.item !== null && onTogglePause}
      {@const item = row.item}
      <button
        type="button"
        class="icon pause"
        tabindex="-1"
        disabled={!canPause(download)}
        aria-label={item.paused
          ? `Resume downloading ${rowTitle(row)}`
          : `Pause downloading ${rowTitle(row)}`}
        aria-pressed={item.paused}
        title={item.paused ? 'Resume downloading' : 'Pause downloading (playing it still works)'}
        onclick={(event: MouseEvent) => {
          event.stopPropagation();
          onTogglePause([item.id], !item.paused);
          announcement = `${rowTitle(row)} ${item.paused ? 'resumed' : 'paused'}`;
        }}
      >
        {#if item.paused}
          <svg viewBox="0 0 24 24" aria-hidden="true"
            ><path d="M12 5v10M7.5 10.5 12 15l4.5-4.5M6 19h12" /></svg
          >
        {:else}
          <svg viewBox="0 0 24 24" aria-hidden="true"><path d="M9 6.5v11M15 6.5v11" /></svg>
        {/if}
      </button>
    {/if}
    <button
      type="button"
      class="icon play"
      tabindex="-1"
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
    {#if row.item !== null}
      {@const id = row.item.id}
      <button
        type="button"
        class="icon remove"
        tabindex="-1"
        aria-label={`Remove ${rowTitle(row)} from the queue`}
        title="Remove from the queue"
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
                tabindex="-1"
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
                tabindex="-1"
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
              <button
                type="button"
                class="icon"
                tabindex="-1"
                aria-label={paused
                  ? `Resume downloading ${groupName(group)}`
                  : `Pause downloading ${groupName(group)}`}
                aria-pressed={paused}
                title={paused ? 'Resume downloading its videos' : 'Pause downloading its videos'}
                onclick={() => {
                  onTogglePause(
                    group.items.map((i) => i.id),
                    !paused,
                  );
                }}
              >
                {#if paused}
                  <svg viewBox="0 0 24 24" aria-hidden="true"
                    ><path d="M12 5v10M7.5 10.5 12 15l4.5-4.5M6 19h12" /></svg
                  >
                {:else}
                  <svg viewBox="0 0 24 24" aria-hidden="true"><path d="M9 6.5v11M15 6.5v11" /></svg>
                {/if}
              </button>
            {/if}
            {#if onRemoveGroup}
              <button
                type="button"
                class="icon group-remove"
                tabindex="-1"
                aria-label={`Remove all files of ${groupName(group)} from the queue`}
                title={`Remove all ${String(group.items.length)} queued files`}
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
                  class:skipped={node.row.item?.skipped === true}
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
            class:skipped={node.row.item?.skipped === true}
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

  /* Skipped: kept in the list, dimmed. */
  .leaf.skipped .main {
    opacity: 0.5;
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
    display: block;
    height: 2px;
    margin-top: 0.15rem;
    overflow: hidden;
    border-radius: 1px;
    background: rgb(255 255 255 / 8%);
  }

  .bar {
    height: 3px;
  }

  .bar > span {
    display: block;
    height: 100%;
    background: #6b6b78;
    transition: width 0.8s linear;
  }

  .bar.playable {
    background: rgb(111 191 127 / 22%);
  }

  .bar.playable > span {
    background: #6fbf7f;
  }

  .bar.downloading {
    background: rgb(232 197 106 / 18%);
  }

  .bar.downloading > span {
    background: #e8c56a;
  }

  .fastest {
    margin-left: 0.3rem;
    padding: 0 0.3rem;
    border-radius: 0.6rem;
    background: rgb(111 191 127 / 18%);
    color: #9fd3a8;
    font-size: 0.68rem;
  }

  .pause[aria-pressed='true'] {
    color: var(--accent);
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
