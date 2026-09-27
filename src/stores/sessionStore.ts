// Session history state (Phase 4), backed by the engine's session routes,
// a page at a time, with full-text search over every chat.
// Chat folders (Round-2 item 15) are an app-side mapping layered on top.
import { create } from 'zustand';
import { ipc } from '@/lib/ipc';
import { parseSession, type SessionSummary } from '@/lib/types';
import { useStackStore, selectBooting } from '@/stores/stackStore';

export const UNCATEGORIZED = 'Uncategorized';

/** Chats fetched per page (#47). */
export const PAGE_SIZE = 100;
/** Queries this long search every chat's text on the engine; shorter ones
    just filter the loaded titles. */
export const SERVER_SEARCH_MIN = 3;

const byNewest = (a: SessionSummary, b: SessionSummary) =>
  a.updatedAt < b.updatedAt ? 1 : a.updatedAt > b.updatedAt ? -1 : 0;

/** `next` merged into `current` by id, newest first. Pure. */
export function mergeSessions(current: SessionSummary[], next: SessionSummary[]): SessionSummary[] {
  const byId = new Map(current.map((s) => [s.sessionId, s]));
  for (const s of next) byId.set(s.sessionId, s);
  return [...byId.values()].sort(byNewest);
}

/** Search hits as list rows, using the loaded row when there is one (it knows
    the chat's folder and card). Pure. */
export function searchRows(
  hits: { sessionId: string; title: string; snippet: string | null }[],
  loaded: SessionSummary[]
): SessionSummary[] {
  return hits.map((h) => {
    const known = loaded.find((s) => s.sessionId === h.sessionId);
    return known
      ? { ...known, snippet: h.snippet }
      : { sessionId: h.sessionId, title: h.title, cwd: '', updatedAt: '', snippet: h.snippet };
  });
}

export interface SessionGroup {
  folder: string; // display name; UNCATEGORIZED for unassigned
  sessions: SessionSummary[];
}

interface SessionState {
  sessions: SessionSummary[];
  /** How many chats exist in all (the list may hold fewer, a page at a time). */
  total: number;
  loadingMore: boolean;
  /** Full-text search hits for the current query, null when not searching. */
  searchResults: SessionSummary[] | null;
  searching: boolean;
  loadMore: () => Promise<void>;
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
  /** Patches one session's title in local state only — for `chat://
      session-title` (BigTiny's own auto-derived title, arriving after the
      first turn), which has no IPC round-trip of its own to await; `rename`
      is the user-initiated equivalent that does. */
  applyTitle: (sessionId: string, title: string) => void;
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
  total: 0,
  loadingMore: false,
  searchResults: null,
  searching: false,
  loading: false,
  loadError: null,
  query: '',
  folders: [],
  assignments: {},

  refresh: async () => {
    set({ loading: true, loadError: null });
    // Set when a booting-retry is scheduled, so `finally` leaves the list in
    // its loading state (the spinner) instead of flipping to "No sessions."
    // between attempts.
    let retryScheduled = false;
    try {
      // As many as are already showing (at least a page), so a refresh after
      // scrolling down doesn't shrink the list back to the first page.
      const limit = Math.max(PAGE_SIZE, get().sessions.length);
      const { sessions: raw, total } = await ipc.listSessions(0, limit);
      // Verbatim string compare of the backend's naive `"YYYY-MM-DD HH:MM:SS"`
      // timestamps, newest first. Must return 0 for equal values — a comparator
      // that only ever returns ±1 gives equal timestamps an arbitrary,
      // engine-dependent order that can shuffle between refreshes.
      const sessions = raw.map(parseSession).sort(byNewest);
      set({ sessions, total });
      await get().refreshFolders();
    } catch (e) {
      // During the startup grace window the daemon simply isn't listening yet
      // (first start on Windows, every start on Android). Surfacing the hard
      // "BigTiny request failed … Retry" error on that first miss is the flash
      // item 2 removes: swallow it, keep the list in its loading state, and
      // retry shortly. The stack→ok subscription in SessionList also re-runs
      // refresh the moment the backend connects.
      if (selectBooting(useStackStore.getState())) {
        retryScheduled = true;
        setTimeout(() => void get().refresh(), 1200);
        return;
      }
      // Previously this escaped the try/finally with no catch, so a caller's
      // `void refresh()` produced an unhandled promise rejection whenever the
      // IPC call failed — capture the error in state instead.
      set({ loadError: e instanceof Error ? e.message : String(e) });
    } finally {
      if (!retryScheduled) set({ loading: false });
    }
  },

