// SPDX-License-Identifier: MIT OR Apache-2.0

import { afterEach, describe, expect, it, vi } from 'vitest';
import { ConvertedPlayback, REACH_SECS, bufferedAhead, endedEarly, planSeek } from './converted';

/** `TimeRanges` from `[start, end]` pairs. */
function ranges(...pairs: [number, number][]): TimeRanges {
  return {
    length: pairs.length,
    start: (i: number) => pairs[i]?.[0] ?? Number.NaN,
    end: (i: number) => pairs[i]?.[1] ?? Number.NaN,
  };
}

describe('bufferedAhead', () => {
  it('measures from the playhead to the end of its range', () => {
    const buffered = ranges([0, 30], [100, 160]);
    expect(bufferedAhead(buffered, 10)).toBe(20);
    expect(bufferedAhead(buffered, 120)).toBe(40);
  });

  it('is zero outside every range', () => {
    expect(bufferedAhead(ranges([0, 30]), 50)).toBe(0);
    expect(bufferedAhead(ranges(), 0)).toBe(0);
  });

  it('counts a range that starts a moment after the playhead', () => {
    // Video starts at the keyframe, audio a few milliseconds later.
    expect(bufferedAhead(ranges([30.02, 40]), 30)).toBeCloseTo(10);
  });
});

describe('planSeek', () => {
  const running = { start: 100, end: 130 };

  it('keeps the conversion when it continues where the buffer ends', () => {
    expect(planSeek(ranges([100, 130]), 120, 600, running)).toEqual({ action: 'keep' });
    // Just past what it has delivered: it will be there in a moment.
    expect(planSeek(ranges([100, 130]), 130 + REACH_SECS - 1, 600, running)).toEqual({
      action: 'keep',
    });
    // Where it started, before anything arrived.
    expect(planSeek(ranges(), 100, 600, { start: 100, end: 100 })).toEqual({ action: 'keep' });
  });

  it('converts from where the buffered run at the playhead ends', () => {
    // Back into an older run: data is needed after it, not at the playhead.
    expect(planSeek(ranges([0, 30], [100, 130]), 10, 600, running)).toEqual({
      action: 'convert',
      from: 30,
    });
    expect(planSeek(ranges([100, 130]), 500, 600, running)).toEqual({
      action: 'convert',
      from: 500,
    });
    expect(planSeek(ranges([0, 30]), 50, 600, null)).toEqual({ action: 'convert', from: 50 });
  });

  it('finishes when everything to the end is buffered', () => {
    expect(planSeek(ranges([94, 120.04]), 112, 120, running)).toEqual({ action: 'complete' });
  });

  it('never finishes with an unknown duration', () => {
    expect(planSeek(ranges([94, 120]), 112, Number.NaN, null)).toEqual({
      action: 'convert',
      from: 120,
    });
  });
});

describe('endedEarly', () => {
  it('treats a conversion that reached the end as complete', () => {
    expect(endedEarly(120.04, 120)).toBe(false);
    expect(endedEarly(119.5, 120)).toBe(false);
  });

  it('notices a conversion that stopped before the end', () => {
    expect(endedEarly(64, 120)).toBe(true);
  });

  it('counts an unknown duration as complete', () => {
    expect(endedEarly(64, Number.NaN)).toBe(false);
    expect(endedEarly(64, Number.POSITIVE_INFINITY)).toBe(false);
  });
});

/** Minimal browser objects for exercising the asynchronous conversion lifecycle. */
class TestBuffer extends EventTarget {
  updating = false;
  buffered = ranges([0, 10]);
  mode = 'segments';
  fail = false;
  abort(): void {}
  remove(): void {}
  appendBuffer(): void {
    this.updating = true;
    queueMicrotask(() => {
      this.updating = false;
      if (this.fail) {
        this.dispatchEvent(new Event('error'));
      }
      this.dispatchEvent(new Event('updateend'));
    });
  }
}

