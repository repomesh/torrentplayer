// SPDX-License-Identifier: MIT OR Apache-2.0

import { describe, expect, it } from 'vitest';
import type { SearchResultDto } from './bindings/SearchResultDto';
import {
  DEFAULT_FILTERS,
  activeFilterCount,
  applyFilters,
  bestPickHidden,
  categoryChoices,
  categoryIds,
  facets,
  hiddenNonVideos,
  matches,
  peerCount,
  resolutionValue,
  sourceRank,
  SORT_LABELS,
  topCategory,
  type SearchFilters,
} from './filters';

function result(title: string, changes: Partial<SearchResultDto> = {}): SearchResultDto {
  return {
    title,
    sizeBytes: 1_500_000_000,
    seeders: 50,
    peers: 60,
    key: title,
    link: `magnet:?xt=urn:btih:${title}`,
    magnet: `magnet:?xt=urn:btih:${title}`,
    infoHash: title,
    download: null,
    detailsUrl: null,
    indexers: ['TheRARBG'],
    category: 'Movies/HD',
    published: '2026-01-01T00:00:00Z',
    license: null,
    health: null,
    suspicious: false,
    quality: {
      resolution: '1080p',
      hdr: false,
      codec: 'x264',
      source: 'WEB-DL',
      container: 'MP4',
      camera: false,
      suspectSize: false,
      playable: 'plays',
      label: '1080p WEB-DL',
      kind: 'video',
      audio: null,
      audioPlays: null,
      audioConverted: false,
    },
    reason: '',
    bestPick: false,
    ...changes,
  };
}

const withQuality = (
  title: string,
  quality: Partial<SearchResultDto['quality']>,
  changes: Partial<SearchResultDto> = {},
): SearchResultDto => {
  const r = result(title, changes);
  return { ...r, quality: { ...r.quality, ...quality } };
};

const f = (changes: Partial<SearchFilters>): SearchFilters => ({ ...DEFAULT_FILTERS, ...changes });
const titles = (rs: SearchResultDto[]): string[] => rs.map((r) => r.title);

describe('categories', () => {
  it('groups indexer categories by their top level', () => {
    expect(topCategory('Movies/HD')).toBe('Movies');
    expect(topCategory('TV > Episodes')).toBe('TV');
    expect(topCategory('Anime - English-translated')).toBe('Anime');
    expect(topCategory('TV/Anime')).toBe('Anime');
    expect(topCategory('5070')).toBe('Anime');
    expect(topCategory('Animation')).toBe('Animation');
    expect(topCategory('2040')).toBe('Movies');
    expect(topCategory('5040')).toBe('TV');
    expect(topCategory('99999')).toBeNull();
    expect(topCategory('  ')).toBeNull();
    expect(topCategory(null)).toBeNull();
  });

  it('offers the usual categories with counts, plus others found and the chosen one', () => {
    const found = facets([
      result('a', { category: 'TV/HD' }),
      result('b', { category: 'TV' }),
      result('c', { category: 'Lectures' }),
    ]).categories;
    const choices = categoryChoices(found, 'Podcasts');
    expect(choices.find((c) => c.value === 'TV')?.count).toBe(2);
    expect(choices.find((c) => c.value === 'Movies')?.count).toBe(0);
    expect(choices.map((c) => c.value).slice(-2)).toEqual(['Lectures', 'Podcasts']);
    expect(categoryChoices([], null).map((c) => c.value)).toContain('Anime');
  });
});

