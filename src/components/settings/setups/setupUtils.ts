// Pure helpers for Settings → Setups and the provider form's "used by"
// marks.

import type { ModelInfo, ProviderAccountView, ProviderView, Setup } from '@/lib/types';

/** One provider's checked models, ranked, for the setup editor's menu. */
export interface CheckedGroup {
  providerId: string;
  provider: string;
  models: ModelInfo[];
}

/** Every provider's checked models in rank order (the order
    `model_infos` arrives in), providers with none left out. */
export function checkedModels(accounts: ProviderAccountView[]): CheckedGroup[] {
  return accounts
    .filter((a) => !a.disabled_reason)
    .map((a) => ({
      providerId: a.id,
      provider: a.name,
      models: a.model_infos.filter((m) => m.enabled),
    }))
    .filter((g) => g.models.length > 0);
}

/** A `<select>` value naming a provider's model. Model ids may hold any
    character but a newline. */
export function modelKey(providerId: string, model: string): string {
  return `${providerId}\n${model}`;
}

export function splitModelKey(key: string): { providerId: string; model: string } {
  const at = key.indexOf('\n');
  return at < 0
    ? { providerId: key, model: '' }
    : { providerId: key.slice(0, at), model: key.slice(at + 1) };
}

/** A new setup: the first checked model, named after it. */
export function blankSetup(accounts: ProviderAccountView[]): Setup {
  const first = checkedModels(accounts)[0]?.models[0];
  return {
    id: '',
    name: first?.name ?? '',
    provider_id: first?.provider_id ?? '',
    model: first?.model ?? '',
    created_at: '',
  };
}

/** A copy of `setup` to edit as a new one. */
export function duplicateOf(setup: Setup): Setup {
  return { ...setup, id: '', name: `${setup.name} (copy)`, created_at: '' };
}

/** The names of the setups that run `model` of `providerId`. */
export function setupsUsing(setups: Setup[], providerId: string, model: string): string[] {
  return setups.filter((s) => s.provider_id === providerId && s.model === model).map((s) => s.name);
}

/** "OpenRouter · Claude Sonnet 5" — what a setup runs, for a hover. */
export function setupTitle(card: Pick<ProviderView, 'info'>): string {
  return `${card.info.provider_name} · ${card.info.name}`;
}

/** `ids` with the one at `index` moved by `delta` (clamped). */
export function moved<T>(items: T[], index: number, delta: number): T[] {
  const to = Math.max(0, Math.min(items.length - 1, index + delta));
  if (to === index) return items;
  const next = [...items];
  const [item] = next.splice(index, 1);
  next.splice(to, 0, item);
  return next;
}
