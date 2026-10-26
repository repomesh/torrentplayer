// SPDX-License-Identifier: MIT OR Apache-2.0

/**
 * Playing a video whose audio the built-in player can't decode (Dolby, DTS), through the
 * backend's audio conversion (`convert_audio`, see docs/player.md).
 *
 * The backend converts the audio to AAC from a given position and serves fragmented MP4 that
 * keeps the video's own timestamps. This feeds it to the video element through Media Source
 * Extensions, so the seek bar, duration, and subtitles work as usual. Reading pauses once a
 * minute is buffered ahead (less for high-bitrate video, see {@link bufferLimits}), which also
 * pauses the conversion. After a seek, data is next needed where the buffered run at the playhead
 * ends; a new conversion starts there unless the running one will deliver it soon (see
 * {@link planSeek}). A conversion that stops before the end, such as when FFmpeg fails, is
 * continued from where it stopped.
 */

import type { ConvertedStream } from './bindings/ConvertedStream';

export type { ConvertedStream };

/** Reading pauses with at most this many seconds buffered ahead of the playhead... */
export const AHEAD_MAX_SECS = 60;
/** ...and resumes below this share of the limit. */
const AHEAD_RESUME_SHARE = 2 / 3;
/** At most this many seconds are kept behind the playhead; older data is removed. */
export const BEHIND_KEEP_SECS = 30;
/**
 * Bytes of converted video kept buffered. Chromium refuses appends beyond about 150 MB per source
 * buffer (`QuotaExceededError`), which a 4K stream at 27 Mbit/s fills in 45 seconds, so such
 * video is buffered for fewer seconds than {@link AHEAD_MAX_SECS}.
 */
export const BUFFER_BUDGET_BYTES = 96 * 1024 * 1024;
/** Look-ahead and look-behind kept however high the bitrate. */
const MIN_AHEAD_SECS = 10;
const MIN_BEHIND_SECS = 4;
/** Media seconds a conversion must deliver before its bitrate counts. */
const RATE_AFTER_SECS = 2;
/** A seek at most this far past what the running conversion has delivered waits for it. */
export const REACH_SECS = 3;
/** How often a paused reader checks whether to resume. */
const ROOM_POLL_MS = 500;
/** Retry temporary conversion/read failures without abandoning audio on the first stall. */
const MAX_RETRIES = 2;
const RETRY_DELAY_MS = 1000;

/** How much to buffer ahead of and keep behind the playhead, in seconds. */
export interface BufferLimits {
  /** Reading pauses with this much buffered ahead... */
  aheadMax: number;
  /** ...and resumes below this. */
  aheadResume: number;
  /** Data further behind the playhead is removed. */
  behindKeep: number;
}

/**
 * The buffer limits for converted media of `bytesPerSec` (0 while unknown), keeping what is
 * buffered within `budget` bytes: a quarter of it behind the playhead, the rest ahead.
 */
export function bufferLimits(bytesPerSec: number, budget = BUFFER_BUDGET_BYTES): BufferLimits {
  const fits = bytesPerSec > 0 ? budget / bytesPerSec : Number.POSITIVE_INFINITY;
  const behindKeep = clamp(fits / 4, MIN_BEHIND_SECS, BEHIND_KEEP_SECS);
  const aheadMax = clamp(fits - behindKeep, MIN_AHEAD_SECS, AHEAD_MAX_SECS);
  return { aheadMax, aheadResume: aheadMax * AHEAD_RESUME_SHARE, behindKeep };
}

function clamp(value: number, min: number, max: number): number {
  return Math.min(Math.max(value, min), max);
}

/** Seconds buffered from `time` onward in the range that contains it (0 if none does). */
export function bufferedAhead(ranges: TimeRanges, time: number): number {
  for (let i = 0; i < ranges.length; i++) {
    if (ranges.start(i) <= time + 0.1 && time <= ranges.end(i)) {
      return ranges.end(i) - time;
    }
  }
  return 0;
}

/** What to do after a seek: nothing, finish (all is buffered), or convert from a position. */
export type SeekPlan =
  { action: 'keep' } | { action: 'complete' } | { action: 'convert'; from: number };

/**
 * The plan after a seek to `time`. Data is next needed where the buffered run at the playhead
 * ends (at the playhead if it is not buffered). The running conversion, which covers from `start`
 * to `end` so far and keeps going, is kept if it will deliver that soon.
 */
