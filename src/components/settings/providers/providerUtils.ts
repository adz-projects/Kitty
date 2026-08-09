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
import type { NetworkTier, ProviderProfile } from '@/lib/types';

/** One-click quick-start presets for NPU/hybrid-NPU+GPU local inference —
    both are just `custom_openai` profiles pointed at a well-known local
    server, since Kitty's `custom_openai` type already handles the whole
    request/env/trust/network-tier path generically (no backend changes
    needed). Ports are the best-documented current defaults, but both
    projects have shipped different defaults across versions (Foundry
    Local: 5272 vs an older 5273; Lemonade: 13305 vs an older 8000) — the
    help text below says so rather than presenting false confidence. */
export const LOCAL_NPU_PRESETS: { label: string; name: string; baseUrl: string; note: string }[] = [
  {
    label: 'Foundry Local',
    name: 'Foundry Local',
    baseUrl: 'http://localhost:5272/v1',
    note: "Microsoft's vendor-neutral local server — auto-detects AMD/Intel/Qualcomm NPU, GPU, or CPU. Install: winget install Microsoft.FoundryLocal. If this port doesn't connect, run `foundry service status` to find the real one.",
  },
  {
    label: 'Lemonade Server (AMD)',
    name: 'Lemonade Server',
    baseUrl: 'http://localhost:13305/api/v1',
    note: "AMD's own local server — purpose-built NPU+GPU hybrid scheduling on Ryzen AI (XDNA), may outperform a generic execution-provider abstraction on that hardware specifically. If this port doesn't connect, check Lemonade's own settings for the port your installed version uses.",
  },
];

// Context-length detents (item 28): not linearly spaced, so the slider indexes
// into this array rather than mapping its position directly to a value. When
// auto-detection (Round-6 Feature 1) finds a real number for the selected
// model, it's spliced in as an extra detent (see `detentsFor` below) rather
// than snapped to the nearest static stop, so the exact real max is always
// reachable and reads correctly on the badge.
export const CTX_DETENTS = [4096, 8192, 16384, 32768, 65536, 131072, 262144];
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
      case 'custom_openai':
        return lookupContextLength(CUSTOM_OPENAI_CONTEXT_TABLE, model);
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
