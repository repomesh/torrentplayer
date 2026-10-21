<!-- SPDX-License-Identifier: MIT OR Apache-2.0 -->
<script lang="ts">
  /*
   * The file explorer: the torrents in the queue and those opened this session on the left;
   * the chosen torrent's videos (lecture order) in the top half and its other files (by
   * folder) in the bottom half; a preview of the selected file at the side (below, on narrow
   * windows). Previews are safe by construction: pictures as data: URLs, text as text, a
   * video's first frame only when its start is downloaded, and metadata for everything else.
   */
  import { onMount } from 'svelte';
  import type { FileSummary } from '../bindings/FileSummary';
  import type { TorrentSummary } from '../bindings/TorrentSummary';
  import { defaultSelection, previewHint, splitFiles, type ExplorerTorrent } from '../explorer';
  import { fileName, folderOf, playableInWebview } from '../files';
  import { formatBytes } from '../format';
  import {
    errorMessage,
    previewFile,
    preloadStatus,
    type FilePreviewDto,
    type PreloadStatusDto,
  } from '../ipc';
  import { preloadActive, preloadLabel } from '../search';

  interface Props {
    /** Every torrent to explore (see `explorerTorrents`). */
    torrents: readonly ExplorerTorrent[];
    /** The torrent shown. */
    torrent: TorrentSummary;
    busy: boolean;
    /** Only the selected entry's metadata request disables its button. */
    isLoading: (entry: ExplorerTorrent) => boolean;
    starting: number | null;
    error: string | null;
    /** The file of this torrent playing now, if any. */
    playingIndex: number | null;
    isQueued: (file: FileSummary) => boolean;
    onSelectTorrent: (entry: ExplorerTorrent) => void;
    onPlay: (file: FileSummary) => void;
    onQueue: (file: FileSummary) => void;
    /** Queues every video not queued yet, in lecture order. */
    onQueueAll: (files: FileSummary[]) => void;
  }

  let {
    torrents,
    torrent,
    busy,
    isLoading,
    starting,
    error,
    playingIndex,
    isQueued,
    onSelectTorrent,
    onPlay,
    onQueue,
    onQueueAll,
  }: Props = $props();

  /** Wait before asking for a preview, so arrowing through files does not fetch each one. */
  const PREVIEW_DELAY_MS = 200;

  const split = $derived(splitFiles(torrent.files));
  const unqueued = $derived(split.videos.filter((f) => !isQueued(f)));
  let selected = $state<number | null>(null);
  const selectedFile = $derived(torrent.files.find((f) => f.index === selected) ?? null);
  let preview = $state.raw<FilePreviewDto | null>(null);
  let previewError = $state<string | null>(null);
  let previewing = $state(false);

  // A torrent shown anew keeps a still-present selection, else picks the file playing or the
  // first video.
  let shownTorrent: number | null = null;
  $effect.pre(() => {
    if (torrent.id !== shownTorrent) {
      shownTorrent = torrent.id;
      selected = defaultSelection(splitFiles(torrent.files), null, playingIndex);
    }
  });

  // The preview follows the selection; an answer for an older selection is dropped.
  let previewSeq = 0;
  $effect(() => {
    const id = torrent.id;
    const index = selected;
    const seq = ++previewSeq;
    preview = null;
    previewError = null;
    if (index === null) {
      previewing = false;
      return;
    }
    previewing = true;
    const timer = setTimeout(() => {
      previewFile(id, index)
        .then((p) => {
          if (seq === previewSeq) {
            preview = p;
          }
        })
        .catch((reason: unknown) => {
          if (seq === previewSeq) {
            previewError = errorMessage(reason);
          }
        })
        .finally(() => {
          if (seq === previewSeq) {
            previewing = false;
          }
        });
    }, PREVIEW_DELAY_MS);
    return () => {
      clearTimeout(timer);
    };
  });

  // A search may have preloaded the start of one of these files.
  let preload = $state<PreloadStatusDto | null>(null);
  const preloaded = $derived(preloadLabel(preload, torrent.infoHash));
  onMount(() => {
    let timer: ReturnType<typeof setTimeout> | undefined;
    let stopped = false;
    async function follow(): Promise<void> {
      try {
        const status = await preloadStatus();
        if (stopped) {
          return;
        }
        preload = status;
        if (preloadActive(status)) {
          timer = setTimeout(() => void follow(), 700);
        }
      } catch {
        preload = null;
      }
    }
    void follow();
    return () => {
      stopped = true;
      clearTimeout(timer);
    };
  });

  function kindLabel(file: FileSummary): string {
    switch (file.kind) {
      case 'video':
        return 'Video';
      case 'audio':
        return 'Audio';
      case 'subtitle':
        return 'Subtitles';
      case 'document':
        return 'Document';
      default:
        return previewHint(file) === 'image' ? 'Picture' : 'File';
    }
  }

  /** Arrow keys move the selection through the rows of one list. */
  function onListKey(event: KeyboardEvent, list: readonly FileSummary[]): void {
    if (event.key !== 'ArrowDown' && event.key !== 'ArrowUp') {
      return;
    }
    const at = list.findIndex((f) => f.index === selected);
    const next =
      list[Math.min(Math.max(at + (event.key === 'ArrowDown' ? 1 : -1), 0), list.length - 1)];
    if (next) {
      event.preventDefault();
      selected = next.index;
      const host = event.currentTarget as HTMLElement;
      host.querySelector<HTMLElement>(`[data-file="${String(next.index)}"]`)?.focus();
    }
  }
