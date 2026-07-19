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

let subscribed = false;

export const useStackStore = create<StackState>((set) => ({
  status: 'starting',
  detail: null,
  startupPhase: 'spawning_goosed',
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
    if (!subscribed) {
      subscribed = true;
      await onStackStatus((p) => set({ status: p.status, detail: p.detail }));
      await onStartupPhase((p) => set({ startupPhase: p.phase }));
    }
  },
}));
