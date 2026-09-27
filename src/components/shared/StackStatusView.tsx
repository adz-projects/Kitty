// Renders the machine-readable stack status as UI (CLAUDE.md rule 6: errors are
// states, not toasts). Degraded states show a panel with a "Fix this" button.
// `ok`/`starting` render nothing. Shared by the overlay and full window.

import { useState } from 'react';
import { ipc } from '@/lib/ipc';
import { useChatStore } from '@/stores/chatStore';
import { useStackStore } from '@/stores/stackStore';
import type { StackStatus } from '@/lib/types';

interface Copy {
  title: string;
  body: string;
  severity: 'warn' | 'bad';
  canRestartBackend?: boolean;
}

const COPY: Partial<Record<StackStatus, Copy>> = {
  backend_down: {
    title: 'Kitty’s engine isn’t running',
    body: 'The chat engine stopped. Restart it, or open settings to repair the setup.',
    severity: 'bad',
    canRestartBackend: true,
  },
};

export function StackStatusView({ status }: { status: StackStatus }) {
  const [busy, setBusy] = useState(false);
  const [restartError, setRestartError] = useState<string | null>(null);
  const detail = useStackStore((s) => s.detail);

  if (status === 'ok' || status === 'starting') return null;

  const copy = COPY[status];
  if (!copy) return null;

  // Setup & Repair merged into Advanced (release-fixes item 21).
  const section = 'advanced';

  return (
    <div className="status-panel" role="alert">
      <h2>
        <span className={`status-dot ${copy.severity}`} />
        {copy.title}
      </h2>
      <p className="muted" style={{ margin: 0 }}>
        {copy.body}
      </p>
      {detail && (
        <p className="muted" style={{ margin: 0 }}>
          {detail}
        </p>
      )}
      {restartError && (
        <p className="error" style={{ margin: 0 }} role="alert">
          Restart failed: {restartError}
        </p>
      )}
      <div className="actions">
        <button className="primary" onClick={() => ipc.openSettings(section)}>
          Fix this
        </button>
        {copy.canRestartBackend && (
          <button
            disabled={busy}
            onClick={async () => {
              setBusy(true);
              setRestartError(null);
              try {
                const outcome = await ipc.restartBackend();
                if (!outcome.restarted) {
                  const who = outcome.blocked_by.map((b) => b.display_name).join(', ');
                  setRestartError(`the engine is in use by ${who}`);
                  return;
                }
                // Reconnect + rebuild the active session (resume by id).
                await useChatStore.getState().reloadCurrent();
              } catch (e) {
                // Previously escaped the onClick uncaught — the button just
                // re-enabled with no sign anything went wrong.
                setRestartError(String(e));
              } finally {
                setBusy(false);
              }
            }}
          >
            {busy ? 'Restarting…' : 'Restart Kitty engine'}
          </button>
        )}
      </div>
    </div>
  );
}
