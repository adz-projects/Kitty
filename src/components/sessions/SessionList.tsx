import { useEffect } from 'react';
import { UNCATEGORIZED, useSessionStore, type SessionGroup } from '@/stores/sessionStore';
import { useChatStore } from '@/stores/chatStore';
import type { SessionSummary } from '@/lib/types';

/** Left sidebar in the full window: searchable history from goosed, organized
    into app-side folders (Round-2 item 15). Click a session to resume; use each
    row's folder dropdown to move it; manage folders from the header. */
export function SessionList() {
  const { loading, query, refresh, setQuery, folders, createFolder, grouped } = useSessionStore();
  const activeId = useChatStore((s) => s.sessionId);

  useEffect(() => {
    void refresh();
  }, [refresh]);

  const groups = grouped();
  const total = groups.reduce((n, g) => n + g.sessions.length, 0);

  return (
    <aside className="session-list">
      <div className="session-search">
        <input
          value={query}
          placeholder="Search sessions…"
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
        <FolderGroup key={g.folder} group={g} folders={folders} activeId={activeId} />
      ))}
    </aside>
  );
}

function FolderGroup({
  group,
  folders,
  activeId,
}: {
  group: SessionGroup;
  folders: string[];
  activeId: string | null;
}) {
  const { renameFolder, deleteFolder } = useSessionStore();
  const isReal = group.folder !== UNCATEGORIZED;
  // Hide an empty Uncategorized bucket only when real folders exist (keeps the
  // list clean); always show real folders even when empty so they're targetable.
  if (!isReal && group.sessions.length === 0 && folders.length > 0) return null;

  return (
    <details className="folder-group" open>
      <summary className="folder-head">
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
        />
      ))}
    </details>
  );
}

function SessionRow({
  session: s,
  folders,
  active,
}: {
  session: SessionSummary;
  folders: string[];
  active: boolean;
}) {
  const { remove, assignFolder, assignments } = useSessionStore();
  const loadSession = useChatStore((st) => st.loadSession);
  const current = assignments[s.sessionId] ?? '';

  return (
    <div
      className={`session-item${active ? ' active' : ''}`}
      onClick={() => void loadSession(s.sessionId, s.cwd, s.title)}
      role="button"
      tabIndex={0}
    >
      <div className="session-title">{s.title}</div>
      <div className="session-meta muted">
        {s.cwd.split(/[\\/]/).filter(Boolean).pop() ?? s.cwd}
        {s.modelId ? ` · ${s.modelId}` : ''}
      </div>
      <div className="session-row-actions">
        <select
          className="session-folder-select"
          value={current}
          title="Move to folder"
          onClick={(e) => e.stopPropagation()}
          onChange={(e) => {
            e.stopPropagation();
            void assignFolder(s.sessionId, e.target.value || null);
          }}
        >
          <option value="">Uncategorized</option>
          {folders.map((f) => (
            <option key={f} value={f}>
              {f}
            </option>
          ))}
        </select>
        <button
          className="session-del"
          title="Delete session"
          onClick={(e) => {
            e.stopPropagation();
            if (confirm(`Delete "${s.title}"? This cannot be undone.`)) {
              void remove(s.sessionId);
            }
          }}
        >
          🗑
        </button>
      </div>
    </div>
  );
}
