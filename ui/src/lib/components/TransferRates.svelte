<!-- SPDX-License-Identifier: MIT OR Apache-2.0 -->
<script lang="ts">
  /*
   * The app's total download and upload speed, for the status bar: every torrent added up,
   * as the background downloader measures it each second (`link_use`).
   */
  import { onMount } from 'svelte';
  import { describeRates, type LinkUse } from '../downloads';
  import { linkUse } from '../ipc';
  import { pollWhileVisible } from '../poll';

  /** How often the totals are read while the window is on screen. */
  const REFRESH_MS = 2000;

  let link = $state<LinkUse | null>(null);

  onMount(() => {
    let stopped = false;
    const stop = pollWhileVisible(
      () =>
        linkUse().then((totals) => {
          if (!stopped) {
            link = totals;
          }
        }),
      REFRESH_MS,
    );
    return () => {
      stopped = true;
      stop();
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
