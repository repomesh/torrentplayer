// SPDX-License-Identifier: MIT OR Apache-2.0

import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { MAX_TOASTS, TOAST_MS, Toasts, UNDO_TOAST_MS, toastLifetime } from './toasts';

describe('toasts', () => {
  let shown: readonly string[] = [];
  let toasts: Toasts;

  beforeEach(() => {
    vi.useFakeTimers();
    shown = [];
    toasts = new Toasts((list) => {
      shown = list.map((t) => t.message);
    });
  });

  afterEach(() => {
    vi.useRealTimers();
  });

  it('keeps errors until closed, and Undo longer than plain messages', () => {
    expect(toastLifetime({ error: true, undo: null })).toBeNull();
    expect(toastLifetime({ error: false, undo: null })).toBe(TOAST_MS);
    expect(toastLifetime({ error: false, undo: () => undefined })).toBe(UNDO_TOAST_MS);

    const error = toasts.show('Could not play', { error: true });
    toasts.show('Removed a video', { undo: () => undefined });
    toasts.show('Saved');
    vi.advanceTimersByTime(TOAST_MS);
    expect(shown).toEqual(['Could not play', 'Removed a video']);
    vi.advanceTimersByTime(UNDO_TOAST_MS - TOAST_MS);
    expect(shown).toEqual(['Could not play']);
    vi.advanceTimersByTime(60_000);
    expect(shown).toEqual(['Could not play']);
    toasts.dismiss(error);
    expect(shown).toEqual([]);
  });

  it('never hides an earlier Undo behind a new toast', () => {
    const undo = vi.fn();
    const first = toasts.show('Removed A', { undo });
    toasts.show('Removed B', { undo: () => undefined });
    expect(shown).toEqual(['Removed A', 'Removed B']);
    toasts.undo(first);
    expect(undo).toHaveBeenCalledOnce();
    expect(shown).toEqual(['Removed B']);
  });

  it('stops the clock while hovered or focused', () => {
    const id = toasts.show('Removed A', { undo: () => undefined });
    vi.advanceTimersByTime(5000);
    toasts.hold(id);
    toasts.hold(id);
    vi.advanceTimersByTime(60_000);
    expect(shown).toEqual(['Removed A']);
    toasts.release(id);
    vi.advanceTimersByTime(60_000);
    expect(shown).toEqual(['Removed A']);
    toasts.release(id);
    vi.advanceTimersByTime(UNDO_TOAST_MS - 5000 - 1);
    expect(shown).toEqual(['Removed A']);
    vi.advanceTimersByTime(1);
    expect(shown).toEqual([]);
  });

  it('shows a few at most, dropping the oldest', () => {
    for (let i = 0; i <= MAX_TOASTS; i += 1) {
      toasts.show(`Toast ${String(i)}`);
    }
    expect(shown).toHaveLength(MAX_TOASTS);
    expect(shown[0]).toBe('Toast 1');
    toasts.clear();
    expect(shown).toEqual([]);
  });
});
