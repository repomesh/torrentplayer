<!-- SPDX-License-Identifier: MIT OR Apache-2.0 -->
<script lang="ts">
  /*
   * The network indicator, from the backend's real state: gray when direct, green when traffic
   * goes through the VPN interface or proxy, yellow while that is being checked, red when the
   * kill switch pauses traffic or the settings cannot work. Beside it, the public address the
   * internet sees (unless turned off in Settings → Network), hidden until the eye button reveals
   * it (remembered); the tooltip has the mode, endpoint, public IPv4 and IPv6, uptime, and latest
   * events; clicking opens Settings → Network.
   */
  import { onMount } from 'svelte';
  import type { NetworkStatus, PublicIp } from '../ipc';
  import { describeNetwork, describePublicIp, followNetwork, followPublicIp } from '../network';

  interface Props {
    /** Opens Settings → Network. Without it the indicator is not clickable. */
    onOpen?: () => void;
  }

  let { onOpen }: Props = $props();

  let status = $state<NetworkStatus | null>(null);
  let ip = $state<PublicIp | null>(null);

  /** Remembered per computer: whether the public address is shown or masked. */
  const REVEAL_KEY = 'torrentplayer.revealIp';
  /** The key before the rename to TorrentPlayer. */
  const LEGACY_REVEAL_KEY = 'torrentstream.revealIp';
  function readReveal(): boolean {
    try {
      return (
        (localStorage.getItem(REVEAL_KEY) ?? localStorage.getItem(LEGACY_REVEAL_KEY)) === 'true'
      );
    } catch {
      return false;
    }
  }
  let revealed = $state(readReveal());
  function toggleReveal(): void {
    revealed = !revealed;
    try {
      localStorage.setItem(REVEAL_KEY, String(revealed));
    } catch {
      // Not remembered; the choice still applies now.
    }
  }
  // Refreshed when the tooltip is about to show, so the uptime is current.
  let now = $state(Date.now());

  const summary = $derived(status === null ? null : describeNetwork(status, now));
  const address = $derived(ip === null ? null : describePublicIp(ip, now, !revealed));
  const tooltip = $derived(
    summary === null
      ? ''
      : [
          ...summary.details,
          ...(address ? ['', ...address.details] : []),
          ...(onOpen ? ['', 'Click for network settings'] : []),
        ].join('\n'),
  );

  function refresh(): void {
    now = Date.now();
  }

  onMount(() => {
    const stopStatus = followNetwork((next) => {
      status = next;
    });
    const stopIp = followPublicIp((next) => {
      ip = next;
    });
    return () => {
      stopStatus();
      stopIp();
    };
  });
</script>

{#snippet eye()}
  {#if address}
    <button
      type="button"
      class="eye"
      aria-pressed={revealed}
      aria-label={revealed ? 'Hide public IP address' : 'Show public IP address'}
      title={revealed ? 'Hide IP address' : 'Show IP address'}
      onclick={toggleReveal}
    >
      {#if revealed}
        <svg viewBox="0 0 24 24" aria-hidden="true"
          ><path
            d="M2.5 12s3.5-6.5 9.5-6.5 9.5 6.5 9.5 6.5-3.5 6.5-9.5 6.5S2.5 12 2.5 12Z"
          /><circle cx="12" cy="12" r="2.8" /></svg
        >
      {:else}
        <svg viewBox="0 0 24 24" aria-hidden="true"
          ><path
            d="M3 3l18 18M10.6 5.6A10 10 0 0 1 12 5.5c6 0 9.5 6.5 9.5 6.5a17 17 0 0 1-2.9 3.7M6.4 6.9C3.9 8.6 2.5 12 2.5 12S6 18.5 12 18.5a9.6 9.6 0 0 0 4.3-1M9.9 9.9a2.8 2.8 0 0 0 4.2 4.2"
          /></svg
        >
      {/if}
    </button>
  {/if}
{/snippet}

{#if summary}
  {#if onOpen}
    <button
      type="button"
      class="network {summary.tone}"
      title={tooltip}
      aria-label={`${summary.label}${address && revealed ? `. Public address ${address.short}` : ''}. Network settings`}
      onpointerenter={refresh}
      onfocus={refresh}
      onclick={onOpen}
    >
      <span class="dot" aria-hidden="true"></span>
      <span class="label">{summary.label}</span>
      {#if address}<span class="ip">{address.short}</span>{/if}
    </button>
    <!-- eslint-disable-next-line @typescript-eslint/no-confusing-void-expression -- a local snippet is typed as returning void -->
    {@render eye()}
  {:else}
    <span class="network {summary.tone}" title={tooltip} role="status" onpointerenter={refresh}>
      <span class="dot" aria-hidden="true"></span>
      <span class="label">{summary.label}</span>
      {#if address}<span class="ip">{address.short}</span>{/if}
    </span>
    <!-- eslint-disable-next-line @typescript-eslint/no-confusing-void-expression -- a local snippet is typed as returning void -->
    {@render eye()}
  {/if}
{/if}

<style>
  .network {
    display: inline-flex;
    align-items: center;
    gap: 0.4rem;
    max-width: 36rem;
    min-width: 0;
    padding: 0;
    border: none;
    background: none;
    color: inherit;
    font: inherit;
    white-space: nowrap;
  }

  button.network {
    border-radius: 0.3rem;
    cursor: pointer;
  }

  button.network:hover .label {
    color: var(--text);
  }

  button.network:focus-visible {
    outline: 2px solid var(--accent);
    outline-offset: 2px;
  }

  .label {
    overflow: hidden;
    text-overflow: ellipsis;
  }

  .ip {
    flex: none;
    color: var(--text-muted);
    font-variant-numeric: tabular-nums;
  }

  .ip::before {
    content: '·';
    margin-right: 0.4rem;
  }

  .eye {
    display: inline-flex;
    flex: none;
    margin-left: -0.15rem;
    padding: 0.15rem;
    border: none;
    border-radius: 0.3rem;
    background: none;
    color: inherit;
    opacity: 0.7;
    cursor: pointer;
  }

  .eye:hover {
    opacity: 1;
  }

  .eye:focus-visible {
    outline: 2px solid var(--accent);
    outline-offset: 1px;
  }

  .eye svg {
    width: 0.85rem;
    height: 0.85rem;
    fill: none;
    stroke: currentcolor;
    stroke-width: 1.7;
    stroke-linecap: round;
    stroke-linejoin: round;
  }

  .dot {
    flex: none;
    width: 0.45rem;
    height: 0.45rem;
    border-radius: 50%;
    background: #6b6862;
  }

  .protected .dot {
    background: #57b26f;
  }

  .pending .dot {
    background: #d9b44a;
  }

  .blocked .dot {
    background: #d9604f;
  }

  .blocked .label {
    color: #e8907a;
  }
</style>
