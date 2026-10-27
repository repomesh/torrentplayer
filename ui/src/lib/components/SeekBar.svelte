<!-- SPDX-License-Identifier: MIT OR Apache-2.0 -->
<script lang="ts">
  import {
    formatTime,
    fractionAt,
    playedFraction,
    thumbnailTime,
    type ThumbnailSource,
  } from '../seekbar';
  import DownloadedChunks from './DownloadedChunks.svelte';

  interface Props {
    currentTime: number;
    duration: number;
    /** Piece map cells (0–255) spanning the whole file. */
    pieceMap: readonly number[];
    /** Seek to a fraction (0–1) of the duration. */
    onSeek: (fraction: number) => void;
    /** Whether the user is dragging the thumb (keeps the controls visible). */
    onScrub?: (dragging: boolean) => void;
    /** Pictures of the video to show above the pointer, if any. */
    thumbnails?: ThumbnailSource | null;
  }

  let { currentTime, duration, pieceMap, onSeek, onScrub, thumbnails = null }: Props = $props();

  /** Pointer rest before a thumbnail is asked for, so sweeping across asks for nothing. */
  const PREVIEW_DELAY_MS = 120;

  let bar: HTMLDivElement | undefined = $state();
  let hover = $state<number | null>(null);
  let drag = $state<number | null>(null);

  const played = $derived(drag ?? playedFraction(currentTime, duration));
  const known = $derived(Number.isFinite(duration) && duration > 0);
  const previewSecs = $derived(
    hover === null || thumbnails === null ? null : thumbnailTime(hover, duration),
  );
  let preview = $state<string | null>(null);

  $effect(() => {
    const secs = previewSecs;
    const source = thumbnails;
    if (secs === null || source === null) {
      preview = null;
      return;
    }
    const remembered = source.peek(secs);
    if (remembered !== undefined) {
      preview = remembered;
      return;
    }
    // Do not show the previous bucket's frame under the new bucket's timestamp while this one
    // is waiting for its debounce or FFmpeg.
    preview = null;
    let stale = false;
    const timer = setTimeout(() => {
      source
        .get(secs)
        .then((picture) => {
          if (!stale) {
            preview = picture;
          }
        })
        .catch(() => {
          if (!stale) {
            preview = null;
          }
        });
    }, PREVIEW_DELAY_MS);
    return () => {
      stale = true;
      clearTimeout(timer);
    };
  });

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
    <DownloadedChunks cells={pieceMap} />
    <div class="played" style:width={pct(played)}></div>
  </div>
  {#if known}
    <div class="thumb" style:left={pct(played)}></div>
  {/if}
  {#if hover !== null && known}
    <!-- With a picture, kept inside the bar so it is not cut off at the ends. -->
    <div
      class="hover"
      style:left={preview === null ? pct(hover) : `clamp(5rem, ${pct(hover)}, calc(100% - 5rem))`}
    >
      {#if preview !== null}
        <img class="preview" src={preview} alt="" />
      {/if}
      <span class="tooltip">{formatTime(hover * duration)}</span>
    </div>
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

  .hover {
    position: absolute;
    bottom: 1.2rem;
    display: flex;
    flex-direction: column;
    align-items: center;
    gap: 0.3rem;
    transform: translateX(-50%);
    pointer-events: none;
  }

  .preview {
    display: block;
    width: 10rem;
    max-width: none;
    border: 1px solid rgb(255 255 255 / 25%);
    border-radius: 0.3rem;
    background: #000;
    box-shadow: 0 2px 10px rgb(0 0 0 / 60%);
  }

  .tooltip {
    padding: 0.15rem 0.4rem;
    border-radius: 0.25rem;
    background: rgb(10 10 12 / 85%);
    color: var(--text);
    font-size: 0.75rem;
    font-variant-numeric: tabular-nums;
    white-space: nowrap;
  }
</style>
