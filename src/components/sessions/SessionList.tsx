import { useEffect, useRef, useState, type PointerEvent as ReactPointerEvent } from 'react';
import { UNCATEGORIZED, useSessionStore, type SessionGroup } from '@/stores/sessionStore';
import { useChatStore } from '@/stores/chatStore';
import { onSessionCreated, onFoldersChanged } from '@/lib/ipc';
import { SessionKebabMenu } from './SessionKebabMenu';
import type { SessionSummary } from '@/lib/types';

const DRAG_THRESHOLD_PX = 6;

/** Left sidebar in the full window: searchable history from goosed, organized
    into app-side folders (Round-2 item 15). Click a session to resume; use each
    row's kebab menu (Round-3 item 5) to move it or delete it; drag a row onto a
    folder header to reassign it; manage folders from the toolbar.

    Drag-and-drop is implemented with pointer events, not the HTML5 Drag and
    Drop API: Tauri's window-level native drag-drop handler (needed so the
    composer can receive real OS file paths on drop, Phase 4) is enabled by
    default on Windows and — per Tauri's own docs — that disables HTML5 DnD in
    the same webview. Tracking pointer position ourselves sidesteps the native
    handler entirely and works regardless of that setting. */
export function SessionList() {
  const {
    loading,
    query,
    refresh,
    refreshFolders,
    setQuery,
    folders,
    createFolder,
    grouped,
    assignFolder,
  } = useSessionStore();
  const activeId = useChatStore((s) => s.sessionId);

  const [dragId, setDragId] = useState<string | null>(null);
  const [dragOverFolder, setDragOverFolder] = useState<string | null>(null);
  // `dragState`/`dragOverFolderRef` are refs (not reactive) so the listeners
  // below can be attached exactly once on mount — they're set synchronously
  // from pointer events and read fresh inside those same handlers, avoiding
  // both a stale-closure bug and a churn of add/removeEventListener calls.
  const dragState = useRef<{
    sessionId: string;
    startX: number;
    startY: number;
    dragging: boolean;
  } | null>(null);
  const dragOverFolderRef = useRef<string | null>(null);

  useEffect(() => {
    void refresh();
  }, [refresh]);

  useEffect(() => {
    // Round-4 item 6: a session created in the *other* window (overlay/main
    // each own an independent store) doesn't otherwise show up here until a
    // manual refresh.
    const un = onSessionCreated(() => void refresh());
    return () => void un.then((fn) => fn());
  }, [refresh]);

  useEffect(() => {
    // Round-5: a folder created/renamed/deleted or a session reassigned in the
    // *other* window otherwise doesn't reach this sidebar until reload. Only
    // the folder mapping changed, so refresh that (not the whole session list).
    const un = onFoldersChanged(() => void refreshFolders());
    return () => void un.then((fn) => fn());
  }, [refreshFolders]);

  useEffect(() => {
    const setOverFolder = (v: string | null) => {
      dragOverFolderRef.current = v;
      setDragOverFolder(v);
    };

    const onMove = (e: PointerEvent) => {
      const st = dragState.current;
      if (!st) return;
      if (!st.dragging) {
        const dx = e.clientX - st.startX;
        const dy = e.clientY - st.startY;
        if (Math.hypot(dx, dy) < DRAG_THRESHOLD_PX) return;
        st.dragging = true;
        setDragId(st.sessionId);
      }
      const el = document.elementFromPoint(e.clientX, e.clientY);
      const head = el?.closest<HTMLElement>('[data-folder-target]');
      setOverFolder(head?.dataset.folderTarget ?? null);
    };

    const onUp = () => {
      const st = dragState.current;
      dragState.current = null;
      if (st?.dragging && dragOverFolderRef.current != null) {
        const target = dragOverFolderRef.current;
        void assignFolder(st.sessionId, target === '' ? null : target);
      }
      setDragId(null);
      setOverFolder(null);
    };

    window.addEventListener('pointermove', onMove);
    window.addEventListener('pointerup', onUp);
    return () => {
      window.removeEventListener('pointermove', onMove);
      window.removeEventListener('pointerup', onUp);
    };
  }, [assignFolder]);

  const startDrag = (sessionId: string, e: ReactPointerEvent) => {
    dragState.current = { sessionId, startX: e.clientX, startY: e.clientY, dragging: false };
  };

  const groups = grouped();
  const total = groups.reduce((n, g) => n + g.sessions.length, 0);

  return (
    <aside className="session-list">
      <div className="session-search">
        <input
          value={query}
          placeholder="Search chats"
          onChange={(e) => setQuery(e.target.value)}
        />
        <button title="Refresh" onClick={() => void refresh()}>
          ⟳
        </button>
      </div>
      <div className="session-toolbar">
        <span className="muted" style={{ fontSize: 11 }}>
          {total} session{total === 1 ? '' : 's'}
        </span>
        <button
          className="link"
          onClick={() => {
            const name = prompt('New folder name:')?.trim();
            if (name) void createFolder(name);
          }}
        >
          ＋ Folder
        </button>
      </div>

      {loading && total === 0 && <p className="muted session-empty">Loading…</p>}
      {!loading && total === 0 && folders.length === 0 && (
        <p className="muted session-empty">No sessions.</p>
      )}

      {groups.map((g) => (
        <FolderGroup
          key={g.folder}
          group={g}
          folders={folders}
          activeId={activeId}
          dragOverFolder={dragOverFolder}
          dragId={dragId}
          onStartDrag={startDrag}
        />
      ))}
    </aside>
  );
}

