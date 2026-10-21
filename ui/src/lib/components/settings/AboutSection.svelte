<!-- SPDX-License-Identifier: MIT OR Apache-2.0 -->
<script lang="ts">
  import { onMount } from 'svelte';
  import { getAppInfo, type AppInfo } from '../../ipc';
  import { licenseSummary } from '../../settings';

  let info = $state<AppInfo | null>(null);
  let notices = $state<string | null>(null);
  let showNotices = $state(false);
  let loadingNotices = $state(false);

  const summary = $derived(notices === null ? [] : licenseSummary(notices));

  async function loadNotices(): Promise<void> {
    loadingNotices = true;
    try {
      // A separate chunk, loaded only when asked for.
      notices = (await import('../../../../../THIRD_PARTY_NOTICES.md?raw')).default;
    } finally {
      loadingNotices = false;
    }
  }

  onMount(() => {
    getAppInfo()
      .then((value) => {
        info = value;
      })
      .catch(() => undefined);
    void loadNotices();
  });
</script>

<section class="card" aria-labelledby="about-title">
  <h3 id="about-title">About</h3>
  <dl class="about">
    <dt>Version</dt>
    <dd>{info ? `${info.name} ${info.version}` : '…'}</dd>
    <dt>License</dt>
    <dd>{info?.license ?? 'MIT OR Apache-2.0'} — free and open source</dd>
    <dt>Website</dt>
    <dd>torrentplayer.io</dd>
  </dl>
  <h4>Third-party software</h4>
  <p class="intro">
    TorrentPlayer is built on open-source software. Each component keeps its own license; the full
    texts are below.
  </p>
  {#if summary.length > 0}
    <ul class="licenses">
      {#each summary as item (item.license)}
        <li><span>{item.license}</span><span class="count">{item.count}</span></li>
      {/each}
    </ul>
  {/if}
  <button
    type="button"
    aria-expanded={showNotices}
    aria-controls="about-notices"
    disabled={notices === null && !loadingNotices}
    onclick={() => {
      showNotices = !showNotices;
    }}
  >
    {showNotices ? 'Hide full notices' : 'Show full notices'}
  </button>
  {#if showNotices && notices !== null}
    <pre id="about-notices" class="notices">{notices}</pre>
  {/if}
</section>

<style>
  .about {
    display: grid;
    grid-template-columns: max-content 1fr;
    gap: 0.4rem 1.25rem;
    margin: 0 0 1.25rem;
  }

  dt {
    color: var(--text-muted);
  }

  dd {
    margin: 0;
  }

  .intro {
    margin: 0 0 0.75rem;
    color: var(--text-muted);
    font-size: 0.85rem;
  }

  .licenses {
    display: grid;
    grid-template-columns: repeat(auto-fill, minmax(16rem, 1fr));
    gap: 0.3rem 1.5rem;
    margin: 0 0 1rem;
    padding: 0;
    list-style: none;
    font-size: 0.85rem;
  }

  .licenses li {
    display: flex;
    justify-content: space-between;
    gap: 1rem;
  }

  .count {
    color: var(--text-muted);
    font-variant-numeric: tabular-nums;
  }

  .notices {
    max-height: 24rem;
    margin: 1rem 0 0;
    padding: 1rem;
    overflow: auto;
    border-radius: 0.5rem;
    background: rgb(0 0 0 / 30%);
    color: var(--text-muted);
    font-size: 0.75rem;
    line-height: 1.5;
    white-space: pre-wrap;
  }

  .notices:focus-visible {
    outline: 2px solid var(--accent);
  }
</style>
