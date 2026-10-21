// SPDX-License-Identifier: MIT OR Apache-2.0

/**
 * Navigation between the app's views, with browser-style back and forward history.
 *
 * The history is plain data and every operation returns a new value, so it can live in a
 * `$state` rune and be tested without a DOM. Views are small descriptors (which page, which
 * torrent, which search); the pages themselves keep their own state while hidden.
 */

import type { SettingsSection } from './settings';

/** A place in the app. */
export type View =
  /** Search box and results. `search` identifies one search in the recent-search cache. */
  | { name: 'search'; search: number | null }
  /** Paste a link or open a `.torrent` file; torrents opened this session. */
  | { name: 'open' }
  /** The file explorer, showing an added torrent. */
  | { name: 'torrent'; torrentId: number }
  /** The video playing (or a placeholder) and the queue beside it. */
  | { name: 'player' }
  /** Playback history and saved positions. */
  | { name: 'library' }
  | { name: 'settings'; section: SettingsSection };

export type ViewName = View['name'];

/** Back and forward stacks around the current view. Oldest entries first. */
export interface NavHistory {
  readonly back: readonly View[];
  readonly current: View;
  readonly forward: readonly View[];
}

/** Entries kept behind the current view; older ones are dropped. */
export const HISTORY_LIMIT = 50;

export function initialHistory(view: View): NavHistory {
  return { back: [], current: view, forward: [] };
}

/** Whether two views are the same place (pushing one onto the other changes nothing). */
export function sameView(a: View, b: View): boolean {
  switch (a.name) {
    case 'search':
      return b.name === 'search' && a.search === b.search;
    case 'torrent':
      return b.name === 'torrent' && a.torrentId === b.torrentId;
    case 'settings':
      // Sections are tabs within one page, not separate places.
      return b.name === 'settings';
    default:
      return a.name === b.name;
  }
}

/** Goes to `view`, clearing the forward stack. Going to the current view changes nothing. */
export function push(history: NavHistory, view: View, limit = HISTORY_LIMIT): NavHistory {
  if (sameView(history.current, view)) {
    return history;
  }
  const back = [...history.back, history.current];
  return { back: back.slice(Math.max(0, back.length - limit)), current: view, forward: [] };
}

/** Replaces the current view without adding an entry (for example, a search refinement). */
export function replace(history: NavHistory, view: View): NavHistory {
  return { ...history, current: view };
}

export function canGoBack(history: NavHistory): boolean {
  return history.back.length > 0;
}

export function canGoForward(history: NavHistory): boolean {
  return history.forward.length > 0;
}

/** One step back, or the same history if there is nowhere to go. */
export function back(history: NavHistory): NavHistory {
  const previous = history.back.at(-1);
  if (previous === undefined) {
    return history;
  }
  return {
    back: history.back.slice(0, -1),
    current: previous,
    forward: [history.current, ...history.forward],
  };
}

/** One step forward, or the same history if there is nowhere to go. */
export function forward(history: NavHistory): NavHistory {
  const next = history.forward[0];
  if (next === undefined) {
    return history;
  }
  return {
    back: [...history.back, history.current],
    current: next,
    forward: history.forward.slice(1),
  };
}

/**
 * Removes entries that no longer lead anywhere (the player after playback stops, a search
 * evicted from the cache). If the current view is invalid, `fallback` replaces it. Adjacent
 * duplicates left behind are merged, so Back never appears to do nothing.
 */
export function prune(
  history: NavHistory,
  valid: (view: View) => boolean,
  fallback: View,
): NavHistory {
  const current = valid(history.current) ? history.current : fallback;
  const back = collapse(history.back.filter(valid));
  const forward = collapse(history.forward.filter(valid));
  while (back.length > 0 && sameView(back[back.length - 1] ?? current, current)) {
    back.pop();
  }
  while (forward.length > 0 && sameView(forward[0] ?? current, current)) {
    forward.shift();
  }
  return { back, current, forward };
}

/** Merges runs of the same view into one entry. */
function collapse(views: readonly View[]): View[] {
  const out: View[] = [];
  for (const view of views) {
    const last = out.at(-1);
    if (last === undefined || !sameView(last, view)) {
      out.push(view);
    }
  }
  return out;
}

/**
 * The search the search page shows: the current view's, or else the latest one behind it,
 * so returning from the player or a file list finds the same results.
 */
export function lastSearchId(history: NavHistory): number | null {
  const views = [...history.back, history.current];
  for (let i = views.length - 1; i >= 0; i -= 1) {
    const v = views[i];
    if (v?.name === 'search') {
      return v.search;
    }
  }
  return null;
}

/** A stable key per view, for remembering scroll positions and focus. */
export function viewKey(view: View): string {
  switch (view.name) {
    case 'search':
      return `search:${String(view.search)}`;
    case 'torrent':
      return `torrent:${String(view.torrentId)}`;
    default:
      return view.name;
  }
}

/** What a pointer's auxiliary button means: `back` (button 3), `forward` (4), or null. */
export function mouseNavigation(button: number): 'back' | 'forward' | null {
  if (button === 3) {
    return 'back';
  }
  return button === 4 ? 'forward' : null;
}

/**
 * What a key press means for navigation: Alt+Left and the BrowserBack key go back, Alt+Right
 * and BrowserForward go forward. Anything else is null.
 */
export function keyNavigation(
  event: Pick<KeyboardEvent, 'key' | 'altKey' | 'ctrlKey' | 'metaKey' | 'shiftKey'>,
): 'back' | 'forward' | null {
  if (event.key === 'BrowserBack') {
    return 'back';
  }
  if (event.key === 'BrowserForward') {
    return 'forward';
  }
  if (!event.altKey || event.ctrlKey || event.metaKey || event.shiftKey) {
    return null;
  }
  if (event.key === 'ArrowLeft') {
    return 'back';
  }
  return event.key === 'ArrowRight' ? 'forward' : null;
}