export function planSeek(
  ranges: TimeRanges,
  time: number,
  duration: number,
  running: { start: number; end: number } | null,
): SeekPlan {
  const from = reachedFrom(ranges, time);
  if (Number.isFinite(duration) && from >= duration - 0.5) {
    return { action: 'complete' };
  }
  if (running !== null && from >= running.start - 0.5 && from <= running.end + REACH_SECS) {
    return { action: 'keep' };
  }
  return { action: 'convert', from };
}

/**
 * Whether a conversion that delivered up to `end` stopped before the video's `duration`
 * (unknown durations count as complete).
 */
export function endedEarly(end: number, duration: number): boolean {
  return Number.isFinite(duration) && end < duration - 1;
}

/** The end of the buffered range that contains `time`, or `time` if none does. */
function reachedFrom(ranges: TimeRanges, time: number): number {
  return time + bufferedAhead(ranges, time);
}

export interface ConvertedOptions {
  /** Starts a conversion from `start` seconds (the `convert_audio` command). */
  convert: (start: number) => Promise<ConvertedStream>;
  /** The conversion is playing from `start`: the caller may resume playback. */
  onReady: () => void;
  /** Conversion failed; the caller should go back to the original stream. */
  onError: (reason: unknown) => void;
}

/** Feeds the backend's audio conversion to a video element. */
export class ConvertedPlayback {
  private readonly source = new MediaSource();
  private readonly objectUrl = URL.createObjectURL(this.source);
  private buffer: SourceBuffer | null = null;
  private reader: ReadableStreamDefaultReader<Uint8Array<ArrayBuffer>> | null = null;
  private request: AbortController | null = null;
  // Bumped by every new conversion and by `destroy`, which ends older readers.
  private generation = 0;
  private running: { start: number; end: number } | null = null;
  /** The converted media's bitrate as measured so far (0 until known). */
  private bytesPerSec = 0;
  /** Lowered when the webview's buffer quota turns out smaller than {@link BUFFER_BUDGET_BYTES}. */
  private budget = BUFFER_BUDGET_BYTES;
  private destroyed = false;
  private stopOpening: (() => void) | null = null;

  constructor(
    private readonly video: HTMLVideoElement,
    private readonly options: ConvertedOptions,
  ) {}

  /** Whether this webview has Media Source Extensions. */
  static supported(): boolean {
    return typeof MediaSource !== 'undefined';
  }

  /** The `src` for the video element. */
  get src(): string {
    return this.objectUrl;
  }

  /** Converts from `start`; call after the video element's `src` is set to {@link src}. */
  async start(start: number): Promise<void> {
    if (this.destroyed) {
      return;
    }
    if (this.source.readyState !== 'open') {
      await new Promise<void>((resolve) => {
        const finish = (): void => {
          this.source.removeEventListener('sourceopen', finish);
          this.stopOpening = null;
          resolve();
        };
        this.stopOpening = finish;
        this.source.addEventListener('sourceopen', finish, { once: true });
      });
    }
    if (!this.current(this.generation)) {
      return;
    }
    await this.load(start, true);
  }

  /** Call on the video's `seeking` event. */
  seeking(): void {
    const buffer = this.buffer;
    if (buffer === null) {
      return;
    }
    const plan = planSeek(
      buffer.buffered,
      this.video.currentTime,
      this.source.duration,
      this.running,
    );
    if (plan.action === 'convert') {
      void this.load(plan.from, false);
    } else if (plan.action === 'complete') {
      void this.complete(buffer).catch((reason: unknown) => {
        if (!this.destroyed) {
          this.options.onError(reason);
        }
      });
    }
  }

  destroy(): void {
    this.destroyed = true;
    this.generation += 1;
    this.stopOpening?.();
    this.request?.abort();
    void this.reader?.cancel().catch(() => undefined);
    URL.revokeObjectURL(this.objectUrl);
  }

  private current(generation: number): boolean {
    return generation === this.generation && !this.destroyed;
  }

  /** Everything up to the end is buffered: stops converting and ends the stream. */
  private async complete(buffer: SourceBuffer): Promise<void> {
    const generation = ++this.generation;
    this.request?.abort();
    void this.reader?.cancel().catch(() => undefined);
    this.reader = null;
    this.running = null;
    await idle(buffer);
    if (this.current(generation) && this.source.readyState === 'open') {
      this.source.endOfStream();
    }
  }