describe('filters', () => {
  const results = [
    result('movie', { bestPick: true }),
    result('show', { category: 'TV', indexers: ['EZTV', 'TheRARBG'], seeders: 3 }),
    withQuality('cam', { camera: true, resolution: '720p', playable: 'likely' }),
    withQuality('avi', { playable: 'needsFullPlayer', resolution: null }, { sizeBytes: null }),
    withQuality('uhd', { resolution: '2160p' }, { sizeBytes: 20_000_000_000, seeders: null }),
  ];

  it('passes everything by default, in the backend order', () => {
    expect(applyFilters(results, DEFAULT_FILTERS)).toEqual(results);
    expect(activeFilterCount(DEFAULT_FILTERS)).toBe(0);
    expect(activeFilterCount(f({ sort: 'newest' })), 'sorting is not a filter').toBe(0);
  });

  it('filters by category, source, and seeders (unknown counts fail a minimum)', () => {
    expect(titles(applyFilters(results, f({ category: 'tv' })))).toEqual(['show']);
    expect(titles(applyFilters(results, f({ indexer: 'eztv' })))).toEqual(['show']);
    expect(titles(applyFilters(results, f({ minSeeders: 5 })))).toEqual(['movie', 'cam', 'avi']);
    expect(activeFilterCount(f({ category: 'TV', minSeeders: 5 }))).toBe(2);
  });

  it('filters by resolution, size, playability, and camera copies', () => {
    expect(titles(applyFilters(results, f({ minResolution: 1080 })))).toEqual([
      'movie',
      'show',
      'uhd',
    ]);
    expect(titles(applyFilters(results, f({ minResolution: 2160 })))).toEqual(['uhd']);
    expect(titles(applyFilters(results, f({ maxSizeBytes: 4_000_000_000 })))).toEqual([
      'movie',
      'show',
      'cam',
    ]);
    expect(titles(applyFilters(results, f({ minSizeBytes: 10_000_000_000 })))).toEqual(['uhd']);
    expect(titles(applyFilters(results, f({ playableOnly: true })))).not.toContain('avi');
    expect(titles(applyFilters(results, f({ hideCamera: true })))).not.toContain('cam');
    expect(
      matches(results[0] as SearchResultDto, f({ playableOnly: true, hideCamera: true })),
    ).toBe(true);
  });

  it('sorts stably with unknown values last', () => {
    expect(titles(applyFilters(results, f({ sort: 'seeders' })))).toEqual([
      'movie',
      'cam',
      'avi',
      'show',
      'uhd',
    ]);
    expect(titles(applyFilters(results, f({ sort: 'largest' })))[0]).toBe('uhd');
    expect(titles(applyFilters(results, f({ sort: 'smallest' }))).at(-1)).toBe('avi');
    const dated = [
      result('old', { published: '2020-01-01' }),
      result('none', { published: null }),
      result('new', { published: '2026-05-01T10:00:00Z' }),
    ];
    expect(titles(applyFilters(dated, f({ sort: 'newest' })))).toEqual(['new', 'old', 'none']);
    expect(titles(applyFilters(dated, f({ sort: 'oldest' })))).toEqual(['old', 'new', 'none']);
  });

  it('sorts by all peers, seeders plus leechers', () => {
    const swarm = [
      result('few', { seeders: 5, peers: 9 }),
      result('unknown', { seeders: null, peers: null }),
      result('many', { seeders: 2, peers: 300 }),
      result('seeders only', { seeders: 40, peers: null }),
      result('leechers below seeders', { seeders: 70, peers: 10 }),
      result('tie', { seeders: 5, peers: 9 }),
    ];
    expect(titles(applyFilters(swarm, f({ sort: 'peers' })))).toEqual([
      'many',
      'leechers below seeders',
      'seeders only',
      'few',
      'tie',
      'unknown',
    ]);
    expect(peerCount({ seeders: 70, peers: 10 })).toBe(70);
    expect(peerCount({ seeders: null, peers: null })).toBeNull();
  });

  it('sorts by resolution, then the release source, with camera copies last', () => {
    const releases = [
      withQuality('720 bluray', { resolution: '720p', source: 'BluRay' }),
      withQuality('1080 hdtv', { resolution: '1080p', source: 'HDTV' }),
      withQuality('no resolution', { resolution: null, source: 'BluRay' }),
      withQuality('1080 cam', { resolution: '1080p', source: 'CAM', camera: true }),
      withQuality('1080 bluray', { resolution: '1080p', source: 'BluRay' }),
      withQuality('1080 unknown', { resolution: '1080p', source: null }),
      withQuality('4k web', { resolution: '2160p', source: 'WEB-DL' }),
      withQuality('1080 webrip', { resolution: '1080p', source: 'WEBRip' }),
    ];
    expect(titles(applyFilters(releases, f({ sort: 'quality' })))).toEqual([
      '4k web',
      '1080 bluray',
      '1080 webrip',
      '1080 hdtv',
      '1080 cam',
      '1080 unknown',
      '720 bluray',
      'no resolution',
    ]);
    const telesync = withQuality('ts', { source: 'Telesync', camera: false }).quality;
    expect(sourceRank(telesync), 'unknown sources go last').toBeNull();
  });

  it('sorts by name, with numbers in order and case ignored', () => {
    const named = [
      result('lecture 10'),
      result('Lecture 2'),
      result('algebra'),
      result('Lecture 1'),
    ];
    expect(titles(applyFilters(named, f({ sort: 'name' })))).toEqual([
      'algebra',
      'Lecture 1',
      'Lecture 2',
      'lecture 10',
    ]);
  });

  it('offers every sort in the menu, best match first', () => {
    expect(Object.keys(SORT_LABELS)).toEqual([
      'relevance',
      'peers',
      'quality',
      'seeders',
      'newest',
      'oldest',
      'largest',
      'smallest',
      'name',
    ]);
    expect(DEFAULT_FILTERS.sort).toBe('relevance');
  });

  it('reads resolutions', () => {
    expect(resolutionValue('1080p')).toBe(1080);
    expect(resolutionValue('4K')).toBe(2160);
    expect(resolutionValue('720i')).toBe(720);
    expect(resolutionValue('HD')).toBe(0);
    expect(resolutionValue(null)).toBe(0);
  });

  it('counts sources once per result and says when the best pick is filtered out', () => {
    const { indexers } = facets([result('a', { indexers: ['X', 'X', 'Y'] }), result('b')]);
    expect(indexers).toEqual([
      { value: 'TheRARBG', count: 1 },
      { value: 'X', count: 1 },
      { value: 'Y', count: 1 },
    ]);
    const shown = applyFilters(results, f({ category: 'TV' }));
    expect(bestPickHidden(results, shown)).toBe(true);
    expect(bestPickHidden(results, results)).toBe(false);
    expect(bestPickHidden(shown, shown)).toBe(false);
  });
});

