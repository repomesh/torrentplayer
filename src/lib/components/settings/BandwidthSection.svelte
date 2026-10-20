<!-- SPDX-License-Identifier: MIT OR Apache-2.0 -->
<script lang="ts">
  import type { Settings } from '../../ipc';
  import Field from './Field.svelte';
  import RateField from './RateField.svelte';
  import Toggle from './Toggle.svelte';

  interface Props {
    settings: Settings;
    errors: Record<string, string>;
    update: (path: string, value: unknown) => void;
  }

  let { settings, errors, update }: Props = $props();

  const b = $derived(settings.bandwidth);
</script>

<section class="card" aria-labelledby="bandwidth-title">
  <h3 id="bandwidth-title">Bandwidth &amp; seeding</h3>
  <RateField
    id="bandwidth-down"
    label="Download limit"
    path="bandwidth.downloadLimitKib"
    kib={b.downloadLimitKib}
    error={errors['bandwidth.downloadLimitKib']}
    {update}
  />
  <RateField
    id="bandwidth-up"
    label="Upload limit"
    path="bandwidth.uploadLimitKib"
    kib={b.uploadLimitKib}
    error={errors['bandwidth.uploadLimitKib']}
    {update}
  />
  <Field
    id="bandwidth-seed"
    label="Keep sharing after watching"
    path="bandwidth.seedAfterWatching"
    help="Seeding gives back to the swarm, so what you watch and listen to stays fast for everyone."
  >
    <Toggle
      id="bandwidth-seed"
      checked={b.seedAfterWatching}
      describedby="bandwidth-seed-help"
      onchange={(v) => {
        update('bandwidth.seedAfterWatching', v);
      }}
    />
  </Field>
  <Field
    id="bandwidth-seed-time"
    label="Stop seeding after"
    path="bandwidth.seedTimeLimitMinutes"
    help="Minutes after watching finishes. 0 has no time limit; the ratio target can still stop it."
    error={errors['bandwidth.seedTimeLimitMinutes']}
  >
    <div class="with-unit">
      <input
        id="bandwidth-seed-time"
        class="number"
        type="number"
        min="0"
        max="525600"
        step="1"
        value={b.seedTimeLimitMinutes}
        disabled={!b.seedAfterWatching}
        aria-describedby="bandwidth-seed-time-help"
        aria-invalid={errors['bandwidth.seedTimeLimitMinutes'] !== undefined}
        oninput={(e: Event) => {
          update(
            'bandwidth.seedTimeLimitMinutes',
            (e.currentTarget as HTMLInputElement).valueAsNumber,
          );
        }}
      />
      <span>minutes</span>
    </div>
  </Field>
  <Field
    id="bandwidth-ratio"
    label="Stop seeding at ratio"
    path="bandwidth.seedRatioTarget"
    help="Uploaded ÷ downloaded. 0 keeps seeding while TorrentStream runs."
    error={errors['bandwidth.seedRatioTarget']}
  >
    <input
      id="bandwidth-ratio"
      class="number"
      type="number"
      min="0"
      max="100"
      step="0.1"
      value={b.seedRatioTarget}
      disabled={!b.seedAfterWatching}
      aria-describedby="bandwidth-ratio-help"
      aria-invalid={errors['bandwidth.seedRatioTarget'] !== undefined}
      oninput={(e: Event) => {
        update('bandwidth.seedRatioTarget', (e.currentTarget as HTMLInputElement).valueAsNumber);
      }}
    />
  </Field>
  <Field
    id="bandwidth-peers"
    label="Peer limit per torrent"
    path="bandwidth.peerLimit"
    help="More peers can be faster but use more connections (5–500)."
    error={errors['bandwidth.peerLimit']}
  >
    <input
      id="bandwidth-peers"
      class="number"
      type="number"
      min="5"
      max="500"
      step="1"
      value={b.peerLimit}
      aria-describedby="bandwidth-peers-help"
      aria-invalid={errors['bandwidth.peerLimit'] !== undefined}
      oninput={(e: Event) => {
        update('bandwidth.peerLimit', (e.currentTarget as HTMLInputElement).valueAsNumber);
      }}
    />
  </Field>
  <Field
    id="bandwidth-downloads"
    label="Queued videos downloading at once"
    path="bandwidth.maxDownloads"
    help="0 downloads everything in the queue at once (0–100). The video you watch always comes first."
    error={errors['bandwidth.maxDownloads']}
  >
    <input
      id="bandwidth-downloads"
      class="number"
      type="number"
      min="0"
      max="100"
      step="1"
      value={b.maxDownloads}
      aria-describedby="bandwidth-downloads-help"
      aria-invalid={errors['bandwidth.maxDownloads'] !== undefined}
      oninput={(e: Event) => {
        update('bandwidth.maxDownloads', (e.currentTarget as HTMLInputElement).valueAsNumber);
      }}
    />
  </Field>
</section>

<style>
  .with-unit {
    display: flex;
    align-items: center;
    gap: 0.5rem;
    color: var(--text-muted);
    font-size: 0.8rem;
  }
</style>