class TestSource extends EventTarget {
  static latest: TestSource;
  static isTypeSupported(): boolean {
    return true;
  }
  readyState = 'open';
  duration = 10;
  buffer = new TestBuffer();
  constructor() {
    super();
    TestSource.latest = this;
  }
  addSourceBuffer(): TestBuffer {
    return this.buffer;
  }
  endOfStream(): void {
    this.readyState = 'ended';
  }
}

describe('ConvertedPlayback lifecycle', () => {
  afterEach(() => {
    vi.useRealTimers();
    vi.restoreAllMocks();
    vi.unstubAllGlobals();
  });

  function setup() {
    vi.stubGlobal('MediaSource', TestSource);
    vi.stubGlobal('HTMLMediaElement', { HAVE_METADATA: 1 });
    vi.spyOn(URL, 'createObjectURL').mockReturnValue('blob:test');
    vi.spyOn(URL, 'revokeObjectURL').mockImplementation(() => undefined);
    const convert = vi
      .fn()
      .mockResolvedValue({ url: 'http://localhost/converted', mime: 'video/mp4', duration: 10 });
    const onError = vi.fn();
    const video = { currentTime: 0, readyState: 1 } as HTMLVideoElement;
    const playback = new ConvertedPlayback(video, { convert, onReady: vi.fn(), onError });
    vi.stubGlobal(
      'fetch',
      vi.fn().mockImplementation(() =>
        Promise.resolve(
          new Response(
            new ReadableStream({
              start(controller) {
                controller.enqueue(new Uint8Array([1]));
                controller.close();
              },
            }),
          ),
        ),
      ),
    );
    return { playback, convert, onError, video, source: TestSource.latest };
  }

  it('retries a temporary conversion failure and preserves audio', async () => {
    vi.useFakeTimers();
    const { playback, convert, onError } = setup();
    convert.mockRejectedValueOnce(new Error('FFmpeg did not start writing in time'));
    const starting = playback.start(0);
    await vi.runAllTimersAsync();
    await starting;
    expect(convert).toHaveBeenCalledTimes(2);
    expect(onError).not.toHaveBeenCalled();
    playback.destroy();
  });

  it('bounds retries when conversion keeps failing', async () => {
    vi.useFakeTimers();
    const { playback, convert, onError } = setup();
    convert.mockRejectedValue(new Error('Conversion failed'));
    const starting = playback.start(0);
    await vi.runAllTimersAsync();
    await starting;
    expect(convert).toHaveBeenCalledTimes(3);
    expect(onError).toHaveBeenCalledOnce();
    playback.destroy();
  });

  it('cancels a pending retry when destroyed', async () => {
    vi.useFakeTimers();
    const { playback, convert, onError } = setup();
    convert.mockRejectedValue(new Error('Conversion failed'));
    const starting = playback.start(0);
    await vi.advanceTimersByTimeAsync(0);
    playback.destroy();
    await vi.runAllTimersAsync();
    await starting;
    expect(convert).toHaveBeenCalledOnce();
    expect(onError).not.toHaveBeenCalled();
  });

  it('a newer seek supersedes a pending retry', async () => {
    vi.useFakeTimers();
    const { playback, convert, onError, video, source } = setup();
    await playback.start(0);
    source.readyState = 'open';
    source.duration = 1000;
    convert.mockRejectedValueOnce(new Error('Conversion failed'));
    video.currentTime = 100;
    playback.seeking();
    await vi.advanceTimersByTimeAsync(0);
    video.currentTime = 500;
    source.buffer.buffered = ranges([500, 510]);
    vi.spyOn(source.buffer, 'appendBuffer').mockImplementationOnce(() => {
      source.duration = 520;
      source.buffer.buffered = ranges([500, 520]);
    });
    playback.seeking();
    await vi.runAllTimersAsync();
    expect(convert).toHaveBeenCalledTimes(3);
    expect(convert).toHaveBeenNthCalledWith(1, 0);
    expect(convert).toHaveBeenNthCalledWith(2, 100);
    expect(convert).toHaveBeenNthCalledWith(3, 510);
    expect(onError).not.toHaveBeenCalled();
    playback.destroy();
  });

  it('evicts distant future data on quota pressure after a backward seek', async () => {
    const { playback, onError, source } = setup();
    source.buffer.buffered = ranges([0, 10], [500, 560]);
    const remove = vi.spyOn(source.buffer, 'remove');
    const append = vi.spyOn(source.buffer, 'appendBuffer');
    append.mockImplementationOnce(() => {
      throw new DOMException('Buffer full', 'QuotaExceededError');
    });
    await playback.start(0);
    expect(remove).toHaveBeenCalledWith(3, 560);
    expect(append).toHaveBeenCalledTimes(2);
    expect(onError).not.toHaveBeenCalled();
    playback.destroy();
  });

  it('retries a failed converted stream request', async () => {
    vi.useFakeTimers();
    const { playback, convert, onError } = setup();
    vi.mocked(fetch).mockRejectedValueOnce(new TypeError('Network error'));
    const starting = playback.start(0);
    await vi.runAllTimersAsync();
    await starting;
    expect(convert).toHaveBeenCalledTimes(2);
    expect(onError).not.toHaveBeenCalled();
    playback.destroy();
  });

  it('retries when the converted response body fails', async () => {
    vi.useFakeTimers();
    const { playback, convert, onError } = setup();
    vi.mocked(fetch).mockResolvedValueOnce(
      new Response(
        new ReadableStream({
          start(controller) {
            controller.error(new Error('Read interrupted'));
          },
        }),
      ),
    );
    const starting = playback.start(0);
    await vi.runAllTimersAsync();
    await starting;
    expect(convert).toHaveBeenCalledTimes(2);
    expect(onError).not.toHaveBeenCalled();
    playback.destroy();
  });

  it('bounds retries when FFmpeg repeatedly ends without making progress', async () => {
    vi.useFakeTimers();
    const { playback, convert, onError, source } = setup();
    source.buffer.buffered = ranges();
    const starting = playback.start(0);
    await vi.runAllTimersAsync();
    await starting;
    expect(convert).toHaveBeenCalledTimes(3);
    expect(onError).toHaveBeenCalledOnce();
    playback.destroy();
  });

  it('does not start a backend process if destroyed before sourceopen', async () => {
    const { playback, convert, source } = setup();
    source.readyState = 'closed';
    const starting = playback.start(0);
    playback.destroy();
    await starting;
    expect(convert).not.toHaveBeenCalled();
  });

  it('starts another conversion when seeking into an evicted range after EOF', async () => {
    const { playback, convert, source } = setup();
    await playback.start(0);
    expect(source.readyState).toBe('ended');
    source.buffer.buffered = ranges();
    playback.seeking();
    expect(convert).toHaveBeenCalledTimes(2);
    playback.destroy();
  });

  it('reports asynchronous media parser errors to the fallback player', async () => {
    const { playback, onError, source } = setup();
    source.buffer.fail = true;
    await playback.start(0);
    expect(onError).toHaveBeenCalledOnce();
    playback.destroy();
  });

  it('aborts a fetch still waiting for response headers when destroyed', async () => {
    const { playback, convert } = setup();
    let signal: AbortSignal | undefined;
    vi.stubGlobal(
      'fetch',
      vi.fn().mockImplementation((_url: string, init: RequestInit) => {
        signal = init.signal ?? undefined;
        return new Promise((_resolve, reject) => {
          signal?.addEventListener('abort', () => {
            reject(new DOMException('Stopped', 'AbortError'));
          });
        });
      }),
    );
    const starting = playback.start(0);
    await vi.waitFor(() => {
      expect(signal).toBeDefined();
    });
    expect(convert).toHaveBeenCalledOnce();
    playback.destroy();
    await starting;
    expect(signal?.aborted).toBe(true);
  });
});
