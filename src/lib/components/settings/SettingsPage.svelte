<!-- SPDX-License-Identifier: MIT OR Apache-2.0 -->
<script lang="ts">
  /*
   * The Settings page: a section list on the left, grouped cards on the right. Every change is
   * validated as it is made and saved shortly after (merged into one update), with a quiet
   * "Saved" confirmation. Esc or Back returns to the previous screen.
   */
  import { onDestroy, onMount, untrack } from 'svelte';
  import {
    errorMessage,
    getSettings,
    secretStatus,
    updateSettings,
    type SecretStatus,
    type Settings,
    type SettingsView,
  } from '../../ipc';
  import {
    createSaver,
    fieldError,
    patchAt,
    validate,
    withPatch,
    type SettingsSection,
  } from '../../settings';
  import AboutSection from './AboutSection.svelte';
  import AdvancedSection from './AdvancedSection.svelte';
  import BandwidthSection from './BandwidthSection.svelte';
  import NetworkSection from './NetworkSection.svelte';
  import PlaybackSection from './PlaybackSection.svelte';
  import SearchSection from './SearchSection.svelte';
  import StorageSection from './StorageSection.svelte';

  interface Props {
    onBack: () => void;
    /** The section shown first. */
    section?: SettingsSection;
    /** Where Back goes, for its label. */
    backLabel?: string;
  }

  let { onBack, section = 'playback', backLabel = 'Back' }: Props = $props();

  type SectionId = SettingsSection;

  const SECTIONS: { id: SectionId; label: string }[] = [
    { id: 'playback', label: 'Playback' },
    { id: 'storage', label: 'Downloads & storage' },
    { id: 'bandwidth', label: 'Bandwidth & seeding' },
    { id: 'search', label: 'Search' },
    { id: 'network', label: 'Network & VPN' },
    { id: 'advanced', label: 'Advanced' },
    { id: 'about', label: 'About' },
  ];

  const RESTART_NAMES: Record<string, string> = {
    'storage.downloadDir': 'download folder',
    'advanced.engine': 'torrent engine',
  };

  let view = $state<SettingsView | null>(null);
  // What the page shows: the saved settings plus changes not yet saved.
  let local = $state<Settings | null>(null);
  let errors = $state<Record<string, string>>({});
  let loadError = $state<string | null>(null);
  let secrets = $state<SecretStatus | null>(null);
  let active = $state<SectionId>(untrack(() => section));
  let toast = $state<{ text: string; error: boolean } | null>(null);
  let toastTimer: ReturnType<typeof setTimeout> | undefined;
  let navButtons = $state<HTMLButtonElement[]>([]);

  function showToast(text: string, error = false): void {
    toast = { text, error };
    clearTimeout(toastTimer);
    toastTimer = setTimeout(
      () => {
        toast = null;
      },
      error ? 6000 : 1600,
    );
  }

  function showSaved(next: SettingsView): void {
    view = next;
    local = withPatch(next.settings, saver.pending() ?? {});
  }

  const saver = createSaver({
    save: (patch) => updateSettings(patch),
    onSaved: (next, more) => {
      showSaved(next);
      if (!more) {
        showToast('Saved');
      }
    },
    onError: (reason) => {
      const field = fieldError(reason);
      if (field) {
        errors = { ...errors, [field.field]: field.message };
      }
      showToast(`Not saved: ${field ? field.message : errorMessage(reason)}`, true);
      if (view) {
        showSaved(view);
      }
    },
  });

  function update(path: string, value: unknown): void {
    const problem = validate(path, value);
    if (problem !== null) {
      errors = { ...errors, [path]: problem };
      return;
    }
    if (path in errors) {
      errors = Object.fromEntries(Object.entries(errors).filter(([key]) => key !== path));
    }
    const patch = patchAt(path, value);
    if (local) {
      local = withPatch(local, patch);
    }
    saver.queue(patch);
  }

  function refreshSecrets(): void {
    secretStatus()
      .then((status) => {
        secrets = status;
      })
      .catch(() => undefined);
  }

  async function leave(): Promise<void> {
    await saver.flush();
    onBack();
  }

  function select(id: SectionId): void {
    active = id;
  }

  function onNavKey(event: KeyboardEvent, index: number): void {
    const step = event.key === 'ArrowDown' ? 1 : event.key === 'ArrowUp' ? -1 : 0;
    if (step === 0) {
      return;
    }
    event.preventDefault();
    const next = (index + step + SECTIONS.length) % SECTIONS.length;
    const section = SECTIONS[next];
    if (section) {
      select(section.id);
      navButtons[next]?.focus();
    }
  }

  function onKeydown(event: KeyboardEvent): void {
    if (event.key === 'Escape' && !event.defaultPrevented) {
      event.preventDefault();
      void leave();
    }
  }

  onMount(() => {
    getSettings()
      .then((next) => {
        view = next;
        local = next.settings;
      })
      .catch((reason: unknown) => {
        loadError = errorMessage(reason);
      });
    refreshSecrets();
  });

  onDestroy(() => {
    void saver.flush();
    clearTimeout(toastTimer);
  });
