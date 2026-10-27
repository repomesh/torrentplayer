<!-- SPDX-License-Identifier: MIT OR Apache-2.0 -->
<script lang="ts">
  /*
   * The notice shown while traffic is held (see `src/lib/networkHold.ts`): why, whether traffic
   * is blocked, and a button that dismisses it for good. `onOpen`, if given, adds a button to
   * Settings → Network & VPN; `onClose` one that only hides it for now.
   */
  import type { NetworkHold } from '../bindings/NetworkHold';
  import { holdNotice } from '../networkHold';

  interface Props {
    /** Why traffic is held. */
    reason: NetworkHold;
    /** Whether traffic is held until the notice is dismissed (Direct mode). */
    held: boolean;
    /** Dismisses the notice for good. */
    onDismiss: () => void;
    /** Opens Settings → Network & VPN. */
    onOpen?: (() => void) | undefined;
    /** Hides the notice until it is shown again. */
    onClose?: () => void;
    /** Why the last dismissal could not be saved, if it failed. */
    error?: string | null;
  }

  let { reason, held, onDismiss, onOpen, onClose, error = null }: Props = $props();

  const notice = $derived(holdNotice(reason, held));
</script>

<div class="notice" role="alert">
  <p>{notice.text}</p>
  {#if notice.held}
    <p class="held">{notice.held}</p>
  {/if}
  <div class="actions">
    <button type="button" class="primary" onclick={onDismiss}>
      {held ? 'Connect directly' : 'Dismiss'}
    </button>
    {#if onOpen}
      <button type="button" class="quiet" onclick={onOpen}>Network settings</button>
    {/if}
    {#if onClose}
      <button type="button" class="quiet" onclick={onClose}>Close</button>
    {/if}
  </div>
  {#if error}
    <p class="error" role="alert">{error}</p>
  {/if}
</div>

<style>
  .notice {
    padding: 0.75rem 1rem;
    border: 1px solid rgb(217 96 79 / 35%);
    border-radius: 0.6rem;
    background: rgb(217 96 79 / 10%);
    color: var(--text);
    font-size: 0.85rem;
    line-height: 1.45;
  }

  p {
    margin: 0;
  }

  .held {
    margin-top: 0.4rem;
    font-weight: 600;
  }

  .error {
    margin-top: 0.4rem;
    color: #e8907a;
  }

  .actions {
    display: flex;
    flex-wrap: wrap;
    gap: 0.4rem 1rem;
    margin-top: 0.6rem;
  }

  button {
    padding: 0;
    border: none;
    background: none;
    font: inherit;
    cursor: pointer;
  }

  .primary {
    color: var(--accent);
    font-weight: 600;
  }

  .quiet {
    color: var(--text-muted);
  }

  .quiet:hover {
    color: var(--text);
  }

  button:focus-visible {
    border-radius: 0.2rem;
    outline: 2px solid var(--accent);
    outline-offset: 2px;
  }
</style>
