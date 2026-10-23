<!-- SPDX-License-Identifier: MIT OR Apache-2.0 -->
<script lang="ts">
  /*
   * Everything known about one search result, shown when its row is expanded. Unknown things
   * are left out. All text is plain text; links open in the system browser after a check in
   * the backend; the search service's download link (it carries a token) is never shown.
   */
  import { onMount } from 'svelte';
  import { fileName } from '../files';
  import { formatBytes } from '../format';
  import {
    errorMessage,
    openLink,
    torrentDetails,
    type SearchResultDto,
    type TorrentDetailsDto,
  } from '../ipc';
  import {
    browserNote,
    formatDay,
    formatPieceSize,
    healthLabel,
    playsVerdict,
    rememberDetails,
    rememberedDetails,
    soundVerdict,
    summarizeFiles,
    trackerSummary,
  } from '../details';
  import { followNetwork } from '../network';
  import { formatPeers, formatPublished } from '../search';
  import type { NetworkModeKind } from '../bindings/NetworkModeKind';

  interface Props {
    result: SearchResultDto;
    id: string;
  }

  let { result, id }: Props = $props();

  let details = $state<TorrentDetailsDto | null>(null);
  let loading = $state(false);
  let loadError = $state<string | null>(null);
  let udpAllowed = $state<boolean | null>(null);
  let mode = $state<NetworkModeKind | null>(null);
  // Indexer pages open in the system browser, which a protected mode does not cover.
  const linkNote = $derived(browserNote(mode));
  let copied = $state<string | null>(null);

  const files = $derived(details ? summarizeFiles(details.files) : null);
  const trackers = $derived(
    details
      ? trackerSummary(details.udpTrackers, details.httpTrackers, details.webSeeds, udpAllowed)
      : null,
  );
  const sound = $derived(soundVerdict(result.quality));
  const health = $derived(healthLabel(result.health));
  const q = $derived(result.quality);

  onMount(() => {
    const cached = rememberedDetails(result.key);
    if (cached) {
      details = cached;
    } else {
      void torrentDetails(result, false)
        .then((d) => {
          if (d) {
            rememberDetails(result.key, d);
            details = d;
          }
        })
        .catch(() => undefined);
    }
    return followNetwork((s) => {
      udpAllowed = s.udpAllowed;
      mode = s.mode;
    });
  });

  async function load(): Promise<void> {
    loading = true;
    loadError = null;
    try {
      const d = await torrentDetails(result, true);
      if (d) {
        rememberDetails(result.key, d);
        details = d;
      } else {
        loadError = 'No details were found.';
      }
    } catch (reason: unknown) {
      loadError = errorMessage(reason);
    } finally {
      loading = false;
    }
  }

  async function copy(what: string, text: string): Promise<void> {
    try {
      await navigator.clipboard.writeText(text);
      copied = what;
      setTimeout(() => {
        if (copied === what) {
          copied = null;
        }
      }, 1500);
    } catch {
      copied = null;
    }
  }
</script>

