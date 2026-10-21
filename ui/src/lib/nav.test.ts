// SPDX-License-Identifier: MIT OR Apache-2.0

import { describe, expect, it } from 'vitest';
import {
  HISTORY_LIMIT,
  back,
  canGoBack,
  canGoForward,
  forward,
  initialHistory,
  keyNavigation,
  lastSearchId,
  mouseNavigation,
  prune,
  push,
  replace,
  sameView,
  viewKey,
  type View,
} from './nav';

const search = (id: number | null): View => ({ name: 'search', search: id });
const torrent = (id: number): View => ({ name: 'torrent', torrentId: id });
const player: View = { name: 'player' };

describe('history', () => {
  it('goes back and forward like a browser', () => {
    let h = initialHistory(search(1));
    expect(canGoBack(h)).toBe(false);
    h = push(h, torrent(7));
    h = push(h, player);
    expect(h.current).toEqual(player);
    h = back(h);
    expect(h.current).toEqual(torrent(7));
    expect(canGoForward(h)).toBe(true);
    h = back(h);
    expect(h.current).toEqual(search(1));
    expect(back(h)).toBe(h);
    h = forward(forward(h));
    expect(h.current).toEqual(player);
    expect(forward(h)).toBe(h);
  });

  it('clears the forward stack on a new place, and ignores the current one', () => {
    let h = push(push(initialHistory(search(1)), torrent(7)), player);
    h = back(back(h));
    h = push(h, { name: 'open' });
    expect(canGoForward(h)).toBe(false);
    expect(h.back).toEqual([search(1)]);
    expect(push(h, { name: 'open' })).toBe(h);
    expect(push(h, { name: 'settings', section: 'network' }).back).toHaveLength(2);
  });

  it('treats settings sections as one place and searches as separate ones', () => {
    expect(
      sameView({ name: 'settings', section: 'network' }, { name: 'settings', section: 'about' }),
    ).toBe(true);
    expect(sameView(search(1), search(2))).toBe(false);
    expect(sameView(torrent(1), torrent(1))).toBe(true);
    const h = replace(initialHistory(search(1)), search(2));
    expect(h.current).toEqual(search(2));
    expect(h.back).toEqual([]);
  });

  it('keeps a bounded history', () => {
    let h = initialHistory(search(0));
    for (let i = 1; i <= HISTORY_LIMIT + 10; i += 1) {
      h = push(h, torrent(i));
    }
    expect(h.back).toHaveLength(HISTORY_LIMIT);
    expect(h.back[0]).toEqual(torrent(10));
  });

  it('prunes places that are gone without leaving duplicates', () => {
    // search 1 → player → search 1 → player, then playback stops.
    let h = push(push(push(initialHistory(search(1)), player), torrent(3)), player);
    h = back(h);
    h = prune(h, (v) => v.name !== 'player', search(1));
    expect(h.current).toEqual(torrent(3));
    expect(h.back).toEqual([search(1)]);
    expect(h.forward).toEqual([]);

    const onPlayer = prune(push(initialHistory(search(1)), player), (v) => v !== player, search(1));
    expect(onPlayer.current).toEqual(search(1));
    expect(onPlayer.back, 'no duplicate of the fallback behind it').toEqual([]);
  });

  it('finds the search the search page shows', () => {
    let h = initialHistory(search(null));
    expect(lastSearchId(h)).toBeNull();
    h = push(push(push(h, search(4)), torrent(1)), player);
    expect(lastSearchId(h)).toBe(4);
    h = push(back(back(h)), search(5));
    expect(lastSearchId(h)).toBe(5);
  });

  it('keys views for scroll and focus memory', () => {
    expect(viewKey(search(3))).toBe('search:3');
    expect(viewKey(search(null))).toBe('search:null');
    expect(viewKey(torrent(2))).toBe('torrent:2');
    expect(viewKey({ name: 'settings', section: 'about' })).toBe('settings');
  });
});

describe('input', () => {
  const key = (k: string, mods: Partial<KeyboardEvent> = {}) => ({
    key: k,
    altKey: false,
    ctrlKey: false,
    metaKey: false,
    shiftKey: false,
    ...mods,
  });

  it('maps Alt+arrows and browser keys to back and forward', () => {
    expect(keyNavigation(key('ArrowLeft', { altKey: true }))).toBe('back');
    expect(keyNavigation(key('ArrowRight', { altKey: true }))).toBe('forward');
    expect(keyNavigation(key('BrowserBack'))).toBe('back');
    expect(keyNavigation(key('BrowserForward'))).toBe('forward');
    expect(keyNavigation(key('ArrowLeft')), 'plain arrows seek the video').toBeNull();
    expect(keyNavigation(key('ArrowLeft', { altKey: true, shiftKey: true }))).toBeNull();
    expect(keyNavigation(key('ArrowLeft', { altKey: true, ctrlKey: true }))).toBeNull();
  });

  it('maps the mouse side buttons', () => {
    expect(mouseNavigation(3)).toBe('back');
    expect(mouseNavigation(4)).toBe('forward');
    expect(mouseNavigation(0)).toBeNull();
    expect(mouseNavigation(1)).toBeNull();
  });
});
