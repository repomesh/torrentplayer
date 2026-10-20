<!-- SPDX-License-Identifier: MIT OR Apache-2.0 -->
<script lang="ts">
  /*
   * Filters and sorting over the search results on screen: one compact, wrapping row of
   * menus. They apply at once, without searching again, and stay set for the session.
   */
  import type { SearchResultDto } from '../bindings/SearchResultDto';
  import {
    DEFAULT_FILTERS,
    RESOLUTION_STEPS,
    SEEDER_STEPS,
    SIZE_STEPS,
    SORT_LABELS,
    activeFilterCount,
    facets,
    hiddenNonVideos,
    type SearchFilters,
    type SortKey,
  } from '../filters';
  import { formatBytes } from '../format';

  interface Props {
    filters: SearchFilters;
    /** Every result of the search, for the category and source menus. */
    results: readonly SearchResultDto[];
    shown: number;
    /** The best pick exists but the filters hide it. */
    bestPickHidden: boolean;
  }

  // eslint-disable-next-line @typescript-eslint/no-useless-default-assignment -- $bindable() makes the prop bindable; it is not a default value
  let { filters = $bindable(), results, shown, bestPickHidden }: Props = $props();

  const options = $derived(facets(results));
  const active = $derived(activeFilterCount(filters));
  const notVideos = $derived(hiddenNonVideos(results, filters));
  const sorts = Object.keys(SORT_LABELS) as SortKey[];

  /** The menu's choices, keeping a chosen value that this search does not have. */
  function withChosen(
    list: { value: string; count: number }[],
    chosen: string | null,
  ): { value: string; count: number }[] {
    if (chosen === null || list.some((f) => f.value.toLowerCase() === chosen.toLowerCase())) {
      return list;
    }
    return [{ value: chosen, count: 0 }, ...list];
  }

  function reset(): void {
    filters = { ...DEFAULT_FILTERS, sort: filters.sort };
  }

  const resolutionLabel = (r: number): string =>
    r === 0 ? 'Any quality' : r === 2160 ? '4K only' : `${String(r)}p and up`;
</script>

<div class="filters" role="group" aria-label="Filter and sort results">
  <label class="sort" class:set={filters.sort !== 'relevance'}>
    <span>Sort</span>
    <select bind:value={filters.sort}>
      {#each sorts as key (key)}
        <option value={key}>{SORT_LABELS[key]}</option>
      {/each}
    </select>
  </label>
  <span class="divider" aria-hidden="true"></span>

  <select
    bind:value={filters.minResolution}
    aria-label="Minimum quality"
    class:set={filters.minResolution !== 0}
  >
    {#each RESOLUTION_STEPS as r (r)}
      <option value={r}>{resolutionLabel(r)}</option>
    {/each}
  </select>

  <select
    bind:value={filters.minSeeders}
    aria-label="Minimum seeders"
    class:set={filters.minSeeders !== 0}
  >
    {#each SEEDER_STEPS as n (n)}
      <option value={n}>{n === 0 ? 'Any seeders' : `${String(n)}+ seeders`}</option>
    {/each}
  </select>

  <select
    bind:value={filters.minSizeBytes}
    aria-label="Smallest size"
    class:set={filters.minSizeBytes !== null}
  >
    <option value={null}>Any min size</option>
    {#each SIZE_STEPS as bytes (bytes)}
      <option value={bytes}>≥ {formatBytes(bytes)}</option>
    {/each}
  </select>

  <select
    bind:value={filters.maxSizeBytes}
    aria-label="Largest size"
    class:set={filters.maxSizeBytes !== null}
  >
    <option value={null}>Any max size</option>
    {#each SIZE_STEPS as bytes (bytes)}
      <option value={bytes}>≤ {formatBytes(bytes)}</option>
    {/each}
  </select>

  <select bind:value={filters.indexer} aria-label="Source" class:set={filters.indexer !== null}>
    <option value={null}>All sources</option>
    {#each withChosen(options.indexers, filters.indexer) as facet (facet.value)}
      <option value={facet.value}>{facet.value} ({facet.count})</option>
    {/each}
  </select>

  <label class="check" class:set={filters.playableOnly}>
    <input type="checkbox" bind:checked={filters.playableOnly} />
    Plays here
  </label>
  <label class="check" class:set={filters.hideCamera}>
    <input type="checkbox" bind:checked={filters.hideCamera} />
    No camera copies
  </label>
  <label class="check" title="Hide ebooks, software, and games (video and audio stay)">
    <input type="checkbox" bind:checked={filters.videosOnly} />
    Playable only
  </label>

  <p class="summary" aria-live="polite">
    {#if active > 0}
      {shown} of {results.length} shown
      <button type="button" class="link" onclick={reset}>Reset filters</button>
    {/if}
    {#if notVideos > 0}
      <span class="hidden-pick">{active > 0 ? '· ' : ''}{notVideos} hidden: nothing to play</span>
      <button
        type="button"
        class="link"
        onclick={() => {
          filters = { ...filters, videosOnly: false };
        }}>Show</button
      >
    {/if}
    {#if bestPickHidden}
      <span class="hidden-pick">· Best pick hidden by the filters</span>
    {/if}
  </p>
</div>

<style>
  .filters {
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    gap: 0.35rem 0.4rem;
    width: 100%;
    min-width: 0;
    text-align: left;
  }

  /* "Sort" first in the row, so it is the first thing seen and stays at the start when the
     row wraps on narrow windows. */
  .sort {
    display: inline-flex;
    flex: none;
    align-items: center;
    gap: 0.35rem;
    color: var(--text);
    font-size: 0.8rem;
    font-weight: 500;
  }

  .sort select {
    color: var(--text);
  }

  .sort.set select {
    border-color: #4a3f2c;
    color: var(--accent);
  }

  .divider {
    width: 1px;
    height: 1.1rem;
    margin: 0 0.15rem;
    background: rgb(255 255 255 / 10%);
  }

  select,
  .check {
    max-width: 12rem;
    padding: 0.3rem 0.45rem;
    border: 1px solid #2c2a26;
    border-radius: 0.45rem;
    background: var(--surface);
    color: var(--text-muted);
    font: inherit;
    font-size: 0.8rem;
    cursor: pointer;
  }

  select:hover,
  .check:hover {
    color: var(--text);
  }

  select:focus-visible,
  .check:focus-within {
    outline: 2px solid rgb(242 180 90 / 50%);
    outline-offset: 1px;
  }

  .set {
    border-color: #4a3f2c;
    color: var(--accent);
  }

  .check {
    display: inline-flex;
    align-items: center;
    gap: 0.3rem;
    user-select: none;
  }

  .check input {
    margin: 0;
    accent-color: var(--accent);
  }

  .check input:focus {
    outline: none;
  }

  .summary {
    margin: 0 0 0 0.25rem;
    color: var(--text-muted);
    font-size: 0.8rem;
    font-variant-numeric: tabular-nums;
  }

  .link {
    padding: 0 0.2rem;
    border: none;
    background: none;
    color: var(--accent);
    font: inherit;
    text-decoration: underline;
    cursor: pointer;
  }

  .hidden-pick {
    color: var(--text-muted);
  }
</style>