<div class="details" {id}>
  <section>
    <h3>Quality</h3>
    <dl>
      {#if q.resolution}<dt>Resolution</dt>
        <dd>{q.resolution}{q.hdr ? ' · HDR' : ''}</dd>{/if}
      {#if q.source}<dt>Source</dt>
        <dd>{q.source}{q.camera ? ' (camera copy)' : ''}</dd>{/if}
      {#if q.codec}<dt>Video</dt>
        <dd>{q.codec}</dd>{/if}
      {#if sound}<dt>Audio</dt>
        <dd>{sound}</dd>{/if}
      {#if q.container}<dt>Container</dt>
        <dd>{q.container}</dd>{/if}
      <dt>Plays here</dt>
      <dd>{playsVerdict(q)}</dd>
      {#if q.suspectSize}<dt>Size check</dt>
        <dd>Too small to be real 4K</dd>{/if}
    </dl>
  </section>

  <section>
    <h3>Files</h3>
    {#if details && files}
      <dl>
        <dt>Videos</dt>
        <dd>
          {files.videoCount}{files.largestVideo !== null
            ? ` · largest ${formatBytes(files.largestVideo)}`
            : ''}
        </dd>
        <dt>Total</dt>
        <dd>
          {formatBytes(details.totalLength)} · {details.files.length} file{details.files.length ===
          1
            ? ''
            : 's'} · pieces of {formatPieceSize(details.pieceLength)}
        </dd>
        {#if details.private !== null}
          <dt>Private</dt>
          <dd>{details.private ? 'Yes: peers only from its trackers' : 'No'}</dd>
        {/if}
      </dl>
      {#if files.videos.length > 0}
        <ol class="files">
          {#each files.videos as video (video.index)}
            <li><span class="name">{fileName(video.path)}</span> {formatBytes(video.length)}</li>
          {/each}
        </ol>
        {#if files.moreVideos > 0}<p class="more">+{files.moreVideos} more</p>{/if}
      {/if}
      {#if files.subtitles.length > 0}
        <p class="line">
          Subtitles: {files.subtitles.map((s) => fileName(s.path)).join(', ')}
        </p>
      {/if}
      {#if files.others !== ''}<p class="line">Other files: {files.others}</p>{/if}
    {:else}
      <p class="line">
        <button type="button" class="link" disabled={loading} onclick={() => void load()}>
          {loading ? 'Loading the file list…' : 'Load file list'}
        </button>
        {#if loadError}<span class="error">{loadError}</span>{/if}
      </p>
    {/if}
  </section>

  <section>
    <h3>Swarm</h3>
    <dl>
      <dt>Peers</dt>
      <dd>
        {formatPeers(result.seeders, result.peers)} (seeders ↑, others ↓, as the indexers reported them
        when searched)
      </dd>
      {#if health}<dt>Health</dt>
        <dd>{health}</dd>{/if}
      {#if result.suspicious}
        <dt>Warning</dt>
        <dd>The indexer gave a placeholder seed count, as fake listings do</dd>
      {/if}
      {#if trackers}
        <dt>Peers from</dt>
        <dd>
          {trackers.text}
          {#if trackers.needsDirect}
            <span class="error">· needs Direct mode (the current network mode blocks UDP)</span>
          {/if}
        </dd>
      {/if}
    </dl>
  </section>

  <section>
    <h3>Source</h3>
    <dl>
      <dt>Indexers</dt>
      <dd>
        {result.indexers.join(', ')}
        {#if result.detailsUrl}
          ·
          <button
            type="button"
            class="link"
            onclick={() => {
              if (result.detailsUrl) {
                void openLink(result.detailsUrl).catch(() => undefined);
              }
            }}>Open the indexer's page</button
          >
          {#if linkNote}
            <span class="note">{linkNote}</span>
          {/if}
        {/if}
      </dd>
      {#if formatPublished(result.published)}
        <dt>Published</dt>
        <dd>{formatPublished(result.published)}</dd>
      {/if}
      {#if result.category}<dt>Category</dt>
        <dd>{result.category}</dd>{/if}
      <dt>Release</dt>
      <dd class="wrap">{result.title}</dd>
      {#if result.infoHash}
        <dt>Info hash</dt>
        <dd>
          <code>{result.infoHash}</code>
          <button
            type="button"
            class="link"
            onclick={() => {
              if (result.infoHash) {
                void copy('hash', result.infoHash);
              }
            }}>{copied === 'hash' ? 'Copied' : 'Copy'}</button
          >
        </dd>
      {/if}
      {#if result.magnet}
        <dt>Magnet</dt>
        <dd>
          <button
            type="button"
            class="link"
            onclick={() => {
              if (result.magnet) {
                void copy('magnet', result.magnet);
              }
            }}>{copied === 'magnet' ? 'Copied' : 'Copy magnet link'}</button
          >
        </dd>
      {/if}
      {#if details?.createdBy}<dt>Made with</dt>
        <dd class="wrap">{details.createdBy}</dd>{/if}
      {#if details && formatDay(details.createdMs)}
        <dt>Made on</dt>
        <dd>{formatDay(details.createdMs)}</dd>
      {/if}
      {#if details?.comment}<dt>Comment</dt>
        <dd class="wrap">{details.comment}</dd>{/if}
    </dl>
  </section>
</div>

<style>
  .details {
    display: grid;
    grid-template-columns: repeat(auto-fit, minmax(min(100%, 16rem), 1fr));
    gap: 0.75rem 1.5rem;
    padding: 0.75rem 0.9rem 0.9rem;
    border-top: 1px solid #24221e;
    color: var(--text-muted);
    font-size: 0.8125rem;
  }

  .details > section {
    min-width: 0;
  }

  h3 {
    margin: 0 0 0.35rem;
    color: var(--text);
    font-size: 0.8125rem;
    font-weight: 600;
  }

  dl {
    display: grid;
    grid-template-columns: fit-content(40%) minmax(0, 1fr);
    gap: 0.15rem 0.6rem;
    margin: 0;
  }

  dt {
    overflow-wrap: anywhere;
    color: var(--text-muted);
    opacity: 0.8;
  }

  dd {
    min-width: 0;
    margin: 0;
    color: var(--text);
    overflow-wrap: anywhere;
  }

  .wrap {
    white-space: pre-wrap;
  }

  code {
    font-size: 0.75rem;
  }

  .files {
    margin: 0.4rem 0 0;
    padding-left: 1.4rem;
    color: var(--text);
    font-variant-numeric: tabular-nums;
  }

  .files .name {
    overflow-wrap: anywhere;
  }

  .line,
  .more {
    margin: 0.35rem 0 0;
  }

  .error {
    color: #e8907a;
  }

  .note {
    display: block;
    color: var(--text-muted);
    font-size: 0.8rem;
  }

  .link {
    padding: 0 0.2rem;
    border: none;
    background: none;
    color: var(--accent);
    font: inherit;
    font-weight: 500;
    text-decoration: underline;
    cursor: pointer;
  }

  .link:disabled {
    opacity: 0.6;
    cursor: default;
  }
</style>
