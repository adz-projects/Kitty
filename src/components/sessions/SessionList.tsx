import {
  useEffect,
  useMemo,
  useRef,
  useState,
  type PointerEvent as ReactPointerEvent,
} from 'react';
import { UNCATEGORIZED, useSessionStore, type SessionGroup } from '@/stores/sessionStore';
import { useChatStore } from '@/stores/chatStore';
import { useRouteStore } from '@/stores/routeStore';
import {
  ipc,
  pickFolder,
  onSessionCreated,
  onSessionDeleted,
  onFoldersChanged,
  onSessionsCleared,
  onSessionTitle,
} from '@/lib/ipc';
import { isAndroid } from '@/lib/platform';
import { buildExport, sanitizeFilename } from '@/lib/chatml';
import { SessionKebabMenu } from './SessionKebabMenu';
import { SessionSelectionBar } from './SessionSelectionBar';
import type { SessionSummary } from '@/lib/types';
import { FolderIcon } from '@/components/icons/FolderIcon';
import { RefreshIcon } from '@/components/icons/RefreshIcon';
import { PencilIcon } from '@/components/icons/PencilIcon';
import { TrashIcon } from '@/components/icons/TrashIcon';
import { Modal } from '@/components/shared/Modal';

const DRAG_THRESHOLD_PX = 6;
// How long a touch must be held, without moving past DRAG_THRESHOLD_PX,
// before it's treated as "enter bulk-select" rather than "start a drag" or
// "just a tap" (Android only — see `startDrag`).
const LONG_PRESS_MS = 500;

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
  // Narrow slices, not the whole store: a whole-store subscription re-renders
  // this entire (potentially long) sidebar whenever ANY slice changes — e.g.
  // the 150ms-debounced `setQuery` on a keystroke, or a single session being
  // reassigned — even though nothing on screen necessarily moved.
  const loading = useSessionStore((s) => s.loading);
  const loadError = useSessionStore((s) => s.loadError);
  const query = useSessionStore((s) => s.query);
  const folders = useSessionStore((s) => s.folders);
  const refresh = useSessionStore((s) => s.refresh);
  const refreshFolders = useSessionStore((s) => s.refreshFolders);
  const setQuery = useSessionStore((s) => s.setQuery);
  const createFolder = useSessionStore((s) => s.createFolder);
  const grouped = useSessionStore((s) => s.grouped);
  const assignFolder = useSessionStore((s) => s.assignFolder);
  const applyTitle = useSessionStore((s) => s.applyTitle);
  // `grouped` is a stable function reference, so subscribing to it alone
  // never re-renders this list — `grouped()` *reads* `sessions`/`assignments`
  // out of the store, and these two subscriptions are what make a rename,
  // delete, or cross-window folder move actually show up (a rename used to
  // leave the old title on screen; a delete could ghost its row).
  const sessions = useSessionStore((s) => s.sessions);
  const assignments = useSessionStore((s) => s.assignments);
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
    // Android only: the long-press timer fired for this touch. Doesn't commit
    // to anything by itself — see `onUp` — so a long-press-then-drag still
    // reassigns the folder same as a plain drag would, instead of the long
    // press always winning and blocking the drag from ever starting.
    longPressFired: boolean;
  } | null>(null);
  const dragOverFolderRef = useRef<string | null>(null);

  // Bulk selection (Android only — release-fixes items 8-10): long-press a
  // row to enter, tap toggles while active. Lives here rather than per-row
  // so the bottom action bar can see the whole set.
  const [selectionMode, setSelectionMode] = useState(false);
  const [selectedIds, setSelectedIds] = useState<Set<string>>(new Set());
  const [selectionBusy, setSelectionBusy] = useState(false);
  const [selectionError, setSelectionError] = useState<string | null>(null);
  const longPressTimer = useRef<ReturnType<typeof setTimeout> | null>(null);

  const cancelLongPress = () => {
    if (longPressTimer.current) {
      clearTimeout(longPressTimer.current);
      longPressTimer.current = null;
    }
  };

  const toggleSelected = (sessionId: string) => {
    setSelectedIds((prev) => {
      const next = new Set(prev);
      if (next.has(sessionId)) next.delete(sessionId);
      else next.add(sessionId);
      return next;
    });
  };

  const exitSelectionMode = () => {
    setSelectionMode(false);
    setSelectedIds(new Set());
    setSelectionError(null);
  };

  const deleteSelected = async () => {
    setSelectionBusy(true);
    setSelectionError(null);
    try {
      const remove = useSessionStore.getState().remove;
      for (const id of selectedIds) await remove(id);
      // `remove` surfaces its own failures via `loadError` on the store but
      // doesn't throw — a partial failure still leaves this loop's remaining
      // ids attempted, matching per-row delete's own best-effort shape.
      exitSelectionMode();
    } catch (e) {
      setSelectionError(String(e));
    } finally {
      setSelectionBusy(false);
    }
  };

  /** Bulk "Export from here"-equivalent for the sidebar: reuses
      `chatStore.loadSession` (the same fetch a normal resume uses — there is
      no lighter-weight "just give me the messages" endpoint, since BigTiny's
      session load replays the whole conversation as a stream of Tauri
      events, not a single return value) to pull each selected session's
      messages one at a time, exports each through the same
      `buildExport`/`sanitizeFilename` "Export from here" already uses
      per-message, and writes one `.jsonl` per session into a single chosen
      folder. Whatever session this window had open before the export
      started is reloaded afterward so a bulk export from the Saved Chats
      tab doesn't leave the Chat tab silently pointed at the last-exported
      session instead of what the user actually had open. */
  const exportSelected = async () => {
    const dir = await pickFolder();
    if (!dir) return;
    setSelectionBusy(true);
    setSelectionError(null);
    const chat = useChatStore.getState();
    const restore =
      chat.sessionId != null
        ? {
            sessionId: chat.sessionId,
            cwd: chat.cwd ?? '',
            title: chat.title ?? undefined,
            providerId: chat.sessionProviderId ?? undefined,
            modelId: chat.sessionModelId ?? undefined,
          }
        : null;
    const usedNames = new Set<string>();
    try {
      for (const id of selectedIds) {
        const summary = sessions.find((s) => s.sessionId === id);
        if (!summary) continue;
        await useChatStore
          .getState()
          .loadSession(summary.sessionId, summary.cwd, summary.title, summary.providerId, summary.modelId);
        const { messages, title } = useChatStore.getState();
        const chatMessages = buildExport(messages);
        let base = sanitizeFilename(title ?? summary.title);
        if (usedNames.has(base)) base = `${base}-${id.slice(0, 8)}`;
        usedNames.add(base);
        await ipc.writeFile(`${dir}/${base}.jsonl`, JSON.stringify({ messages: chatMessages }) + '\n');
      }
      exitSelectionMode();
    } catch (e) {
      setSelectionError(String(e));
    } finally {
      if (restore) {
        await useChatStore
          .getState()
          .loadSession(restore.sessionId, restore.cwd, restore.title, restore.providerId, restore.modelId);
      }
      setSelectionBusy(false);
    }
  };

  useEffect(() => () => cancelLongPress(), []);

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
    // A session deleted in the *other* window (e.g. regenerate()'s background
    // cleanup of the session it forked away from) otherwise leaves a stale
    // entry here until a manual refresh. Skip the refetch when it's this
    // window's own delete — sessionStore.remove() already filtered it out of
    // local state before this event ever arrives, so a full refresh here
    // would just be a redundant round-trip.
    const un = onSessionDeleted((sessionId) => {
      if (useSessionStore.getState().sessions.some((s) => s.sessionId === sessionId)) {
        void refresh();
      }
    });
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
    // "Clear all chat history" (Settings → General) run from any window —
    // this sidebar must empty without a manual refresh.
    const un = onSessionsCleared(() => void refresh());
    return () => void un.then((fn) => fn());
  }, [refresh]);

  useEffect(() => {
    // release-fixes item 12: BigTiny auto-derives a title after the first
    // full turn and emits chat://session-title, which chatStore already
    // consumed for the active window's own header — this sidebar had no
    // listener at all, so the row kept showing "New Chat" until something
    // else (a manual reload) triggered a full refresh.
    const un = onSessionTitle((e) => applyTitle(e.session_id, e.title));
    return () => void un.then((fn) => fn());
  }, [applyTitle]);

  useEffect(() => {
    const setOverFolder = (v: string | null) => {
      dragOverFolderRef.current = v;
      setDragOverFolder(v);
    };

    // `pointermove` fires 60+×/sec during a drag; `elementFromPoint` forces a
    // hit-test and each call re-renders via `setDragOverFolder`. Coalescing
    // into a single rAF caps that to once per frame instead of once per
    // event, so a long session list no longer rubber-bands while dragging.
    let raf: number | null = null;
    let pending: { x: number; y: number } | null = null;

    const processMove = () => {
      raf = null;
      const pos = pending;
      pending = null;
      if (!pos) return;
      const st = dragState.current;
      if (!st) return;
      if (!st.dragging) {
        const dx = pos.x - st.startX;
        const dy = pos.y - st.startY;
        if (Math.hypot(dx, dy) < DRAG_THRESHOLD_PX) return;
        // Real movement — this is a drag, not a long-press. Whichever
        // pending long-press timer was armed for this pointer-down no longer
        // applies.
        cancelLongPress();
        st.dragging = true;
        setDragId(st.sessionId);
      }
      const el = document.elementFromPoint(pos.x, pos.y);
      const head = el?.closest<HTMLElement>('[data-folder-target]');
      setOverFolder(head?.dataset.folderTarget ?? null);
    };

    const onMove = (e: PointerEvent) => {
      if (!dragState.current) return;
      pending = { x: e.clientX, y: e.clientY };
      if (raf == null) raf = requestAnimationFrame(processMove);
    };

    const onUp = () => {
      if (raf != null) {
        cancelAnimationFrame(raf);
        raf = null;
      }
      // A release before the long-press timer fires is just a tap (or the
      // start of a real drag, already cancelled above) — either way, the
      // pending long-press callback must not fire late.
      cancelLongPress();
      const st = dragState.current;
      dragState.current = null;
      if (st?.dragging && dragOverFolderRef.current != null) {
        const target = dragOverFolderRef.current;
        void assignFolder(st.sessionId, target === '' ? null : target);
      } else if (st?.longPressFired && !st.dragging) {
        // Long press fired and the finger lifted without ever moving past
        // the drag threshold — that's bulk-select's entry gesture. If it
        // HAD moved, the branch above already ran instead (see `startDrag`'s
        // comment: the threshold check in `processMove` doesn't care why a
        // long press did or didn't already fire).
        setSelectionMode(true);
        setSelectedIds(new Set([st.sessionId]));
      }
      setDragId(null);
      setOverFolder(null);
    };

    window.addEventListener('pointermove', onMove);
    window.addEventListener('pointerup', onUp);
    return () => {
      if (raf != null) cancelAnimationFrame(raf);
      window.removeEventListener('pointermove', onMove);
      window.removeEventListener('pointerup', onUp);
    };
  }, [assignFolder]);

  const startDrag = (sessionId: string, e: ReactPointerEvent) => {
    dragState.current = {
      sessionId,
      startX: e.clientX,
      startY: e.clientY,
      dragging: false,
      longPressFired: false,
    };
    // Long-press-to-select (Android only): armed alongside the existing drag
    // detection, not instead of it. Firing only *marks* the touch
    // (`longPressFired`) rather than committing to selection mode outright —
    // `processMove`'s threshold check (unchanged, still measured from the
    // original touch-down point) keeps running underneath it, so a
    // long-press-then-drag still starts a real drag exactly like an
    // immediate one would; only a long-press followed by lifting the finger
    // with no drag becomes "enter bulk-select" (decided in `onUp`). This is
    // also just how touch dragging has to work in practice: holding still
    // is what tells a touchscreen apart from a scroll gesture, so long-press
    // is the natural "pick this row up" gesture, not a rival to dragging.
    // Not armed while already in selection mode — every tap there should
    // toggle immediately, not require another long-press.
    if (isAndroid() && !selectionMode) {
      cancelLongPress();
      longPressTimer.current = setTimeout(() => {
        longPressTimer.current = null;
        if (dragState.current?.sessionId === sessionId && !dragState.current.dragging) {
          dragState.current.longPressFired = true;
        }
      }, LONG_PRESS_MS);
    }
  };

  const groups = useMemo(
    () => grouped(),
    // `grouped()` derives from all four of these store slices (via `filtered`
    // + the folder map) — recompute only when one of them actually changes.
    // The linter calls them unnecessary because it can't see through the
    // stable `grouped` function reference; they are the entire point.
    // eslint-disable-next-line react-hooks/exhaustive-deps
    [grouped, sessions, assignments, folders, query]
  );
  const total = groups.reduce((n, g) => n + g.sessions.length, 0);

  const [creatingFolder, setCreatingFolder] = useState(false);
  const [newFolderName, setNewFolderName] = useState('');

  // The input's own value updates instantly (so typing feels responsive);
  // the store's `query` — which drives the actual re-filter/re-render of
  // the whole (potentially long) session list — only follows 150ms after
  // the user stops typing.
  const [searchInput, setSearchInput] = useState(query);
  useEffect(() => setSearchInput(query), [query]);
  const debounceRef = useRef<ReturnType<typeof setTimeout> | null>(null);
  const onSearchChange = (value: string) => {
    setSearchInput(value);
    if (debounceRef.current) clearTimeout(debounceRef.current);
    debounceRef.current = setTimeout(() => setQuery(value), 150);
  };
  useEffect(
    () => () => {
      if (debounceRef.current) clearTimeout(debounceRef.current);
    },
    []
  );

  const submitNewFolder = () => {
    const name = newFolderName.trim();
    if (name) void createFolder(name);
    setCreatingFolder(false);
  };

  return (
    <aside className="session-list">
      <div className="session-search">
        <input
          value={searchInput}
          placeholder="Search chats"
          onChange={(e) => onSearchChange(e.target.value)}
        />
        <button title="Refresh" onClick={() => void refresh()}>
          <RefreshIcon />
        </button>
      </div>
      <div className="session-toolbar">
        <span className="muted" style={{ fontSize: 13 }}>
          {total} session{total === 1 ? '' : 's'}
        </span>
        <button
          className="link"
          onClick={() => {
            setNewFolderName('');
            setCreatingFolder(true);
          }}
        >
          ＋ Folder
        </button>
      </div>

      {/* `loadError` covers both a failed refresh and a failed delete. It was
          being set by the store and rendered by nobody, which is what made a
          delete that the backend rejected look like it had silently done
          nothing — the row simply stayed put with no explanation. */}
      {loadError && (
        <p className="session-empty error" role="alert">
          {loadError}{' '}
          <button className="link" onClick={() => void refresh()}>
            Retry
          </button>
        </p>
      )}

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
          selectionMode={selectionMode}
          selectedIds={selectedIds}
          onToggleSelected={toggleSelected}
        />
      ))}

      {selectionMode && (
        <SessionSelectionBar
          count={selectedIds.size}
          busy={selectionBusy}
          error={selectionError}
          onDelete={() => void deleteSelected()}
          onExport={() => void exportSelected()}
          onCancel={exitSelectionMode}
        />
      )}

      {creatingFolder && (
        <Modal title="New folder" onClose={() => setCreatingFolder(false)}>
          <label className="field">
            <span>Folder name</span>
            <input
              autoFocus
              value={newFolderName}
              onChange={(e) => setNewFolderName(e.target.value)}
              onKeyDown={(e) => {
                if (e.key === 'Enter') {
                  e.preventDefault();
                  submitNewFolder();
                }
              }}
            />
          </label>
          <div className="row">
            <button className="primary" onClick={submitNewFolder}>
              Create
            </button>
            <button onClick={() => setCreatingFolder(false)}>Cancel</button>
          </div>
        </Modal>
      )}
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
  selectionMode,
  selectedIds,
  onToggleSelected,
}: {
  group: SessionGroup;
  folders: string[];
  activeId: string | null;
  dragOverFolder: string | null;
  dragId: string | null;
  onStartDrag: (sessionId: string, e: ReactPointerEvent) => void;
  selectionMode: boolean;
  selectedIds: Set<string>;
  onToggleSelected: (sessionId: string) => void;
}) {
  const renameFolder = useSessionStore((s) => s.renameFolder);
  const deleteFolder = useSessionStore((s) => s.deleteFolder);
  const isReal = group.folder !== UNCATEGORIZED;
  const folderTarget = isReal ? group.folder : '';
  // Defaults open, matching the previous always-open behavior.
  const [open, setOpen] = useState(true);
  const [renaming, setRenaming] = useState(false);
  const [renameValue, setRenameValue] = useState(group.folder);
  const [confirmingDelete, setConfirmingDelete] = useState(false);

  const submitRename = () => {
    const next = renameValue.trim();
    if (next && next !== group.folder) void renameFolder(group.folder, next);
    setRenaming(false);
  };

  // Hide an empty Uncategorized bucket only when real folders exist (keeps the
  // list clean); always show real folders even when empty so they're targetable.
  if (!isReal && group.sessions.length === 0 && folders.length > 0) return null;

  return (
    <div className="folder-group">
      <div
        className={`folder-head${dragOverFolder === folderTarget && dragId ? ' folder-head-dragover' : ''}`}
        data-folder-target={folderTarget}
        role="button"
        tabIndex={0}
        onClick={() => setOpen((o) => !o)}
        onKeyDown={(e) => {
          if (e.key === 'Enter' || e.key === ' ') {
            e.preventDefault();
            setOpen((o) => !o);
          }
        }}
      >
        <span className="folder-chevron">{open ? '▾' : '▸'}</span>
        <span className="folder-name">
          <FolderIcon variant={isReal ? 'folder' : 'tray'} /> {group.folder}
        </span>
        <span className="folder-count muted">{group.sessions.length}</span>
        {isReal && (
          <span className="folder-actions">
            <button
              title="Rename folder"
              onClick={(e) => {
                e.stopPropagation();
                setRenameValue(group.folder);
                setRenaming(true);
              }}
            >
              <PencilIcon />
            </button>
            <button
              title="Delete folder (sessions move to Uncategorized)"
              onClick={(e) => {
                e.stopPropagation();
                setConfirmingDelete(true);
              }}
            >
              <TrashIcon />
            </button>
          </span>
        )}
      </div>
      {/* Explicit conditional render, not native <details> collapse — this
          WebView2/Chromium build doesn't actually hide non-open <details>
          content even when `open` is false (confirmed live via CDP), so
          visibility can't be left to native collapse + CSS. Same finding as
          Providers.tsx/AdaptivePathway.tsx/Advanced.tsx. */}
      {open && (
        <>
          {group.sessions.length === 0 && <p className="muted folder-empty">Empty</p>}
          {group.sessions.map((s) => (
            <SessionRow
              key={s.sessionId}
              session={s}
              folders={folders}
              active={s.sessionId === activeId}
              dragging={s.sessionId === dragId}
              onStartDrag={onStartDrag}
              selectionMode={selectionMode}
              selected={selectedIds.has(s.sessionId)}
              onToggleSelected={onToggleSelected}
            />
          ))}
        </>
      )}

      {renaming && (
        <Modal title="Rename folder" onClose={() => setRenaming(false)}>
          <label className="field">
            <span>Folder name</span>
            <input
              autoFocus
              value={renameValue}
              onChange={(e) => setRenameValue(e.target.value)}
              onKeyDown={(e) => {
                if (e.key === 'Enter') {
                  e.preventDefault();
                  submitRename();
                }
              }}
            />
          </label>
          <div className="row">
            <button className="primary" onClick={submitRename}>
              Rename
            </button>
            <button onClick={() => setRenaming(false)}>Cancel</button>
          </div>
        </Modal>
      )}
      {confirmingDelete && (
        <Modal title="Delete this folder?" onClose={() => setConfirmingDelete(false)}>
          <p>
            Delete folder &quot;{group.folder}&quot;? Its chats become {UNCATEGORIZED}.
          </p>
          <div className="row">
            <button
              className="primary"
              onClick={() => {
                void deleteFolder(group.folder);
                setConfirmingDelete(false);
              }}
            >
              Delete
            </button>
            <button onClick={() => setConfirmingDelete(false)}>Cancel</button>
          </div>
        </Modal>
      )}
    </div>
  );
}

