<!-- SPDX-License-Identifier: MIT OR Apache-2.0 -->
<script lang="ts">
  import type { LibraryItem } from '../bindings/LibraryItem';
  import { formatBytes } from '../format';
  import { groupLibrary, resumePosition, watchedFraction, watchedLabel } from '../library';
  import { formatTime } from '../seekbar';

  interface Props {
    items: readonly LibraryItem[];
    isBusy: (item: LibraryItem) => boolean;
    onPlay: (item: LibraryItem) => void;
  }

  let { items, isBusy, onPlay }: Props = $props();
  const courses = $derived(groupLibrary(items));

  function progress(item: LibraryItem): string {
    const saved = watchedLabel(item);
    if (saved === 'Watched') {
      return saved;
    }
    if (item.durationSecs !== null) {
      return `${formatTime(item.positionSecs)} of ${formatTime(item.durationSecs)}`;
    }
    return saved ?? 'Not started';
  }

  function lastWatched(item: LibraryItem): string {
    return new Date(item.updatedAtSecs * 1000).toLocaleDateString(undefined, {
      month: 'short',
      day: 'numeric',
      year:
        item.updatedAtSecs * 1000 < Date.now() - 300 * 24 * 60 * 60 * 1000 ? 'numeric' : undefined,
    });
  }
</script>

