<!-- SPDX-License-Identifier: MIT OR Apache-2.0 -->
<script lang="ts">
  import type { Settings } from '../../ipc';
  import Field from './Field.svelte';
  import RateField from './RateField.svelte';

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
  <p class="intro">
    Queued torrents keep downloading and sharing, finished or not, until you pause or remove them in
    the queue. Pausing stops both.
  </p>
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
  .intro {
    margin: 0 0 0.75rem;
    color: var(--text-muted);
    font-size: 0.85rem;
    line-height: 1.45;
  }
</style>
