import { create } from 'zustand';
import { ipc, onStackStatus } from '@/lib/ipc';
import type { StackStatus } from '@/lib/types';

interface StackState {
  status: StackStatus;
  detail: string | null;
  /** Prime from the current status, then subscribe to changes. Idempotent. */
  init: () => Promise<void>;
}

let subscribed = false;

export const useStackStore = create<StackState>((set) => ({
  status: 'starting',
  detail: null,
  init: async () => {
    try {
      set({ status: await ipc.getStackStatus() });
    } catch {
      // Backend not ready yet; the health loop event will update us.
    }
    if (!subscribed) {
      subscribed = true;
      await onStackStatus((p) => set({ status: p.status, detail: p.detail }));
    }
  },
}));
