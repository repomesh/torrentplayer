<!-- SPDX-License-Identifier: MIT OR Apache-2.0 -->
<script lang="ts">
  /*
   * The app's main navigation: a narrow rail at the left, always present (except in
   * fullscreen). Back and forward at the top, the views in the middle, Settings at the bottom.
   */
  import type { ViewName } from '../nav';

  interface Props {
    current: ViewName;
    canBack: boolean;
    canForward: boolean;
    /** Whether a torrent's file list can be shown. */
    hasTorrent: boolean;
    /** Whether something is playing (or paused) that the player can return to. */
    playing: boolean;
    /** Items waiting in the queue. */
    upcoming: number;
    onBack: () => void;
    onForward: () => void;
    onNavigate: (view: ViewName) => void;
  }

  let {
    current,
    canBack,
    canForward,
    hasTorrent,
    playing,
    upcoming,
    onBack,
    onForward,
    onNavigate,
  }: Props = $props();

  type Item = {
    view: ViewName;
    label: string;
    title: string;
    disabled: boolean;
    icon: string;
  };

  const items: Item[] = $derived([
    {
      view: 'search',
      label: 'Search',
      title: 'Search (Ctrl+F)',
      disabled: false,
      icon: 'M10.5 4.5a6 6 0 1 0 0 12 6 6 0 0 0 0-12zM15 15l5 5',
    },
    {
      view: 'open',
      label: 'Open',
      title: 'Open a link or .torrent file (Ctrl+O)',
      disabled: false,
      icon: 'M3.5 7.5V18a1 1 0 0 0 1 1h15a1 1 0 0 0 1-1V9.5a1 1 0 0 0-1-1h-8l-2-2.5h-5a1 1 0 0 0-1 1z',
    },
    {
      view: 'library',
      label: 'Library',
      title: 'Library and watch progress',
      disabled: false,
      icon: 'M5 4.5h11.5A2.5 2.5 0 0 1 19 7v12H7a2 2 0 0 1-2-2V4.5ZM7 19a2 2 0 0 1 2-2h10M9 8h6M9 11.5h5',
    },
    {
      view: 'torrent',
      label: 'Files',
      title: hasTorrent ? 'Files of the last opened torrent' : 'Files (open a torrent first)',
      disabled: !hasTorrent,
      icon: 'M9 6.5h11M9 12h11M9 17.5h11M4.5 6.5h.01M4.5 12h.01M4.5 17.5h.01',
    },
    {
      view: 'player',
      label: 'Queue',
      title: playing ? 'Back to the video and its queue' : 'The queue',
      disabled: false,
      icon: 'M4.5 5h15a1 1 0 0 1 1 1v12a1 1 0 0 1-1 1h-15a1 1 0 0 1-1-1V6a1 1 0 0 1 1-1zM10 9v6l5-3z',
    },
  ]);
</script>

