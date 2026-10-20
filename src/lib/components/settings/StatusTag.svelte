<!-- SPDX-License-Identifier: MIT OR Apache-2.0 -->
<script lang="ts">
  /* Says honestly when a setting takes effect. Nothing is shown for settings that apply now. */
  import { availability } from '../../settings';

  interface Props {
    path: string;
  }

  let { path }: Props = $props();

  const status = $derived(availability(path));
</script>

{#if status === 'restart'}
  <span class="tag restart" title="Saved now; TorrentStream uses it the next time it starts">
    Applies on next start
  </span>
{:else if status === 'later'}
  <span class="tag later" title="Saved now; it takes effect when this feature ships">
    Coming in a later milestone
  </span>
{/if}

<style>
  .tag {
    display: inline-block;
    padding: 0.1rem 0.5rem;
    border-radius: 999px;
    font-size: 0.68rem;
    letter-spacing: 0.02em;
    white-space: nowrap;
    vertical-align: middle;
  }

  .restart {
    background: rgb(242 180 90 / 12%);
    color: var(--accent);
  }

  .later {
    background: rgb(255 255 255 / 6%);
    color: var(--text-muted);
  }
</style>
