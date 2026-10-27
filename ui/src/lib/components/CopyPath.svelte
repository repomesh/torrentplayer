<!-- SPDX-License-Identifier: MIT OR Apache-2.0 -->
<script lang="ts">
  import { onDestroy } from 'svelte';

  interface Props {
    id: string;
    path: string;
    label: string;
    describedBy?: string;
    invalid?: boolean;
  }

  let { id, path, label, describedBy, invalid = false }: Props = $props();
  let copiedPath = $state<string | null>(null);
  let error = $state<string | null>(null);
  let timer: ReturnType<typeof setTimeout> | undefined;

  async function copy(): Promise<void> {
    clearTimeout(timer);
    copiedPath = null;
    error = null;
    const location = path;
    try {
      await navigator.clipboard.writeText(location);
      copiedPath = location;
      timer = setTimeout(() => {
        copiedPath = null;
      }, 1500);
    } catch {
      error = 'Could not copy the path. Click the location to select it, then copy it manually.';
    }
  }

  onDestroy(() => {
    clearTimeout(timer);
  });
</script>

<div class="location">
  <div class="row">
    <input
      {id}
      type="text"
      spellcheck="false"
      autocomplete="off"
      readonly
      value={path}
      title={path}
      aria-label={label}
      aria-describedby={describedBy}
      aria-invalid={invalid}
      onclick={(event) => {
        event.currentTarget.select();
      }}
      onfocus={(event) => {
        event.currentTarget.select();
      }}
    />
    <button
      type="button"
      aria-label={`Copy ${label.toLowerCase()} path`}
      aria-live="polite"
      onclick={() => void copy()}
    >
      {copiedPath === path ? 'Copied' : 'Copy path'}
    </button>
  </div>
  {#if error}<p class="error" role="alert">{error}</p>{/if}
</div>

<style>
  .location {
    width: 27rem;
    max-width: 100%;
    min-width: 0;
  }

  .row {
    display: flex;
    align-items: center;
    gap: 0.4rem;
  }

  input {
    flex: 1;
    min-width: 0;
    width: 0;
    box-sizing: border-box;
    padding: 0.4rem 0.6rem;
    border: 1px solid rgb(255 255 255 / 10%);
    border-radius: 0.45rem;
    background: rgb(0 0 0 / 25%);
    color: var(--text);
    font: inherit;
    font-size: 0.85rem;
  }

  button {
    flex: none;
    padding: 0.4rem 0.6rem;
    border: 1px solid rgb(255 255 255 / 15%);
    border-radius: 0.45rem;
    background: rgb(255 255 255 / 5%);
    color: var(--text);
    font: inherit;
    font-size: 0.8rem;
    cursor: pointer;
  }

  input:focus-visible,
  button:focus-visible {
    outline: 2px solid var(--accent);
    outline-offset: 1px;
  }

  .error {
    margin: 0.4rem 0 0;
    color: #e8907a;
    font-size: 0.8rem;
  }
</style>
