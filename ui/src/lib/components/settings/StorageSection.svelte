<!-- SPDX-License-Identifier: MIT OR Apache-2.0 -->
<script lang="ts">
  import { onMount } from 'svelte';
  import { formatBytes } from '../../format';
  import {
    cacheUsage,
    chooseDownloadDir,
    clearCache,
    errorMessage,
    type CacheUsage,
    type Settings,
    type SettingsView,
  } from '../../ipc';
  import Field from './Field.svelte';
  import Toggle from './Toggle.svelte';

  interface Props {
    settings: Settings;
    view: SettingsView;
    errors: Record<string, string>;
    update: (path: string, value: unknown) => void;
    /** Shows settings saved by the folder picker. */
    onView: (view: SettingsView) => void;
  }

  let { settings, view, errors, update, onView }: Props = $props();

  const CLEANUP_DAYS = [7, 14, 30, 90, 365];
  const s = $derived(settings.storage);
  const cleanupChoices = $derived(
    s.cleanupAfterDays === null || CLEANUP_DAYS.includes(s.cleanupAfterDays)
      ? CLEANUP_DAYS
      : [...CLEANUP_DAYS, s.cleanupAfterDays].sort((a, b) => a - b),
  );
  const shownLimit = $derived(s.cacheLimitGb === null ? null : s.cacheLimitGb * 1_000_000_000);

  let usage = $state<CacheUsage | null>(null);
  let usageError = $state<string | null>(null);
  let confirming = $state(false);
  let clearing = $state(false);
  let clearMessage = $state<string | null>(null);
  // The last cache limit typed, restored when "Unlimited" is switched off again.
  let lastLimit = $state(50);
  let picking = $state(false);
  let pickError = $state<string | null>(null);

  /**
   * Opens the system folder picker, which checks and saves the chosen folder: the only way to
   * choose one, so the field is read-only.
   */
  async function pickFolder(): Promise<void> {
    picking = true;
    pickError = null;
    try {
      const saved = await chooseDownloadDir();
      if (saved !== null) {
        onView(saved);
      }
    } catch (reason: unknown) {
      pickError = errorMessage(reason);
    } finally {
      picking = false;
    }
  }

  async function refresh(): Promise<void> {
    usageError = null;
    try {
      usage = await cacheUsage();
    } catch (reason: unknown) {
      usageError = errorMessage(reason);
    }
  }

  async function clear(): Promise<void> {
    clearing = true;
    clearMessage = null;
    try {
      const outcome = await clearCache();
      clearMessage =
        outcome.downloadsRemoved === 0
          ? 'No finished, unqueued downloads could be cleared.'
          : `Cleared ${String(outcome.downloadsRemoved)} ${outcome.downloadsRemoved === 1 ? 'download' : 'downloads'} (${formatBytes(outcome.bytesFreed)}).`;
      confirming = false;
      await refresh();
    } catch (reason: unknown) {
      clearMessage = errorMessage(reason);
    } finally {
      clearing = false;
    }
  }

  onMount(() => {
    void refresh();
  });
</script>

