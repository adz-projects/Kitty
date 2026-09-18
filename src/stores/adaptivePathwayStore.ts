import { create } from 'zustand';
import { ipc } from '@/lib/ipc';

interface AdaptivePathwayState {
  /** Whether *either* memory engine — behavioral (pathway) or factual
      (memorabilia) — has its in-process MCP server connected with tools
      registered. Gates whether the chat-header incognito control renders at
      all, since that one control now pauses both engines for the session.
      Unlike the retired sidecar's status, there's no live-update event for
      this, so it's re-queried on each `init()` call (component mount) rather
      than subscribed to once — two local Tauri commands are cheap enough that
      re-checking on every `ChatView` mount (e.g. the stack-status
      degraded/recovered swap in `main/App.tsx`/`overlay/App.tsx`) is not
      worth avoiding. */
  available: boolean;
  init: () => Promise<void>;
}

export const useAdaptivePathwayStore = create<AdaptivePathwayState>((set) => ({
  available: false,
  init: async () => {
    try {
      const [pathway, memorabilia] = await Promise.all([
        ipc.getAdaptivePathwayMcpStatus().catch(() => null),
        ipc.getMemorabiliaMcpStatus().catch(() => null),
      ]);
      const anyAvailable =
        (pathway?.tool_count ?? 0) > 0 || (memorabilia?.tool_count ?? 0) > 0;
      set({ available: anyAvailable });
    } catch {
      set({ available: false });
    }
  },
}));
