// SPDX-License-Identifier: MIT OR Apache-2.0

/**
 * Client-side filters and sorting for search results.
 *
 * They narrow the results already on screen without searching again, and are kept for the
 * session, so a new search starts with the same filters. The backend's ranking is the
 * default order ("Best match"); the other sorts are stable, so ties keep that order.
 */

import type { SearchResultDto } from './bindings/SearchResultDto';

export type SortKey =
  | 'relevance'
  | 'peers'
  | 'quality'
  | 'seeders'
  | 'newest'
  | 'oldest'
  | 'largest'
  | 'smallest'
  | 'name';

export interface SearchFilters {
  sort: SortKey;
  /** Top-level category (`Movies`, `TV`), or null for all. */
  category: string | null;
  /** Indexer that listed the result, or null for all. */
  indexer: string | null;
  /** Minimum seeders; 0 accepts unknown counts too. */
  minSeeders: number;
  /** Minimum vertical resolution (720 means 720p and up); 0 accepts unknown. */
  minResolution: number;
  /** Size bounds in bytes; results of unknown size fail any bound. */
  minSizeBytes: number | null;
  maxSizeBytes: number | null;
  /** Only results the built-in player plays or likely plays. */
  playableOnly: boolean;
  /** Hide camera recordings, telesyncs, and screeners. */
  hideCamera: boolean;
  /**
   * Hide results with nothing to play (ebooks, software, games); video and audio stay. Results
   * that could be either are always kept.
   */
  videosOnly: boolean;
}

export const DEFAULT_FILTERS: Readonly<SearchFilters> = {
  sort: 'relevance',
  category: null,
  indexer: null,
  minSeeders: 0,
  minResolution: 0,
  minSizeBytes: null,
  maxSizeBytes: null,
  playableOnly: false,
  hideCamera: false,
  videosOnly: true,
};

/** Sort menu labels, in menu order. */
export const SORT_LABELS: Record<SortKey, string> = {
  relevance: 'Best match',
  peers: 'Most peers',
  quality: 'Best quality',
  seeders: 'Most seeders',
  newest: 'Newest',
  oldest: 'Oldest',
  largest: 'Largest',
  smallest: 'Smallest',
  name: 'Name A–Z',
};

/** Release sources from best to worst, for "Best quality" (camera copies come last). */
const SOURCE_RANK: Record<string, number> = {
  bluray: 5,
  'web-dl': 4,
  webrip: 3,
  hdtv: 2,
  dvd: 1,
  cam: 0,
};

/** All peers of a result (seeders plus leechers), or null if neither count is reported. */
export function peerCount(result: Pick<SearchResultDto, 'seeders' | 'peers'>): number | null {
  if (result.peers === null) {
    return result.seeders;
  }
  // Some indexers report leechers only; the total is never below the seeders.
  return Math.max(result.peers, result.seeders ?? 0);
}

/** The rank of a result's source for "Best quality": higher is better, null if unknown. */
export function sourceRank(quality: SearchResultDto['quality']): number | null {
  if (quality.camera) {
    return SOURCE_RANK.cam ?? 0;
  }
  return quality.source === null ? null : (SOURCE_RANK[quality.source.toLowerCase()] ?? null);
}

/** Choices for the minimum-seeders menu. */
export const SEEDER_STEPS = [0, 1, 5, 20, 100] as const;
/** Choices for the minimum-resolution menu. */
export const RESOLUTION_STEPS = [0, 480, 720, 1080, 2160] as const;

const GB = 1_000_000_000;
/** Choices for the size menus, in bytes. */
export const SIZE_STEPS = [
  0.25 * GB,
  0.5 * GB,
  GB,
  2 * GB,
  4 * GB,
  8 * GB,
  16 * GB,
  32 * GB,
] as const;

/** Number of filters that differ from the defaults (sorting is not a filter). */
export function activeFilterCount(filters: SearchFilters): number {
  const keys = Object.keys(DEFAULT_FILTERS) as (keyof SearchFilters)[];
  return keys.filter((k) => k !== 'sort' && filters[k] !== DEFAULT_FILTERS[k]).length;
}