<section class="card" aria-labelledby="storage-title">
  <h3 id="storage-title">Downloads &amp; storage</h3>
  <Field
    id="storage-dir"
    label="Download folder"
    path="storage.downloadDir"
    help="Where TorrentPlayer saves downloaded videos and its cache (in torrentplayer-swarm and librqbit folders inside it). Data already downloaded stays in the old folder. In use now: {view.activeDownloadDir}"
    error={errors['storage.downloadDir'] ?? pickError ?? undefined}
  >
    <div class="folder">
      <input
        id="storage-dir"
        class="wide"
        type="text"
        spellcheck="false"
        autocomplete="off"
        placeholder={view.defaultDownloadDir}
        readonly
        value={s.downloadDir ?? ''}
        aria-describedby="storage-dir-help"
        aria-invalid={errors['storage.downloadDir'] !== undefined}
      />
      <div class="folder-actions">
        <button type="button" disabled={picking} onclick={() => void pickFolder()}>
          {picking ? 'Choosing…' : 'Choose folder…'}
        </button>
        {#if s.downloadDir !== null}
          <button
            type="button"
            onclick={() => {
              update('storage.downloadDir', null);
            }}>Use default</button
          >
        {/if}
      </div>
    </div>
  </Field>
  <Field
    id="storage-limit"
    label="Cache size limit"
    path="storage.cacheLimitGb"
    help="The oldest downloaded data is removed to stay under the limit."
    error={errors['storage.cacheLimitGb']}
  >
    <input
      id="storage-limit"
      class="number"
      type="number"
      min="1"
      max="100000"
      step="1"
      value={s.cacheLimitGb ?? lastLimit}
      disabled={s.cacheLimitGb === null}
      aria-describedby="storage-limit-help"
      aria-invalid={errors['storage.cacheLimitGb'] !== undefined}
      oninput={(e: Event) => {
        const value = (e.currentTarget as HTMLInputElement).valueAsNumber;
        if (Number.isInteger(value) && value > 0) {
          lastLimit = value;
        }
        update('storage.cacheLimitGb', value);
      }}
    />
    <span class="unit">GB</span>
    <label class="inline">
      <Toggle
        id="storage-unlimited"
        checked={s.cacheLimitGb === null}
        onchange={(unlimited) => {
          update('storage.cacheLimitGb', unlimited ? null : lastLimit);
        }}
      />
      Unlimited
    </label>
  </Field>
  <Field
    id="storage-cleanup"
    label="Clean up finished downloads"
    path="storage.cleanupAfterDays"
    help="Remove downloaded data for videos and tracks you finished this long ago."
    error={errors['storage.cleanupAfterDays']}
  >
    <select
      id="storage-cleanup"
      value={s.cleanupAfterDays ?? 'never'}
      onchange={(e: Event) => {
        const value = (e.currentTarget as HTMLSelectElement).value;
        update('storage.cleanupAfterDays', value === 'never' ? null : Number(value));
      }}
    >
      <option value="never">Never</option>
      {#each cleanupChoices as days (days)}
        <option value={days}>After {days} days</option>
      {/each}
    </select>
  </Field>
  <Field
    id="storage-clear"
    label="Cache"
    help="TorrentPlayer's downloaded data (the torrentplayer-swarm and librqbit folders inside the download folder). Nothing else is touched."
  >
    <div class="cache">
      <span class="usage" aria-live="polite">
        {#if usageError}
          {usageError}
        {:else if usage}
          {usage.complete ? '' : 'At least '}{formatBytes(usage.bytes)}{shownLimit === null
            ? ' · Unlimited'
            : ` of ${formatBytes(shownLimit)}`}
          · {usage.files}
          {usage.files === 1 ? 'file' : 'files'}{usage.reclaimableBytes > 0
            ? ` · ${formatBytes(usage.reclaimableBytes)} finished`
            : ''}
        {:else}
          Measuring…
        {/if}
      </span>
      {#if !confirming}
        <button
          id="storage-clear"
          type="button"
          disabled={clearing || usage?.reclaimableDownloads === 0}
          onclick={() => {
            confirming = true;
            clearMessage = null;
          }}>Clear now…</button
        >
      {/if}
    </div>
  </Field>
  {#if confirming}
    <div class="confirm" role="group" aria-labelledby="storage-confirm-text">
      <p id="storage-confirm-text">
        Delete {usage ? formatBytes(usage.reclaimableBytes) : 'finished'} of finished downloads that are
        no longer in the queue? Playing and queued downloads are kept.
      </p>
      <div class="actions">
        <button type="button" class="danger" disabled={clearing} onclick={() => void clear()}>
          {clearing ? 'Clearing…' : 'Clear finished downloads'}
        </button>
        <button
          type="button"
          disabled={clearing}
          onclick={() => {
            confirming = false;
          }}>Cancel</button
        >
      </div>
    </div>
  {/if}
  {#if clearMessage}
    <p class="note" role="status">{clearMessage}</p>
  {/if}
</section>

<style>
  .folder {
    display: flex;
    flex-direction: column;
    align-items: flex-end;
    gap: 0.4rem;
  }

  .folder-actions {
    display: flex;
    gap: 0.4rem;
  }

  .cache {
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    justify-content: flex-end;
    gap: 0.75rem;
  }

  .usage {
    color: var(--text-muted);
    font-size: 0.85rem;
    font-variant-numeric: tabular-nums;
  }

  .confirm {
    margin: 0.25rem 0 0.75rem;
    padding: 0.9rem 1rem;
    border: 1px solid rgb(232 144 122 / 30%);
    border-radius: 0.6rem;
    background: rgb(232 144 122 / 6%);
  }

  .confirm p {
    margin: 0 0 0.75rem;
  }

  .actions {
    display: flex;
    gap: 0.5rem;
  }
</style>
