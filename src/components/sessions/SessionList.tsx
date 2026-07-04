import { useEffect } from 'react';
import { useSessionStore } from '@/stores/sessionStore';
import { useChatStore } from '@/stores/chatStore';

/** Left sidebar in the full window: searchable history from goosed. Click to
    resume (rebuilds the transcript via session/load replay), delete with
    confirm. */
export function SessionList() {
  const { loading, query, refresh, remove, setQuery, filtered } = useSessionStore();
  const activeId = useChatStore((s) => s.sessionId);
  const loadSession = useChatStore((s) => s.loadSession);

  useEffect(() => {
    void refresh();
  }, [refresh]);

  const sessions = filtered();

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
      <div className="muted" style={{ fontSize: 11, padding: '4px 10px' }}>
        {sessions.length} session{sessions.length === 1 ? '' : 's'}
      </div>
      {loading && sessions.length === 0 && <p className="muted session-empty">Loading…</p>}
      {!loading && sessions.length === 0 && <p className="muted session-empty">No sessions.</p>}
      <div className="session-items">
        {sessions.map((s) => (
          <div
            key={s.sessionId}
            className={`session-item${s.sessionId === activeId ? ' active' : ''}`}
            onClick={() => void loadSession(s.sessionId, s.cwd, s.title)}
            role="button"
            tabIndex={0}
          >
            <div className="session-title">{s.title}</div>
            <div className="session-meta muted">
              {s.cwd.split(/[\\/]/).filter(Boolean).pop() ?? s.cwd}
              {s.modelId ? ` · ${s.modelId}` : ''}
            </div>
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
        ))}
      </div>
    </aside>
  );
}
