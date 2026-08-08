import { create } from 'zustand';
import { ipc, onStackStatus, onStartupPhase } from '@/lib/ipc';
import type { StackStatus, StartupPhase } from '@/lib/types';

interface StackState {
  status: StackStatus;
  detail: string | null;
  /** One-time startup progress, separate from `status` (see types.ts). */
  startupPhase: StartupPhase;
  /** Prime from the current status, then subscribe to changes. Idempotent. */
  init: () => Promise<void>;
}

// Per-channel bind state so the two `init()` subscription paths can't leak or
// double-bind: `subscribed` guards "already successfully bound", `pending`
// dedupes concurrent in-flight binds (two StrictMode/concurrent init() calls
// would otherwise BOTH pass the `subscribed` check before either await
// resolves and attach duplicate listeners), and a failed bind stays pending-
// cleared so a later init() retries that channel without re-bounding the
// already-successful one.
const bound: { status: boolean; startupPhase: boolean } = { status: false, startupPhase: false };
const pending: { status: Promise<void> | null; startupPhase: Promise<void> | null } = {
  status: null,
  startupPhase: null,
};

export const useStackStore = create<StackState>((set) => ({
  status: 'starting',
  detail: null,
  startupPhase: 'spawning_backend',
  init: async () => {
    try {
      set({ status: await ipc.getStackStatus() });
    } catch {
      // Backend not ready yet; the health loop event will update us.
    }
    try {
      set({ startupPhase: await ipc.getStartupPhase() });
    } catch {
      // Backend not ready yet; the stack://startup-phase event will update us.
    }
    if (!bound.status && !pending.status) {
      pending.status = onStackStatus((p) => set({ status: p.status, detail: p.detail }))
        .then(() => {
          bound.status = true;
        })
        .catch(() => {
          // Leave `bound.status` false so a later init() retries this channel.
        })
        .finally(() => {
          pending.status = null;
        });
    }
    if (!bound.startupPhase && !pending.startupPhase) {
      pending.startupPhase = onStartupPhase((p) => set({ startupPhase: p.phase }))
        .then(() => {
          bound.startupPhase = true;
        })
        .catch(() => {
          // Leave `bound.startupPhase` false so a later init() retries it.
        })
        .finally(() => {
          pending.startupPhase = null;
        });
    }
    // Await any just-started binds so concurrent init() callers don't race
    // ahead of the flag flips; already-bound/pending calls short-circuit.
    await pending.status;
    await pending.startupPhase;
  },
}));
