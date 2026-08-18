import type { ModelPickerEntry } from '@/lib/types';

/** The three filter chips (provider-add redesign) — each cuts the list to
    its top 10, ranked by the field named. ZDR was explicitly dropped from
    scope; OpenRouter's `zdr=true` param would be a trivial 4th chip later. */
export type PickerFilter = 'newest' | 'cheapest' | 'most_capable';

export const FILTER_LABEL: Record<PickerFilter, string> = {
  newest: 'Newest',
  cheapest: 'Cheapest',
  most_capable: 'Most Capable',
};

const TOP_N = 10;

/** The field each filter ranks by, `null` when the model has no value for
    it — entries without one are excluded from the top-10 cut, not treated
    as "worst". */
function rankValue(filter: PickerFilter, m: ModelPickerEntry): number | null {
  switch (filter) {
    case 'newest':
      return m.created;
    case 'cheapest':
      return m.price_rank;
    case 'most_capable':
      return m.capability_score;
  }
}

/** Alphabetical by name — the neutral baseline when no filter or search is
    active. */
export function sortAlphabetical(models: ModelPickerEntry[]): ModelPickerEntry[] {
  return [...models].sort((a, b) => a.name.localeCompare(b.name));
}

/** Top 10 by the given filter's rank field (cheapest = ascending price,
    newest/most-capable = descending). If fewer than 10 models actually have
    a value for this filter, the remaining slots are filled alphabetically
    from the rest — documented fallback, not a bug: a thin catalog (e.g. a
    brand-new provider with only 3 ranked models) still shows a full,
    useful list rather than a half-empty one. */
export function applyFilter(models: ModelPickerEntry[], filter: PickerFilter): ModelPickerEntry[] {
  const ranked = models.filter((m) => rankValue(filter, m) != null);
  const unranked = models.filter((m) => rankValue(filter, m) == null);
  ranked.sort((a, b) => {
    const av = rankValue(filter, a) as number;
    const bv = rankValue(filter, b) as number;
    return filter === 'cheapest' ? av - bv : bv - av;
  });
  const top = ranked.slice(0, TOP_N);
  if (top.length >= TOP_N) return top;
  const fillCount = TOP_N - top.length;
  const filler = sortAlphabetical(unranked).slice(0, fillCount);
  return [...top, ...filler];
}

/** Full, uncapped substring match on name/id — search and a filter chip are
    mutually exclusive in the picker UI (see `ModelPicker.tsx`), so this
    never needs to compose with `applyFilter`. */
export function applySearch(models: ModelPickerEntry[], query: string): ModelPickerEntry[] {
  const q = query.trim().toLowerCase();
  if (!q) return models;
  return models.filter(
    (m) => m.name.toLowerCase().includes(q) || m.id.toLowerCase().includes(q),
  );
}

/** The single entry point `ModelPicker.tsx` renders from: search (if any)
    wins over an active filter chip, an active filter wins over the
    alphabetical default. */
export function visibleModels(
  models: ModelPickerEntry[],
  filter: PickerFilter | null,
  search: string,
): ModelPickerEntry[] {
  if (search.trim()) return applySearch(sortAlphabetical(models), search);
  if (filter) return applyFilter(models, filter);
  return sortAlphabetical(models);
}

export const COST_TIER_LABEL: Record<string, string> = {
  premium: 'Premium',
  moderate: 'Moderate',
  economy: 'Economy',
};
