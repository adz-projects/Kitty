// Trust presentation for provider profiles (Round-2 item 18). Replaces the old
// tri-tier auto-classification badges. A loopback provider is always "local"
// (lock); a non-loopback provider is "trusted" (globe) only if the user marked it
// so, otherwise "untrusted" (caution). `network_tier` is still computed in Rust and
// used only to detect loopback here.

import type { NetworkTier } from './types';
import { LockIcon } from '@/components/icons/LockIcon';
import { GlobeIcon } from '@/components/icons/GlobeIcon';
import { WarningIcon } from '@/components/icons/WarningIcon';

export type TrustKind = 'local' | 'trusted' | 'untrusted';

export function trustKind(tier: NetworkTier, isTrusted: boolean): TrustKind {
  if (tier === 'local') return 'local';
  return isTrusted ? 'trusted' : 'untrusted';
}

const LABEL: Record<TrustKind, string> = {
  local: 'local',
  trusted: 'trusted',
  untrusted: 'untrusted',
};

/** Icon-only — for inline use immediately before a label the caller already
    renders (e.g. `ProviderBadge`'s own status-badge text). Replaces the old
    🔒/🌐/⚠️ string glyphs (UX-simplification Batch 4). */
export function TrustIcon({ tier, isTrusted }: { tier: NetworkTier; isTrusted: boolean }) {
  const k = trustKind(tier, isTrusted);
  if (k === 'local') return <LockIcon />;
  if (k === 'trusted') return <GlobeIcon />;
  return <WarningIcon />;
}

/** Icon + label — for standalone trust badges (provider list rows, popover
    items, the base-URL trust note). */
export function TrustBadge({ tier, isTrusted }: { tier: NetworkTier; isTrusted: boolean }) {
  return (
    <>
      <TrustIcon tier={tier} isTrusted={isTrusted} /> {LABEL[trustKind(tier, isTrusted)]}
    </>
  );
}
