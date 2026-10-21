<!-- SPDX-License-Identifier: MIT OR Apache-2.0 -->
<script lang="ts">
  /* An on/off switch. Labelled by a <label for={id}> in the surrounding Field. */
  interface Props {
    id: string;
    checked: boolean;
    disabled?: boolean;
    describedby?: string;
    onchange: (checked: boolean) => void;
  }

  let { id, checked, disabled = false, describedby, onchange }: Props = $props();
</script>

<!-- svelte-ignore a11y_consider_explicit_label (named by the <label for={id}> around it) -->
<button
  {id}
  type="button"
  role="switch"
  class="switch"
  aria-checked={checked}
  aria-describedby={describedby}
  {disabled}
  onclick={() => {
    onchange(!checked);
  }}
>
  <span class="thumb" aria-hidden="true"></span>
</button>

<style>
  .switch {
    position: relative;
    flex: none;
    width: 2.4rem;
    height: 1.35rem;
    padding: 0;
    border: 1px solid rgb(255 255 255 / 12%);
    border-radius: 999px;
    background: rgb(255 255 255 / 8%);
    cursor: pointer;
    transition: background 150ms ease;
  }

  .switch[aria-checked='true'] {
    border-color: transparent;
    background: var(--accent);
  }

  .switch:disabled {
    cursor: default;
    opacity: 0.45;
  }

  .switch:focus-visible {
    outline: 2px solid var(--accent);
    outline-offset: 2px;
  }

  .thumb {
    position: absolute;
    top: 50%;
    left: 0.15rem;
    width: 1rem;
    height: 1rem;
    border-radius: 50%;
    background: var(--text);
    transform: translateY(-50%);
    transition: left 150ms ease;
  }

  .switch[aria-checked='true'] .thumb {
    left: calc(100% - 1.15rem);
    background: #1a1408;
  }

  @media (prefers-reduced-motion: reduce) {
    .switch,
    .thumb {
      transition: none;
    }
  }
</style>
