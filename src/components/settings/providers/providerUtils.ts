// Pure helpers shared by Providers.tsx (the list) and ProviderForm.tsx (the
// add/edit modal): network-tier classification, context-length detents, the
// blank profile, and which models a fresh fetch starts with checked.

import { DEFAULT_URL } from '@/lib/provider_defaults';
import type { NetworkTier, ProviderProfile, ProviderType } from '@/lib/types';

/** Provider types at a fixed, well-known endpoint: their base URL is never
    shown. `local`, `custom_openai` and `ollama` need an editable one. */
export const EDITABLE_URL_TYPES: ProviderType[] = ['local', 'custom_openai', 'ollama'];

export function hasFixedEndpoint(providerType: ProviderType): boolean {
  return !EDITABLE_URL_TYPES.includes(providerType);
}

/** A provider is worth saving once at least one of its models is checked. */
export function canSaveProvider(profile: Pick<ProviderProfile, 'models' | 'enabled_models'>) {
  return profile.models.some((m) => m.trim() && profile.enabled_models.includes(m));
}

/** Above this many models a fresh fetch starts with only the suggested model
    checked: a long catalog (OpenRouter, OpenAI) would otherwise flood every
    model menu. A short list (a self-hosted server, Anthropic) starts all
    checked. */
export const CHECK_ALL_UP_TO = 20;

/** The models checked after a fetch: what was checked before and is still
    listed, plus — for a provider with nothing checked yet — every model of a
    short list, or the suggested one of a long list. */
export function initialChecks(
  listed: string[],
  previouslyChecked: string[],
  suggested: string | null
): string[] {
  const kept = previouslyChecked.filter((m) => listed.includes(m));
  if (kept.length > 0) return kept;
  if (listed.length <= CHECK_ALL_UP_TO) return [...listed];
  return suggested && listed.includes(suggested) ? [suggested] : [];
}

// Context-length detents (item 28): not linearly spaced, so the slider indexes
// into this array rather than mapping its position directly to a value. When
// auto-detection (Round-6 Feature 1) finds a real number for the selected
// model, it's spliced in as an extra detent (see `detentsFor` below) rather
// than snapped to the nearest static stop, so the exact real max is always
// reachable and reads correctly on the badge.
export const CTX_DETENTS = [
  4096, 8192, 12288, 16384, 24576, 32768, 49152, 65536, 98304, 131072, 196608, 262144, 393216,
  524288, 786432, 1048576,
];
export const ctxLabel = (v: number) => (v % 1024 === 0 ? `${v / 1024}K` : String(v));
export function nearestCtxIndex(detents: number[], v: number): number {
  let best = 0;
  let bd = Infinity;
  detents.forEach((d, i) => {
    const dist = Math.abs(d - v);
    if (dist < bd) {
      bd = dist;
      best = i;
    }
  });
  return best;
}

/** Static detents plus a live-detected value, if any and not already present. */
export function detentsFor(suggested: number | null): number[] {
  if (suggested == null || CTX_DETENTS.includes(suggested)) return CTX_DETENTS;
  return [...CTX_DETENTS, suggested].sort((a, b) => a - b);
}

/** Client-side mirror of providers::network_tier_for — only used to detect
    loopback (which is always "local"/trusted). */
export function tierOf(url: string): NetworkTier {
  const hostPort = (url.split('://').pop() ?? '').split('/')[0];
  const afterAt = hostPort.split('@').pop() ?? '';
  // IPv6 hosts stay bracketed (e.g. `[::1]`), matching Rust's `host_of` — splitting
  // on `:` first would truncate `[::1]:11434` down to just `[`.
  const host = afterAt.startsWith('[') ? afterAt : (afterAt.split(':')[0] ?? '');
  const h = host.toLowerCase();
  if (!h || h === 'localhost' || h === '127.0.0.1' || h === '::1' || h === '[::1]') return 'local';
  if (h.endsWith('.ts.net')) return 'personal';
  const o = h.split('.').map(Number);
  if (o.length === 4 && o[0] === 100 && o[1] >= 64 && o[1] <= 127) return 'personal';
  return 'remote';
}

export const isLocal = (url: string) => tierOf(url) === 'local';

export const blank = (): ProviderProfile => ({
  id: '',
  name: '',
  provider_type: 'openrouter',
  base_url: DEFAULT_URL.openrouter,
  models: [],
  enabled_models: [],
  model_ids: {},
  vision_models: [],
  context_lengths: {},
  is_trusted: false,
  temperature: null,
  top_p: null,
  top_k: null,
  min_p: null,
  presence_penalty: null,
  frequency_penalty: null,
  max_tokens: null,
  context_length: null,
  supports_vision: false,
  system_prompt: null,
  prompt_idle_timeout_secs: null,
  parallel_slots: null,
  created_at: '',
});

export function hostOf(url: string): string {
  try {
    return new URL(url).host;
  } catch {
    return url;
  }
}