/** `1080p` → 1080, `4K`/`2160p` → 2160; 0 if unknown. */
export function resolutionValue(resolution: string | null): number {
  if (resolution === null) {
    return 0;
  }
  const r = resolution.trim().toLowerCase();
  if (r === '4k' || r === 'uhd') {
    return 2160;
  }
  const match = /^(\d{3,4})[pi]?$/.exec(r);
  return match?.[1] === undefined ? 0 : Number(match[1]);
}

/** Torznab category ranges and the names they stand for. */
const TORZNAB: readonly [number, string][] = [
  [1000, 'Console'],
  [2000, 'Movies'],
  [3000, 'Audio'],
  [4000, 'PC'],
  [5000, 'TV'],
  [6000, 'XXX'],
  [7000, 'Books'],
  [8000, 'Other'],
];

/**
 * The top-level part of an indexer category, for grouping: `Movies/HD` → `Movies`,
 * `TV > Episodes` → `TV`, Torznab `2040` → `Movies`. Null if there is none.
 */
export function topCategory(category: string | null): string | null {
  if (category === null) {
    return null;
  }
  const trimmed = category.trim();
  if (/^\d+$/.test(trimmed)) {
    const id = Number(trimmed);
    if (id === ANIME_ID) {
      return 'Anime';
    }
    const range = TORZNAB.find(([start]) => id >= start && id < start + 1000);
    return range?.[1] ?? null;
  }
  // Anime is its own category here, wherever an indexer files it (`TV/Anime`).
  if (/\banime\b/i.test(trimmed)) {
    return 'Anime';
  }
  const top = trimmed.split(/\s*(?:\/|>|:|\|)\s*|\s+-\s+/)[0]?.trim() ?? '';
  return top === '' ? null : top;
}

/** Torznab's TV/Anime category. */
const ANIME_ID = 5070;

/**
 * The Torznab category ids the search service is asked for, so it searches only one category
 * (`Movies` → 2000, which includes its subcategories). Empty for all categories, or for a
 * category the service has no id for (results are then filtered here instead).
 */
export function categoryIds(category: string | null): number[] {
  if (category === null) {
    return [];
  }
  if (sameText(category, 'Anime')) {
    return [ANIME_ID];
  }
  const range = TORZNAB.find(([, name]) => sameText(name, category) && name !== 'XXX');
  return range ? [range[0]] : [];
}

/** Categories offered before a search has results (adult content is never searched). */
export const CATEGORY_CHOICES = [
  'Movies',
  'TV',
  'Anime',
  'Audio',
  'Books',
  'PC',
  'Console',
  'Other',
] as const;

/**
 * The category menu's choices: the usual categories, then any other top-level categories in
 * the results, then the chosen one if neither has it. Counts come from the results (0 if
 * there are none or no results yet).
 */
export function categoryChoices(
  found: readonly Facet[],
  chosen: string | null,
): { value: string; count: number }[] {
  const count = (value: string): number => found.find((f) => sameText(f.value, value))?.count ?? 0;
  const choices: { value: string; count: number }[] = CATEGORY_CHOICES.map((value) => ({
    value,
    count: count(value),
  }));
  for (const facet of found) {
    if (!choices.some((c) => sameText(c.value, facet.value))) {
      choices.push({ value: facet.value, count: facet.count });
    }
  }
  if (chosen !== null && !choices.some((c) => sameText(c.value, chosen))) {
    choices.push({ value: chosen, count: 0 });
  }
  return choices;
}

function sameText(a: string, b: string): boolean {
  return a.localeCompare(b, undefined, { sensitivity: 'base' }) === 0;
}

/** Whether a result passes every filter. */
export function matches(result: SearchResultDto, filters: SearchFilters): boolean {
  if (filters.category !== null) {
    const top = topCategory(result.category);
    if (top === null || !sameText(top, filters.category)) {
      return false;
    }
  }
  if (filters.indexer !== null) {
    const wanted = filters.indexer;
    if (!result.indexers.some((i) => sameText(i, wanted))) {
      return false;
    }
  }
  if (filters.minSeeders > 0 && (result.seeders ?? -1) < filters.minSeeders) {
    return false;
  }
  if (
    filters.minResolution > 0 &&
    resolutionValue(result.quality.resolution) < filters.minResolution
  ) {
    return false;
  }
  if (filters.minSizeBytes !== null && (result.sizeBytes ?? -1) < filters.minSizeBytes) {
    return false;
  }
  if (
    filters.maxSizeBytes !== null &&
    (result.sizeBytes === null || result.sizeBytes > filters.maxSizeBytes)
  ) {
    return false;
  }
  if (
    filters.playableOnly &&
    result.quality.playable !== 'plays' &&
    result.quality.playable !== 'likely'
  ) {
    return false;
  }
  if (filters.videosOnly && result.quality.kind === 'notVideo') {
    return false;
  }
  return !(filters.hideCamera && result.quality.camera);
}

