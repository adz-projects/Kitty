import { useEffect, useState } from 'react';
import { useSessionStore } from '@/stores/sessionStore';
import { useChatStore } from '@/stores/chatStore';
import { onSessionCreated } from '@/lib/ipc';

/** Lightweight last-10 recent-sessions dropdown for the compact overlay. */
export function RecentSessions() {
  const [open, setOpen] = useState(false);
  const refresh = useSessionStore((s) => s.refresh);
  const sessions = useSessionStore((s) => s.sessions);
  const loadSession = useChatStore((s) => s.loadSession);

  useEffect(() => {
    // Round-4 item 6: keep the list fresh even while closed, so a session
    // created in the other window (overlay/main each own an independent
    // store) is there the next time this dropdown opens — and updates live
    // if it's already open.
    const un = onSessionCreated(() => void refresh());
    return () => void un.then((fn) => fn());
  }, [refresh]);

  const toggle = () => {
    const next = !open;
    setOpen(next);
    if (next) void refresh();
  };

  return (
    <div style={{ position: 'relative' }}>
      <button onClick={toggle} title="Recent sessions">
        Recent ▾
      </button>
      {open && (
        <div
          className="mode-popover"
          role="menu"
          style={{ minWidth: 240, maxHeight: 320, overflow: 'auto' }}
        >
          {sessions.length === 0 && (
            <span className="muted" style={{ padding: '6px 8px' }}>
              No sessions
            </span>
          )}
          {sessions.slice(0, 10).map((s) => (
            <button
              key={s.sessionId}
              role="menuitem"
              title={s.cwd}
              onClick={() => {
                void loadSession(s.sessionId, s.cwd, s.title);
                setOpen(false);
              }}
            >
              {s.title}
            </button>
          ))}
        </div>
      )}
    </div>
  );
}
