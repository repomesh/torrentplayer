// SPDX-License-Identifier: MIT OR Apache-2.0

/**
 * Recent searches: the queries typed this session, and a small cache of finished searches
 * so that Back (or a recent-search chip) shows the same results again without searching.
 * Nothing here is saved to disk.
 */

/** Queries remembered for the suggestions and chips. */
export const RECENT_QUERY_LIMIT = 8;
/** Searches whose results are kept for Back and Forward. */
export const SEARCH_CACHE_LIMIT = 6;

/** Puts `query` first, without duplicates (ignoring case and spacing), keeping `limit`. */
export function rememberQuery(
  recent: readonly string[],
  query: string,
  limit = RECENT_QUERY_LIMIT,
): string[] {
  const q = normalizeQuery(query);
  if (q === '') {
    return [...recent];
  }
  const key = q.toLowerCase();
  return [q, ...recent.filter((r) => r.toLowerCase() !== key)].slice(0, limit);
}

/** Trims and collapses runs of spaces. */
export function normalizeQuery(query: string): string {
  return query.trim().replace(/\s+/g, ' ');
}

/**
 * Drops the oldest searches beyond `limit`, never the one shown (`keep`). Returns the kept
 * searches and the ids dropped, so history entries pointing at them can be removed.
 */
export function evictSearches<T extends { id: number }>(
  searches: readonly T[],
  keep: number | null,
  limit = SEARCH_CACHE_LIMIT,
): { kept: T[]; dropped: number[] } {
  const kept = [...searches];
  const dropped: number[] = [];
  while (kept.length > limit) {
    const at = kept.findIndex((s) => s.id !== keep);
    if (at < 0) {
      break;
    }
    const [gone] = kept.splice(at, 1);
    if (gone) {
      dropped.push(gone.id);
    }
  }
  return { kept, dropped };
}
