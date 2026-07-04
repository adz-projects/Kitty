import { useEffect, useState } from 'react';
import { ipc } from '@/lib/ipc';
import { useStackStore } from '@/stores/stackStore';
import { useChatStore } from '@/stores/chatStore';
import { StackStatusView } from '@/components/StackStatusView';
import { ChatView } from '@/components/chat/ChatView';
import { SessionList } from '@/components/sessions/SessionList';
import { ArtifactsPane } from '@/components/artifacts/ArtifactsPane';
import type { StackStatus } from '@/lib/types';

const DEGRADED: StackStatus[] = ['ollama_down', 'goosed_down', 'no_model', 'provider_unreachable'];

/** Full window: history sidebar + shared chat surface + artifacts pane. On open
    it adopts the session handed over from the overlay (Expand). */
export function App() {
  const status = useStackStore((s) => s.status);
  const init = useStackStore((s) => s.init);
  const [showArtifacts, setShowArtifacts] = useState(true);

  useEffect(() => {
    void init();
    void (async () => {
      const info = await ipc.getActiveSession();
      if (info) useChatStore.getState().adoptSession(info);
    })();
  }, [init]);

  const degraded = DEGRADED.includes(status);

  return (
    <div className="main-window">
      <SessionList />
      <div className="main-center">
        <header className="main-header">
          <h1>Goose</h1>
          <div style={{ display: 'flex', gap: 8 }}>
            <button onClick={() => setShowArtifacts((v) => !v)}>
              {showArtifacts ? 'Hide artifacts' : 'Show artifacts'}
            </button>
            <button onClick={() => ipc.openSettings()}>Settings</button>
          </div>
        </header>
        {status === 'conflict_goose_desktop' && <StackStatusView status={status} />}
        <div className="main-body">
          {degraded ? <StackStatusView status={status} /> : <ChatView />}
        </div>
      </div>
      {showArtifacts && !degraded && <ArtifactsPane />}
    </div>
  );
}
