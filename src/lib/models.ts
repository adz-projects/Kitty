// How a model is named and priced wherever Kitty lists models: a provider's
// checklist, the default-model menu, and a chat's, a scheduled task's or a
// specialist's model menu. Lists arrive ranked from the backend
// (`ai_models/mod.rs`: intelligence less age); these keep that order.

import type { ModelInfo, ProviderView } from './types';

/** "Claude Sonnet 5 · $$$" — the name and its price band. */
export function modelLabel(m: ModelInfo): string {
  return m.price_tier ? `${m.name} · ${m.price_tier}` : m.name;
}

/** The exact price, for a hint: "$3.00 in / $15.00 out per million tokens". */
export function priceHint(m: ModelInfo): string | null {
  if (m.price_out_per_m === null) return null;
  if (m.price_out_per_m === 0) return 'Free';
  const out = `$${m.price_out_per_m.toFixed(2)}`;
  return m.price_in_per_m === null
    ? `${out} per million output tokens`
    : `$${m.price_in_per_m.toFixed(2)} in / ${out} out per million tokens`;
}

/** A model's hover text: its id, price and intelligence index, when known. */
export function modelTitle(m: ModelInfo): string {
  const parts = [m.model];
  const price = priceHint(m);
  if (price) parts.push(price);
  if (m.intelligence !== null) parts.push(`Intelligence index ${Math.round(m.intelligence)}`);
  return parts.join(' — ');
}

/** Cards grouped by provider for `<optgroup>`s: groups in the order their
    first model appears (the list arrives ranked), models keep their rank. */
export function groupByProvider<T extends Pick<ProviderView, 'info'>>(
  cards: T[]
): { providerId: string; provider: string; cards: T[] }[] {
  const groups = new Map<string, { providerId: string; provider: string; cards: T[] }>();
  for (const c of cards) {
    const id = c.info.provider_id;
    const group = groups.get(id) ?? { providerId: id, provider: c.info.provider_name, cards: [] };
    group.cards.push(c);
    groups.set(id, group);
  }
  return [...groups.values()];
}

/** A card's label in a menu that lists one provider's models among others'
    ("Claude Sonnet 5 · $$$"), with the provider's name when `withProvider`. */
export function cardLabel(c: Pick<ProviderView, 'info'>, withProvider = false): string {
  const label = modelLabel(c.info);
  return withProvider ? `${label} — ${c.info.provider_name}` : label;
}
