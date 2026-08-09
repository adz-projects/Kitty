// Renders the machine-readable stack status as UI (CLAUDE.md rule 6: errors are
// states, not toasts). Degraded states show a panel with a "Fix this" button.
// `ok`/`starting` render nothing. Shared by the overlay and full window.

import { useState } from 'react';
import { ipc } from '@/lib/ipc';
import { useChatStore } from '@/stores/chatStore';
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
  local_model_missing: {
    title: 'No model downloaded',
    body: 'Kitty needs a local model to run. Open settings to download one — it takes a minute.',
    severity: 'bad',
  },
  provider_unreachable: {
    title: 'Provider unreachable',
    body: 'The active model provider can’t be reached. Check its configuration in settings.',
    severity: 'bad',
  },
};

export function StackStatusView({ status }: { status: StackStatus }) {
  const [busy, setBusy] = useState(false);

  if (status === 'ok' || status === 'starting') return null;

  const copy = COPY[status];
  if (!copy) return null;

  // Deep-link "Fix this" to the most relevant settings section.
  const section =
    status === 'provider_unreachable'
      ? 'providers'
      : status === 'local_model_missing'
        ? 'local_models'
        : 'setup';

  return (
    <div className="status-panel" role="alert">
      <h2>
        <span className={`status-dot ${copy.severity}`} />
        {copy.title}
      </h2>
      <p className="muted" style={{ margin: 0 }}>
        {copy.body}
      </p>
      <div className="actions">
        <button className="primary" onClick={() => ipc.openSettings(section)}>
          Fix this
        </button>
        {copy.canRestartBackend && (
          <button
            disabled={busy}
            onClick={async () => {
              setBusy(true);
              try {
                await ipc.restartBackend();
                // Reconnect + rebuild the active session (resume by id).
                await useChatStore.getState().reloadCurrent();
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
