<!-- SPDX-License-Identifier: MIT OR Apache-2.0 -->
<script lang="ts">
  import { downloadedRuns } from '../seekbar';

  let { cells }: { cells: readonly number[] } = $props();
  const runs = $derived(downloadedRuns(cells));
  const pct = (fraction: number): string => `${(fraction * 100).toFixed(3)}%`;
</script>

{#each runs as run (run.start)}
  <span
    class="downloaded"
    aria-hidden="true"
    style:left={pct(run.start)}
    style:width={pct(run.end - run.start)}
    style:opacity={run.opacity.toFixed(2)}
  ></span>
{/each}

<style>
  .downloaded {
    position: absolute;
    top: 0;
    bottom: 0;
    background: #4f9f6a;
  }
</style>