  /** Starts a conversion from `start` and appends its output until it ends or is replaced. */
  private async load(start: number, first: boolean, retries = 0): Promise<void> {
    const generation = ++this.generation;
    this.request?.abort();
    const request = new AbortController();
    this.request = request;
    void this.reader?.cancel().catch(() => undefined);
    this.reader = null;
    this.running = { start, end: start };
    let retryable = true;
    let retryFrom = start;
    let ready = !first;
    try {
      const converted = await this.options.convert(start);
      if (!this.current(generation)) {
        return;
      }
      retryable = false;
      const buffer = await this.prepare(converted);
      if (!this.current(generation)) {
        return;
      }
      retryable = true;
      const response = await fetch(converted.url, { cache: 'no-store', signal: request.signal });
      if (!response.ok || response.body === null) {
        throw new Error(`The converted stream failed (HTTP ${String(response.status)}).`);
      }
      const reader = response.body.getReader();
      if (!this.current(generation)) {
        void reader.cancel().catch(() => undefined);
        return;
      }
      this.reader = reader;
      let received = 0;
      for (;;) {
        await this.roomAhead(generation);
        if (!this.current(generation)) {
          return;
        }
        retryable = true;
        const { done, value } = await reader.read();
        if (!this.current(generation)) {
          return;
        }
        if (done) {
          await idle(buffer);
          if (!this.current(generation)) {
            return;
          }
          const end = this.running.end;
          retryFrom = Math.max(start, end);
          this.running = null;
          this.reader = null;
          if (endedEarly(end, this.source.duration)) {
            // FFmpeg stopped before the end (a read error, say). Continue from there, unless
            // that is where the previous attempt stopped too.
            if (end <= start + 1) {
              throw new Error(`The conversion stopped at ${end.toFixed(1)} s.`);
            }
            void this.load(end, false);
          } else if (this.source.readyState === 'open') {
            this.source.endOfStream();
          }
          return;
        }
        retryable = false;
        if (!(await this.append(buffer, value, generation))) {
          return;
        }
        // Follow the conversion's leading edge: data behind it may have been trimmed already.
        this.running.end = Math.max(
          this.running.end,
          reachedFrom(buffer.buffered, this.running.end),
        );
        received += value.byteLength;
        if (this.running.end - start >= RATE_AFTER_SECS) {
          this.bytesPerSec = received / (this.running.end - start);
        }
        if (!ready && this.video.readyState >= HTMLMediaElement.HAVE_METADATA) {
          ready = true;
          if (start > 0) {
            this.video.currentTime = start;
          }
          this.options.onReady();
        }
      }
    } catch (reason: unknown) {
      if (this.current(generation)) {
        if (retryable && retries < MAX_RETRIES) {
          const from = Math.max(retryFrom, this.running?.end ?? start);
          request.abort();
          void this.reader?.cancel().catch(() => undefined);
          await sleep(RETRY_DELAY_MS);
          // A new seek or closing the player supersedes the pending retry.
          if (this.current(generation)) {
            await this.load(from, !ready, retries + 1);
          }
          return;
        }
        this.options.onError(reason);
      }
    }
  }

  /** The source buffer: created for the first conversion, reset for later ones. */
  private async prepare(converted: ConvertedStream): Promise<SourceBuffer> {
    if (this.buffer === null) {
      if (!MediaSource.isTypeSupported(converted.mime)) {
        throw new Error(`This video's format can't be played (${converted.mime}).`);
      }
      this.buffer = this.source.addSourceBuffer(converted.mime);
      this.buffer.mode = 'segments';
      if (converted.duration !== null) {
        this.source.duration = converted.duration;
      }
      return this.buffer;
    }
    const buffer = this.buffer;
    // Drop a half-appended fragment of the previous conversion; the new one starts with its own
    // initialization segment.
    if (this.source.readyState === 'open') {
      buffer.abort();
    }
    await idle(buffer);
    return buffer;
  }

  /** The buffer limits for the bitrate measured so far. */
  private limits(): BufferLimits {
    return bufferLimits(this.bytesPerSec, this.budget);
  }

