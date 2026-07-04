// Renders the machine-readable stack status as UI (CLAUDE.md rule 6: errors are
// states, not toasts). Degraded states show a panel with a "Fix this" button;
// the Goose Desktop conflict shows a non-blocking banner. `ok`/`starting` render
// nothing. Shared by the overlay and full window.

import { useState } from 'react';
import { ipc } from '@/lib/ipc';
import type { StackStatus } from '@/lib/types';

interface Copy {
  title: string;
  body: string;
  severity: 'warn' | 'bad';
  canRestartGoosed?: boolean;
}

const COPY: Partial<Record<StackStatus, Copy>> = {
  ollama_down: {
    title: 'Ollama isn’t responding',
    body: 'The local model server is down. Open settings to start or configure Ollama.',
    severity: 'bad',
  },
  goosed_down: {
    title: 'Goose isn’t running',
    body: 'The Goose agent server stopped. Restart it, or open settings to repair the setup.',
    severity: 'bad',
    canRestartGoosed: true,
  },
  no_model: {
    title: 'No model installed',
    body: 'Ollama has no models yet. Open settings to pull one before you start chatting.',
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

  if (status === 'conflict_goose_desktop') {
    return (
      <div className="conflict-banner" role="status">
        <span className="status-dot warn" />
        <span>
          Stock <strong>Goose Desktop</strong> is running. Two clients sharing the same Goose
          config/sessions can behave unpredictably.
        </span>
      </div>
    );
  }

  const copy = COPY[status];
  if (!copy) return null;

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
        <button className="primary" onClick={() => ipc.openSettings('setup')}>
          Fix this
        </button>
        {copy.canRestartGoosed && (
          <button
            disabled={busy}
            onClick={async () => {
              setBusy(true);
              try {
                await ipc.restartGoosed();
              } finally {
                setBusy(false);
              }
            }}
          >
            {busy ? 'Restarting…' : 'Restart Goose'}
          </button>
        )}
      </div>
    </div>
  );
}
