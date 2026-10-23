<!-- SPDX-License-Identifier: MIT OR Apache-2.0 -->
<script lang="ts">
  /*
   * The app's total download and upload speed, for the status bar: every torrent added up,
   * as the background downloader measures it each second (`link_use`).
   */
  import { onMount } from 'svelte';
  import { describeRates, type LinkUse } from '../downloads';
  import { linkUse } from '../ipc';

  /** How often the totals are read. */
  const REFRESH_MS = 2000;

  let link = $state<LinkUse | null>(null);

  onMount(() => {
    let stopped = false;
    const refresh = (): void => {
      linkUse().then(
        (totals) => {
          if (!stopped) {
            link = totals;
          }
        },
        () => undefined,
      );
    };
    refresh();
    const timer = setInterval(refresh, REFRESH_MS);
    return () => {
      stopped = true;
      clearInterval(timer);
    };
  });
</script>

{#if link}
  <span
    class="rates"
    class:idle={link.usedBps === 0 && link.uploadBps === 0}
    title="Total download and upload speed of all torrents"
  >
    {describeRates(link)}
  </span>
{/if}

<style>
  .rates {
    flex: none;
    white-space: nowrap;
    font-variant-numeric: tabular-nums;
  }

  .rates.idle {
    color: var(--text-muted);
  }
</style>
