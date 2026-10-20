// SPDX-License-Identifier: MIT OR Apache-2.0

import { describe, expect, it } from 'vitest';
import { RECENT_QUERY_LIMIT, evictSearches, normalizeQuery, rememberQuery } from './recent';

describe('recent searches', () => {
  it('keeps the latest queries first, without duplicates', () => {
    let recent = rememberQuery([], 'sintel');
    recent = rememberQuery(recent, '  big   buck bunny ');
    recent = rememberQuery(recent, 'SINTEL');
    expect(recent).toEqual(['SINTEL', 'big buck bunny']);
    expect(rememberQuery(recent, '   ')).toEqual(recent);
    for (let i = 0; i < 20; i += 1) {
      recent = rememberQuery(recent, `q${String(i)}`);
    }
    expect(recent).toHaveLength(RECENT_QUERY_LIMIT);
    expect(recent[0]).toBe('q19');
    expect(normalizeQuery(' a \t b ')).toBe('a b');
  });

  it('evicts the oldest cached searches but never the one shown', () => {
    const searches = [1, 2, 3, 4].map((id) => ({ id }));
    expect(evictSearches(searches, 4, 2)).toEqual({
      kept: [{ id: 3 }, { id: 4 }],
      dropped: [1, 2],
    });
    expect(evictSearches(searches, 1, 2)).toEqual({
      kept: [{ id: 1 }, { id: 4 }],
      dropped: [2, 3],
    });
    expect(evictSearches(searches, null, 10).dropped).toEqual([]);
  });
});