</script>

<svelte:window onkeydown={onKeydown} />

<div class="page">
  <header>
    <button type="button" class="back" onclick={() => void leave()}>← {backLabel}</button>
    <h2>Settings</h2>
  </header>
  <div class="layout">
    <nav aria-label="Settings sections">
      <ul>
        {#each SECTIONS as section, i (section.id)}
          <li>
            <button
              type="button"
              bind:this={navButtons[i]}
              class:active={active === section.id}
              aria-current={active === section.id ? 'page' : undefined}
              onclick={() => {
                select(section.id);
              }}
              onkeydown={(event: KeyboardEvent) => {
                onNavKey(event, i);
              }}>{section.label}</button
            >
          </li>
        {/each}
      </ul>
    </nav>
    <div class="content">
      {#if loadError}
        <p class="load-error" role="alert">Settings could not be loaded: {loadError}</p>
      {:else if !view || !local}
        <p class="muted">Loading…</p>
      {:else}
        {#if view.warnings.length > 0}
          <ul class="warnings" role="status">
            {#each view.warnings as warning, i (i)}
              <li>{warning}</li>
            {/each}
          </ul>
        {/if}
        {#if view.restartRequired.length > 0}
          <p class="restart" role="status">
            Restart TorrentStream to use the new {view.restartRequired
              .map((path) => RESTART_NAMES[path] ?? path)
              .join(' and ')}.
          </p>
        {/if}
        {#if active === 'playback'}
          <PlaybackSection settings={local} {errors} {update} />
        {:else if active === 'storage'}
          <StorageSection settings={local} {view} {errors} {update} />
        {:else if active === 'bandwidth'}
          <BandwidthSection settings={local} {errors} {update} />
        {:else if active === 'search'}
          <SearchSection settings={local} {errors} {update} />
        {:else if active === 'network'}
          <NetworkSection
            settings={local}
            {errors}
            {update}
            {secrets}
            onSecrets={(status: SecretStatus) => {
              secrets = status;
            }}
            onView={(next: SettingsView) => {
              showSaved(next);
              refreshSecrets();
            }}
          />
        {:else if active === 'advanced'}
          <AdvancedSection settings={local} {view} {update} />
        {:else}
          <AboutSection />
        {/if}
      {/if}
    </div>
  </div>
  <div class="toast-region" aria-live="polite">
    {#if toast}
      <span class="toast" class:error={toast.error}>{toast.text}</span>
    {/if}
  </div>
</div>

<style>
  .page {
    position: relative;
    box-sizing: border-box;
    height: 100%;
    overflow-y: auto;
    padding: 2rem clamp(1rem, 5vw, 4rem) 3rem;
    background: linear-gradient(180deg, #17150f 0%, var(--bg) 35%);
  }

  header {
    max-width: 64rem;
    margin: 0 auto 1.5rem;
  }

  h2 {
    margin: 0.75rem 0 0;
    font-family: var(--font-display);
    font-size: clamp(1.6rem, 3.5vw, 2.4rem);
    font-weight: 400;
  }

  .back {
    padding: 0;
    border: none;
    background: none;
    color: var(--text-muted);
    font: inherit;
    cursor: pointer;
  }

  .back:hover {
    color: var(--text);
  }

  .layout {
    display: grid;
    grid-template-columns: 12rem minmax(0, 1fr);
    gap: 2rem;
    max-width: 64rem;
    margin: 0 auto;
  }

  nav {
    position: sticky;
    top: 0;
    align-self: start;
  }

  nav ul {
    display: flex;
    flex-direction: column;
    gap: 0.15rem;
    margin: 0;
    padding: 0;
    list-style: none;
  }

  nav button {
    width: 100%;
    padding: 0.5rem 0.75rem;
    border: none;
    border-radius: 0.5rem;
    background: none;
    color: var(--text-muted);
    font: inherit;
    font-size: 0.9rem;
    text-align: left;
    cursor: pointer;
  }

  nav button:hover {
    color: var(--text);
  }

  nav button.active {
    background: var(--surface);
    color: var(--text);
  }

  .content {
    display: flex;
    flex-direction: column;
    gap: 1.25rem;
    min-width: 0;
  }

  .muted {
    color: var(--text-muted);
  }

  .load-error {
    color: #e8907a;
  }

  .warnings,
  .restart {
    margin: 0;
    padding: 0.75rem 1rem;
    border-radius: 0.6rem;
    background: rgb(242 180 90 / 8%);
    font-size: 0.85rem;
    line-height: 1.45;
  }

  .warnings {
    padding-left: 2rem;
  }

  .toast-region {
    position: fixed;
    right: 1.5rem;
    bottom: 2.75rem;
    pointer-events: none;
  }

  .toast {
    display: inline-block;
    padding: 0.4rem 0.9rem;
    border-radius: 999px;
    background: rgb(21 21 28 / 92%);
    box-shadow: 0 4px 18px rgb(0 0 0 / 35%);
    color: var(--text-muted);
    font-size: 0.8rem;
    animation: fade-in 150ms ease;
  }

  .toast.error {
    color: #e8907a;
  }

  @keyframes fade-in {
    from {
      opacity: 0;
    }
  }

  @media (prefers-reduced-motion: reduce) {
    .toast {
      animation: none;
    }
  }

  @media (width < 760px) {
    .layout {
      grid-template-columns: 1fr;
      gap: 1rem;
    }

    nav {
      position: static;
      overflow-x: auto;
    }

    nav ul {
      flex-direction: row;
    }

    nav button {
      white-space: nowrap;
    }
  }

  /* Shared by the section components. */
  .page :global(.card) {
    padding: 1.25rem 1.5rem;
    border: 1px solid rgb(255 255 255 / 6%);
    border-radius: 0.9rem;
    background: rgb(255 255 255 / 2.5%);
  }

  .page :global(.card h3) {
    margin: 0 0 0.75rem;
    font-family: var(--font-display);
    font-size: 1.25rem;
    font-weight: 400;
  }

  .page :global(.card h4) {
    margin: 1rem 0 0.25rem;
    color: var(--text-muted);
    font-size: 0.75rem;
    font-weight: 500;
    letter-spacing: 0.08em;
    text-transform: uppercase;
  }

  .page :global(.card h3 + h4) {
    margin-top: 0;
  }

  .page :global(input[type='text']),
  .page :global(input[type='number']),
  .page :global(input[type='password']),
  .page :global(select) {
    box-sizing: border-box;
    padding: 0.4rem 0.6rem;
    border: 1px solid rgb(255 255 255 / 10%);
    border-radius: 0.45rem;
    background: rgb(0 0 0 / 25%);
    color: var(--text);
    font: inherit;
    font-size: 0.9rem;
  }

  .page :global(input:disabled),
  .page :global(select:disabled) {
    opacity: 0.5;
  }

  .page :global(input[aria-invalid='true']) {
    border-color: rgb(232 144 122 / 70%);
  }

  .page :global(input:focus-visible),
  .page :global(select:focus-visible),
  .page :global(button:focus-visible) {
    outline: 2px solid var(--accent);
    outline-offset: 1px;
  }

  .page :global(input.wide) {
    width: min(22rem, 100%);
  }

  .page :global(input.number) {
    width: 6.5rem;
    font-variant-numeric: tabular-nums;
  }

  .page :global(.unit) {
    color: var(--text-muted);
    font-size: 0.85rem;
  }

  .page :global(label.inline) {
    display: inline-flex;
    gap: 0.4rem;
    align-items: center;
    color: var(--text-muted);
    font-size: 0.85rem;
    cursor: pointer;
  }

  .page :global(.card button:not([role='switch'])) {
    padding: 0.4rem 0.85rem;
    border: 1px solid rgb(255 255 255 / 12%);
    border-radius: 0.45rem;
    background: rgb(255 255 255 / 4%);
    color: var(--text);
    font: inherit;
    font-size: 0.85rem;
    cursor: pointer;
  }

  .page :global(.card button:not([role='switch']):hover:not(:disabled)) {
    background: rgb(255 255 255 / 8%);
  }

  .page :global(.card button:disabled) {
    cursor: default;
    opacity: 0.5;
  }

  .page :global(.card button.primary) {
    border-color: transparent;
    background: var(--accent);
    color: #1a1408;
  }

  .page :global(.card button.primary:hover:not(:disabled)) {
    background: #f6c477;
  }

  .page :global(.card button.danger) {
    border-color: transparent;
    background: #c9634b;
    color: #fff;
  }

  .page :global(.card button.danger-quiet) {
    color: #e8907a;
  }

  .page :global(.note) {
    margin: 0.5rem 0 0;
    color: var(--text-muted);
    font-size: 0.85rem;
  }

  .page :global(code) {
    font-family: ui-monospace, 'Cascadia Mono', Consolas, monospace;
    font-size: 0.85em;
  }
</style>
