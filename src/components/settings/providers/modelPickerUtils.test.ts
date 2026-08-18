import { describe, it, expect } from 'vitest';
import { applyFilter, applySearch, sortAlphabetical, visibleModels } from './modelPickerUtils';
import type { ModelPickerEntry } from '@/lib/types';

function entry(overrides: Partial<ModelPickerEntry> & { id: string }): ModelPickerEntry {
  return {
    name: overrides.id,
    cost_tier: null,
    capability_score: null,
    price_rank: null,
    created: null,
    context_length: null,
    matched: false,
    ...overrides,
  };
}

describe('applyFilter — top 10, ranked', () => {
  it('sorts "cheapest" ascending by price_rank', () => {
    const models = [
      entry({ id: 'a', price_rank: 3 }),
      entry({ id: 'b', price_rank: 1 }),
      entry({ id: 'c', price_rank: 2 }),
    ];
    expect(applyFilter(models, 'cheapest').map((m) => m.id)).toEqual(['b', 'c', 'a']);
  });

  it('sorts "newest" descending by created', () => {
    const models = [
      entry({ id: 'a', created: 100 }),
      entry({ id: 'b', created: 300 }),
      entry({ id: 'c', created: 200 }),
    ];
    expect(applyFilter(models, 'newest').map((m) => m.id)).toEqual(['b', 'c', 'a']);
  });

  it('sorts "most_capable" descending by capability_score', () => {
    const models = [
      entry({ id: 'a', capability_score: 40 }),
      entry({ id: 'b', capability_score: 80 }),
      entry({ id: 'c', capability_score: 60 }),
    ];
    expect(applyFilter(models, 'most_capable').map((m) => m.id)).toEqual(['b', 'c', 'a']);
  });

  it('caps the ranked result at 10', () => {
    const models = Array.from({ length: 15 }, (_, i) =>
      entry({ id: `m${i}`, price_rank: i }),
    );
    expect(applyFilter(models, 'cheapest')).toHaveLength(10);
  });

  it('excludes unranked entries from the ranked portion, filling remaining slots alphabetically', () => {
    const ranked = [entry({ id: 'ranked-1', price_rank: 1 })];
    const unranked = [entry({ id: 'zeta' }), entry({ id: 'alpha' }), entry({ id: 'mu' })];
    const result = applyFilter([...ranked, ...unranked], 'cheapest');
    // The one ranked entry comes first, then the unranked fill alphabetically.
    expect(result.map((m) => m.id)).toEqual(['ranked-1', 'alpha', 'mu', 'zeta']);
  });

  it('an empty list produces an empty result, not a crash', () => {
    expect(applyFilter([], 'cheapest')).toEqual([]);
  });
});

describe('applySearch', () => {
  const models = [
    entry({ id: 'anthropic/claude-sonnet-5', name: 'Claude Sonnet 5' }),
    entry({ id: 'openai/gpt-5.1', name: 'GPT-5.1' }),
  ];

  it('matches by name, case-insensitively', () => {
    expect(applySearch(models, 'claude').map((m) => m.id)).toEqual(['anthropic/claude-sonnet-5']);
  });

  it('matches by id when the name does not match', () => {
    expect(applySearch(models, 'openai/gpt').map((m) => m.id)).toEqual(['openai/gpt-5.1']);
  });

  it('an empty query returns everything unfiltered', () => {
    expect(applySearch(models, '   ')).toEqual(models);
  });

  it('is not capped at 10 — search works across the full list', () => {
    const many = Array.from({ length: 20 }, (_, i) => entry({ id: `match-${i}`, name: `Match ${i}` }));
    expect(applySearch(many, 'match')).toHaveLength(20);
  });
});

describe('sortAlphabetical', () => {
  it('sorts by name', () => {
    const models = [entry({ id: '1', name: 'Zeta' }), entry({ id: '2', name: 'Alpha' })];
    expect(sortAlphabetical(models).map((m) => m.name)).toEqual(['Alpha', 'Zeta']);
  });
});

describe('visibleModels — the picker\'s single entry point', () => {
  const models = [
    entry({ id: 'a', name: 'Alpha', price_rank: 2 }),
    entry({ id: 'b', name: 'Beta', price_rank: 1 }),
  ];

  it('search wins over an active filter', () => {
    const result = visibleModels(models, 'cheapest', 'alpha');
    expect(result.map((m) => m.id)).toEqual(['a']);
  });

  it('an active filter wins over the alphabetical default', () => {
    const result = visibleModels(models, 'cheapest', '');
    expect(result.map((m) => m.id)).toEqual(['b', 'a']);
  });

  it('falls back to alphabetical with no filter and no search', () => {
    const result = visibleModels(models, null, '');
    expect(result.map((m) => m.id)).toEqual(['a', 'b']);
  });
});
