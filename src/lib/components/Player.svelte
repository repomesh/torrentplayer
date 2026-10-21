<!-- SPDX-License-Identifier: MIT OR Apache-2.0 -->
<script lang="ts">
  import { onDestroy, onMount, type Snippet } from 'svelte';
  import { SvelteMap } from 'svelte/reactivity';
  import {
    AudioDecodeWatch,
    audioNotice,
    hasDecodeCounters,
    silenceThreshold,
    type AudioDecoding,
    type StreamAudio,
  } from '../audio';
  import { startPlayback } from '../autoplay';
  import type { StreamInfo } from '../bindings/StreamInfo';
  import type { StreamStats } from '../bindings/StreamStats';
  import { loadingMessage } from '../format';
  import {
    errorMessage,
    getSettings,
    loadSubtitles,
    parseSubtitles,
    recordPlayerMetric,
    reportPlayback,
    seekThumbnail,
    streamAudio,
    streamStats,
    streamSubtitles,
    type NetworkStatus,
    type Settings,
  } from '../ipc';
  import {
    SUBTITLE_ACCEPT,
    addCues,
    checkSubtitleFile,
    initialSubtitle,
    rememberSubtitle,
    rememberedSubtitle,
    subtitleOptions,
    toggledSubtitle,
    type LocalSubtitle,
    type StreamSubtitles,
    type SubtitleCues,
  } from '../subtitles';
  import { PlaybackTimer } from '../metrics';
  import { followNetwork, pausedMessage } from '../network';
  import { SeekGate } from '../seekgate';
  import { thumbnailSource } from '../seekbar';
  import type { SettingsSection } from '../settings';
  import { Ewma, bufferHealth, bufferedSeconds, etaSeconds, type StatusInfo } from '../stats';
  import Controls from './Controls.svelte';
  import DebugOverlay from './DebugOverlay.svelte';
  import SettingsPage from './settings/SettingsPage.svelte';

  interface Props {
    stream: StreamInfo;
    /** `performance.now()` when the user chose this lecture. */
    startedAt: number;
    /** Development autoplay: seek positions (fractions of the duration), 8 s apart. */
    autoSeeks?: number[];
    /** Seconds to continue from once the duration is known (returning to the player). */
    startAt?: number;
    onBack: () => void;
    /** The video played to its end (the queue may play the next one). */
    onEnded?: () => void;
    /** The position changed; `duration` is NaN until known. */
    onTime?: (currentTime: number, duration: number) => void;
    /** Shown over the video, also in fullscreen (the "Up next" card). */
    children?: Snippet;
    /**
     * The element to show fullscreen, if not the player itself: one that outlives this
     * player, so fullscreen stays on when the next video of the queue replaces it.
     */
    fullscreenTarget?: HTMLElement | undefined;
    /** Audio-only media (music, podcasts, audiobooks): shown as "now playing" artwork. */
    audio?: { title: string; album: string | null } | null;
  }

  let {
    stream,
    startedAt,
    autoSeeks = [],
    startAt = 0,
    onBack,
    onEnded,
    onTime,
    children,
    audio = null,
    fullscreenTarget,
  }: Props = $props();
  // Whether `startAt` has been applied.
  let resumed = false;

  const SEEK_STEP_SECS = 10;
  const AUTO_SEEK_INTERVAL_MS = 8000;
  /** Controls fade out after this long without pointer movement while playing. */
  const HIDE_AFTER_MS = 2500;
  /** How often the audio decoder counters are sampled. */
  const AUDIO_SAMPLE_MS = 500;
  /** A seek hold is released this long after `seeked` even if no frame was seen painted. */
  const SEEK_HOLD_MAX_MS = 1000;

  let video: HTMLVideoElement | undefined = $state();
  let container: HTMLDivElement | undefined = $state();
  let firstFrameSeen = $state(false);
  let stalled = $state(false);
  let showDebug = $state(false);
  let needsClick = $state(false);
  let playbackError = $state<string | null>(null);
  let stats = $state<StreamStats | null>(null);
  let statsError = $state<string | null>(null);
  let seekHolding = $state(false);
  let network = $state<NetworkStatus | null>(null);
  // Settings open over the video, so closing them returns to the same position.
  let settingsSection = $state<SettingsSection | null>(null);
  let resumeAfterSettings = false;
  // `advanced.debugOverlay`: whether D shows the statistics overlay.
  let debugKey = $state(true);

  // Mirrors of the video element's state, for the custom controls.
  let paused = $state(true);
  let currentTime = $state(0);
  let duration = $state(Number.NaN);
  let rate = $state(1);
  let muted = $state(false);
  let volume = $state(1);
  let fullscreen = $state(false);
  // Seek-bar pictures of this video (not of audio). The duration is read when one is asked
  // for, so it learning the duration does not forget the pictures.
  const thumbnails = $derived.by(() => {
    if (audio !== null) {
      return null;
    }
    const streamId = stream.streamId;
    return thumbnailSource((secs) => seekThumbnail(streamId, secs, duration).catch(() => null));
  });
  // Muted only because the autoplay policy refused sound; an "unmute" button shows meanwhile.
  let autoMuted = $state(false);

  // Whether the audio can be played: the backend's probe of the container header, and the
  // decoder counters (see audio.ts).
  let audioInfo = $state<StreamAudio | null>(null);
  let audioDecoding = $state<AudioDecoding>('unknown');
  let hasCounters = $state(false);
  let audioDismissed = $state(false);
  const audioMessage = $derived(
    audioDismissed || !firstFrameSeen ? null : audioNotice(audioInfo, audioDecoding, hasCounters),
  );

  // Subtitles: the torrent's files and embedded tracks, files the user loaded, and the one
  // showing (see subtitles.ts). Each is added as a text track the first time it is chosen.
  let subtitleList = $state<StreamSubtitles | null>(null);
  let localSubtitles = $state<LocalSubtitle[]>([]);
  let subtitle = $state<string | null>(null);
  let subtitleLoading = $state(false);
  let subtitleError = $state<string | null>(null);
  let subtitleMenuOpen = $state(false);
  let subtitleInput: HTMLInputElement | undefined = $state();
  const subtitleMenu = $derived(subtitleOptions(subtitleList, localSubtitles));
  const textTracks = new SvelteMap<string, TextTrack>();
  // The last track shown in this video, for the C key.
  let lastSubtitle: string | null = null;
  // `playback.subtitlesByDefault`, once the settings are read (null before).
  let subtitlesByDefault: boolean | null = null;
  let subtitleAutoSelected = false;
  let localCount = 0;
  let probeDone = false;
  let subtitleRefreshes = 0;

  // Control visibility: shown on pointer movement, hover, focus, pause, an open menu, or a drag.
  let recentlyActive = $state(true);
  let overControls = $state(false);
  let scrubbing = $state(false);
  let menuOpen = $state(false);
  const controlsVisible = $derived(
    recentlyActive ||
      paused ||
      overControls ||
      scrubbing ||
      menuOpen ||
      subtitleMenuOpen ||
      !firstFrameSeen ||
      settingsSection !== null,
  );

  const timer = new PlaybackTimer();
  const gate = new SeekGate();
  const decodeWatch = new AudioDecodeWatch();
  let audioTimer: ReturnType<typeof setInterval> | undefined;
  // Bumped by every start and every deliberate pause, so an interrupted start is retried only
  // while it is still the latest request.
  let playRequest = 0;
  let destroyed = false;
  const downRate = new Ewma(3);
  const upRate = new Ewma(3);
  let smoothedDown = $state(0);
  let smoothedUp = $state(0);
  const timeouts: ReturnType<typeof setTimeout>[] = [];
  let hideTimer: ReturnType<typeof setTimeout> | undefined;
  let seekHoldTimer: ReturnType<typeof setTimeout> | undefined;
  let frameHandle: number | null = null;
  let reportTimer: ReturnType<typeof setInterval> | undefined;
  let statsTimer: ReturnType<typeof setInterval> | undefined;
  let stopNetwork: (() => void) | undefined;
  let lastPoll = 0;

  const bufferedSecs = $derived(
    stats === null
      ? null
      : bufferedSeconds(stats.bufferedSecs, stats.bufferedBytes, stats.fileLength, duration),
  );

  const statusInfo = $derived<StatusInfo | null>(
    stats === null
      ? null
      : {
          downloadBps: smoothedDown,
          uploadBps: smoothedUp,
          peersLive: stats.peersLive,
          peersConnecting: stats.peersConnecting,
          peersSeen: stats.peersSeen,
          bufferedSecs,
          health: bufferHealth(bufferedSecs, stalled),
          fileDownloaded: stats.fileDownloaded,
          fileLength: stats.fileLength,
          etaSecs: etaSeconds(stats.fileLength - stats.fileDownloaded, smoothedDown),
          engine: stats.engine,
        },
  );

  const loading = $derived(
    loadingMessage({
      firstFrameSeen,
      stalled,
      hasData: stats !== null && (stats.fileDownloaded > 0 || stats.bufferedBytes > 0),
      bufferedSecs,
    }),
  );
  const seeking = $derived(seekHolding && firstFrameSeen);

  /**
   * The pill over the video: shown only while playback is actually blocked, saying why. When
   * the kill switch pauses traffic, that is the reason, not "Buffering…".
   */
  const status = $derived(
    playbackError ??
      (seeking || loading !== null ? pausedMessage(network) : null) ??
      (seeking ? 'Seeking…' : null) ??
      loading,
  );

  function poke(): void {
    recentlyActive = true;
    clearTimeout(hideTimer);
    hideTimer = setTimeout(() => {
      recentlyActive = false;
    }, HIDE_AFTER_MS);
  }

  function report(): void {
    if (!video) {
      return;
    }
    const known = Number.isFinite(video.duration) ? video.duration : null;
    reportPlayback(stream.streamId, video.currentTime, known).catch(() => undefined);
  }

  function frame(): void {
    const metric = timer.frame();
    if (!metric) {
      return;
    }
    recordPlayerMetric(stream.streamId, metric.kind, metric.millis).catch(() => undefined);
    if (metric.kind === 'firstFrame') {
      firstFrameSeen = true;
      poke();
      scheduleAutoSeeks();
    }
  }

  /** A frame was painted: release a seek hold so audio and video resume together. */
  function painted(mediaTime?: number): void {
    if (gate.framePainted(mediaTime)) {
      void startPlaying();
    }
    seekHolding = gate.holding;
  }

  function onVideoFrame(_now: number, metadata: VideoFrameCallbackMetadata): void {
    frame();
    painted(metadata.mediaTime);
    if (video) {
      frameHandle = video.requestVideoFrameCallback(onVideoFrame);
    }
  }

  function scheduleAutoSeeks(): void {
    autoSeeks.forEach((fraction, i) => {
      timeouts.push(
        setTimeout(
          () => {
            seekTo(fraction);
          },
          AUTO_SEEK_INTERVAL_MS * (i + 1),
        ),
      );
    });
  }

  /**
   * Starts playback. An interruption (a pause, a seek hold, a new load) is retried with the sound
   * unchanged; only an autoplay refusal mutes, and then the "unmute" button shows.
   */
  async function startPlaying(): Promise<void> {
    if (!video) {
      return;
    }
    const media = video;
    const request = ++playRequest;
    const outcome = await startPlayback(media, {
      // A seek hold resumes playback itself once the new position is painted.
      stillWanted: () => request === playRequest && !destroyed && !gate.holding,
    });
    switch (outcome) {
      case 'playing':
        needsClick = false;
        break;
      case 'mutedByPolicy':
        needsClick = false;
        autoMuted = true;
        break;
      case 'blocked':
        needsClick = true;
        break;
      case 'failed':
        needsClick = media.paused && playbackError === null;
        break;
      case 'cancelled':
        break;
    }
  }

  /** A deliberate pause: an interrupted start must not resume playback. */
  function pauseVideo(): void {
    playRequest += 1;
    video?.pause();
  }

  function unmute(): void {
    if (!video) {
      return;
    }
    video.muted = false;
    if (video.volume === 0) {
      video.volume = 1;
    }
    autoMuted = false;
  }

  /** Samples the decoder counters until they show audio being decoded. */
  function sampleAudio(): void {
    if (!video) {
      return;
    }
    decodeWatch.sample(video, !video.paused && !stalled && !seekHolding, performance.now());
    audioDecoding = decodeWatch.state(silenceThreshold(audioInfo));
    if (audioDecoding === 'decoding') {
      clearInterval(audioTimer);
    }
  }

  /** Shows only the text track of `id` (none for null). */
  function showSubtitleTrack(id: string | null): void {
    for (const [trackId, track] of textTracks) {
      track.mode = trackId === id ? 'showing' : 'disabled';
    }
  }

  /** The cues of a subtitle option: downloaded from the torrent, or the parsed local file. */
  function subtitleCues(id: string): Promise<SubtitleCues> {
    const local = localSubtitles.find((l) => l.id === id);
    if (local) {
      return Promise.resolve(local.cues);
    }
    const index = Number(id.replace(/^file:/, ''));
    return loadSubtitles(stream.streamId, index);
  }

  /**
   * Shows the subtitles of option `id`, or none. A track is created the first time it is
   * chosen; `remember` keeps the choice for the next video this session.
   */
  async function selectSubtitle(id: string | null, remember = true): Promise<void> {
    subtitle = id;
    subtitleError = null;
    const option = subtitleMenu.find((o) => o.id === id) ?? null;
    if (remember) {
      rememberSubtitle(option);
    }
    showSubtitleTrack(id);
    if (id === null || option === null || textTracks.has(id)) {
      if (id !== null) {
        lastSubtitle = id;
      }
      return;
    }
    subtitleLoading = true;
    try {
      const parsed = await subtitleCues(id);
      if (!video || destroyed) {
        return;
      }
      const track = video.addTextTrack('subtitles', option.label, option.language);
      addCues(track, parsed.cues);
      textTracks.set(id, track);
      lastSubtitle = id;
      // The choice may have changed while the file downloaded.
      showSubtitleTrack(subtitle);
    } catch (reason: unknown) {
      if (subtitle === id) {
        subtitle = null;
        subtitleError = `These subtitles could not be loaded: ${errorMessage(reason)}`;
      }
    } finally {
      subtitleLoading = false;
    }
  }

  /** Turns subtitles on as the session's choice or the setting says, once both are known. */
  function autoSelectSubtitle(): void {
    if (subtitleAutoSelected || subtitleList === null || subtitlesByDefault === null) {
      return;
    }
    subtitleAutoSelected = true;
    const id = initialSubtitle(
      subtitleMenu,
      rememberedSubtitle(),
      subtitlesByDefault,
      navigator.languages,
    );
    if (id !== null) {
      // Not a new choice: keep what the session remembers.
      void selectSubtitle(id, false);
    }
  }

  async function loadSubtitleFile(file: File): Promise<void> {
    const problem = checkSubtitleFile(file);
    if (problem !== null) {
      subtitleError = problem;
      return;
    }
    try {
      const cues = await parseSubtitles(new Uint8Array(await file.arrayBuffer()));
      localCount += 1;
      const id = `local:${String(localCount)}`;
      localSubtitles = [...localSubtitles, { id, name: file.name, cues }];
      await selectSubtitle(id);
    } catch (reason: unknown) {
      subtitleError = `This subtitle file could not be read: ${errorMessage(reason)}`;
    }
  }

  /**
   * Lists the stream's subtitles. Embedded tracks are known only once the container probe has
   * finished (`probeDone`), so the list is fetched again then if it lacked them.
   */
  async function refreshSubtitles(): Promise<void> {
    try {
      const list = await streamSubtitles(stream.streamId);
      subtitleList = list;
      autoSelectSubtitle();
      if (list.embedded === null && probeDone && subtitleRefreshes < 2) {
        subtitleRefreshes += 1;
        void refreshSubtitles();
      }
    } catch {
      // No subtitles then; the CC button stays hidden.
    }
  }

  async function pollStats(): Promise<void> {
    try {
      const next = await streamStats(stream.streamId);
      const now = performance.now();
      const dt = lastPoll === 0 ? 1 : (now - lastPoll) / 1000;
      lastPoll = now;
      smoothedDown = downRate.next(next.downloadBps, dt);
      smoothedUp = upRate.next(next.uploadBps, dt);
      stats = next;
      statsError = null;
    } catch (reason: unknown) {
      statsError = errorMessage(reason);
    }
  }

  function seekTo(fraction: number): void {
    if (video && Number.isFinite(video.duration)) {
      video.currentTime = Math.min(Math.max(fraction, 0), 1) * video.duration;
    }
  }

  function seekBy(secs: number): void {
    if (!video) {
      return;
    }
    const end = Number.isFinite(video.duration) ? video.duration : Infinity;
    video.currentTime = Math.min(Math.max(video.currentTime + secs, 0), end);
    poke();
  }

  function togglePlay(): void {
    if (!video) {
      return;
    }
    // The user's choice overrides any hold for a seek.
    gate.cancel();
    seekHolding = false;
    if (video.paused) {
      // The user pressed play, so sound is allowed now.
      if (autoMuted) {
        unmute();
      }
      void startPlaying();
    } else {
      pauseVideo();
    }
    poke();
  }

  function toggleMute(): void {
    if (!video) {
      return;
    }
    if (video.muted || video.volume === 0) {
      unmute();
    } else {
      video.muted = true;
    }
  }

  function setVolume(value: number): void {
    if (!video) {
      return;
    }
    video.volume = Math.min(Math.max(value, 0), 1);
    video.muted = value === 0;
  }

  function setRate(value: number): void {
    if (video) {
      video.playbackRate = value;
    }
  }

  function toggleFullscreen(): void {
    if (document.fullscreenElement) {
      void document.exitFullscreen();
    } else {
      void (fullscreenTarget ?? container)?.requestFullscreen();
    }
  }

  /**
   * Applies the playback settings: the default speed only when the stream starts (`initial`),
   * the debug overlay key whenever settings are read.
   */
  function applySettings(settings: Settings, initial: boolean): void {
    debugKey = settings.advanced.debugOverlay;
    if (!debugKey) {
      showDebug = false;
    }
    if (initial && video) {
      video.defaultPlaybackRate = settings.playback.defaultSpeed;
      video.playbackRate = settings.playback.defaultSpeed;
    }
    if (initial) {
      subtitlesByDefault = settings.playback.subtitlesByDefault;
      autoSelectSubtitle();
    }
  }

  function loadSettings(initial: boolean): void {
    getSettings()
      .then((view) => {
        applySettings(view.settings, initial);
      })
      .catch(() => {
        if (initial) {
          subtitlesByDefault = false;
          autoSelectSubtitle();
        }
      });
  }

  /** Opens Settings over the video, pausing it until they close. */
  function openSettings(section: SettingsSection): void {
    if (settingsSection !== null) {
      return;
    }
    menuOpen = false;
    subtitleMenuOpen = false;
    resumeAfterSettings = video !== undefined && !video.paused;
    pauseVideo();
    settingsSection = section;
  }

  function closeSettings(): void {
    settingsSection = null;
    loadSettings(false);
    if (resumeAfterSettings) {
      void startPlaying();
    }
    poke();
  }

  function onKey(event: KeyboardEvent): void {
    // The settings, and dialogs over the player (the queue's "Choose files"), handle their own
    // keys (Escape closes them).
    if (
      settingsSection !== null ||
      (event.target instanceof Element && event.target.closest('dialog') !== null)
    ) {
      return;
    }
    if (event.key === ',' && (event.ctrlKey || event.metaKey) && !event.altKey) {
      event.preventDefault();
      openSettings('playback');
      return;
    }
    if (event.ctrlKey || event.metaKey || event.altKey) {
      return;
    }
    switch (event.key) {
      case ' ':
        event.preventDefault();
        togglePlay();
        break;
      case 'ArrowLeft':
        event.preventDefault();
        seekBy(-SEEK_STEP_SECS);
        break;
      case 'ArrowRight':
        event.preventDefault();
        seekBy(SEEK_STEP_SECS);
        break;
      case 'f':
      case 'F':
        event.preventDefault();
        toggleFullscreen();
        break;
      case 'm':
      case 'M':
        event.preventDefault();
        toggleMute();
        poke();
        break;
      case 'd':
      case 'D':
        if (debugKey) {
          event.preventDefault();
          showDebug = !showDebug;
        }
        break;
      case 'c':
      case 'C':
        if (subtitleMenu.some((o) => o.enabled)) {
          event.preventDefault();
          void selectSubtitle(
            toggledSubtitle(subtitleMenu, subtitle, lastSubtitle, navigator.languages),
          );
          poke();
        }
        break;
      case 'Escape':
        if (menuOpen || subtitleMenuOpen) {
          event.preventDefault();
          menuOpen = false;
          subtitleMenuOpen = false;
        } else if (!document.fullscreenElement) {
          // While fullscreen, Escape only leaves fullscreen.
          event.preventDefault();
          onBack();
        }
        break;
      default:
        break;
    }
  }

  function onFullscreenChange(): void {
    fullscreen = document.fullscreenElement !== null;
  }

  onMount(() => {
    // Already fullscreen when the queue's next video replaces the last one.
    onFullscreenChange();
    timer.play(startedAt);
    if (video && 'requestVideoFrameCallback' in video) {
      frameHandle = video.requestVideoFrameCallback(onVideoFrame);
    }
    reportTimer = setInterval(report, 1000);
    statsTimer = setInterval(() => void pollStats(), 1000);
    void pollStats();
    window.addEventListener('keydown', onKey, { capture: true });
    document.addEventListener('fullscreenchange', onFullscreenChange);
    poke();
    loadSettings(true);
    stopNetwork = followNetwork((next) => {
      network = next;
    });
    hasCounters = video !== undefined && hasDecodeCounters(video);
    if (hasCounters) {
      audioTimer = setInterval(sampleAudio, AUDIO_SAMPLE_MS);
    }
    void refreshSubtitles();
    // Resolves once the backend has read the container header (or given up); the embedded
    // subtitle tracks are known then too.
    streamAudio(stream.streamId)
      .then((info) => {
        audioInfo = info;
        probeDone = true;
        if (subtitleList?.embedded === null) {
          void refreshSubtitles();
        }
      })
      .catch(() => undefined);
    void startPlaying();
  });

  onDestroy(() => {
    destroyed = true;
    playRequest += 1;
    clearInterval(audioTimer);
    stopNetwork?.();
    window.removeEventListener('keydown', onKey, { capture: true });
    document.removeEventListener('fullscreenchange', onFullscreenChange);
    clearInterval(reportTimer);
    clearInterval(statsTimer);
    clearTimeout(hideTimer);
    clearTimeout(seekHoldTimer);
    timeouts.forEach(clearTimeout);
    if (video) {
      if (frameHandle !== null) {
        video.cancelVideoFrameCallback(frameHandle);
      }
      // Close the connection to the stream server.
      video.pause();
      video.removeAttribute('src');
      video.load();
    }
  });

  const hasFrameCallback = $derived(video !== undefined && 'requestVideoFrameCallback' in video);