/**
 * How many results "Videos only" hides that the other filters would show, for the
 * "N hidden: not videos" note. 0 while the filter is off.
 */
export function hiddenNonVideos(
  results: readonly SearchResultDto[],
  filters: SearchFilters,
): number {
  if (!filters.videosOnly) {
    return 0;
  }
  const others = { ...filters, videosOnly: false };
  return results.filter((r) => r.quality.kind === 'notVideo' && matches(r, others)).length;
}

const names = new Intl.Collator(undefined, { numeric: true, sensitivity: 'base' });

/** Larger first, unknown (null) last. */
function desc(x: number | null, y: number | null): number {
  if (x === y) {
    return 0;
  }
  if (x === null) {
    return 1;
  }
  return y === null ? -1 : y - x;
}

/** Smaller first, unknown (null) last. */
function asc(x: number | null, y: number | null): number {
  return desc(x === null ? null : -x, y === null ? null : -y);
}

/** Orders two results by a sort key; 0 keeps the backend's order. Unknown values go last. */
function compare(a: SearchResultDto, b: SearchResultDto, sort: SortKey): number {
  switch (sort) {
    case 'peers':
      return desc(peerCount(a), peerCount(b));
    case 'quality': {
      // Resolution first (unknown last), then the release source.
      const res = (r: SearchResultDto): number | null =>
        resolutionValue(r.quality.resolution) || null;
      return desc(res(a), res(b)) || desc(sourceRank(a.quality), sourceRank(b.quality));
    }
    case 'seeders':
      return desc(a.seeders, b.seeders);
    case 'largest':
      return desc(a.sizeBytes, b.sizeBytes);
    case 'smallest':
      return asc(a.sizeBytes, b.sizeBytes);
    case 'newest':
      return desc(publishedTime(a.published), publishedTime(b.published));
    case 'oldest':
      return asc(publishedTime(a.published), publishedTime(b.published));
    case 'name':
      return names.compare(a.title, b.title);
    case 'relevance':
      return 0;
  }
}

function publishedTime(iso: string | null): number | null {
  if (iso === null) {
    return null;
  }
  const t = Date.parse(iso);
  return Number.isNaN(t) ? null : t;
}

/** The results that pass the filters, in the chosen order. */
export function applyFilters(
  results: readonly SearchResultDto[],
  filters: SearchFilters,
): SearchResultDto[] {
  const kept = results.filter((r) => matches(r, filters));
  return filters.sort === 'relevance' ? kept : kept.sort((a, b) => compare(a, b, filters.sort));
}

export interface Facet {
  value: string;
  count: number;
}

/** Categories and indexers present in the results, most common first, for the menus. */
export function facets(results: readonly SearchResultDto[]): {
  categories: Facet[];
  indexers: Facet[];
} {
  const count = (values: Iterable<string>): Facet[] => {
    const counts = new Map<string, Facet>();
    for (const value of values) {
      const key = value.toLowerCase();
      const facet = counts.get(key);
      if (facet) {
        facet.count += 1;
      } else {
        counts.set(key, { value, count: 1 });
      }
    }
    return [...counts.values()].sort((a, b) => b.count - a.count || a.value.localeCompare(b.value));
  };
  return {
    categories: count(
      results.map((r) => topCategory(r.category)).filter((c): c is string => c !== null),
    ),
    indexers: count(results.flatMap((r) => [...new Set(r.indexers)])),
  };
}

/**
 * Whether the backend's best pick is among the results but hidden by the filters, so the
 * filter bar can say so (the preload stays on the best pick either way).
 */
export function bestPickHidden(
  results: readonly SearchResultDto[],
  shown: readonly SearchResultDto[],
): boolean {
  return results.some((r) => r.bestPick) && !shown.some((r) => r.bestPick);
}
