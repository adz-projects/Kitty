// Chat render state. Fleshed out in Phase 2 (streamed messages, tool cards) —
// scaffold only for now: the active session id shared across windows.
import { create } from 'zustand';

interface ChatState {
  sessionId: string | null;
  setSessionId: (id: string | null) => void;
}

export const useChatStore = create<ChatState>((set) => ({
  sessionId: null,
  setSessionId: (id) => set({ sessionId: id }),
}));
