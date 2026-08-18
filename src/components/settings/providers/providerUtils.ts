// Pure helpers shared by Providers.tsx (the list) and ProviderForm.tsx (the
// add/edit modal): network-tier classification, context-length detents, and
// the blank-profile/context-length-suggestion helpers.

import { ipc } from '@/lib/ipc';
import {
  ANTHROPIC_CONTEXT_TABLE,
  CUSTOM_OPENAI_CONTEXT_TABLE,
  lookupContextLength,
} from '@/lib/context_length_table';
import { DEFAULT_URL } from '@/lib/provider_defaults';
import type { NetworkTier, ProviderProfile, ProviderType } from '@/lib/types';

/** Provider types that keep the original free-text base-URL/models form
    (provider-add redesign): `local` has nothing to validate a key against,
    `custom_openai` is a hand-configured endpoint that may not even
    implement `/v1/models`, and `ollama` takes no API key at all. Every
    other type gets the key-validate-then-pick-one-model flow. */
export const LEGACY_FORM_TYPES: ProviderType[] = ['local', 'custom_openai', 'ollama'];

export function usesModelPicker(providerType: ProviderType): boolean {
  return !LEGACY_FORM_TYPES.includes(providerType);
}

/** The actual mechanism enforcing "exactly one model" for the new-flow
    types — legacy-form types keep whatever gating existed before (none
    beyond the fields already required, i.e. always savable from this
    check's point of view). */
export function canSaveProvider(providerType: ProviderType, models: string[]): boolean {
  if (!usesModelPicker(providerType)) return true;
  return models.length === 1 && Boolean(models[0]?.trim());
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

/** Best-effort context-window suggestion for the model currently selected on
    `profile`, per provider type (Round-6 Feature 1): OpenRouter queries live;
    Anthropic/custom_openai use a small hardcoded table. `null` when nothing is
    known — the field stays fully manual in that case, which is now also what
    an Ollama profile gets: the live lookup went with managed Ollama, and
    probing an arbitrary self-hosted server from here isn't worth a round trip
    the user can shortcut by typing the number. */
export async function suggestContextLength(profile: ProviderProfile): Promise<number | null> {
  const model = profile.models[0];
  if (!model) return null;
  try {
    switch (profile.provider_type) {
      case 'openrouter':
        return await ipc.openrouterContextLength(model);
      case 'anthropic':
        return lookupContextLength(ANTHROPIC_CONTEXT_TABLE, model);
      case 'openai':
        return lookupContextLength(CUSTOM_OPENAI_CONTEXT_TABLE, model);
      case 'custom_openai': {
        // Try a live lookup against the server itself first (many self-hosted
        // OpenAI-compatible servers expose it via /props or /v1/models); fall
        // back to the same hardcoded table OpenAI uses when that comes up empty.
        const live = await ipc.customOpenaiContextLength(profile.base_url, model);
        return live ?? lookupContextLength(CUSTOM_OPENAI_CONTEXT_TABLE, model);
      }
      case 'local': {
        // The GGUF header already carries the trained context window, parsed at
        // download time — no probe, just find the installed model and read it.
        const models = await ipc.listLocalModels();
        const m = models.find((x) => x.id === model || x.file === model);
        return m?.info?.context_length ?? null;
      }
      case 'ollama':
        // A remote Ollama the user runs themselves — ask it directly.
        return await ipc.ollamaContextLength(profile.base_url, model);
      default:
        return null;
    }
  } catch {
    return null;
  }
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
  is_trusted: false,
  temperature: null,
  top_p: null,
  top_k: null,
  min_p: null,
  presence_penalty: null,
  frequency_penalty: null,
  max_tokens: null,
  context_length: null,
  strip_reasoning: false,
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