</script>

<div
  class="player"
  class:audio={audio !== null}
  class:idle={!controlsVisible}
  bind:this={container}
  role="presentation"
  onpointermove={poke}
  onpointerdown={poke}
>
  {#if audio}
    <div class="now-playing" aria-hidden="true">
      <div class="art">
        <svg viewBox="0 0 24 24"
          ><path
            d="M9 18V6l11-2v12M9 18a3 3 0 1 1-3-3 3 3 0 0 1 3 3Zm11-2a3 3 0 1 1-3-3 3 3 0 0 1 3 3Z"
          /></svg
        >
      </div>
      <p class="track">{audio.title}</p>
      {#if audio.album}<p class="album">{audio.album}</p>{/if}
    </div>
  {/if}
  <!-- svelte-ignore a11y_media_has_caption (captions are added as text tracks from script) -->
  <video
    bind:this={video}
    src={stream.url}
    preload="auto"
    playsinline
    onclick={togglePlay}
    ondblclick={toggleFullscreen}
    onplay={() => {
      paused = false;
    }}
    onpause={() => {
      paused = true;
    }}
    ontimeupdate={() => {
      if (video) {
        currentTime = video.currentTime;
        onTime?.(video.currentTime, video.duration);
      }
    }}
    ondurationchange={() => {
      if (video) {
        duration = video.duration;
        if (!resumed && Number.isFinite(duration)) {
          resumed = true;
          if (startAt > 0 && startAt < duration - 1) {
            video.currentTime = startAt;
          }
        }
        onTime?.(video.currentTime, video.duration);
      }
    }}
    onended={() => {
      report();
      onEnded?.();
    }}
    onratechange={() => {
      if (video) {
        rate = video.playbackRate;
      }
    }}
    onvolumechange={() => {
      if (video) {
        muted = video.muted;
        volume = video.volume;
        if (!video.muted) {
          autoMuted = false;
        }
      }
    }}
    onseeking={() => {
      timer.seeking();
      if (video) {
        currentTime = video.currentTime;
        if (gate.seeking(video.paused, video.currentTime)) {
          video.pause();
        }
      }
      seekHolding = gate.holding;
      report();
    }}
    onseeked={() => {
      timer.seeked();
      // The new position may already be painted (WebView2 paints it before `seeked`).
      if (gate.seeked()) {
        void startPlaying();
      }
      seekHolding = gate.holding;
      if (!hasFrameCallback) {
        frame();
        painted();
      }
      // Never stay paused because a paint went unseen.
      clearTimeout(seekHoldTimer);
      seekHoldTimer = setTimeout(() => {
        if (gate.expire()) {
          void startPlaying();
        }
        seekHolding = gate.holding;
      }, SEEK_HOLD_MAX_MS);
    }}
    onplaying={() => {
      stalled = false;
      if (!hasFrameCallback) {
        frame();
      }
    }}
    onwaiting={() => {
      stalled = true;
    }}
    oncanplay={() => {
      stalled = false;
    }}
    onerror={() => {
      playbackError =
        'This video could not be played by the built-in player (its format may not be supported yet).';
    }}
  ></video>

  <div class="topbar" class:visible={controlsVisible}>
    <span class="title">{stream.fileName}</span>
  </div>

  {#if status}
    <div class="pill" aria-live="polite">{status}</div>
  {:else if needsClick}
    <div class="pill">Press Space or the play button to start</div>
  {/if}

  {#if autoMuted || audioMessage || subtitleLoading || subtitleError}
    <div class="notices">
      {#if autoMuted}
        <button
          type="button"
          class="unmute"
          onclick={() => {
            unmute();
            poke();
          }}
        >
          <svg viewBox="0 0 24 24" aria-hidden="true">
            <path d="M4 9v6h4l5 4V5L8 9zM16 9l5 6M21 9l-5 6" />
          </svg>
          Muted — click to unmute
        </button>
      {/if}
      {#if audioMessage}
        <div class="notice" role="status">
          <span>{audioMessage}</span>
          <button
            type="button"
            class="dismiss"
            aria-label="Dismiss"
            onclick={() => {
              audioDismissed = true;
            }}
          >
            <svg viewBox="0 0 24 24" aria-hidden="true"><path d="M6 6l12 12M18 6L6 18" /></svg>
          </button>
        </div>
      {/if}
      {#if subtitleLoading}
        <div class="notice" role="status"><span>Loading subtitles…</span></div>
      {/if}
      {#if subtitleError}
        <div class="notice" role="status">
          <span>{subtitleError}</span>
          <button
            type="button"
            class="dismiss"
            aria-label="Dismiss"
            onclick={() => {
              subtitleError = null;
            }}
          >
            <svg viewBox="0 0 24 24" aria-hidden="true"><path d="M6 6l12 12M18 6L6 18" /></svg>
          </button>
        </div>
      {/if}
    </div>
  {/if}

  <input
    bind:this={subtitleInput}
    class="file-input"
    type="file"
    accept={SUBTITLE_ACCEPT}
    tabindex="-1"
    aria-hidden="true"
    onchange={(event) => {
      const input = event.currentTarget;
      const file = input.files?.[0];
      // Cleared, so choosing the same file again still fires `change`.
      input.value = '';
      if (file) {
        void loadSubtitleFile(file);
      }
    }}
  />

  <Controls
    visible={controlsVisible}
    {paused}
    {currentTime}
    {duration}
    pieceMap={stats?.pieceMap ?? []}
    {thumbnails}
    {rate}
    {muted}
    {volume}
    {fullscreen}
    status={statusInfo}
    bind:menuOpen
    onTogglePlay={togglePlay}
    onSeek={(fraction: number) => {
      seekTo(fraction);
    }}
    onRate={(value: number) => {
      setRate(value);
    }}
    onToggleMute={toggleMute}
    onVolume={(value: number) => {
      setVolume(value);
    }}
    onToggleFullscreen={toggleFullscreen}
    {onBack}
    onScrub={(dragging: boolean) => {
      scrubbing = dragging;
    }}
    onHover={(inside: boolean) => {
      overControls = inside;
    }}
    onSettings={openSettings}
    subtitles={subtitleMenu}
    {subtitle}
    bind:subtitleMenuOpen
    onSubtitle={(id: string | null) => {
      void selectSubtitle(id);
    }}
    onLoadSubtitles={() => {
      subtitleInput?.click();
    }}
  />

  {@render children?.()}

  {#if showDebug}
    <DebugOverlay {stats} error={statsError} />
  {/if}

  {#if settingsSection !== null}
    <div class="settings-overlay">
      <SettingsPage
        section={settingsSection}
        backLabel="Back to the video"
        onBack={closeSettings}
      />
    </div>
  {/if}
</div>

<style>
  .player {
    position: relative;
    height: 100%;
    overflow: hidden;
    background: #000;
  }

  .player.idle {
    cursor: none;
  }

  /* Audio: artwork in place of a picture; clicks still reach the media element beneath. */
  .now-playing {
    position: absolute;
    inset: 0;
    z-index: 1;
    display: flex;
    flex-direction: column;
    align-items: center;
    justify-content: center;
    gap: 0.4rem;
    padding: 2rem;
    background: radial-gradient(circle at 50% 40%, rgb(242 180 90 / 14%), #000 65%);
    pointer-events: none;
    text-align: center;
  }

  .now-playing .art {
    display: grid;
    width: min(14rem, 40vmin);
    aspect-ratio: 1;
    margin-bottom: 1rem;
    place-items: center;
    border: 1px solid rgb(255 255 255 / 8%);
    border-radius: 1rem;
    background: linear-gradient(145deg, #2a2117, #121014);
    box-shadow: 0 1.5rem 4rem rgb(0 0 0 / 50%);
  }

  .now-playing svg {
    width: 38%;
    height: 38%;
    fill: none;
    stroke: #f2b45a;
    stroke-width: 1.4;
    stroke-linecap: round;
    stroke-linejoin: round;
  }

  .now-playing .track {
    max-width: 40rem;
    margin: 0;
    overflow: hidden;
    color: #f4f1ea;
    font-size: 1.3rem;
    font-weight: 600;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  .now-playing .album {
    max-width: 40rem;
    margin: 0;
    overflow: hidden;
    color: rgb(244 241 234 / 60%);
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  /* The cursor never hides over audio: there is no picture to keep clear. */
  .player.audio.idle {
    cursor: default;
  }

  video {
    /* Block, not inline: inline media adds a baseline gap that overflows the window. */
    display: block;
    width: 100%;
    height: 100%;
    background: #000;
    object-fit: contain;
  }

  /*
   * Subtitles. Chromium sizes cue text from the video's size (about 5% of its height), so
   * it grows in fullscreen; this sets a readable look on a background.
   */
  video::cue {
    background: rgb(0 0 0 / 72%);
    color: #fff;
    font-family: inherit;
    line-height: 1.35;
  }

  /* Lift subtitles above the controls while they show. */
  video::-webkit-media-text-track-container {
    transition: transform 0.35s ease;
  }

  .player:not(.idle) video::-webkit-media-text-track-container {
    transform: translateY(-4.5rem);
  }

  .file-input {
    display: none;
  }

  .topbar {
    position: absolute;
    top: 0;
    left: 0;
    right: 0;
    padding: 0.75rem 1.25rem 2rem;
    background: linear-gradient(180deg, rgb(0 0 0 / 60%) 0%, rgb(0 0 0 / 0%) 100%);
    opacity: 0;
    transition: opacity 0.35s ease;
    pointer-events: none;
  }

  .topbar.visible {
    opacity: 1;
  }

  .title {
    display: block;
    overflow: hidden;
    color: var(--text);
    font-size: 0.9rem;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  /* Inside the player, so it also shows in fullscreen. */
  .settings-overlay {
    position: absolute;
    inset: 0;
    z-index: 5;
    background: var(--bg);
    cursor: auto;
  }

  .pill {
    position: absolute;
    left: 50%;
    top: 50%;
    max-width: 80%;
    padding: 0.5rem 1rem;
    border-radius: 2rem;
    background: rgb(10 10 12 / 65%);
    color: var(--text);
    font-size: 0.9rem;
    text-align: center;
    letter-spacing: 0.03em;
    transform: translate(-50%, -50%);
    pointer-events: none;
  }

  /* Below the title, above the video; they stay while the controls fade. */
  .notices {
    position: absolute;
    top: 3rem;
    left: 50%;
    z-index: 3;
    display: flex;
    flex-direction: column;
    align-items: center;
    gap: 0.5rem;
    width: max-content;
    max-width: min(36rem, 90%);
    transform: translateX(-50%);
  }

  .unmute,
  .notice {
    background: rgb(10 10 12 / 65%);
    color: var(--text);
    font: inherit;
    font-size: 0.85rem;
  }

  .unmute {
    display: inline-flex;
    align-items: center;
    gap: 0.45rem;
    padding: 0.45rem 1rem 0.45rem 0.8rem;
    border: none;
    border-radius: 2rem;
    letter-spacing: 0.03em;
    cursor: pointer;
  }

  .unmute:hover,
  .unmute:focus-visible {
    background: rgb(30 30 36 / 80%);
  }

  .notice {
    display: flex;
    align-items: flex-start;
    gap: 0.5rem;
    padding: 0.55rem 0.5rem 0.55rem 0.9rem;
    border-radius: 0.6rem;
    line-height: 1.4;
  }

  .dismiss {
    display: grid;
    flex: none;
    place-items: center;
    width: 1.5rem;
    height: 1.5rem;
    padding: 0;
    border: none;
    border-radius: 0.3rem;
    background: none;
    color: var(--text-muted);
    cursor: pointer;
  }

  .dismiss:hover,
  .dismiss:focus-visible {
    background: rgb(255 255 255 / 10%);
    color: var(--text);
  }

  .notices svg {
    width: 1.1rem;
    height: 1.1rem;
    fill: none;
    stroke: currentColor;
    stroke-width: 1.6;
    stroke-linecap: round;
    stroke-linejoin: round;
  }

  .dismiss svg {
    width: 0.9rem;
    height: 0.9rem;
  }
</style>
