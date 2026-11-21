import { create } from 'zustand';
import { ipc, onResearchProgress } from '@/lib/ipc';
import { isLive, type ResearchDetail, type ResearchStatus } from '@/lib/research';

/** Merge an update into the run list, keyed by run id. Pure. */
export function upsertRun(list: ResearchDetail[], run: ResearchDetail): ResearchDetail[] {
  const i = list.findIndex((r) => r.id === run.id);
  if (i === -1) return [run, ...list];
  const next = list.slice();
  next[i] = { ...next[i], ...run };
  return next;
}

/** The run a chat should show: its live one, else its newest. Pure. */
export function runForSession(
  list: ResearchDetail[],
  sessionId: string | null
): ResearchDetail | null {
  if (!sessionId) return null;
  const mine = list.filter((r) => r.session_id === sessionId);
  return mine.find((r) => isLive(r.status)) ?? mine[0] ?? null;
}

interface ResearchState {
  /** Runs this window has heard about, newest first. */
  runs: ResearchDetail[];
  /** Runs whose report this window has already reloaded the chat for. */
  reloaded: string[];
  init: () => Promise<void>;
  /** Load a chat's runs (on opening it). */
  loadForSession: (sessionId: string) => Promise<void>;
  cancel: (researchId: string) => Promise<void>;
  markReloaded: (researchId: string) => void;
}

let subscribed = false;

export const useResearchStore = create<ResearchState>((set, get) => ({
  runs: [],
  reloaded: [],

  init: async () => {
    if (subscribed) return;
    subscribed = true;
    await onResearchProgress((e) => {
      const detail: ResearchDetail = {
        ...(e.detail ?? { id: e.research_id }),
        id: e.research_id,
        session_id: e.session_id,
        status: (e.status as ResearchStatus) ?? e.detail?.status ?? 'planning',
      };
      set((s) => ({ runs: upsertRun(s.runs, detail) }));
    });
  },

  loadForSession: async (sessionId) => {
    try {
      const runs = await ipc.listResearch(sessionId);
      // Finished runs were already reloaded, or never needed it.
      const done = runs.filter((r) => !isLive(r.status)).map((r) => r.id);
      set((s) => ({
        runs: runs.reduce(upsertRun, s.runs),
        reloaded: Array.from(new Set([...s.reloaded, ...done])),
      }));
    } catch (e) {
      console.warn('listResearch failed', e);
    }
  },

  cancel: async (researchId) => {
    try {
      await ipc.cancelResearch(researchId);
    } catch (e) {
      console.warn('cancelResearch failed', e);
    }
  },

  markReloaded: (researchId) => {
    if (get().reloaded.includes(researchId)) return;
    set((s) => ({ reloaded: [...s.reloaded, researchId] }));
  },
}));