  loadMore: async () => {
    const { sessions, total, loadingMore } = get();
    if (loadingMore || sessions.length >= total) return;
    set({ loadingMore: true });
    try {
      const page = await ipc.listSessions(sessions.length, PAGE_SIZE);
      set((s) => ({
        sessions: mergeSessions(s.sessions, page.sessions.map(parseSession)),
        total: page.total,
      }));
    } catch (e) {
      set({ loadError: e instanceof Error ? e.message : String(e) });
    } finally {
      set({ loadingMore: false });
    }
  },

  remove: async (sessionId: string) => {
    const cwd = get().sessions.find((s) => s.sessionId === sessionId)?.cwd;
    // Clear any previous failure first, so a retry that succeeds doesn't leave
    // the old message sitting above a list that's now correct.
    set({ loadError: null });
    try {
      await ipc.deleteSession(sessionId, cwd);
    } catch (e) {
      // Same treatment as refresh: a failed delete must not leave a silent
      // unhandled rejection AND a stale row pretending the session is gone —
      // surface the failure so the sidebar keeps the row and can show why.
      set({ loadError: e instanceof Error ? e.message : String(e) });
      return;
    }
    set((s) => ({
      sessions: s.sessions.filter((x) => x.sessionId !== sessionId),
      total: Math.max(0, s.total - 1),
      searchResults: s.searchResults?.filter((x) => x.sessionId !== sessionId) ?? null,
    }));
    // Drop any dangling folder assignment. Best-effort: a stale cross-window
    // `assignments` map could skip this, but the delete already succeeded so
    // surfacing a folder-cleanup failure is worse than leaving the mapping.
    if (get().assignments[sessionId]) await get().assignFolder(sessionId, null);
  },

  rename: async (sessionId: string, title: string) => {
    const trimmed = title.trim();
    if (!trimmed) return;
    // Same treatment as refresh/remove: callers fire these via `void`, so an
    // uncaught IPC failure is a silent unhandled rejection — surface it in
    // `loadError` (rendered above the list) instead.
    set({ loadError: null });
    try {
      await ipc.renameSession(sessionId, trimmed);
    } catch (e) {
      set({ loadError: e instanceof Error ? e.message : String(e) });
      return;
    }
    set((s) => ({
      sessions: s.sessions.map((x) => (x.sessionId === sessionId ? { ...x, title: trimmed } : x)),
    }));
  },

  applyTitle: (sessionId: string, title: string) => {
    set((s) => ({
      sessions: s.sessions.map((x) => (x.sessionId === sessionId ? { ...x, title } : x)),
      searchResults:
        s.searchResults?.map((x) => (x.sessionId === sessionId ? { ...x, title } : x)) ?? null,
    }));
  },

  setQuery: (q: string) => {
    set({ query: q });
    const trimmed = q.trim();
    if (trimmed.length < SERVER_SEARCH_MIN) {
      set({ searchResults: null, searching: false });
      return;
    }
    set({ searching: true });
    void ipc
      .searchSessions(trimmed)
      .then((hits) => {
        // A later query won the race: drop this one's answer.
        if (get().query.trim() !== trimmed) return;
        set({ searchResults: searchRows(hits, get().sessions) });
      })
      .catch((e) => set({ loadError: e instanceof Error ? e.message : String(e) }))
      .finally(() => {
        if (get().query.trim() === trimmed) set({ searching: false });
      });
  },

  filtered: () => {
    const { sessions, query, searchResults } = get();
    const q = query.trim().toLowerCase();
    if (!q) return sessions;
    // Every chat's text, searched by the engine (#47); until its answer
    // arrives, the loaded titles.
    if (searchResults) return searchResults;
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
    set({ loadError: null });
    try {
      await ipc.createFolder(name);
    } catch (e) {
      set({ loadError: e instanceof Error ? e.message : String(e) });
      return;
    }
    await get().refreshFolders();
  },
  renameFolder: async (oldName: string, newName: string) => {
    set({ loadError: null });
    try {
      await ipc.renameFolder(oldName, newName);
    } catch (e) {
      set({ loadError: e instanceof Error ? e.message : String(e) });
      return;
    }
    await get().refreshFolders();
  },
  deleteFolder: async (name: string) => {
    set({ loadError: null });
    try {
      await ipc.deleteFolder(name);
    } catch (e) {
      set({ loadError: e instanceof Error ? e.message : String(e) });
      return;
    }
    await get().refreshFolders();
  },
  assignFolder: async (sessionId: string, folder: string | null) => {
    set({ loadError: null });
    try {
      await ipc.assignSessionFolder(sessionId, folder);
    } catch (e) {
      set({ loadError: e instanceof Error ? e.message : String(e) });
      return;
    }
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
