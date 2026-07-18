// localStorage-backed cache of a provider's last-known mode/effort info —
// see `CachedModeInfo`'s doc comment in ./types for why this exists.

import type { CachedModeInfo } from './types';

export type { CachedModeInfo };

export const modeInfoCacheKey = (providerId: string) => `kitty:lastModeInfo:${providerId}`;

export function readCachedModeInfo(providerId: string): CachedModeInfo | null {
  try {
    const raw = localStorage.getItem(modeInfoCacheKey(providerId));
    return raw ? (JSON.parse(raw) as CachedModeInfo) : null;
  } catch {
    return null;
  }
}

export function writeCachedModeInfo(providerId: string, info: CachedModeInfo) {
  try {
    localStorage.setItem(modeInfoCacheKey(providerId), JSON.stringify(info));
  } catch {
    // best-effort — a full/unavailable localStorage just means no seed next time
  }
}
