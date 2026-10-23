<!-- SPDX-License-Identifier: MIT OR Apache-2.0 -->
<script lang="ts">
  import { formatEta } from '../downloads';
  import { formatPercent, formatRate } from '../format';
  import type { StatusInfo } from '../stats';

  let {
    downloadBps,
    uploadBps,
    peersLive,
    peersConnecting,
    peersSeen,
    bufferedSecs,
    health,
    fileDownloaded,
    fileLength,
    etaSecs,
  }: StatusInfo = $props();

  const complete = $derived(fileLength > 0 && fileDownloaded >= fileLength);
  const left = $derived(formatEta(etaSecs));
  const ahead = $derived(
    bufferedSecs === null ? '—' : `${Math.floor(Math.max(0, bufferedSecs)).toString()}s ahead`,
  );
</script>

<div class="status">
  <span title="Download and upload speed"
    >↓ {formatRate(downloadBps)} ↑ {formatRate(uploadBps)}</span
  >
  <span class="sep">·</span>
  <span
    title={`${peersLive.toString()} connected · ${peersConnecting.toString()} connecting · ${peersSeen.toString()} seen`}
  >
    {peersLive} peer{peersLive === 1 ? '' : 's'}
  </span>
  <span class="sep">·</span>
  <span title="Media buffered ahead of the playhead">
    <span class={`health ${health}`} aria-hidden="true"></span>
    {ahead}
  </span>
  <span class="sep">·</span>
  <span title="Share of this file downloaded">{formatPercent(fileDownloaded, fileLength)}</span>
  {#if !complete && left !== null}
    <span class="sep">·</span>
    <span title="Time to download the rest of this file">{left} left</span>
  {/if}
</div>

<style>
  .status {
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    gap: 0 0.4rem;
    color: var(--text-muted);
    font-size: 0.75rem;
    font-variant-numeric: tabular-nums;
    white-space: nowrap;
  }

  .sep {
    opacity: 0.5;
  }

  .health {
    display: inline-block;
    width: 0.45rem;
    height: 0.45rem;
    margin-right: 0.15rem;
    border-radius: 50%;
    background: #6b6862;
    vertical-align: 0.05em;
  }

  .health.good {
    background: #57b26f;
  }

  .health.low {
    background: #d9b44a;
  }

  .health.stalled {
    background: #d9604f;
  }
</style>
