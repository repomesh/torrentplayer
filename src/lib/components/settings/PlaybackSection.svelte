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

  const SPEEDS = [0.5, 0.75, 1, 1.25, 1.5, 1.75, 2, 2.5, 3];
  const p = $derived(settings.playback);
  const speeds = $derived(
    SPEEDS.includes(p.defaultSpeed) ? SPEEDS : [...SPEEDS, p.defaultSpeed].sort((a, b) => a - b),
  );
</script>

<section class="card" aria-labelledby="playback-title">
  <h3 id="playback-title">Playback</h3>
  <Field
    id="playback-speed"
    label="Default speed"
    path="playback.defaultSpeed"
    help="The speed new videos and tracks start at."
    error={errors['playback.defaultSpeed']}
  >
    <select
      id="playback-speed"
      value={p.defaultSpeed}
      onchange={(e: Event) => {
        update('playback.defaultSpeed', Number((e.currentTarget as HTMLSelectElement).value));
      }}
    >
      {#each speeds as speed (speed)}
        <option value={speed}>{speed}×</option>
      {/each}
    </select>
  </Field>
  <Field
    id="playback-autoplay"
    label="Play the next item in the queue automatically"
    path="playback.autoplayNext"
  >
    <Toggle
      id="playback-autoplay"
      checked={p.autoplayNext}
      onchange={(v) => {
        update('playback.autoplayNext', v);
      }}
    />
  </Field>
  <Field
    id="playback-optimize-queue"
    label="Play ready videos first"
    path="playback.optimizeQueue"
    help="Queued videos that are downloaded, or whose start and end are, move ahead of those still waiting. The video playing never moves."
  >
    <Toggle
      id="playback-optimize-queue"
      checked={p.optimizeQueue}
      describedby="playback-optimize-queue-help"
      onchange={(v) => {
        update('playback.optimizeQueue', v);
      }}
    />
  </Field>
  <Field
    id="playback-course-order"
    label="Keep each torrent's order"
    path="playback.keepCourseOrder"
    help="Files of one torrent keep their order when ready videos move up."
  >
    <Toggle
      id="playback-course-order"
      checked={p.keepCourseOrder}
      describedby="playback-course-order-help"
      onchange={(v) => {
        update('playback.keepCourseOrder', v);
      }}
    />
  </Field>
  <Field
    id="playback-resume"
    label="Resume where I left off"
    path="playback.resumePosition"
    help="Reopening a video or track continues from the last position."
  >
    <Toggle
      id="playback-resume"
      checked={p.resumePosition}
      describedby="playback-resume-help"
      onchange={(v) => {
        update('playback.resumePosition', v);
      }}
    />
  </Field>
  <Field
    id="playback-subtitles"
    label="Show subtitles automatically"
    path="playback.subtitlesByDefault"
    help="Turns on subtitles in your system language when a video has them. C turns subtitles on or off while watching."
  >
    <Toggle
      id="playback-subtitles"
      checked={p.subtitlesByDefault}
      describedby="playback-subtitles-help"
      onchange={(v) => {
        update('playback.subtitlesByDefault', v);
      }}
    />
  </Field>
  <Field
    id="playback-thumbnails"
    label="Seek bar previews"
    path="playback.seekThumbnails"
    help="Shows a frame of the video where you point at the seek bar, for the parts already downloaded. Needs FFmpeg (Advanced)."
  >
    <Toggle
      id="playback-thumbnails"
      checked={p.seekThumbnails}
      describedby="playback-thumbnails-help"
      onchange={(v) => {
        update('playback.seekThumbnails', v);
      }}
    />
  </Field>
  <Field
    id="playback-hwdec"
    label="Hardware decoding"
    path="playback.hardwareDecoding"
    help="Uses the graphics card to decode video when it is known to be safe. Applies with the libmpv player."
  >
    <select
      id="playback-hwdec"
      value={p.hardwareDecoding}
      aria-describedby="playback-hwdec-help"
      onchange={(e: Event) => {
        update('playback.hardwareDecoding', (e.currentTarget as HTMLSelectElement).value);
      }}
    >
      <option value="auto">Automatic</option>
      <option value="off">Off (software)</option>
    </select>
  </Field>
</section>
