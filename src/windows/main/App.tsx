import { useEffect } from 'react';
import { ipc } from '@/lib/ipc';
import { useStackStore } from '@/stores/stackStore';
import { useChatStore } from '@/stores/chatStore';
import { StackStatusView } from '@/components/StackStatusView';
import { ChatView } from '@/components/chat/ChatView';
import type { StackStatus } from '@/lib/types';

const DEGRADED: StackStatus[] = ['ollama_down', 'goosed_down', 'no_model', 'provider_unreachable'];

/** Full window. Shares the chat surface with the overlay; on open it adopts the
    session handed over from the overlay (Expand) so the same conversation
    continues. Transcript replay on resume is Phase 4. */
export function App() {
  const status = useStackStore((s) => s.status);
  const init = useStackStore((s) => s.init);

  useEffect(() => {
    void init();
    void (async () => {
      const info = await ipc.getActiveSession();
      if (info) useChatStore.getState().adoptSession(info);
    })();
  }, [init]);

  const degraded = DEGRADED.includes(status);

  return (
    <div
      className="window-root"
      style={{ display: 'flex', flexDirection: 'column', padding: 16, gap: 8 }}
    >
      <header style={{ display: 'flex', justifyContent: 'space-between', alignItems: 'center' }}>
        <h1 style={{ fontSize: 18, margin: 0 }}>Goose</h1>
        <button onClick={() => ipc.openSettings()}>Settings</button>
      </header>
      {status === 'conflict_goose_desktop' && <StackStatusView status={status} />}
      <div style={{ flex: 1, minHeight: 0 }}>
        {degraded ? <StackStatusView status={status} /> : <ChatView />}
      </div>
    </div>
  );
}