describe('categoryIds', () => {
  it('maps the menu to Torznab ids for the search service, never adult ones', () => {
    expect(categoryIds(null)).toEqual([]);
    expect(categoryIds('Movies')).toEqual([2000]);
    expect(categoryIds('tv')).toEqual([5000]);
    expect(categoryIds('Anime')).toEqual([5070]);
    expect(categoryIds('Books')).toEqual([7000]);
    expect(categoryIds('XXX')).toEqual([]);
    expect(categoryIds('Lectures'), 'no id: filtered here instead').toEqual([]);
  });
});

describe('videos only', () => {
  const kind = (k: 'video' | 'notVideo' | 'unknown') => (title: string) =>
    result(title, { quality: { ...result(title).quality, kind: k } });

  it('hides results that are not videos, by default, but never unknown ones', () => {
    expect(DEFAULT_FILTERS.videosOnly).toBe(true);
    const all = [kind('video')('a'), kind('notVideo')('audiobook'), kind('unknown')('b')];
    expect(applyFilters(all, DEFAULT_FILTERS).map((r) => r.title)).toEqual(['a', 'b']);
    expect(hiddenNonVideos(all, DEFAULT_FILTERS)).toBe(1);
    const everything = { ...DEFAULT_FILTERS, videosOnly: false };
    expect(applyFilters(all, everything)).toHaveLength(3);
    expect(hiddenNonVideos(all, everything)).toBe(0);
    // Showing everything counts as a change.
    expect(activeFilterCount(everything)).toBe(1);
  });

  it('counts only what the other filters would show', () => {
    const big = kind('notVideo')('big');
    const small = { ...kind('notVideo')('small'), sizeBytes: 1_000 };
    const filters = { ...DEFAULT_FILTERS, minSizeBytes: 1_000_000 };
    expect(hiddenNonVideos([big, small], filters)).toBe(1);
  });
});
