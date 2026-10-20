<!-- SPDX-License-Identifier: MIT OR Apache-2.0 -->
<script lang="ts">
  import type { Settings, SettingsView } from '../../ipc';
  import Field from './Field.svelte';
  import Toggle from './Toggle.svelte';

  interface Props {
    settings: Settings;
    view: SettingsView;
    update: (path: string, value: unknown) => void;
  }

  let { settings, view, update }: Props = $props();

  const a = $derived(settings.advanced);
</script>

<section class="card" aria-labelledby="advanced-title">
  <h3 id="advanced-title">Advanced</h3>
  <Field
    id="advanced-engine"
    label="Torrent engine"
    path="advanced.engine"
    help={view.engineOverride === null
      ? "Automatic uses TorrentStream's streaming engine for .torrent files and librqbit for magnet links."
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
</section>
