import { create } from 'zustand';
import { ipc, onApprovalResolved, onPendingApproval } from '@/lib/ipc';
import type { PendingApproval } from '@/lib/types';

export type ApprovalDecision = 'allow' | 'always_allow' | 'reject';

/** Add `a`, or replace the entry with its action id. Pure. */
export function upsertApproval(list: PendingApproval[], a: PendingApproval): PendingApproval[] {
  const i = list.findIndex((x) => x.action_id === a.action_id);
  if (i === -1) return [...list, a];
  const next = list.slice();
  next[i] = a;
  return next;
}

/** Approvals for a chat not on screen in this window — the ones the
    interrupting dialog shows. `onScreen` is null when no chat is visible
    (Settings is open, say), which makes every approval off-screen. Pure. */
export function offScreenApprovals(
  list: PendingApproval[],
  onScreen: string | null
): PendingApproval[] {
  return list.filter((a) => a.session_id !== onScreen);
}

interface ApprovalState {
  /** Every approval waiting on a person, across all chats (Rust decides
      what can be answered automatically; see `approvals.rs`). */
  pending: PendingApproval[];
  /** Action ids with an answer on its way, so a double click sends one. */
  answering: string[];
  error: string | null;
  /** Subscribe and load what is already waiting. Idempotent. */
  init: () => Promise<void>;
  refresh: () => Promise<void>;
  /** Resolves whether the answer reached the engine; on failure the
      approval stays so it can be answered again. */
  answer: (actionId: string, decision: ApprovalDecision) => Promise<boolean>;
}

let subscribed = false;

export const useApprovalStore = create<ApprovalState>((set, get) => ({
  pending: [],
  answering: [],
  error: null,

  init: async () => {
    if (!subscribed) {
      subscribed = true;
      void onPendingApproval((a) => set((s) => ({ pending: upsertApproval(s.pending, a) })));
      void onApprovalResolved((e) =>
        set((s) => ({
          pending: s.pending.filter((a) => a.action_id !== e.action_id),
          answering: s.answering.filter((id) => id !== e.action_id),
        }))
      );
    }
    await get().refresh();
  },

  refresh: async () => {
    try {
      const list = await ipc.listPendingApprovals();
      set({ pending: list });
    } catch (e) {
      console.warn('listPendingApprovals failed', e);
    }
  },

  answer: async (actionId, decision) => {
    if (get().answering.includes(actionId)) return false;
    set((s) => ({ answering: [...s.answering, actionId], error: null }));
    try {
      await ipc.answerApproval(actionId, decision);
      set((s) => ({
        pending: s.pending.filter((a) => a.action_id !== actionId),
        answering: s.answering.filter((id) => id !== actionId),
      }));
      return true;
    } catch (e) {
      set((s) => ({
        answering: s.answering.filter((id) => id !== actionId),
        error: String(e),
      }));
      return false;
    }
  },
}));