<section class="library" aria-labelledby="library-heading">
  <header>
    <div>
      <h2 id="library-heading" tabindex="-1">Library</h2>
      <p>Continue what you were watching, grouped by torrent.</p>
    </div>
    {#if items.length > 0}<span class="count"
        >{items.length} {items.length === 1 ? 'item' : 'items'}</span
      >{/if}
  </header>

  {#if courses.length === 0}
    <div class="empty">
      <svg viewBox="0 0 24 24" aria-hidden="true"
        ><path
          d="M5 4.5h11.5A2.5 2.5 0 0 1 19 7v12H7a2 2 0 0 1-2-2V4.5Zm2 14.5a2 2 0 0 1 2-2h10M9 8h6M9 11.5h5"
        /></svg
      >
      <h3>Nothing watched yet</h3>
      <p>Videos and tracks appear here when you open them.</p>
    </div>
  {:else}
    <div class="courses">
      {#each courses as course (course.key)}
        <section class="course" aria-labelledby={`course-${course.key}`}>
          <h3 id={`course-${course.key}`}>{course.name}</h3>
          <ul>
            {#each course.items as item (`${item.infoHash}:${String(item.fileIndex)}`)}
              {@const fraction = watchedFraction(item)}
              <li>
                <button
                  type="button"
                  disabled={isBusy(item)}
                  onclick={() => {
                    onPlay(item);
                  }}
                >
                  <span class="play" aria-hidden="true">
                    {#if isBusy(item)}<span class="spinner"></span>{:else}<svg viewBox="0 0 24 24"
                        ><path d="m9 6 9 6-9 6V6Z" /></svg
                      >{/if}
                  </span>
                  <span class="details">
                    <span class="title">{item.title}</span>
                    <span class="meta">
                      <span class:complete={item.completed}>{progress(item)}</span>
                      {#if item.sizeBytes !== null}
                        · {formatBytes(item.sizeBytes)}{/if}
                      · {lastWatched(item)}
                    </span>
                    <span
                      class="bar"
                      role="progressbar"
                      aria-label={`Watched ${item.title}`}
                      aria-valuemin={0}
                      aria-valuemax={100}
                      aria-valuenow={Math.floor(fraction * 100)}
                      ><span style:width={`${String(fraction * 100)}%`}></span></span
                    >
                  </span>
                  <span class="action"
                    >{isBusy(item)
                      ? 'Opening…'
                      : item.completed
                        ? 'Play again'
                        : resumePosition(item) !== null
                          ? 'Continue'
                          : 'Play'}</span
                  >
                </button>
              </li>
            {/each}
          </ul>
        </section>
      {/each}
    </div>
  {/if}
</section>

<style>
  .library {
    min-height: 100%;
    box-sizing: border-box;
    padding: 2.5rem clamp(1rem, 6vw, 5rem);
    background: linear-gradient(180deg, #17150f 0%, var(--bg) 40%);
  }

  header {
    display: flex;
    align-items: end;
    justify-content: space-between;
    gap: 1rem;
    max-width: 58rem;
    margin-bottom: 2rem;
  }

  h2 {
    margin: 0;
    font-family: var(--font-display);
    font-size: clamp(1.6rem, 3.5vw, 2.4rem);
    font-weight: 400;
  }

  h2:focus {
    outline: none;
  }

  header p,
  .empty p {
    margin: 0.35rem 0 0;
    color: var(--text-muted);
  }

  .count {
    color: var(--text-muted);
    font-size: 0.8rem;
  }

  .courses {
    display: grid;
    gap: 1.8rem;
    max-width: 58rem;
  }

  .course h3 {
    margin: 0 0 0.55rem;
    color: var(--text-muted);
    font-size: 0.8rem;
    font-weight: 600;
    letter-spacing: 0.06em;
    text-transform: uppercase;
  }

  ul {
    display: grid;
    gap: 0.35rem;
    margin: 0;
    padding: 0;
    list-style: none;
  }

  button {
    display: grid;
    grid-template-columns: auto minmax(0, 1fr) auto;
    align-items: center;
    gap: 0.8rem;
    width: 100%;
    padding: 0.75rem 0.9rem;
    border: 1px solid rgb(255 255 255 / 5%);
    border-radius: 0.6rem;
    background: var(--surface);
    color: inherit;
    font: inherit;
    text-align: left;
    cursor: pointer;
  }

  button:hover:not(:disabled),
  button:focus-visible {
    border-color: rgb(242 180 90 / 35%);
    background: #1b1920;
  }

  button:focus-visible {
    outline: 2px solid var(--accent);
    outline-offset: 2px;
  }

  button:disabled {
    cursor: wait;
    opacity: 0.65;
  }

  .play {
    display: grid;
    width: 2.1rem;
    height: 2.1rem;
    place-items: center;
    border-radius: 50%;
    background: rgb(242 180 90 / 12%);
    color: var(--accent);
  }

  .play svg,
  .empty svg {
    width: 1.15rem;
    fill: none;
    stroke: currentcolor;
    stroke-linecap: round;
    stroke-linejoin: round;
    stroke-width: 1.6;
  }

  .details {
    display: grid;
    min-width: 0;
    gap: 0.18rem;
  }

  .title {
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  .meta {
    color: var(--text-muted);
    font-size: 0.76rem;
    font-variant-numeric: tabular-nums;
  }

  .complete {
    color: #9fd3a8;
  }

  .bar {
    display: block;
    height: 3px;
    overflow: hidden;
    border-radius: 2px;
    background: rgb(255 255 255 / 8%);
  }

  .bar > span {
    display: block;
    height: 100%;
    background: var(--accent);
    transition: width 0.4s linear;
  }

  .action {
    color: var(--accent);
    font-size: 0.78rem;
    white-space: nowrap;
  }

  .empty {
    display: grid;
    max-width: 30rem;
    min-height: 18rem;
    place-content: center;
    justify-items: center;
    margin-top: 2rem;
    border: 1px dashed rgb(255 255 255 / 10%);
    border-radius: 0.8rem;
    color: var(--text-muted);
    text-align: center;
  }

  .empty svg {
    width: 2.5rem;
    margin-bottom: 0.7rem;
  }

  .empty h3 {
    margin: 0;
    color: var(--text);
    font-size: 1rem;
  }

  .spinner {
    width: 0.8rem;
    height: 0.8rem;
    border: 2px solid currentcolor;
    border-right-color: transparent;
    border-radius: 50%;
    animation: spin 0.8s linear infinite;
  }

  @keyframes spin {
    to {
      transform: rotate(360deg);
    }
  }

  @media (max-width: 560px) {
    .action {
      display: none;
    }

    button {
      grid-template-columns: auto minmax(0, 1fr);
    }
  }

  @media (prefers-reduced-motion: reduce) {
    .bar > span {
      transition: none;
    }
    .spinner {
      animation: none;
    }
  }
</style>
