import { create } from 'zustand';

/** Transient Android-shell UI state that more than one component touches:
    the header's menu button and the chat's swipe gesture both open the
    drawer, and every message row needs to know whether *it* is the one whose
    actions are showing (only one at a time, so tapping a second message moves
    the actions rather than stacking them). Never persisted. */
export interface MobileUiState {
  drawerOpen: boolean;
  setDrawerOpen: (open: boolean) => void;
  /** `Message.id` of the message whose tap-revealed actions are showing. */
  revealedMessageId: string | null;
  /** Toggle: revealing the already-revealed message hides it. */
  toggleRevealMessage: (id: string) => void;
  hideMessageActions: () => void;
}

export const useMobileUiStore = create<MobileUiState>((set) => ({
  drawerOpen: false,
  setDrawerOpen: (open) => set({ drawerOpen: open }),
  revealedMessageId: null,
  toggleRevealMessage: (id) =>
    set((s) => ({ revealedMessageId: s.revealedMessageId === id ? null : id })),
  hideMessageActions: () => set({ revealedMessageId: null }),
}));