function SessionRow({
  session: s,
  folders,
  active,
  dragging,
  onStartDrag,
  selectionMode,
  selected,
  onToggleSelected,
}: {
  session: SessionSummary;
  folders: string[];
  active: boolean;
  dragging: boolean;
  onStartDrag: (sessionId: string, e: ReactPointerEvent) => void;
  selectionMode: boolean;
  selected: boolean;
  onToggleSelected: (sessionId: string) => void;
}) {
  const remove = useSessionStore((s) => s.remove);
  const rename = useSessionStore((s) => s.rename);
  // Only this row's assignment — subscribing to the whole `assignments` map
  // (or the whole store) re-renders every row when any other session moves.
  const current = useSessionStore((state) => state.assignments[s.sessionId] ?? '');
  const loadSession = useChatStore((st) => st.loadSession);
  const goto = useRouteStore((st) => st.goto);
  const [confirmingDelete, setConfirmingDelete] = useState(false);
  const [renaming, setRenaming] = useState(false);
  const [renameValue, setRenameValue] = useState(s.title);

  const submitRename = () => {
    const next = renameValue.trim();
    if (next && next !== s.title) {
      void rename(s.sessionId, next);
      // Keep the active session's own title in sync (chat header, export
      // default filename, etc.) — sessionStore only owns the list row.
      if (active) useChatStore.setState({ title: next });
    }
    setRenaming(false);
  };
  // Resuming replays the whole conversation (session/load) before this
  // resolves — without feedback, clicking a long-history chat looked
  // unresponsive for however long the replay took.
  const [resuming, setResuming] = useState(false);
  // Set once this row's `dragging` prop goes true (past the movement
  // threshold in the parent); suppresses the subsequent click so a completed
  // drag doesn't also resume the session. Reset on every new pointer-down.
  const didDrag = useRef(false);
  useEffect(() => {
    if (dragging) didDrag.current = true;
  }, [dragging]);

  return (
    <div
      className={`session-item${active ? ' active' : ''}${dragging ? ' dragging' : ''}${resuming ? ' resuming' : ''}${selectionMode ? ' selectable' : ''}${selected ? ' selected' : ''}`}
      onClick={() => {
        if (didDrag.current) return;
        // In bulk-select mode every tap toggles rather than opening the chat
        // — that's what the long-press that got here in the first place is
        // for.
        if (selectionMode) {
          onToggleSelected(s.sessionId);
          return;
        }
        if (resuming) return;
        setResuming(true);
        // Route to the chat view as well as loading it. On desktop this list
        // sits beside the conversation and the route is already 'chat', so
        // this is a no-op; on Android the list *is* its own tab ("Saved
        // Chats"), and loading a session without switching tabs left the user
        // staring at the list wondering whether the tap registered.
        goto('chat');
        void loadSession(s.sessionId, s.cwd, s.title, s.providerId, s.modelId).finally(() =>
          setResuming(false)
        );
      }}
      onPointerDown={(e) => {
        if ((e.target as HTMLElement).closest('.session-kebab, .mode-popover')) return;
        didDrag.current = false;
        onStartDrag(s.sessionId, e);
      }}
      role="button"
      tabIndex={0}
    >
      {selectionMode && (
        <input
          type="checkbox"
          className="session-select-check"
          checked={selected}
          readOnly
          // The row's own onClick already toggles — this exists to show
          // selection state visually, not as a second independent control
          // (a direct click on it would otherwise double-toggle via bubbling).
          onClick={(e) => e.stopPropagation()}
        />
      )}
      <div className="session-title">{s.title}</div>
      <div className="session-meta muted">
        {resuming ? 'Resuming…' : (s.cwd.split(/[\\/]/).filter(Boolean).pop() ?? s.cwd)}
        {!resuming && s.modelId ? ` · ${s.modelId}` : ''}
      </div>
      {/* Kebab menu (rename, move to folder) is desktop-only on Android
          (release-fixes item 9) — long-press bulk-select + the bottom action
          bar is the only way to delete a chat there now, and rename/move
          were dropped rather than finding them a new home. */}
      {!isAndroid() && (
        <div className="session-row-actions">
          <SessionKebabMenu
            sessionId={s.sessionId}
            folders={folders}
            current={current}
            onRename={() => {
              setRenameValue(s.title);
              setRenaming(true);
            }}
            onDelete={() => setConfirmingDelete(true)}
          />
        </div>
      )}
      {renaming && (
        // Nested inside this row's own onClick (which resumes the session) —
        // without stopping propagation here, clicking anything in this modal
        // bubbles up and also fires that resume, racing with the rename.
        <div onClick={(e) => e.stopPropagation()}>
          <Modal title="Rename chat" onClose={() => setRenaming(false)}>
            <label className="field">
              <span>Chat name</span>
              <input
                autoFocus
                value={renameValue}
                onChange={(e) => setRenameValue(e.target.value)}
                onKeyDown={(e) => {
                  if (e.key === 'Enter') {
                    e.preventDefault();
                    submitRename();
                  }
                }}
              />
            </label>
            <div className="row">
              <button className="primary" onClick={submitRename}>
                Rename
              </button>
              <button onClick={() => setRenaming(false)}>Cancel</button>
            </div>
          </Modal>
        </div>
      )}
      {confirmingDelete && (
        // Same bubbling hazard as the rename modal above — confirmed real bug:
        // clicking "Delete" here also bubbled into the row's onClick, which
        // called loadSession on the session being deleted, racing the delete
        // and landing on a blank/errored session instead of leaving whatever
        // session the user was actually viewing untouched.
        <div onClick={(e) => e.stopPropagation()}>
          <Modal title="Delete this chat?" onClose={() => setConfirmingDelete(false)}>
            <p>Delete &quot;{s.title}&quot;? This cannot be undone.</p>
            <div className="row">
              <button
                className="primary"
                onClick={() => {
                  void remove(s.sessionId);
                  setConfirmingDelete(false);
                }}
              >
                Delete
              </button>
              <button onClick={() => setConfirmingDelete(false)}>Cancel</button>
            </div>
          </Modal>
        </div>
      )}
    </div>
  );
}
