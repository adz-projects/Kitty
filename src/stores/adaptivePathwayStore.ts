import { create } from 'zustand';
import { ipc, onAdaptivePathwayStatus } from '@/lib/ipc';
import type { AdaptivePathwayStatus } from '@/lib/types';

interface AdaptivePathwayState {
  status: AdaptivePathwayStatus;
  /** Prime from the current status, then subscribe to changes. Idempotent —
      mirrors `stackStore.ts`'s exact pattern. The Adaptive Pathway toggle
      used to keep this in its own component-local state, which reset to the
      `'disabled'` default every time `ChatView` unmounted (e.g. the
      stack-status degraded/recovered swap in `main/App.tsx`/`overlay/App.tsx`),
      making the button flicker even though the sidecar itself never went
      down. Living in a store with a module-level "subscribed once" guard
      means the status (and its live event subscription) survives any number
      of `ChatView` mount/unmount cycles within a window's lifetime. */
  init: () => Promise<void>;
}

let subscribed = false;

export const useAdaptivePathwayStore = create<AdaptivePathwayState>((set) => ({
  status: 'disabled',
  init: async () => {
    try {
      set({ status: await ipc.getAdaptivePathwayStatus() });
    } catch {
      // Backend not ready yet; the health-loop event will update us.
    }
    if (!subscribed) {
      subscribed = true;
      await onAdaptivePathwayStatus((p) => set({ status: p.status }));
    }
  },
}));
