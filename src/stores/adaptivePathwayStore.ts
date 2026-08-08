import { create } from 'zustand';
import { ipc } from '@/lib/ipc';

interface AdaptivePathwayState {
  /** Whether the pathway (behavioral-memory) engine's in-process MCP server
      is actually connected and has tools registered — gates whether
      `AdaptivePathwayToggle` renders at all. Unlike the retired sidecar's
      status, there's no live-update event for this, so it's re-queried on
      each `init()` call (component mount) rather than subscribed to once —
      a single local Tauri command is cheap enough that re-checking on every
      `ChatView` mount (e.g. the stack-status degraded/recovered swap in
      `main/App.tsx`/`overlay/App.tsx`) is not worth avoiding. */
  available: boolean;
  init: () => Promise<void>;
}

export const useAdaptivePathwayStore = create<AdaptivePathwayState>((set) => ({
  available: false,
  init: async () => {
    try {
      const status = await ipc.getAdaptivePathwayMcpStatus();
      set({ available: (status?.tool_count ?? 0) > 0 });
    } catch {
      set({ available: false });
    }
  },
}));
