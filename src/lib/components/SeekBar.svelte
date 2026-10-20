<!-- SPDX-License-Identifier: MIT OR Apache-2.0 -->
<script lang="ts">
  import { downloadedRuns, formatTime, fractionAt, playedFraction } from '../seekbar';

  interface Props {
    currentTime: number;
    duration: number;
    /** Piece map cells (0–255) spanning the whole file. */
    pieceMap: readonly number[];
    /** Seek to a fraction (0–1) of the duration. */
    onSeek: (fraction: number) => void;
    /** Whether the user is dragging the thumb (keeps the controls visible). */
    onScrub?: (dragging: boolean) => void;
  }

  let { currentTime, duration, pieceMap, onSeek, onScrub }: Props = $props();

  let bar: HTMLDivElement | undefined = $state();
  let hover = $state<number | null>(null);
  let drag = $state<number | null>(null);

  const runs = $derived(downloadedRuns(pieceMap));
  const played = $derived(drag ?? playedFraction(currentTime, duration));
  const known = $derived(Number.isFinite(duration) && duration > 0);

  function at(event: PointerEvent): number {
    const rect = bar?.getBoundingClientRect();
    return rect ? fractionAt(event.clientX, rect.left, rect.width) : 0;
  }

  function down(event: PointerEvent): void {
    if (!known || event.button !== 0) {
      return;
    }
    bar?.setPointerCapture(event.pointerId);
    drag = at(event);
    onScrub?.(true);
  }

  function move(event: PointerEvent): void {
    hover = at(event);
    if (drag !== null) {
      drag = hover;
    }
  }

  function up(event: PointerEvent): void {
    if (drag === null) {
      return;
    }
    // One seek on release rather than one per pointer move: each seek re-prioritizes downloads.
    const target = at(event);
    drag = null;
    bar?.releasePointerCapture(event.pointerId);
    onScrub?.(false);
    onSeek(target);
  }

  function cancel(): void {
    drag = null;
    onScrub?.(false);
  }

  const pct = (f: number): string => `${(f * 100).toFixed(3)}%`;
</script>

<div
  bind:this={bar}
  class="bar"
  class:dragging={drag !== null}
  role="slider"
  tabindex="-1"
  aria-label="Seek"
  aria-valuemin={0}
  aria-valuemax={known ? Math.round(duration) : 0}
  aria-valuenow={Math.round(currentTime)}
  aria-valuetext={`${formatTime(currentTime)} of ${formatTime(duration)}`}
  onpointerdown={down}
  onpointermove={move}
  onpointerup={up}
  onpointercancel={cancel}
  onpointerleave={() => {
    if (drag === null) {
      hover = null;
    }
  }}
>
  <div class="track">
    {#each runs as run (run.start)}
      <div
        class="downloaded"
        style:left={pct(run.start)}
        style:width={pct(run.end - run.start)}
        style:opacity={run.opacity.toFixed(2)}
      ></div>
    {/each}
    <div class="played" style:width={pct(played)}></div>
  </div>
  {#if known}
    <div class="thumb" style:left={pct(played)}></div>
  {/if}
  {#if hover !== null && known}
    <div class="tooltip" style:left={pct(hover)}>{formatTime(hover * duration)}</div>
  {/if}
</div>

<style>
  .bar {
    position: relative;
    height: 1rem;
    cursor: pointer;
    touch-action: none;
  }

  .track {
    position: absolute;
    left: 0;
    right: 0;
    top: 50%;
    height: 4px;
    overflow: hidden;
    border-radius: 2px;
    /* Gray: the whole video, seekable anywhere. */
    background: rgb(255 255 255 / 18%);
    transform: translateY(-50%);
    transition: height 0.15s ease;
  }

  .bar:hover .track,
  .bar.dragging .track {
    height: 6px;
  }

  .downloaded {
    position: absolute;
    top: 0;
    bottom: 0;
    /* Green: already downloaded. */
    background: #4f9f6a;
  }

  .played {
    position: absolute;
    top: 0;
    bottom: 0;
    left: 0;
    /* White: already watched. */
    background: rgb(255 255 255 / 92%);
  }

  .thumb {
    position: absolute;
    top: 50%;
    width: 0.75rem;
    height: 0.75rem;
    border-radius: 50%;
    background: #fff;
    box-shadow: 0 0 6px rgb(0 0 0 / 50%);
    transform: translate(-50%, -50%) scale(0.8);
    transition: transform 0.15s ease;
  }

  .bar:hover .thumb,
  .bar.dragging .thumb {
    transform: translate(-50%, -50%) scale(1);
  }

  .tooltip {
    position: absolute;
    bottom: 1.2rem;
    padding: 0.15rem 0.4rem;
    border-radius: 0.25rem;
    background: rgb(10 10 12 / 85%);
    color: var(--text);
    font-size: 0.75rem;
    font-variant-numeric: tabular-nums;
    white-space: nowrap;
    transform: translateX(-50%);
    pointer-events: none;
  }
</style>
