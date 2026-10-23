<!-- SPDX-License-Identifier: MIT OR Apache-2.0 -->
<script lang="ts">
  import type { SettingsSection } from '../settings';
  import type { StatusInfo } from '../stats';
  import { formatTime, type ThumbnailSource } from '../seekbar';
  import { subtitleGroups, subtitleMenuNote, type SubtitleOption } from '../subtitles';
  import NetworkIndicator from './NetworkIndicator.svelte';
  import TransferRates from './TransferRates.svelte';
  import SeekBar from './SeekBar.svelte';
  import StatusBar from './StatusBar.svelte';

  interface Props {
    visible: boolean;
    paused: boolean;
    currentTime: number;
    duration: number;
    pieceMap: readonly number[];
    rate: number;
    muted: boolean;
    volume: number;
    fullscreen: boolean;
    status: StatusInfo | null;
    menuOpen?: boolean;
    onTogglePlay: () => void;
    onSeek: (fraction: number) => void;
    onSeekBy: (seconds: number) => void;
    onRate: (rate: number) => void;
    onToggleMute: () => void;
    onVolume: (volume: number) => void;
    onToggleFullscreen: () => void;
    onBack: () => void;
    onScrub: (dragging: boolean) => void;
    onHover: (inside: boolean) => void;
    /** Opens Settings over the player, at `section`. */
    onSettings: (section: SettingsSection) => void;
    /** The CC menu's entries; with none, the (dimmed) CC button only offers loading a file. */
    subtitles?: readonly SubtitleOption[];
    /** The subtitle option showing, or null for off. */
    subtitle?: string | null;
    subtitleMenuOpen?: boolean;
    onSubtitle?: (id: string | null) => void;
    /** "Load subtitle file…": opens the file picker. */
    onLoadSubtitles?: () => void;
    /** Pictures for the seek bar, if any. */
    thumbnails?: ThumbnailSource | null;
  }

  let {
    visible,
    paused,
    currentTime,
    duration,
    pieceMap,
    rate,
    muted,
    volume,
    fullscreen,
    status,
    menuOpen = $bindable(false),
    onTogglePlay,
    onSeek,
    onSeekBy,
    onRate,
    onToggleMute,
    onVolume,
    onToggleFullscreen,
    onBack,
    onScrub,
    onHover,
    onSettings,
    subtitles = [],
    subtitle = null,
    subtitleMenuOpen = $bindable(false),
    onSubtitle,
    onLoadSubtitles,
    thumbnails = null,
  }: Props = $props();

  const groups = $derived(subtitleGroups(subtitles));
  const note = $derived(subtitleMenuNote(subtitles));

  function chooseSubtitle(id: string | null): void {
    subtitleMenuOpen = false;
    onSubtitle?.(id);
  }

  const RATES = [0.5, 0.75, 1, 1.25, 1.5, 1.75, 2, 2.5, 3];

  const rateLabel = (r: number): string => `${r.toString()}×`;
</script>

<div
  class="controls"
  class:visible
  role="group"
  aria-label="Playback controls"
  onpointerenter={() => {
    onHover(true);
  }}
  onpointerleave={() => {
    onHover(false);
  }}
  onfocusin={() => {
    onHover(true);
  }}
  onfocusout={() => {
    onHover(false);
  }}
