// Session history state. Populated in Phase 4 from goosed's session routes —
// scaffold only for now.
import { create } from 'zustand';

export interface SessionSummary {
  id: string;
  title: string;
  workingDir: string;
  updatedAt: string;
}

interface SessionState {
  sessions: SessionSummary[];
  setSessions: (s: SessionSummary[]) => void;
}

export const useSessionStore = create<SessionState>((set) => ({
  sessions: [],
  setSessions: (sessions) => set({ sessions }),
}));
