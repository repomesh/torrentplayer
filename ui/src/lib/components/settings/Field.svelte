<!-- SPDX-License-Identifier: MIT OR Apache-2.0 -->
<script lang="ts">
  /* One setting: label, status tag, helper text, the control, and its validation message. */
  import type { Snippet } from 'svelte';
  import StatusTag from './StatusTag.svelte';

  interface Props {
    /** The control's id, so the label is linked to it. */
    id: string;
    label: string;
    /** Dotted settings path, for the status tag. */
    path?: string;
    help?: string;
    error?: string | undefined;
    children: Snippet;
  }

  let { id, label, path, help, error, children }: Props = $props();
</script>

<div class="field" class:invalid={error !== undefined}>
  <div class="text">
    <label for={id}>{label}</label>
    {#if path}
      <StatusTag {path} />
    {/if}
    {#if help}
      <p class="help" id="{id}-help">{help}</p>
    {/if}
  </div>
  <div class="control">
    {@render children()}
  </div>
  {#if error}
    <p class="error" id="{id}-error" role="alert">{error}</p>
  {/if}
</div>

<style>
  .field {
    display: grid;
    grid-template-columns: minmax(0, 1fr) auto;
    align-items: center;
    gap: 0.35rem 1.5rem;
    padding: 0.9rem 0;
    border-top: 1px solid rgb(255 255 255 / 5%);
  }

  .field:first-of-type {
    border-top: none;
  }

  label {
    margin-right: 0.5rem;
    color: var(--text);
  }

  .help {
    margin: 0.25rem 0 0;
    color: var(--text-muted);
    font-size: 0.8rem;
    line-height: 1.4;
  }

  .control {
    display: flex;
    align-items: center;
    justify-content: flex-end;
    gap: 0.5rem;
  }

  .error {
    grid-column: 1 / -1;
    margin: 0;
    color: #e8907a;
    font-size: 0.8rem;
  }

  @media (width < 640px) {
    .field {
      grid-template-columns: 1fr;
    }

    .control {
      justify-content: flex-start;
    }
  }
</style>
