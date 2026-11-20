import { create } from 'zustand';

/** Whether this window's find-in-chat bar is open (Ctrl+F, or "Find in chat"
    on a message's right-click menu). Per window, like every store: each hub
    and the overlay find in their own chat. The search itself lives in
    `FindBar`. While open, the message list renders every message (no
    virtualization), so every match is in the page to be found. */
interface FindState {
  open: boolean;
  /** Bumped on every open, so a second Ctrl+F re-focuses the field. */
  focusTick: number;
  show: () => void;
  close: () => void;
}

export const useFindStore = create<FindState>((set) => ({
  open: false,
  focusTick: 0,
  show: () => set((s) => ({ open: true, focusTick: s.focusTick + 1 })),
  close: () => set({ open: false }),
}));
