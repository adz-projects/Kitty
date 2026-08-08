// Session history state (Phase 4), backed entirely by goosed's session routes.
// Chat folders (Round-2 item 15) are an app-side mapping layered on top.
import { create } from 'zustand';
import { ipc } from '@/lib/ipc';
import { parseSession, type SessionSummary } from '@/lib/types';

export const UNCATEGORIZED = 'Uncategorized';

export interface SessionGroup {
  folder: string; // display name; UNCATEGORIZED for unassigned
  sessions: SessionSummary[];
}

interface SessionState {
  sessions: SessionSummary[];
  loading: boolean;
  /** Last refresh failure message (WS8) — callers using `void refresh()` can
      no longer hit an unhandled rejection from `ipc.listSessions`, and the
      sidebar can surface why the list is stale instead of silently sitting
      on old data. */
  loadError: string | null;
  query: string;
  folders: string[];
  assignments: Record<string, string>;
  refresh: () => Promise<void>;
  remove: (sessionId: string) => Promise<void>;
  rename: (sessionId: string, title: string) => Promise<void>;
  setQuery: (q: string) => void;
  filtered: () => SessionSummary[];
  // Folders
  refreshFolders: () => Promise<void>;
  createFolder: (name: string) => Promise<void>;
  renameFolder: (oldName: string, newName: string) => Promise<void>;
  deleteFolder: (name: string) => Promise<void>;
  assignFolder: (sessionId: string, folder: string | null) => Promise<void>;
  grouped: () => SessionGroup[];
}

export const useSessionStore = create<SessionState>((set, get) => ({
  sessions: [],
  loading: false,
  loadError: null,
  query: '',
  folders: [],
  assignments: {},

  refresh: async () => {
    set({ loading: true, loadError: null });
    try {
      const raw = await ipc.listSessions();
      // Verbatim string compare of the backend's naive `"YYYY-MM-DD HH:MM:SS"`
      // timestamps, newest first. Must return 0 for equal values — a comparator
      // that only ever returns ±1 gives equal timestamps an arbitrary,
      // engine-dependent order that can shuffle between refreshes.
      const sessions = raw
        .map(parseSession)
        .sort((a, b) => (a.updatedAt < b.updatedAt ? 1 : a.updatedAt > b.updatedAt ? -1 : 0));
      set({ sessions });
      await get().refreshFolders();
    } catch (e) {
      // Previously this escaped the try/finally with no catch, so a caller's
      // `void refresh()` produced an unhandled promise rejection whenever the
      // IPC call failed — capture the error in state instead.
      set({ loadError: e instanceof Error ? e.message : String(e) });
    } finally {
      set({ loading: false });
    }
  },

  remove: async (sessionId: string) => {
    const cwd = get().sessions.find((s) => s.sessionId === sessionId)?.cwd;
    try {
      await ipc.deleteSession(sessionId, cwd);
    } catch (e) {
      // Same treatment as refresh: a failed delete must not leave a silent
      // unhandled rejection AND a stale row pretending the session is gone —
      // surface the failure so the sidebar keeps the row and can show why.
      set({ loadError: e instanceof Error ? e.message : String(e) });
      return;
    }
    set((s) => ({ sessions: s.sessions.filter((x) => x.sessionId !== sessionId) }));
    // Drop any dangling folder assignment. Best-effort: a stale cross-window
    // `assignments` map could skip this, but the delete already succeeded so
    // surfacing a folder-cleanup failure is worse than leaving the mapping.
    if (get().assignments[sessionId]) await get().assignFolder(sessionId, null);
  },

  rename: async (sessionId: string, title: string) => {
    const trimmed = title.trim();
    if (!trimmed) return;
    await ipc.renameSession(sessionId, trimmed);
    set((s) => ({
      sessions: s.sessions.map((x) => (x.sessionId === sessionId ? { ...x, title: trimmed } : x)),
    }));
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

  refreshFolders: async () => {
    try {
      const data = await ipc.listFolders();
      set({ folders: data.folders, assignments: data.assignments });
    } catch {
      /* leave existing folder state */
    }
  },

  createFolder: async (name: string) => {
    await ipc.createFolder(name);
    await get().refreshFolders();
  },
  renameFolder: async (oldName: string, newName: string) => {
    await ipc.renameFolder(oldName, newName);
    await get().refreshFolders();
  },
  deleteFolder: async (name: string) => {
    await ipc.deleteFolder(name);
    await get().refreshFolders();
  },
  assignFolder: async (sessionId: string, folder: string | null) => {
    await ipc.assignSessionFolder(sessionId, folder);
    await get().refreshFolders();
  },

  grouped: () => {
    const sessions = get().filtered();
    const { folders, assignments } = get();
    const byFolder = new Map<string, SessionSummary[]>();
    for (const f of folders) byFolder.set(f, []);
    for (const s of sessions) {
      const f = assignments[s.sessionId];
      const key = f && folders.includes(f) ? f : UNCATEGORIZED;
      if (!byFolder.has(key)) byFolder.set(key, []);
      byFolder.get(key)!.push(s);
    }
    const groups: SessionGroup[] = folders.map((f) => ({
      folder: f,
      sessions: byFolder.get(f) ?? [],
    }));
    groups.push({ folder: UNCATEGORIZED, sessions: byFolder.get(UNCATEGORIZED) ?? [] });
    return groups;
  },
}));