>
  <SeekBar {currentTime} {duration} {pieceMap} {onSeek} {onScrub} {thumbnails} />

  <div class="row">
    <button
      type="button"
      class="icon"
      onclick={onTogglePlay}
      aria-label={paused ? 'Play' : 'Pause'}
    >
      {#if paused}
        <svg viewBox="0 0 24 24" aria-hidden="true"><path d="M8 5v14l11-7z" /></svg>
      {:else}
        <svg viewBox="0 0 24 24" aria-hidden="true"><path d="M7 5h4v14H7zM13 5h4v14h-4z" /></svg>
      {/if}
    </button>

    <button
      type="button"
      class="icon skip"
      aria-label="Rewind 10 seconds"
      title="Rewind 10 seconds (←)"
      disabled={!Number.isFinite(duration) || duration <= 0 || currentTime <= 0}
      onclick={() => {
        onSeekBy(-10);
      }}
    >
      <svg viewBox="0 0 24 24" aria-hidden="true">
        <path d="M4 10a8 8 0 1 1 1.7 8.2M4 10V4m0 6h6" />
        <text x="12" y="15.5" text-anchor="middle">10</text>
      </svg>
    </button>
    <button
      type="button"
      class="icon skip"
      aria-label="Forward 10 seconds"
      title="Forward 10 seconds (→)"
      disabled={!Number.isFinite(duration) || duration <= 0 || currentTime >= duration}
      onclick={() => {
        onSeekBy(10);
      }}
    >
      <svg viewBox="0 0 24 24" aria-hidden="true">
        <path d="M20 10a8 8 0 1 0-1.7 8.2M20 10V4m0 6h-6" />
        <text x="12" y="15.5" text-anchor="middle">10</text>
      </svg>
    </button>

    <span class="time">{formatTime(currentTime)} / {formatTime(duration)}</span>

    <div class="menu">
      <button
        type="button"
        class="text"
        aria-haspopup="listbox"
        aria-expanded={menuOpen}
        aria-label="Playback speed"
        onclick={() => {
          menuOpen = !menuOpen;
          subtitleMenuOpen = false;
        }}
      >
        {rateLabel(rate)}
      </button>
      {#if menuOpen}
        <ul class="rates" role="listbox" aria-label="Playback speed">
          {#each RATES as r (r)}
            <li>
              <button
                type="button"
                role="option"
                aria-selected={r === rate}
                class:selected={r === rate}
                onclick={() => {
                  onRate(r);
                  menuOpen = false;
                }}
              >
                {rateLabel(r)}
              </button>
            </li>
          {/each}
        </ul>
      {/if}
    </div>

    <button
      type="button"
      class="icon"
      onclick={onToggleMute}
      aria-label={muted || volume === 0 ? 'Unmute' : 'Mute'}
    >
      {#if muted || volume === 0}
        <svg viewBox="0 0 24 24" aria-hidden="true">
          <path d="M4 9v6h4l5 4V5L8 9zM16 9l5 6M21 9l-5 6" />
        </svg>
      {:else}
        <svg viewBox="0 0 24 24" aria-hidden="true">
          <path d="M4 9v6h4l5 4V5L8 9zM16.5 8.5a5 5 0 0 1 0 7M19 6a8.5 8.5 0 0 1 0 12" />
        </svg>
      {/if}
    </button>
    <input
      class="volume"
      type="range"
      min="0"
      max="1"
      step="0.05"
      value={muted ? 0 : volume}
      aria-label="Volume"
      oninput={(event) => {
        onVolume(Number(event.currentTarget.value));
      }}
    />

    <span class="spacer"></span>

    <div class="menu">
      <button
        type="button"
        class="icon cc"
        class:on={subtitle !== null}
        class:dim={note !== null}
        aria-haspopup="listbox"
        aria-expanded={subtitleMenuOpen}
        aria-label={subtitle === null ? 'Subtitles (off)' : 'Subtitles (on)'}
        title="Subtitles (C)"
        onclick={() => {
          subtitleMenuOpen = !subtitleMenuOpen;
          menuOpen = false;
        }}
      >
        <svg viewBox="0 0 24 24" aria-hidden="true" class="outline">
          <rect x="3" y="5.5" width="18" height="13" rx="2.5" />
          <path d="M10.6 10.3a2.1 2.1 0 1 0 0 3.4M16.6 10.3a2.1 2.1 0 1 0 0 3.4" />
        </svg>
      </button>
      {#if subtitleMenuOpen}
        <ul class="subs" role="listbox" aria-label="Subtitles">
          {#if note !== null}
            <li class="heading" role="presentation">{note}</li>
          {:else}
            <li>
              <button
                type="button"
                role="option"
                aria-selected={subtitle === null}
                class:selected={subtitle === null}
                onclick={() => {
                  chooseSubtitle(null);
                }}
              >
                <span class="label">Off</span>
              </button>
            </li>
            {#each groups as group (group.title)}
              {#if group.title !== null}
                <li class="heading" role="presentation">{group.title}</li>
              {/if}
              {#each group.options as option (option.id)}
                <li>
                  <button
                    type="button"
                    role="option"
                    aria-selected={option.id === subtitle}
                    aria-disabled={!option.enabled}
                    disabled={!option.enabled}
                    class:selected={option.id === subtitle}
                    onclick={() => {
                      chooseSubtitle(option.id);
                    }}
                  >
                    <span class="label">{option.label}</span>
                    {#if option.detail !== null}
                      <span class="detail">{option.detail}</span>
                    {/if}
                  </button>
                </li>
              {/each}
            {/each}
            <li class="divider" role="presentation"></li>
          {/if}
          <li>
            <button
              type="button"
              role="option"
              aria-selected={false}
              onclick={() => {
                subtitleMenuOpen = false;
                onLoadSubtitles?.();
              }}
            >
              <span class="label">Load subtitle file…</span>
            </button>
          </li>
        </ul>
      {/if}
    </div>

    <button
      type="button"
      class="icon quiet"
      onclick={() => {
        onSettings('playback');
      }}
      aria-label="Settings"
      title="Settings (Ctrl+,)"
    >
      <svg viewBox="0 0 24 24" aria-hidden="true" class="gear">
        <path
          d="M19.4 13a7.5 7.5 0 0 0 0-2l2-1.6-2-3.4-2.4 1a7.4 7.4 0 0 0-1.7-1L15 3.5h-4l-.4 2.5a7.4 7.4 0 0 0-1.7 1l-2.4-1-2 3.4 2 1.6a7.5 7.5 0 0 0 0 2l-2 1.6 2 3.4 2.4-1a7.4 7.4 0 0 0 1.7 1l.4 2.5h4l.4-2.5a7.4 7.4 0 0 0 1.7-1l2.4 1 2-3.4zM13 15.5a3.5 3.5 0 1 1 0-7 3.5 3.5 0 0 1 0 7z"
          transform="translate(-1 0)"
        />
      </svg>
    </button>
    <button
      type="button"
      class="icon"
      onclick={onToggleFullscreen}
      aria-label={fullscreen ? 'Exit fullscreen' : 'Fullscreen'}
    >
      {#if fullscreen}
        <svg viewBox="0 0 24 24" aria-hidden="true">
          <path d="M9 4v5H4M15 4v5h5M9 20v-5H4M15 20v-5h5" />
        </svg>
      {:else}
        <svg viewBox="0 0 24 24" aria-hidden="true">
          <path d="M4 9V4h5M20 9V4h-5M4 15v5h5M20 15v5h-5" />
        </svg>
      {/if}
    </button>
    <button
      type="button"
      class="text"
      onclick={onBack}
      title="Back (Esc). The video keeps its place."
    >
      Back
    </button>
  </div>

  <div class="statusline">
    {#if status}
      <StatusBar {...status} />
    {:else}
      <span></span>
    {/if}
    <span class="network">
      <TransferRates />
      <NetworkIndicator
        onOpen={() => {
          onSettings('network');
        }}
      />
    </span>
  </div>
</div>

<style>
  .controls {
    position: absolute;
    left: 0;
    right: 0;
    bottom: 0;
    z-index: 2;
    padding: 2.5rem clamp(0.75rem, 2vw, 1.5rem) 0.6rem;
    background: linear-gradient(0deg, rgb(0 0 0 / 72%) 0%, rgb(0 0 0 / 40%) 55%, rgb(0 0 0 / 0%));
    opacity: 0;
    transition: opacity 0.35s ease;
    pointer-events: none;
  }

  .controls.visible {
    opacity: 1;
    pointer-events: auto;
  }

  .row {
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    gap: 0.5rem;
    margin-top: 0.2rem;
  }

  button {
    border: none;
    background: none;
    color: var(--text);
    font: inherit;
    cursor: pointer;
  }

  .icon {
    display: grid;
    place-items: center;
    width: 2rem;
    height: 2rem;
    padding: 0;
    border-radius: 0.4rem;
    flex-shrink: 0;
  }

  .icon svg {
    width: 1.25rem;
    height: 1.25rem;
    fill: currentColor;
    stroke: currentColor;
    stroke-width: 1.6;
    stroke-linecap: round;
    stroke-linejoin: round;
  }

  .icon:disabled {
    cursor: default;
    opacity: 0.35;
  }

  .icon.skip svg {
    fill: none;
  }

  .skip text {
    fill: currentColor;
    stroke: none;
    font-size: 8px;
    font-weight: 600;
  }

  .icon:hover:not(:disabled),
  .text:hover {
    background: rgb(255 255 255 / 10%);
  }

  .icon.quiet {
    opacity: 0.7;
  }

  .icon.quiet:hover,
  .icon.quiet:focus-visible {
    opacity: 1;
  }

  /* An outline gear, like the one in the footer. */
  .icon svg.gear {
    width: 1.1rem;
    height: 1.1rem;
    fill: none;
  }

  .text {
    padding: 0.3rem 0.55rem;
    border-radius: 0.4rem;
    font-size: 0.85rem;
    font-variant-numeric: tabular-nums;
  }

  .time {
    margin: 0 0.25rem;
    font-size: 0.85rem;
    font-variant-numeric: tabular-nums;
    white-space: nowrap;
  }

  .menu {
    position: relative;
  }

  .rates {
    position: absolute;
    bottom: 2.4rem;
    left: 50%;
    margin: 0;
    padding: 0.3rem;
    border-radius: 0.5rem;
    background: rgb(16 16 20 / 94%);
    list-style: none;
    transform: translateX(-50%);
    box-shadow: 0 8px 24px rgb(0 0 0 / 45%);
  }

  .rates button {
    display: block;
    width: 100%;
    padding: 0.3rem 1rem;
    border-radius: 0.3rem;
    font-size: 0.85rem;
    font-variant-numeric: tabular-nums;
    text-align: center;
  }

  .rates button:hover {
    background: rgb(255 255 255 / 10%);
  }

  .rates button.selected {
    color: var(--accent);
  }

  .icon svg.outline {
    fill: none;
  }

  /* A short accent line under the CC button while subtitles show. */
  .cc {
    position: relative;
  }

  /* No subtitles: the button only offers loading a file. */
  .cc.dim {
    opacity: 0.5;
  }

  .cc.dim:hover,
  .cc.dim:focus-visible {
    opacity: 1;
  }

  .cc.on::after {
    position: absolute;
    left: 28%;
    right: 28%;
    bottom: 0.15rem;
    height: 2px;
    border-radius: 1px;
    background: var(--accent);
    content: '';
  }

  .subs {
    position: absolute;
    bottom: 2.4rem;
    right: -4rem;
    min-width: 15rem;
    max-width: min(24rem, 80vw);
    max-height: 55vh;
    margin: 0;
    padding: 0.3rem;
    overflow-y: auto;
    border-radius: 0.5rem;
    background: rgb(16 16 20 / 94%);
    list-style: none;
    box-shadow: 0 8px 24px rgb(0 0 0 / 45%);
  }

  .subs button {
    display: flex;
    flex-direction: column;
    width: 100%;
    padding: 0.35rem 0.75rem;
    border-radius: 0.3rem;
    font-size: 0.85rem;
    text-align: left;
  }

  .subs button:hover:not(:disabled) {
    background: rgb(255 255 255 / 10%);
  }

  .subs button.selected {
    color: var(--accent);
  }

  .subs button:disabled {
    cursor: default;
    opacity: 0.55;
  }

  .subs .label,
  .subs .detail {
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  .subs .detail {
    color: var(--text-muted);
    font-size: 0.75rem;
  }

  .subs .heading {
    padding: 0.5rem 0.75rem 0.2rem;
    color: var(--text-muted);
    font-size: 0.7rem;
    letter-spacing: 0.04em;
    text-transform: uppercase;
  }

  .subs .divider {
    height: 1px;
    margin: 0.3rem 0.5rem;
    background: rgb(255 255 255 / 12%);
  }

  .volume {
    width: 5.5rem;
    accent-color: #fff;
  }

  .spacer {
    flex: 1;
  }

  .statusline {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 1rem;
    margin-top: 0.3rem;
    padding: 0.2rem 0.5rem;
    border-radius: 0.3rem;
    background: rgb(255 255 255 / 4%);
  }

  .network {
    display: inline-flex;
    gap: 0.6rem;
    align-items: center;
    min-width: 0;
    color: var(--text-muted);
    font-size: 0.75rem;
  }
</style>
