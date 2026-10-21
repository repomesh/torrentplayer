// SPDX-License-Identifier: MIT OR Apache-2.0

/**
 * Keeps audio and video together after a seek.
 *
 * After a seek, the video decoder has to start from the nearest keyframe before the target and
 * decode forward, while audio can resume almost at once. When the video data is still arriving
 * (or keyframes are far apart, as in low-bitrate lecture encodes), the browser starts the audio
 * first and the picture catches up later. The gate pauses playback on `seeking`, waits until a
 * video frame from the new position is painted and `seeked` has fired, then resumes, so picture
 * and sound start together.
 *
 * WebView2 can paint the new position's frame *before* firing `seeked`, and paints nothing more
 * while the video is paused, so frames are told apart by their media time rather than by when
 * they arrive: a frame near the seek target counts whenever it comes.
 */
export class SeekGate {
  private state: 'idle' | 'seeking' | 'seeked' = 'idle';
  private resume = false;
  private target = Number.NaN;
  private targetPainted = false;

  /** A painted frame this close to the seek target (seconds) is from the new position. */
  static readonly TARGET_TOLERANCE = 0.25;

  /** Whether playback is being held for a seek. */
  get holding(): boolean {
    return this.state !== 'idle';
  }

  /**
   * Call on the `seeking` event with the seek target (`currentTime`). Returns whether the caller
   * should pause now. Repeated seeks while holding keep the original decision to resume.
   */
  seeking(paused: boolean, target = Number.NaN): boolean {
    if (this.state === 'idle') {
      this.resume = !paused;
    }
    this.state = 'seeking';
    this.target = target;
    this.targetPainted = false;
    return !paused;
  }

  /**
   * Call on the `seeked` event. Returns whether the caller should resume playback now, which it
   * should if the new position's frame was already painted.
   */
  seeked(): boolean {
    if (this.state !== 'seeking') {
      return false;
    }
    if (this.targetPainted) {
      return this.release();
    }
    this.state = 'seeked';
    return false;
  }

  /**
   * Call when a video frame is painted, with its media time if known. Returns whether the caller
   * should resume playback. Frames from the old position do not release the gate.
   */
  framePainted(mediaTime = Number.NaN): boolean {
    if (this.state === 'seeked') {
      return this.release();
    }
    if (
      this.state === 'seeking' &&
      Math.abs(mediaTime - this.target) <= SeekGate.TARGET_TOLERANCE
    ) {
      this.targetPainted = true;
    }
    return false;
  }

  /**
   * Call when no frame came for a while after `seeked`, so a missed paint can never leave the
   * video paused. Returns whether the caller should resume playback.
   */
  expire(): boolean {
    return this.state === 'seeked' ? this.release() : false;
  }

  /** The user pressed play or pause while holding: their choice wins. */
  cancel(): void {
    this.state = 'idle';
    this.resume = false;
    this.targetPainted = false;
  }

  private release(): boolean {
    this.state = 'idle';
    this.targetPainted = false;
    const resume = this.resume;
    this.resume = false;
    return resume;
  }
}
