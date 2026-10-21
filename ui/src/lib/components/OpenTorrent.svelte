<!-- SPDX-License-Identifier: MIT OR Apache-2.0 -->
<script lang="ts">
  /*
   * The Open page: add a torrent from a magnet link, an http(s) link to a .torrent file, or a
   * .torrent file, and get back to any torrent opened this session.
   */
  import { onMount } from 'svelte';
  import type { TorrentSummary } from '../bindings/TorrentSummary';
  import { isPlayable, readTorrentFile } from '../files';
  import { formatBytes } from '../format';
  import { errorMessage } from '../ipc';
  import { looksLikeLink } from '../search';

  interface Props {
    isTorrentBusy: (source: string, infoHash?: string | null) => boolean;
    error: string | null;
    /** Torrents opened this session, most recent first. */
    torrents: readonly TorrentSummary[];
    onLink: (link: string) => void;
    onFile: (bytes: Uint8Array) => void;
    onShow: (torrent: TorrentSummary) => void;
  }

  let { isTorrentBusy, error, torrents, onLink, onFile, onShow }: Props = $props();

  let link = $state('');
  const busy = $derived(isTorrentBusy(link.trim()));
  let linkInput: HTMLInputElement | undefined = $state();
  let fileInput: HTMLInputElement | undefined = $state();
  let localError = $state<string | null>(null);

  onMount(() => {
    linkInput?.focus();
  });

  function submit(event: SubmitEvent): void {
    event.preventDefault();
    const trimmed = link.trim();
    if (trimmed === '' || busy) {
      return;
    }
    if (!looksLikeLink(trimmed)) {
      localError = 'Paste a magnet link, or an http(s) link to a .torrent file.';
      return;
    }
    localError = null;
    onLink(trimmed);
  }

  async function chosen(event: Event): Promise<void> {
    const input = event.currentTarget as HTMLInputElement;
    const file = input.files?.[0];
    input.value = '';
    if (!file) {
      return;
    }
    try {
      const bytes = await readTorrentFile(file);
      localError = null;
      onFile(bytes);
    } catch (reason: unknown) {
      localError = errorMessage(reason);
    }
  }

  const videoCount = (t: TorrentSummary): number => t.files.filter(isPlayable).length;
</script>

<section class="open" aria-labelledby="open-heading">
  <h2 id="open-heading" tabindex="-1">Open a torrent</h2>

  <form onsubmit={submit}>
    <label class="sr-only" for="open-link">Magnet link or link to a .torrent file</label>
    <input
      id="open-link"
      bind:this={linkInput}
      type="text"
      bind:value={link}
      placeholder="magnet:?xt=… or https://…/file.torrent"
      autocomplete="off"
      spellcheck="false"
    />
    <button type="submit" class="primary" disabled={busy || link.trim() === ''}>Open</button>
  </form>

  <div class="file">
    <button type="button" class="secondary" onclick={() => fileInput?.click()}>
      Choose a .torrent file…
    </button>
    <span class="hint">or drop one anywhere on this window</span>
    <input
      bind:this={fileInput}
      type="file"
      accept=".torrent,application/x-bittorrent"
      hidden
      onchange={(event: Event) => void chosen(event)}
    />
  </div>

  <p class="status" aria-live="polite">
    {#if busy}
      Fetching torrent details…
    {:else if localError ?? error}
      <span role="alert">{localError ?? error}</span>
    {/if}
  </p>

  {#if torrents.length > 0}
    <h3>Opened this session</h3>
    <ul>
      {#each torrents as torrent (torrent.id)}
        <li>
          <button
            type="button"
            class="torrent"
            onclick={() => {
              onShow(torrent);
            }}
          >
            <span class="name">{torrent.name}</span>
            <span class="meta">
              {formatBytes(torrent.totalLength)} · {videoCount(torrent)} video{videoCount(
                torrent,
              ) === 1
                ? ''
                : 's'}
            </span>
          </button>
        </li>
      {/each}
    </ul>
  {/if}
</section>

<style>
  .open {
    min-height: 100%;
    box-sizing: border-box;
    padding: 2.5rem clamp(1rem, 6vw, 5rem);
    background: linear-gradient(180deg, #17150f 0%, var(--bg) 40%);
  }

  h2 {
    margin: 0 0 1.25rem;
    font-family: var(--font-display);
    font-size: clamp(1.6rem, 3.5vw, 2.4rem);
    font-weight: 400;
  }

  h2:focus {
    outline: none;
  }

  h3 {
    margin: 2rem 0 0.5rem;
    color: var(--text-muted);
    font-size: 0.8rem;
    font-weight: 600;
    letter-spacing: 0.06em;
    text-transform: uppercase;
  }

  form {
    display: flex;
    gap: 0.5rem;
    max-width: 40rem;
  }

  input[type='text'] {
    flex: 1;
    min-width: 0;
    padding: 0.7rem 0.9rem;
    border: 1px solid #2c2a26;
    border-radius: 0.6rem;
    background: var(--surface);
    color: var(--text);
    font: inherit;
  }

  input[type='text']:focus {
    outline: 2px solid rgb(242 180 90 / 50%);
    outline-offset: 1px;
  }

  button {
    border: none;
    font: inherit;
    cursor: pointer;
  }

  button:disabled {
    cursor: default;
    opacity: 0.45;
  }

  button:focus-visible {
    outline: 2px solid var(--accent);
    outline-offset: 1px;
  }

  .primary {
    padding: 0.7rem 1.3rem;
    border-radius: 0.6rem;
    background: var(--accent);
    color: #1a1408;
    font-weight: 600;
  }

  .secondary {
    padding: 0.55rem 1rem;
    border: 1px solid #3a3731;
    border-radius: 0.6rem;
    background: transparent;
    color: var(--text);
  }

  .file {
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    gap: 0.75rem;
    margin-top: 0.9rem;
  }

  .hint {
    color: var(--text-muted);
    font-size: 0.875rem;
  }

  .status {
    min-height: 1.5em;
    color: var(--text-muted);
  }

  .status [role='alert'] {
    color: #e8907a;
  }

  ul {
    display: grid;
    gap: 0.3rem;
    max-width: 48rem;
    margin: 0;
    padding: 0;
    list-style: none;
  }

  .torrent {
    display: grid;
    gap: 0.15rem;
    width: 100%;
    padding: 0.65rem 0.9rem;
    border: 1px solid #24221e;
    border-radius: 0.6rem;
    background: var(--surface);
    color: var(--text);
    text-align: left;
  }

  .torrent:hover {
    border-color: #4a3f2c;
  }

  .name {
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  .meta {
    color: var(--text-muted);
    font-size: 0.8125rem;
    font-variant-numeric: tabular-nums;
  }

  .sr-only {
    position: absolute;
    width: 1px;
    height: 1px;
    overflow: hidden;
    clip-path: inset(50%);
    white-space: nowrap;
  }
</style>
