import { useEffect } from 'react';
import { ipc } from '@/lib/ipc';
import { useStackStore } from '@/stores/stackStore';
import { StackStatusView } from '@/components/StackStatusView';
import type { StackStatus } from '@/lib/types';

const DEGRADED: StackStatus[] = ['ollama_down', 'goosed_down', 'no_model', 'provider_unreachable'];

export function App() {
  const status = useStackStore((s) => s.status);
  const init = useStackStore((s) => s.init);

  useEffect(() => {
    void init();
    // Escape hides the overlay when it has focus (CLAUDE.md Phase 0).
    const onKey = (e: KeyboardEvent) => {
      if (e.key === 'Escape') void ipc.hideOverlay();
    };
    window.addEventListener('keydown', onKey);
    return () => window.removeEventListener('keydown', onKey);
  }, [init]);

  const degraded = DEGRADED.includes(status);

  return (
    <div className="overlay-root">
      <div className="overlay-card">
        <div className="overlay-titlebar" data-tauri-drag-region>
          <strong>Goose</strong>
          <div style={{ display: 'flex', gap: 8 }}>
            <button
              onClick={async () => {
                await ipc.openMain();
                await ipc.hideOverlay();
              }}
              title="Expand to full window"
            >
              Expand
            </button>
            <button onClick={() => ipc.openSettings()} title="Open settings">
              Settings
            </button>
          </div>
        </div>
        <div className="overlay-body">
          {degraded ? (
            <StackStatusView status={status} />
          ) : (
            <>
              {status === 'conflict_goose_desktop' && <StackStatusView status={status} />}
              <ComposerPlaceholder status={status} />
            </>
          )}
        </div>
      </div>
    </div>
  );
}

/** Phase 2 replaces this with the real composer + streamed message list. */
function ComposerPlaceholder({ status }: { status: StackStatus }) {
  return (
    <div style={{ display: 'flex', flexDirection: 'column', gap: 8 }}>
      <textarea
        placeholder="Ask Goose…  (chat wiring lands in Phase 2)"
        rows={3}
        disabled
        style={{
          width: '100%',
          resize: 'none',
          padding: 10,
          borderRadius: 8,
          border: '1px solid var(--border)',
          background: 'var(--surface-2)',
          color: 'var(--text)',
          font: 'inherit',
        }}
      />
      <span className="muted" style={{ fontSize: 12 }}>
        Overlay shell ready. Stack status: <strong>{status.replace(/_/g, ' ')}</strong>
      </span>
    </div>
  );
}
