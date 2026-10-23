// SPDX-License-Identifier: MIT OR Apache-2.0

/**
 * Short messages at the bottom of the window. They stack, so a new one never hides an earlier
 * one's Undo: each leaves on its own clock. Errors stay until closed; a toast with Undo stays
 * 8 seconds, others 4.5. A toast's clock stops while it is hovered or focused.
 */

export interface Toast {
  id: number;
  message: string;
  error: boolean;
  undo: (() => void) | null;
}

export interface ToastOptions {
  error?: boolean;
  undo?: () => void;
}

export const TOAST_MS = 4500;
export const UNDO_TOAST_MS = 8000;
/** At most this many show at once; the oldest leave first. */
export const MAX_TOASTS = 4;

/** How long a toast stays, or null until it is closed (errors). */
export function toastLifetime(toast: Pick<Toast, 'error' | 'undo'>): number | null {
  if (toast.error) {
    return null;
  }
  return toast.undo === null ? TOAST_MS : UNDO_TOAST_MS;
}

interface Clock {
  remaining: number;
  startedAt: number;
  handle: ReturnType<typeof setTimeout> | undefined;
  holds: number;
}

/** The toasts showing, with their clocks. `onChange` receives every new list. */
export class Toasts {
  #list: Toast[] = [];
  #clocks = new Map<number, Clock>();
  #nextId = 1;
  readonly #onChange: (list: readonly Toast[]) => void;
  readonly #now: () => number;

  constructor(onChange: (list: readonly Toast[]) => void, now: () => number = Date.now) {
    this.#onChange = onChange;
    this.#now = now;
  }

  get list(): readonly Toast[] {
    return this.#list;
  }

  /** Shows a toast; returns its id. */
  show(message: string, options: ToastOptions = {}): number {
    const toast: Toast = {
      id: this.#nextId++,
      message,
      error: options.error ?? false,
      undo: options.undo ?? null,
    };
    let list = [...this.#list, toast];
    while (list.length > MAX_TOASTS) {
      const oldest = list[0];
      if (oldest) {
        this.#stop(oldest.id);
      }
      list = list.slice(1);
    }
    const lifetime = toastLifetime(toast);
    if (lifetime !== null) {
      this.#clocks.set(toast.id, {
        remaining: lifetime,
        startedAt: this.#now(),
        handle: undefined,
        holds: 0,
      });
      this.#run(toast.id);
    }
    this.#set(list);
    return toast.id;
  }

  /** Closes a toast. */
  dismiss(id: number): void {
    if (!this.#list.some((t) => t.id === id)) {
      return;
    }
    this.#stop(id);
    this.#set(this.#list.filter((t) => t.id !== id));
  }

  /** Runs a toast's Undo and closes it. */
  undo(id: number): void {
    const toast = this.#list.find((t) => t.id === id);
    this.dismiss(id);
    toast?.undo?.();
  }

  /** Stops a toast's clock (hovered or focused) until as many `release` calls. */
  hold(id: number): void {
    const clock = this.#clocks.get(id);
    if (!clock) {
      return;
    }
    clock.holds += 1;
    if (clock.holds === 1 && clock.handle !== undefined) {
      clearTimeout(clock.handle);
      clock.handle = undefined;
      clock.remaining = Math.max(0, clock.remaining - (this.#now() - clock.startedAt));
    }
  }

  release(id: number): void {
    const clock = this.#clocks.get(id);
    if (!clock || clock.holds === 0) {
      return;
    }
    clock.holds -= 1;
    if (clock.holds === 0) {
      this.#run(id);
    }
  }

  /** Closes every toast and stops their clocks. */
  clear(): void {
    for (const id of this.#clocks.keys()) {
      this.#stop(id);
    }
    this.#set([]);
  }

  #run(id: number): void {
    const clock = this.#clocks.get(id);
    if (!clock) {
      return;
    }
    clock.startedAt = this.#now();
    clock.handle = setTimeout(() => {
      this.dismiss(id);
    }, clock.remaining);
  }

  #stop(id: number): void {
    const clock = this.#clocks.get(id);
    if (clock?.handle !== undefined) {
      clearTimeout(clock.handle);
    }
    this.#clocks.delete(id);
  }

  #set(list: Toast[]): void {
    this.#list = list;
    this.#onChange(list);
  }
}
