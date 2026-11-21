// How a model is named and priced wherever Kitty lists models: a provider's
// checklist and the setup editor's model menu. Lists arrive ranked from the
// backend (`ai_models/mod.rs`: intelligence less age); these keep that order.

import type { ModelInfo } from './types';

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
