<!-- SPDX-License-Identifier: MIT OR Apache-2.0 -->
<script lang="ts">
  import type { Settings } from '../../ipc';
  import Field from './Field.svelte';
  import Toggle from './Toggle.svelte';

  interface Props {
    settings: Settings;
    errors: Record<string, string>;
    update: (path: string, value: unknown) => void;
  }

  let { settings, errors, update }: Props = $props();

  const s = $derived(settings.search);
</script>

<section class="card" aria-labelledby="search-title">
  <h3 id="search-title">Search</h3>
  <h4>Sources</h4>
  <Field
    id="search-emberglow"
    label="Emberglow"
    path="search.sources.emberglow"
    help="search.emberglow.tech, querying all 33 of its indexers."
  >
    <Toggle
      id="search-emberglow"
      checked={s.sources.emberglow}
      describedby="search-emberglow-help"
      onchange={(v) => {
        update('search.sources.emberglow', v);
      }}
    />
  </Field>
  <Field
    id="search-ia"
    label="Internet Archive"
    path="search.sources.internetArchive"
    help="Public-domain films, music, books on tape, and more."
  >
    <Toggle
      id="search-ia"
      checked={s.sources.internetArchive}
      disabled
      onchange={() => undefined}
    />
  </Field>
  <Field
    id="search-at"
    label="Academic Torrents"
    path="search.sources.academicTorrents"
    help="Research datasets and course videos."
  >
    <Toggle
      id="search-at"
      checked={s.sources.academicTorrents}
      disabled
      onchange={() => undefined}
    />
  </Field>

  <h4>Results</h4>
  <Field id="search-adult" label="Hide adult results" path="search.hideAdult">
    <Toggle
      id="search-adult"
      checked={s.hideAdult}
      onchange={(v) => {
        update('search.hideAdult', v);
      }}
    />
  </Field>
  <Field
    id="search-open"
    label="Openly licensed only"
    path="search.openLicenseOnly"
    help="Show only results with a known open license, such as Creative Commons."
  >
    <Toggle
      id="search-open"
      checked={s.openLicenseOnly}
      describedby="search-open-help"
      onchange={(v) => {
        update('search.openLicenseOnly', v);
      }}
    />
  </Field>
  <Field
    id="search-preload"
    label="Preload the best result"
    path="search.preloadTopResult"
    help="Starts loading the first minutes of the top result in the background so it plays at once. Nothing is uploaded unless you play it."
  >
    <Toggle
      id="search-preload"
      checked={s.preloadTopResult}
      describedby="search-preload-help"
      onchange={(v) => {
        update('search.preloadTopResult', v);
      }}
    />
  </Field>
  <Field
    id="search-count"
    label="Results per search"
    path="search.resultsPerSearch"
    error={errors['search.resultsPerSearch']}
  >
    <input
      id="search-count"
      class="number"
      type="number"
      min="10"
      max="500"
      step="10"
      value={s.resultsPerSearch}
      aria-invalid={errors['search.resultsPerSearch'] !== undefined}
      oninput={(e: Event) => {
        update('search.resultsPerSearch', (e.currentTarget as HTMLInputElement).valueAsNumber);
      }}
    />
  </Field>
</section>