</script>

{#snippet fileRow(file: FileSummary, video: boolean)}
  <li class:selected={selected === file.index} class:playing={playingIndex === file.index}>
    <button
      type="button"
      class="pick"
      data-file={file.index}
      aria-pressed={selected === file.index}
      ondblclick={() => {
        if (video && starting === null && !busy) {
          onPlay(file);
        }
      }}
      onclick={() => {
        selected = file.index;
      }}
    >
      <span class="name">{fileName(file.path)}</span>
      <span class="meta">
        {#if playingIndex === file.index}<span class="tag">Playing ·</span>
        {:else if video && preloaded && preload?.infoHash === torrent.infoHash && preload.fileIndex === file.index}<span
            class="tag">{preloaded} ·</span
          >
        {/if}
        {starting === file.index ? 'Starting…' : formatBytes(file.length)}
        {#if video && !playableInWebview(file.path)}· may need the full player{/if}
      </span>
    </button>
    {#if video}
      {@const queued = isQueued(file)}
      <span class="actions">
        <button
          type="button"
          class="small"
          disabled={starting !== null || busy}
          aria-label={`Play ${fileName(file.path)}`}
          title="Play now"
          onclick={() => {
            onPlay(file);
          }}>▶</button
        >
        <button
          type="button"
          class="small"
          class:queued
          disabled={queued}
          aria-label={queued ? `${fileName(file.path)} is queued` : `Queue ${fileName(file.path)}`}
          title={queued ? 'In the queue' : 'Play after the current video'}
          onclick={() => {
            onQueue(file);
          }}>{queued ? 'Queued' : '+ Queue'}</button
        >
      </span>
    {/if}
  </li>
{/snippet}

<section class="explorer" aria-labelledby="explorer-heading">
  <nav class="torrents" aria-label="Torrents">
    <h2 class="sr-only" id="explorer-heading" tabindex="-1">Files</h2>
    <ul>
      {#each torrents as entry (entry.key)}
        <li>
          <button
            type="button"
            class:current={entry.torrent?.id === torrent.id}
            aria-current={entry.torrent?.id === torrent.id ? 'true' : undefined}
            disabled={isLoading(entry) && entry.torrent === null}
            title={entry.name}
            onclick={() => {
              onSelectTorrent(entry);
            }}
          >
            <span class="name">{entry.name}</span>
            <span class="meta">
              {isLoading(entry)
                ? 'Loading…'
                : entry.queued > 0
                  ? `${String(entry.queued)} queued`
                  : 'Opened'}
              {#if entry.torrent}· {formatBytes(entry.torrent.totalLength)}{/if}
            </span>
          </button>
        </li>
      {/each}
    </ul>
  </nav>

  <div class="main">
    <header>
      <h3 title={torrent.name}>{torrent.name}</h3>
      <p class="meta">
        {formatBytes(torrent.totalLength)} · {split.videos.length} video{split.videos.length === 1
          ? ''
          : 's'} · {split.others.reduce((n, g) => n + g.files.length, 0)} other files
        {#if split.videos.length > 1}
          ·
          <button
            type="button"
            class="link"
            disabled={unqueued.length === 0}
            onclick={() => {
              onQueueAll(unqueued);
            }}
            >{unqueued.length === 0
              ? 'All videos queued'
              : `Queue all ${String(unqueued.length)}`}</button
          >
        {/if}
      </p>
      {#if error}<p class="error" role="alert">{error}</p>{/if}
    </header>

    <div class="panes">
      <div class="lists">
        <section class="half" aria-label="Videos">
          <h4>Videos</h4>
          {#if split.videos.length === 0}
            <p class="empty">No videos in this torrent.</p>
          {:else}
            <!-- svelte-ignore a11y_no_noninteractive_element_interactions (↑/↓ move between the rows' buttons) -->
            <ul
              onkeydown={(event: KeyboardEvent) => {
                onListKey(event, split.videos);
              }}
            >
              {#each split.videos as file (file.index)}
                <!-- eslint-disable-next-line @typescript-eslint/no-confusing-void-expression -- a local snippet is typed as returning void -->
                {@render fileRow(file, true)}
              {/each}
            </ul>
          {/if}
        </section>
        <section class="half" aria-label="Other files">
          <h4>Other files</h4>
          {#if split.others.length === 0}
            <p class="empty">Nothing else.</p>
          {:else}
            {@const others = split.others.flatMap((g) => g.files)}
            <div
              role="presentation"
              onkeydown={(event: KeyboardEvent) => {
                onListKey(event, others);
              }}
            >
              {#each split.others as group (group.folder)}
                {#if group.folder}<p class="folder">{group.folder}</p>{/if}
                <ul>
                  {#each group.files as file (file.index)}
                    <!-- eslint-disable-next-line @typescript-eslint/no-confusing-void-expression -- a local snippet is typed as returning void -->
                    {@render fileRow(file, false)}
                  {/each}
                </ul>
              {/each}
            </div>
          {/if}
        </section>
      </div>

      <aside class="preview" aria-label="Preview" aria-live="polite">
        {#if selectedFile}
          <div class="view">
            {#if preview?.image}
              <img src={preview.image} alt={fileName(selectedFile.path)} />
            {:else if preview?.text !== undefined && preview.text !== null}
              <pre>{preview.text}</pre>
              {#if preview.truncated}<p class="note">Showing the first 64 KB.</p>{/if}
            {:else if preview?.video}
              <video src={preview.video} muted playsinline preload="metadata"></video>
            {:else if previewing && previewHint(selectedFile) !== 'none'}
              <p class="note">Loading preview…</p>
            {:else}
              <p class="note">
                {previewError ?? preview?.note ?? 'No preview for this kind of file.'}
              </p>
            {/if}
          </div>
          <dl>
            <dt>Name</dt>
            <dd>{fileName(selectedFile.path)}</dd>
            {#if folderOf(selectedFile.path)}
              <dt>Folder</dt>
              <dd>{folderOf(selectedFile.path)}</dd>
            {/if}
            <dt>Size</dt>
            <dd>{formatBytes(selectedFile.length)}</dd>
            <dt>Kind</dt>
            <dd>{kindLabel(selectedFile)}</dd>
          </dl>
        {:else}
          <p class="note">Select a file to preview it.</p>
        {/if}
      </aside>
    </div>
  </div>
</section>

<style>
  .explorer {
    display: grid;
    grid-template-columns: minmax(11rem, 15rem) minmax(0, 1fr);
    height: 100%;
    background: linear-gradient(180deg, #17150f 0%, var(--bg) 40%);
  }

  .torrents {
    overflow-y: auto;
    padding: 0.75rem 0.4rem;
    border-right: 1px solid rgb(255 255 255 / 5%);
  }

  ul {
    margin: 0;
    padding: 0;
    list-style: none;
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
    opacity: 0.5;
  }

  button:focus-visible {
    outline: 2px solid var(--accent);
    outline-offset: -2px;
  }

  .torrents button {
    display: grid;
    gap: 0.1rem;
    width: 100%;
    padding: 0.5rem 0.6rem;
    border-radius: 0.5rem;
    text-align: left;
  }

  .torrents button:hover:not(:disabled) {
    background: rgb(255 255 255 / 4%);
  }

  .torrents button.current {
    background: rgb(242 180 90 / 9%);
    color: var(--accent);
  }

  .name {
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  .meta {
    color: var(--text-muted);
    font-size: 0.78rem;
    font-variant-numeric: tabular-nums;
  }

  .main {
    display: flex;
    flex-direction: column;
    min-width: 0;
    min-height: 0;
    padding: 1rem clamp(0.75rem, 3vw, 2rem) 0.75rem;
  }

  header h3 {
    margin: 0 0 0.2rem;
    overflow: hidden;
    font-family: var(--font-display);
    font-size: clamp(1.25rem, 2.6vw, 1.8rem);
    font-weight: 400;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  header .meta {
    margin: 0 0 0.75rem;
    font-size: 0.85rem;
  }

  .error {
    color: #e8907a;
  }

  .link {
    padding: 0;
    color: var(--accent);
  }

  .link:hover:not(:disabled) {
    text-decoration: underline;
  }

  .panes {
    display: grid;
    flex: 1;
    grid-template-columns: minmax(0, 1fr) minmax(14rem, 22rem);
    gap: 1rem;
    min-height: 0;
  }

  .lists {
    display: grid;
    grid-template-rows: 1fr 1fr;
    gap: 0.75rem;
    min-height: 0;
  }

  .half {
    min-height: 0;
    overflow-y: auto;
    padding: 0.5rem;
    border: 1px solid #24221e;
    border-radius: 0.6rem;
    background: rgb(21 21 28 / 60%);
  }

  h4 {
    margin: 0 0 0.35rem 0.35rem;
    color: var(--text-muted);
    font-size: 0.72rem;
    font-weight: 600;
    letter-spacing: 0.06em;
    text-transform: uppercase;
  }

  .folder {
    margin: 0.5rem 0.35rem 0.15rem;
    color: var(--text-muted);
    font-size: 0.75rem;
    overflow-wrap: anywhere;
  }

  .empty {
    margin: 0.35rem;
    color: var(--text-muted);
    font-size: 0.85rem;
  }

  .half li {
    display: grid;
    grid-template-columns: minmax(0, 1fr) auto;
    align-items: center;
    border-radius: 0.45rem;
  }

  .half li:hover {
    background: rgb(255 255 255 / 3%);
  }

  .half li.selected {
    background: rgb(255 255 255 / 6%);
  }

  .half li.playing .name {
    color: var(--accent);
  }

  .pick {
    display: grid;
    gap: 0.05rem;
    min-width: 0;
    padding: 0.4rem 0.5rem;
    border-radius: 0.45rem;
    text-align: left;
  }

  .tag {
    color: var(--accent);
  }

  .actions {
    display: flex;
    gap: 0.25rem;
    padding-right: 0.35rem;
  }

  .small {
    padding: 0.25rem 0.55rem;
    border: 1px solid #3a3731;
    border-radius: 0.4rem;
    color: var(--text-muted);
    font-size: 0.78rem;
    white-space: nowrap;
  }

  .small:hover:not(:disabled) {
    color: var(--text);
  }

  .small.queued {
    border-color: transparent;
    color: var(--accent);
  }

  .preview {
    display: flex;
    flex-direction: column;
    gap: 0.75rem;
    min-height: 0;
    overflow-y: auto;
    padding: 0.75rem;
    border: 1px solid #24221e;
    border-radius: 0.6rem;
    background: rgb(21 21 28 / 60%);
  }

  .view {
    display: grid;
    place-items: center;
    min-height: 8rem;
    overflow: hidden;
    border-radius: 0.45rem;
    background: #0d0d12;
  }

  .view img,
  .view video {
    display: block;
    max-width: 100%;
    max-height: 18rem;
  }

  pre {
    width: 100%;
    max-height: 22rem;
    margin: 0;
    padding: 0.6rem;
    box-sizing: border-box;
    overflow: auto;
    color: var(--text);
    font-size: 0.75rem;
    white-space: pre-wrap;
    overflow-wrap: anywhere;
  }

  .note {
    margin: 0.5rem;
    color: var(--text-muted);
    font-size: 0.82rem;
    text-align: center;
  }

  dl {
    display: grid;
    grid-template-columns: auto minmax(0, 1fr);
    gap: 0.25rem 0.75rem;
    margin: 0;
    font-size: 0.82rem;
  }

  dt {
    color: var(--text-muted);
  }

  dd {
    margin: 0;
    overflow-wrap: anywhere;
  }

  .sr-only {
    position: absolute;
    width: 1px;
    height: 1px;
    overflow: hidden;
    clip-path: inset(50%);
    white-space: nowrap;
  }

  /* Narrow windows: torrents in a strip on top, the preview below the lists. */
  @media (max-width: 900px) {
    .explorer {
      grid-template-columns: minmax(0, 1fr);
      grid-template-rows: auto minmax(0, 1fr);
    }

    .torrents {
      padding: 0.4rem;
      border-right: none;
      border-bottom: 1px solid rgb(255 255 255 / 5%);
    }

    .torrents ul {
      display: flex;
      gap: 0.25rem;
      overflow-x: auto;
    }

    .torrents button {
      width: auto;
      max-width: 14rem;
    }

    .panes {
      grid-template-columns: minmax(0, 1fr);
      grid-template-rows: minmax(16rem, 1fr) auto;
      overflow-y: auto;
    }
  }
</style>
