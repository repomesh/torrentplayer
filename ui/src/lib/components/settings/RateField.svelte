<!-- SPDX-License-Identifier: MIT OR Apache-2.0 -->
<script lang="ts">
  /* A bandwidth limit in KB/s or MB/s; 0 means unlimited. Stored as KiB/s. */
  import { untrack } from 'svelte';
  import { describeRate, splitRate, toKib, type RateUnit } from '../../settings';
  import Field from './Field.svelte';

  interface Props {
    id: string;
    label: string;
    path: string;
    kib: number;
    error: string | undefined;
    update: (path: string, value: unknown) => void;
  }

  let { id, label, path, kib, error, update }: Props = $props();

  // The unit is chosen once from the saved value, then left to the user.
  let unit = $state<RateUnit>(untrack(() => splitRate(kib).unit));
  const shown = $derived(unit === 'MB/s' ? kib / 1024 : kib);

  function send(value: number): void {
    update(path, Number.isFinite(value) ? toKib(value, unit) : value);
  }
</script>

<Field {id} {label} {path} help="0 means unlimited. Now: {describeRate(kib)}." {error}>
  <input
    {id}
    class="number"
    type="number"
    min="0"
    step={unit === 'MB/s' ? 0.1 : 1}
    value={shown}
    aria-describedby="{id}-help"
    aria-invalid={error !== undefined}
    oninput={(e: Event) => {
      send((e.currentTarget as HTMLInputElement).valueAsNumber);
    }}
  />
  <select
    aria-label="{label} unit"
    value={unit}
    onchange={(e: Event) => {
      unit = (e.currentTarget as HTMLSelectElement).value as RateUnit;
    }}
  >
    <option value="KB/s">KB/s</option>
    <option value="MB/s">MB/s</option>
  </select>
</Field>
