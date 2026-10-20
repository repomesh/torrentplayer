<!-- SPDX-License-Identifier: MIT OR Apache-2.0 -->
<script lang="ts">
  import type { StreamStats } from '../bindings/StreamStats';
  import { formatBytes, formatMillis, formatPercent, formatRate } from '../format';

  interface Props {
    /** The player's latest stats (it polls once a second; the overlay does not poll itself). */
    stats: StreamStats | null;
    /** The last polling error, if any. */
    error: string | null;
  }

  let { stats, error }: Props = $props();
</script>

<aside class="overlay" aria-label="Streaming debug overlay">
  <div class="title">Debug (D to hide)</div>
  {#if error}
    <div class="row error">{error}</div>
  {/if}
  {#if stats}
    <dl>
      <dt>state</dt>
      <dd>{stats.state}{stats.engine ? ` · ${stats.engine}` : ''}</dd>
      <dt>down / up</dt>
      <dd>{formatRate(stats.downloadBps)} / {formatRate(stats.uploadBps)}</dd>
      <dt>peers</dt>
      <dd>
        {stats.peersLive} live · {stats.peersConnecting} connecting · {stats.peersSeen} seen
      </dd>
      <dt>file</dt>
      <dd>
        {formatPercent(stats.fileDownloaded, stats.fileLength)} of {formatBytes(stats.fileLength)}
      </dd>
      <dt>buffered</dt>
      <dd>
        {formatBytes(stats.bufferedBytes)}{stats.bufferedSecs === null
          ? ''
          : ` · ${stats.bufferedSecs.toFixed(1)} s`}
      </dd>
      <dt>server 1st byte</dt>
      <dd>{formatMillis(stats.serverFirstByteMs)}</dd>
      <dt>first frame</dt>
      <dd>{formatMillis(stats.firstFrameMs)}</dd>
      <dt>last seek</dt>
      <dd>{formatMillis(stats.lastSeekMs)} ({stats.seeks} seeks)</dd>
    </dl>
    <div class="pieces" aria-label="Piece map">
      {#each stats.pieceMap as cell, i (i)}
        <span style:opacity={(0.08 + (cell / 255) * 0.92).toFixed(2)}></span>
      {/each}
    </div>
    {#if stats.anchors.length > 0}
      <div class="anchors">
        {#each stats.anchors as anchor, i (i)}
          <div>{anchor.purpose} @ {formatBytes(anchor.offset)}</div>
        {/each}
      </div>
    {/if}
  {:else if !error}
    <div class="row">Loading…</div>
  {/if}
</aside>

<style>
  .overlay {
    position: absolute;
    top: 0.75rem;
    right: 0.75rem;
    z-index: 3;
    width: min(22rem, calc(100% - 1.5rem));
    padding: 0.75rem 0.9rem;
    border-radius: 0.5rem;
    background: rgb(8 8 10 / 78%);
    color: #d9d4ca;
    font-family: ui-monospace, 'Cascadia Mono', Menlo, Consolas, monospace;
    font-size: 0.72rem;
    line-height: 1.45;
    pointer-events: none;
    backdrop-filter: blur(6px);
  }

  .title {
    margin-bottom: 0.4rem;
    color: var(--accent);
  }

  dl {
    display: grid;
    grid-template-columns: auto 1fr;
    gap: 0 0.75rem;
    margin: 0;
  }

  dt {
    color: #8f8b84;
  }

  dd {
    margin: 0;
    font-variant-numeric: tabular-nums;
  }

  .pieces {
    display: flex;
    height: 0.5rem;
    margin-top: 0.6rem;
    overflow: hidden;
    border-radius: 2px;
    background: #1b1a17;
  }

  .pieces span {
    flex: 1;
    background: var(--accent);
  }

  .anchors {
    margin-top: 0.5rem;
    color: #8f8b84;
  }

  .error {
    color: #e8907a;
  }
</style>
