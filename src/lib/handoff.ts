// The context-handoff gate (decision #16): moving a conversation onto a
// provider the user trusts less than the one it is on asks first whether the
// conversation goes with it.

import type { NetworkTier } from './types';

interface Card {
  network_tier: NetworkTier;
  is_trusted: boolean;
}

/** Higher is less trusted: on this machine, then a card the user marked
    trusted, then an untrusted private-network host, then an untrusted
    remote one. Pure. */
export function trustRank(card: Card): number {
  if (card.network_tier === 'local') return 0;
  if (card.is_trusted) return 1;
  return card.network_tier === 'personal' ? 2 : 3;
}

/** Whether moving from `from` to `to` needs the gate. An unknown origin (its
    card was deleted) counts as the most trusted, so the gate errs on asking. */
export function needsHandoffGate(from: Card | undefined, to: Card): boolean {
  return trustRank(to) > (from ? trustRank(from) : 0);
}

export type HandoffChoice = 'keep' | 'clean';

/** A remembered answer from config, if it is one. Pure. */
export function rememberedChoice(value: string | null | undefined): HandoffChoice | null {
  return value === 'keep' || value === 'clean' ? value : null;
}
