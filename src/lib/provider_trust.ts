// Trust presentation for provider profiles (Round-2 item 18). Replaces the old
// tri-tier auto-classification badges. A loopback provider is always "local"
// (lock); a non-loopback provider is "trusted" (globe) only if the user marked it
// so, otherwise "untrusted" (caution). `network_tier` is still computed in Rust and
// used only to detect loopback here.

import type { NetworkTier } from './types';

export type TrustKind = 'local' | 'trusted' | 'untrusted';

export function trustKind(tier: NetworkTier, isTrusted: boolean): TrustKind {
  if (tier === 'local') return 'local';
  return isTrusted ? 'trusted' : 'untrusted';
}

const ICON: Record<TrustKind, string> = {
  local: '🔒',
  trusted: '🌐',
  untrusted: '⚠️',
};

const LABEL: Record<TrustKind, string> = {
  local: 'local',
  trusted: 'trusted',
  untrusted: 'untrusted',
};

export function trustIcon(tier: NetworkTier, isTrusted: boolean): string {
  return ICON[trustKind(tier, isTrusted)];
}

export function trustBadge(tier: NetworkTier, isTrusted: boolean): string {
  const k = trustKind(tier, isTrusted);
  return `${ICON[k]} ${LABEL[k]}`;
}