function FolderGroup({
  group,
  folders,
  activeId,
  dragOverFolder,
  dragId,
  onStartDrag,
}: {
  group: SessionGroup;
  folders: string[];
  activeId: string | null;
  dragOverFolder: string | null;
  dragId: string | null;
  onStartDrag: (sessionId: string, e: ReactPointerEvent) => void;
}) {
  const { renameFolder, deleteFolder } = useSessionStore();
  const isReal = group.folder !== UNCATEGORIZED;
  const folderTarget = isReal ? group.folder : '';
  // Hide an empty Uncategorized bucket only when real folders exist (keeps the
  // list clean); always show real folders even when empty so they're targetable.
  if (!isReal && group.sessions.length === 0 && folders.length > 0) return null;

  return (
    <details className="folder-group" open>
      <summary
        className={`folder-head${dragOverFolder === folderTarget && dragId ? ' folder-head-dragover' : ''}`}
        data-folder-target={folderTarget}
      >
        <span className="folder-name">
          {isReal ? '📁' : '🗂'} {group.folder}
        </span>
        <span className="folder-count muted">{group.sessions.length}</span>
        {isReal && (
          <span className="folder-actions">
            <button
              title="Rename folder"
              onClick={(e) => {
                e.preventDefault();
                const next = prompt('Rename folder:', group.folder)?.trim();
                if (next && next !== group.folder) void renameFolder(group.folder, next);
              }}
            >
              ✎
            </button>
            <button
              title="Delete folder (sessions move to Uncategorized)"
              onClick={(e) => {
                e.preventDefault();
                if (confirm(`Delete folder "${group.folder}"? Its chats become Uncategorized.`)) {
                  void deleteFolder(group.folder);
                }
              }}
            >
              🗑
            </button>
          </span>
        )}
      </summary>
      {group.sessions.length === 0 && <p className="muted folder-empty">Empty</p>}
      {group.sessions.map((s) => (
        <SessionRow
          key={s.sessionId}
          session={s}
          folders={folders}
          active={s.sessionId === activeId}
          dragging={s.sessionId === dragId}
          onStartDrag={onStartDrag}
        />
      ))}
    </details>
  );
}

function SessionRow({
  session: s,
  folders,
  active,
  dragging,
  onStartDrag,
}: {
  session: SessionSummary;
  folders: string[];
  active: boolean;
  dragging: boolean;
  onStartDrag: (sessionId: string, e: ReactPointerEvent) => void;
}) {
  const { remove, assignments } = useSessionStore();
  const loadSession = useChatStore((st) => st.loadSession);
  const current = assignments[s.sessionId] ?? '';
  // Set once this row's `dragging` prop goes true (past the movement
  // threshold in the parent); suppresses the subsequent click so a completed
  // drag doesn't also resume the session. Reset on every new pointer-down.
  const didDrag = useRef(false);
  useEffect(() => {
    if (dragging) didDrag.current = true;
  }, [dragging]);

  return (
    <div
      className={`session-item${active ? ' active' : ''}${dragging ? ' dragging' : ''}`}
      onClick={() => {
        if (didDrag.current) return;
        void loadSession(s.sessionId, s.cwd, s.title);
      }}
      onPointerDown={(e) => {
        if ((e.target as HTMLElement).closest('.session-kebab, .mode-popover')) return;
        didDrag.current = false;
        onStartDrag(s.sessionId, e);
      }}
      role="button"
      tabIndex={0}
    >
      <div className="session-title">{s.title}</div>
      <div className="session-meta muted">
        {s.cwd.split(/[\\/]/).filter(Boolean).pop() ?? s.cwd}
        {s.modelId ? ` · ${s.modelId}` : ''}
      </div>
      <div className="session-row-actions">
        <SessionKebabMenu
          sessionId={s.sessionId}
          folders={folders}
          current={current}
          onDelete={() => {
            if (confirm(`Delete "${s.title}"? This cannot be undone.`)) {
              void remove(s.sessionId);
            }
          }}
        />
      </div>
    </div>
  );
}
