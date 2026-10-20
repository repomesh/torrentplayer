<!-- SPDX-License-Identifier: MIT OR Apache-2.0 -->
<script lang="ts">
  /* The "Up next" card shown when a video ends and the queue has another one. */
  interface Props {
    title: string;
    secondsLeft: number;
    onPlayNow: () => void;
    onCancel: () => void;
  }

  let { title, secondsLeft, onPlayNow, onCancel }: Props = $props();
</script>

<div class="upnext" role="status" aria-live="polite">
  <p class="when">Up next in {secondsLeft} s</p>
  <p class="title" {title}>{title}</p>
  <div class="actions">
    <button type="button" class="play" onclick={onPlayNow}>Play now</button>
    <button type="button" class="cancel" title="Cancel (Esc)" onclick={onCancel}>Cancel</button>
  </div>
</div>

<style>
  .upnext {
    position: absolute;
    z-index: 4;
    right: 1.25rem;
    bottom: 6rem;
    display: grid;
    gap: 0.35rem;
    width: min(20rem, calc(100% - 2.5rem));
    padding: 0.8rem 0.9rem;
    box-sizing: border-box;
    border: 1px solid rgb(255 255 255 / 8%);
    border-radius: 0.7rem;
    background: rgb(16 16 20 / 92%);
    box-shadow: 0 0.5rem 1.5rem rgb(0 0 0 / 45%);
    cursor: auto;
  }

  p {
    margin: 0;
  }

  .when {
    color: var(--accent);
    font-size: 0.78rem;
    font-variant-numeric: tabular-nums;
  }

  .title {
    overflow: hidden;
    color: var(--text);
    font-size: 0.9rem;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  .actions {
    display: flex;
    gap: 0.4rem;
    margin-top: 0.2rem;
  }

  button {
    padding: 0.4rem 0.85rem;
    border: none;
    border-radius: 0.45rem;
    font: inherit;
    font-size: 0.85rem;
    cursor: pointer;
  }

  button:focus-visible {
    outline: 2px solid var(--accent);
    outline-offset: 1px;
  }

  .play {
    background: var(--accent);
    color: #1a1408;
    font-weight: 600;
  }

  .cancel {
    border: 1px solid #3a3731;
    background: transparent;
    color: var(--text);
  }
</style>
