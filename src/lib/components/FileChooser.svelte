<!-- SPDX-License-Identifier: MIT OR Apache-2.0 -->
<script lang="ts">
  /*
   * "Choose files…" for a torrent in the queue: its videos in lecture order (2 before 10), each
   * with a checkbox for whether it is queued and a button to play it now. Changes apply at
   * once; Esc or Done closes it.
   */
  import { onMount } from 'svelte';
  import type { FileSummary } from '../bindings/FileSummary';
  import type { TorrentSummary } from '../bindings/TorrentSummary';
  import { fileName, folderOf, isPlayable, sortFiles } from '../files';
  import { formatBytes } from '../format';

  interface Props {
    torrent: TorrentSummary;
    isQueued: (file: FileSummary) => boolean;
    /** The file of this torrent playing now, if any. */
    playingIndex: number | null;
    onToggle: (file: FileSummary, queued: boolean) => void;
    onToggleAll: (queued: boolean) => void;
    onPlay: (file: FileSummary) => void;
    onClose: () => void;
  }

  let { torrent, isQueued, playingIndex, onToggle, onToggleAll, onPlay, onClose }: Props = $props();

  let dialog: HTMLDialogElement | undefined = $state();
  const videos = $derived(sortFiles(torrent.files).filter(isPlayable));
  const queuedCount = $derived(videos.filter((f) => isQueued(f)).length);

  onMount(() => {
    dialog?.showModal();
    return () => {
      dialog?.close();
    };
  });
</script>

<dialog
  bind:this={dialog}
  class="chooser"
  aria-labelledby="chooser-title"
  aria-describedby="chooser-hint"
  onclose={onClose}
>
  <header>
    <div class="heading">
      <h2 id="chooser-title">Choose files</h2>
      <p class="name" title={torrent.name}>{torrent.name}</p>
    </div>
    <button type="button" class="close" aria-label="Close" onclick={onClose}>
      <svg viewBox="0 0 24 24" aria-hidden="true"><path d="M7 7l10 10M17 7L7 17" /></svg>
    </button>
  </header>

  <p id="chooser-hint" class="hint">
    Checked videos are in the queue, in this order. {queuedCount} of {videos.length} queued.
    <button
      type="button"
      class="link"
      onclick={() => {
        onToggleAll(queuedCount < videos.length);
      }}>{queuedCount < videos.length ? 'Queue all' : 'Remove all'}</button
    >
  </p>

  <ul>
    {#each videos as file (file.index)}
      {@const queued = isQueued(file)}
      <li class:playing={playingIndex === file.index}>
        <label>
          <input
            type="checkbox"
            checked={queued}
            onchange={(event: Event) => {
              onToggle(file, (event.currentTarget as HTMLInputElement).checked);
            }}
          />
          <span class="text">
            <span class="file">{fileName(file.path)}</span>
            <span class="meta">
              {#if playingIndex === file.index}<span class="state">Playing ·</span>{/if}
              {formatBytes(file.length)}{folderOf(file.path) ? ` · ${folderOf(file.path)}` : ''}
            </span>
          </span>
        </label>
        <button
          type="button"
          class="play"
          title={`Play ${fileName(file.path)} now`}
          aria-label={`Play ${fileName(file.path)} now`}
          onclick={() => {
            onPlay(file);
          }}
        >
          <svg viewBox="0 0 24 24" aria-hidden="true"><path d="M8 5.5v13l10-6.5z" /></svg>
        </button>
      </li>
    {/each}
  </ul>

  <footer>
    <button type="button" class="done" onclick={onClose}>Done</button>
  </footer>
</dialog>

<style>
  .chooser {
    width: min(34rem, calc(100vw - 2rem));
    max-height: min(36rem, calc(100vh - 3rem));
    padding: 0;
    border: 1px solid rgb(255 255 255 / 8%);
    border-radius: 0.8rem;
    background: #121218;
    color: var(--text);
    box-shadow: 0 1rem 3rem rgb(0 0 0 / 55%);
  }

  .chooser[open] {
    display: flex;
    flex-direction: column;
  }

  .chooser::backdrop {
    background: rgb(0 0 0 / 55%);
  }

  header {
    display: flex;
    align-items: flex-start;
    gap: 0.75rem;
    padding: 1rem 1rem 0.25rem 1.1rem;
  }

  .heading {
    flex: 1;
    min-width: 0;
  }

  h2 {
    margin: 0;
    font-size: 1rem;
    font-weight: 600;
  }

  .name {
    margin: 0.15rem 0 0;
    overflow: hidden;
    color: var(--text-muted);
    font-size: 0.85rem;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  .hint {
    margin: 0.25rem 1.1rem 0.5rem;
    color: var(--text-muted);
    font-size: 0.8rem;
  }

  ul {
    flex: 1;
    min-height: 0;
    margin: 0;
    padding: 0 0.6rem;
    overflow-y: auto;
    list-style: none;
  }

  li {
    display: grid;
    grid-template-columns: minmax(0, 1fr) auto;
    align-items: center;
    gap: 0.4rem;
    border-radius: 0.5rem;
  }

  li:hover,
  li:focus-within {
    background: rgb(255 255 255 / 4%);
  }

  li.playing {
    background: rgb(242 180 90 / 7%);
  }

  label {
    display: grid;
    grid-template-columns: auto minmax(0, 1fr);
    align-items: center;
    gap: 0.6rem;
    padding: 0.5rem;
    cursor: pointer;
  }

  input {
    margin: 0;
    accent-color: var(--accent);
  }

  .text {
    display: grid;
    min-width: 0;
  }

  .file {
    overflow-wrap: anywhere;
  }

  .meta {
    color: var(--text-muted);
    font-size: 0.78rem;
    font-variant-numeric: tabular-nums;
  }

  .state {
    color: var(--accent);
  }

  button {
    border: none;
    background: none;
    color: inherit;
    font: inherit;
    cursor: pointer;
  }

  button:focus-visible,
  input:focus-visible {
    outline: 2px solid var(--accent);
    outline-offset: 1px;
  }

  .close,
  .play {
    display: inline-flex;
    padding: 0.35rem;
    border-radius: 0.4rem;
    color: var(--text-muted);
  }

  .play {
    margin-right: 0.3rem;
  }

  .close:hover,
  .play:hover {
    color: var(--text);
    background: rgb(255 255 255 / 6%);
  }

  .play:hover {
    color: var(--accent);
  }

  svg {
    width: 1rem;
    height: 1rem;
    fill: none;
    stroke: currentcolor;
    stroke-width: 1.8;
    stroke-linecap: round;
    stroke-linejoin: round;
  }

  .link {
    padding: 0 0.2rem;
    color: var(--accent);
    text-decoration: underline;
  }

  footer {
    display: flex;
    justify-content: flex-end;
    padding: 0.6rem 1rem 0.9rem;
    border-top: 1px solid rgb(255 255 255 / 5%);
  }

  .done {
    padding: 0.45rem 1.1rem;
    border-radius: 0.5rem;
    background: var(--accent);
    color: #1a1408;
    font-weight: 600;
  }
</style>
