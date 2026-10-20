<!-- SPDX-License-Identifier: MIT OR Apache-2.0 -->
<script lang="ts">
  /*
   * A write-only secret (proxy password, WireGuard key). The value goes straight to the OS
   * credential store; the page only ever learns whether one is set.
   */
  import {
    clearSecret,
    errorMessage,
    setSecret,
    type SecretKey,
    type SecretStatus,
  } from '../../ipc';

  interface Props {
    id: string;
    secret: SecretKey;
    isSet: boolean;
    /** False when the system has no usable credential store. */
    available: boolean;
    onStatus: (status: SecretStatus) => void;
  }

  let { id, secret, isSet, available, onStatus }: Props = $props();

  let editing = $state(false);
  let value = $state('');
  let busy = $state(false);
  let error = $state<string | null>(null);
  let input: HTMLInputElement | undefined = $state();

  function start(): void {
    editing = true;
    error = null;
    queueMicrotask(() => input?.focus());
  }

  function cancel(): void {
    editing = false;
    value = '';
    error = null;
  }

  async function run(action: () => Promise<SecretStatus>): Promise<void> {
    busy = true;
    error = null;
    try {
      onStatus(await action());
      cancel();
    } catch (reason: unknown) {
      error = errorMessage(reason);
    } finally {
      busy = false;
    }
  }

  function save(event: SubmitEvent): void {
    event.preventDefault();
    if (value.length > 0 && !busy) {
      void run(() => setSecret(secret, value));
    }
  }
</script>

<div class="wrap">
  {#if editing}
    <form class="secret" onsubmit={save}>
      <input
        {id}
        bind:this={input}
        bind:value
        type="password"
        autocomplete="off"
        spellcheck="false"
        disabled={busy}
        aria-invalid={error !== null}
        onkeydown={(event: KeyboardEvent) => {
          if (event.key === 'Escape') {
            event.stopPropagation();
            cancel();
          }
        }}
      />
      <button type="submit" class="primary" disabled={busy || value.length === 0}>Save</button>
      <button type="button" disabled={busy} onclick={cancel}>Cancel</button>
    </form>
  {:else}
    <div class="secret">
      <span class="state" class:set={isSet}>{isSet ? '••••••• set' : 'Not set'}</span>
      <button {id} type="button" disabled={!available || busy} onclick={start}>
        {isSet ? 'Replace' : 'Set'}
      </button>
      {#if isSet}
        <button type="button" disabled={busy} onclick={() => void run(() => clearSecret(secret))}>
          Clear
        </button>
      {/if}
    </div>
  {/if}
  {#if error}
    <p class="error" role="alert">{error}</p>
  {/if}
</div>

<style>
  .wrap {
    display: flex;
    flex-direction: column;
    align-items: flex-end;
  }

  .secret {
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    justify-content: flex-end;
    gap: 0.5rem;
  }

  .state {
    color: var(--text-muted);
    font-size: 0.85rem;
  }

  .state.set {
    color: var(--text);
    letter-spacing: 0.04em;
  }

  .error {
    margin: 0.35rem 0 0;
    color: #e8907a;
    font-size: 0.8rem;
    text-align: right;
  }
</style>