<nav class="rail" aria-label="Main">
  <div class="history">
    <button
      type="button"
      class="step"
      aria-label="Back"
      title="Back (Alt+←)"
      disabled={!canBack}
      onclick={onBack}
    >
      <svg viewBox="0 0 24 24" aria-hidden="true"><path d="M14.5 6l-6 6 6 6" /></svg>
    </button>
    <button
      type="button"
      class="step"
      aria-label="Forward"
      title="Forward (Alt+→)"
      disabled={!canForward}
      onclick={onForward}
    >
      <svg viewBox="0 0 24 24" aria-hidden="true"><path d="M9.5 6l6 6-6 6" /></svg>
    </button>
  </div>

  <ul>
    {#each items as item (item.view)}
      <li>
        <button
          type="button"
          class="item"
          class:active={current === item.view}
          aria-current={current === item.view ? 'page' : undefined}
          title={item.title}
          disabled={item.disabled}
          onclick={() => {
            onNavigate(item.view);
          }}
        >
          <span class="icon">
            <svg viewBox="0 0 24 24" aria-hidden="true"><path d={item.icon} /></svg>
            {#if item.view === 'player' && upcoming > 0}
              <span class="badge" aria-hidden="true">{upcoming > 99 ? '99+' : upcoming}</span>
            {/if}
            {#if item.view === 'player' && playing && upcoming === 0}
              <span class="live" aria-hidden="true"></span>
            {/if}
          </span>
          <span class="label">
            {item.label}
            {#if item.view === 'player' && upcoming > 0}
              <span class="sr-only">, {upcoming} to play in the queue</span>
            {/if}
          </span>
        </button>
      </li>
    {/each}
  </ul>

  <button
    type="button"
    class="item settings"
    class:active={current === 'settings'}
    aria-current={current === 'settings' ? 'page' : undefined}
    title="Settings (Ctrl+,)"
    onclick={() => {
      onNavigate('settings');
    }}
  >
    <span class="icon">
      <svg viewBox="0 0 24 24" aria-hidden="true">
        <path
          d="M19.4 13a7.5 7.5 0 0 0 0-2l2-1.6-2-3.4-2.4 1a7.4 7.4 0 0 0-1.7-1L15 3.5h-4l-.4 2.5a7.4 7.4 0 0 0-1.7 1l-2.4-1-2 3.4 2 1.6a7.5 7.5 0 0 0 0 2l-2 1.6 2 3.4 2.4-1a7.4 7.4 0 0 0 1.7 1l.4 2.5h4l.4-2.5a7.4 7.4 0 0 0 1.7-1l2.4 1 2-3.4zM13 15.5a3.5 3.5 0 1 1 0-7 3.5 3.5 0 0 1 0 7z"
          transform="translate(-1 0)"
        />
      </svg>
    </span>
    <span class="label">Settings</span>
  </button>
</nav>

<style>
  .rail {
    display: flex;
    flex: none;
    flex-direction: column;
    align-items: stretch;
    gap: 0.5rem;
    width: 4rem;
    padding: 0.5rem 0.3rem;
    box-sizing: border-box;
    overflow-y: auto;
    border-right: 1px solid rgb(255 255 255 / 5%);
    background: #0d0d12;
  }

  .history {
    display: flex;
    justify-content: center;
    gap: 0.1rem;
    padding-bottom: 0.4rem;
    border-bottom: 1px solid rgb(255 255 255 / 5%);
  }

  ul {
    display: grid;
    gap: 0.15rem;
    margin: 0;
    padding: 0;
    list-style: none;
  }

  button {
    border: none;
    background: none;
    color: var(--text-muted);
    font: inherit;
    cursor: pointer;
  }

  button:disabled {
    cursor: default;
    opacity: 0.35;
  }

  button:focus-visible {
    outline: 2px solid var(--accent);
    outline-offset: -2px;
  }

  .step {
    display: inline-flex;
    padding: 0.25rem;
    border-radius: 0.35rem;
  }

  .step:hover:not(:disabled) {
    color: var(--text);
    background: rgb(255 255 255 / 5%);
  }

  .item {
    position: relative;
    display: grid;
    justify-items: center;
    gap: 0.2rem;
    width: 100%;
    padding: 0.45rem 0 0.4rem;
    border-radius: 0.5rem;
  }

  .item:hover:not(:disabled) {
    color: var(--text);
    background: rgb(255 255 255 / 4%);
  }

  .item.active {
    color: var(--accent);
    background: rgb(242 180 90 / 8%);
  }

  .item.active::before {
    position: absolute;
    top: 25%;
    bottom: 25%;
    left: -0.3rem;
    width: 2px;
    border-radius: 2px;
    background: var(--accent);
    content: '';
  }

  .settings {
    margin-top: auto;
  }

  .icon {
    position: relative;
    display: inline-flex;
  }

  svg {
    width: 1.2rem;
    height: 1.2rem;
    fill: none;
    stroke: currentcolor;
    stroke-width: 1.6;
    stroke-linecap: round;
    stroke-linejoin: round;
  }

  .step svg {
    width: 1rem;
    height: 1rem;
  }

  .label {
    font-size: 0.66rem;
    letter-spacing: 0.02em;
  }

  .badge {
    position: absolute;
    top: -0.35rem;
    right: -0.65rem;
    min-width: 0.95rem;
    padding: 0 0.2rem;
    box-sizing: border-box;
    border-radius: 1rem;
    background: var(--accent);
    color: #1a1408;
    font-size: 0.58rem;
    font-weight: 700;
    line-height: 0.95rem;
    text-align: center;
    font-variant-numeric: tabular-nums;
  }

  .live {
    position: absolute;
    top: -0.15rem;
    right: -0.25rem;
    width: 0.4rem;
    height: 0.4rem;
    border-radius: 50%;
    background: var(--accent);
  }

  .sr-only {
    position: absolute;
    width: 1px;
    height: 1px;
    overflow: hidden;
    clip-path: inset(50%);
    white-space: nowrap;
  }

  /* Small windows: icons only. */
  @media (max-width: 640px) {
    .rail {
      width: 2.9rem;
      padding-inline: 0.2rem;
    }

    .history {
      flex-direction: column;
      align-items: center;
    }

    .label {
      position: absolute;
      width: 1px;
      height: 1px;
      overflow: hidden;
      clip-path: inset(50%);
      white-space: nowrap;
    }

    .item {
      padding: 0.55rem 0;
    }

    .item.active::before {
      left: -0.2rem;
    }
  }
</style>
