// Session history state (Phase 4), backed entirely by goosed's session routes.
import { create } from 'zustand';
import { ipc } from '@/lib/ipc';
import { parseSession, type SessionSummary } from '@/lib/types';

interface SessionState {
  sessions: SessionSummary[];
  loading: boolean;
  query: string;
  refresh: () => Promise<void>;
  remove: (sessionId: string) => Promise<void>;
  setQuery: (q: string) => void;
  filtered: () => SessionSummary[];
}

export const useSessionStore = create<SessionState>((set, get) => ({
  sessions: [],
  loading: false,
  query: '',

  refresh: async () => {
    set({ loading: true });
    try {
      const raw = await ipc.listSessions();
      const sessions = raw.map(parseSession).sort((a, b) => (a.updatedAt < b.updatedAt ? 1 : -1));
      set({ sessions });
    } finally {
      set({ loading: false });
    }
  },

  remove: async (sessionId: string) => {
    await ipc.deleteSession(sessionId);
    set((s) => ({ sessions: s.sessions.filter((x) => x.sessionId !== sessionId) }));
  },

  setQuery: (q: string) => set({ query: q }),

  filtered: () => {
    const { sessions, query } = get();
    const q = query.trim().toLowerCase();
    if (!q) return sessions;
    return sessions.filter(
      (s) => s.title.toLowerCase().includes(q) || s.cwd.toLowerCase().includes(q)
    );
  },
}));