  /**
   * Appends `data`, first removing old data behind the playhead. Returns false, without
   * appending, once the conversion `generation` has been replaced: a newer one may have reset
   * the buffer for its own initialization segment.
   */
  private async append(
    buffer: SourceBuffer,
    data: Uint8Array<ArrayBuffer>,
    generation: number,
  ): Promise<boolean> {
    await this.trimBehind(buffer);
    for (let attempt = 0; ; attempt++) {
      if (!this.current(generation)) {
        return false;
      }
      try {
        buffer.appendBuffer(data);
        break;
      } catch (reason: unknown) {
        if (!(reason instanceof DOMException && reason.name === 'QuotaExceededError')) {
          throw reason;
        }
        this.shrinkBudget(buffer.buffered);
        if (attempt > 0 && bufferedAhead(buffer.buffered, this.video.currentTime) < 1) {
          // Nothing is left to free; waiting cannot help when playback is stalled for this data.
          throw reason;
        }
        if (attempt > 0) {
          // The buffer is full of video still to be played, as when a high bitrate fills the
          // quota before the look-ahead limit: wait for playback to use some of it up.
          await sleep(ROOM_POLL_MS);
        }
        // Backward seeks can leave distant future ranges occupying the quota. Free both sides
        // of a small playback window.
        await this.trimForQuota(buffer, generation);
      }
    }
    await idle(buffer);
    return this.current(generation);
  }

  /** Keeps later buffering below what the webview just refused to exceed. */
  private shrinkBudget(ranges: TimeRanges): void {
    let seconds = 0;
    for (let i = 0; i < ranges.length; i++) {
      seconds += ranges.end(i) - ranges.start(i);
    }
    const held = seconds * this.bytesPerSec;
    if (held > 0) {
      this.budget = Math.min(this.budget, held * 0.8);
    }
  }

  private async trimForQuota(buffer: SourceBuffer, generation: number): Promise<void> {
    await idle(buffer);
    if (!this.current(generation)) {
      return;
    }
    const behind = Math.max(0, this.video.currentTime - 1);
    if (behind > 0) {
      buffer.remove(0, behind);
      await idle(buffer);
    }
    if (!this.current(generation)) {
      return;
    }
    // Keep the active conversion's output: discarding it would leave a gap before the next
    // fragment. Only unrelated future ranges left by earlier seeks may be evicted.
    const ahead = Math.max(this.video.currentTime + REACH_SECS, (this.running?.end ?? 0) + 0.1);
    const ranges = buffer.buffered;
    const end = ranges.length > 0 ? ranges.end(ranges.length - 1) : 0;
    if (end > ahead) {
      buffer.remove(ahead, end);
      await idle(buffer);
    }
  }

  /** Removes data further behind the playhead than the limits keep. */
  private async trimBehind(buffer: SourceBuffer): Promise<void> {
    const until = this.video.currentTime - this.limits().behindKeep;
    const ranges = buffer.buffered;
    if (until > 0 && ranges.length > 0 && ranges.start(0) < until) {
      await idle(buffer);
      buffer.remove(0, until);
      await idle(buffer);
    }
  }

  /** Waits while enough is buffered ahead, until this conversion is replaced. */
  private async roomAhead(generation: number): Promise<void> {
    const buffer = this.buffer;
    if (
      buffer === null ||
      bufferedAhead(buffer.buffered, this.video.currentTime) < this.limits().aheadMax
    ) {
      return;
    }
    while (
      this.current(generation) &&
      bufferedAhead(buffer.buffered, this.video.currentTime) >= this.limits().aheadResume
    ) {
      await sleep(ROOM_POLL_MS);
    }
  }
}

/** Resolves once `buffer` is not updating, rejecting asynchronous MSE parser failures. */
function idle(buffer: SourceBuffer): Promise<void> {
  if (!buffer.updating) {
    return Promise.resolve();
  }
  return new Promise((resolve, reject) => {
    const cleanup = (): void => {
      buffer.removeEventListener('updateend', done);
      buffer.removeEventListener('error', failed);
      buffer.removeEventListener('abort', failed);
    };
    const done = (): void => {
      cleanup();
      resolve();
    };
    const failed = (): void => {
      cleanup();
      reject(new Error('The converted media buffer failed.'));
    };
    buffer.addEventListener('updateend', done, { once: true });
    buffer.addEventListener('error', failed, { once: true });
    buffer.addEventListener('abort', failed, { once: true });
  });
}

function sleep(ms: number): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, ms));
}
