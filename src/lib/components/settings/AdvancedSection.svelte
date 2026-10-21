<!-- SPDX-License-Identifier: MIT OR Apache-2.0 -->
<script lang="ts">
  import { onMount } from 'svelte';
  import {
    chooseFfmpegDir,
    errorMessage,
    mediaTools,
    type MediaToolsDto,
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
  }

  let { settings, view, errors, update }: Props = $props();

  /** Longer than the settings page takes to save a change, so a new folder is searched. */
  const RECHECK_AFTER_MS = 600;

  const a = $derived(settings.advanced);
  let tools = $state<MediaToolsDto | null>(null);
  let toolsError = $state<string | null>(null);
  let checking = $state(false);
  let picking = $state(false);
  let checkRequest = 0;

  /** Looks for ffprobe and ffmpeg again, with the saved folder setting. */
  async function check(): Promise<void> {
    const request = ++checkRequest;
    checking = true;
    toolsError = null;
    try {
      const found = await mediaTools();
      if (request === checkRequest) {
        tools = found;
      }
    } catch (reason: unknown) {
      if (request === checkRequest) {
        toolsError = errorMessage(reason);
      }
    } finally {
      if (request === checkRequest) {
        checking = false;
      }
    }
  }

  function setFolder(folder: string | null): void {
    update('advanced.ffmpegDir', folder);
    setTimeout(() => void check(), RECHECK_AFTER_MS);
  }

  async function pickFolder(): Promise<void> {
    picking = true;
    toolsError = null;
    try {
      const folder = await chooseFfmpegDir();
      if (folder !== null) {
        setFolder(folder);
      }
    } catch (reason: unknown) {
      toolsError = errorMessage(reason);
    } finally {
      picking = false;
    }
  }

  const toolsSummary = $derived.by(() => {
    if (toolsError !== null) {
      return toolsError;
    }
    if (tools === null) {
      return 'Looking for FFmpeg…';
    }
    const where = a.ffmpegDir === null ? 'on PATH' : 'in this folder';
    if (tools.ffprobe === null && tools.ffmpeg === null) {
      return `FFmpeg was not found ${where}.`;
    }
    const found = [tools.ffprobe && 'ffprobe', tools.ffmpeg && 'ffmpeg'].filter(Boolean);
    const missing = [!tools.ffprobe && 'ffprobe', !tools.ffmpeg && 'ffmpeg'].filter(Boolean);
    const version = (tools.ffmpeg ?? tools.ffprobe)?.version;
    return (
      `Found ${found.join(' and ')}${version ? ` (${version})` : ''}` +
      (missing.length > 0 ? `; ${missing.join(' and ')} was not found ${where}.` : '.')
    );
  });

  onMount(() => {
    void check();
  });
</script>

<section class="card" aria-labelledby="advanced-title">
  <h3 id="advanced-title">Advanced</h3>
  <Field
    id="advanced-engine"
    label="Torrent engine"
    path="advanced.engine"
    help={view.engineOverride === null
      ? "Automatic uses TorrentStream's streaming engine for every source; librqbit resolves magnet metadata only."
      : `The LUMEN_ENGINE environment variable (${view.engineOverride}) overrides this setting.`}
  >
    <select
      id="advanced-engine"
      value={a.engine}
      aria-describedby="advanced-engine-help"
      onchange={(e: Event) => {
        update('advanced.engine', (e.currentTarget as HTMLSelectElement).value);
      }}
    >
      <option value="auto">Automatic</option>
      <option value="swarm">TorrentStream engine (lumen-swarm)</option>
      <option value="rqbit">librqbit</option>
    </select>
  </Field>
  <Field
    id="advanced-debug"
    label="Debug overlay key"
    path="advanced.debugOverlay"
    help="Press D during playback to show stream statistics."
  >
    <Toggle
      id="advanced-debug"
      checked={a.debugOverlay}
      describedby="advanced-debug-help"
      onchange={(v) => {
        update('advanced.debugOverlay', v);
      }}
    />
  </Field>
  <Field
    id="advanced-log"
    label="Log detail"
    path="advanced.logLevel"
    help={view.logLevelOverride
      ? 'The RUST_LOG environment variable overrides this setting.'
      : 'Applies immediately. Logs never contain passwords or keys.'}
  >
    <select
      id="advanced-log"
      value={a.logLevel}
      disabled={view.logLevelOverride}
      aria-describedby="advanced-log-help"
      onchange={(e: Event) => {
        update('advanced.logLevel', (e.currentTarget as HTMLSelectElement).value);
      }}
    >
      <option value="error">Errors only</option>
      <option value="warn">Warnings</option>
      <option value="info">Normal</option>
      <option value="debug">Detailed</option>
      <option value="trace">Everything</option>
    </select>
  </Field>
  <Field
    id="advanced-ffmpeg"
    label="FFmpeg folder"
    path="advanced.ffmpegDir"
    help="Optional. With FFmpeg installed, TorrentStream finds the length of videos whose header does not give it (so buffering works for them) and shows previews on the seek bar. It reads only what is already downloaded. Empty: look on PATH."
    error={errors['advanced.ffmpegDir']}
  >
    <div class="folder">
      <input
        id="advanced-ffmpeg"
        class="wide"
        type="text"
        spellcheck="false"
        autocomplete="off"
        placeholder="On PATH"
        value={a.ffmpegDir ?? ''}
        aria-describedby="advanced-ffmpeg-help advanced-ffmpeg-status"
        aria-invalid={errors['advanced.ffmpegDir'] !== undefined}
        onchange={(e: Event) => {
          const value = (e.currentTarget as HTMLInputElement).value.trim();
          setFolder(value === '' ? null : value);
        }}
      />
      <p class="status" id="advanced-ffmpeg-status" aria-live="polite">{toolsSummary}</p>
      <div class="folder-actions">
        <button type="button" disabled={picking} onclick={() => void pickFolder()}>
          {picking ? 'Choosing…' : 'Choose folder…'}
        </button>
        {#if a.ffmpegDir !== null}
          <button
            type="button"
            onclick={() => {
              setFolder(null);
            }}>Use PATH</button
          >
        {/if}
        <button type="button" disabled={checking} onclick={() => void check()}>
          {checking ? 'Checking…' : 'Check again'}
        </button>
      </div>
    </div>
  </Field>
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

  .status {
    margin: 0;
    color: var(--text-muted);
    font-size: 0.85rem;
    text-align: right;
  }
</style>
