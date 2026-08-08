import { useEffect, useState } from 'react';
import { useSessionStore } from '@/stores/sessionStore';
import { useChatStore } from '@/stores/chatStore';
import { onSessionCreated, onSessionDeleted } from '@/lib/ipc';
import { usePopoverPosition } from '@/lib/usePopoverPosition';

/** Lightweight last-10 recent-sessions dropdown for the compact overlay. */
export function RecentSessions() {
  const [open, setOpen] = useState(false);
  const [resumingId, setResumingId] = useState<string | null>(null);
  const refresh = useSessionStore((s) => s.refresh);
  const sessions = useSessionStore((s) => s.sessions);
  const loadSession = useChatStore((s) => s.loadSession);
  const { triggerRef, popoverRef, style } = usePopoverPosition(open, () => setOpen(false));

  useEffect(() => {
    // Round-4 item 6: keep the list fresh even while closed, so a session
    // created in the other window (overlay/main each own an independent
    // store) is there the next time this dropdown opens — and updates live
    // if it's already open.
    const un = onSessionCreated(() => void refresh());
    return () => void un.then((fn) => fn());
  }, [refresh]);

  useEffect(() => {
    // A session deleted in the other window (e.g. regenerate()'s background
    // cleanup) otherwise leaves a stale entry here until reopened. Skip the
    // refetch when it's this window's own delete — sessionStore.remove()
    // already filtered it out of local state before this event arrives.
    const un = onSessionDeleted((sessionId) => {
      if (useSessionStore.getState().sessions.some((s) => s.sessionId === sessionId)) {
        void refresh();
      }
    });
    return () => void un.then((fn) => fn());
  }, [refresh]);

  const toggle = () => {
    const next = !open;
    setOpen(next);
    if (next) void refresh();
  };

  return (
    <div style={{ position: 'relative' }}>
      <button
        ref={triggerRef as React.Ref<HTMLButtonElement>}
        onClick={toggle}
        title="Recent sessions"
      >
        Recent ▾
      </button>
      {open && (
        <div
          ref={popoverRef}
          className="mode-popover"
          role="menu"
          style={{ minWidth: 240, ...style }}
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
              // Locked only while THAT row's resume is in flight — otherwise a
              // single long replay (a big session's session/load is the
              // slowest path in the app) disables every row and holds the
              // dropdown open for seconds, blocking navigation.
              disabled={resumingId === s.sessionId}
              onClick={() => {
                setResumingId(s.sessionId);
                void loadSession(s.sessionId, s.cwd, s.title, s.providerId, s.modelId).finally(
                  () => {
                    setResumingId(null);
                    setOpen(false);
                  }
                );
              }}
            >
              {resumingId === s.sessionId ? 'Resuming…' : s.title}
            </button>
          ))}
        </div>
      )}
    </div>
  );
}
