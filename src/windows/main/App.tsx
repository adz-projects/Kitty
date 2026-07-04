import { useEffect } from 'react';
import { ipc } from '@/lib/ipc';
import { useStackStore } from '@/stores/stackStore';
import { StackStatusView } from '@/components/StackStatusView';

/** Full window. Shares chat components with the overlay from Phase 2; for now it
    shows the stack status and a placeholder where the session view will live. */
export function App() {
  const status = useStackStore((s) => s.status);
  const init = useStackStore((s) => s.init);

  useEffect(() => {
    void init();
  }, [init]);

  return (
    <div className="window-root">
      <header style={{ display: 'flex', justifyContent: 'space-between', alignItems: 'center' }}>
        <h1 style={{ fontSize: 20, margin: 0 }}>Goose</h1>
        <button onClick={() => ipc.openSettings()}>Settings</button>
      </header>
      <div style={{ marginTop: 16, display: 'flex', flexDirection: 'column', gap: 16 }}>
        <StackStatusView status={status} />
        <p className="muted">
          Full window shell ready. Session view, history sidebar, and artifacts pane arrive in
          Phases 2–4.
        </p>
      </div>
    </div>
  );
}
