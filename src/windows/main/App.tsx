import { useEffect, useState } from 'react';
import { ipc } from '@/lib/ipc';
import { useStackStore } from '@/stores/stackStore';
import { useAdaptivePathwayStore } from '@/stores/adaptivePathwayStore';
import { useChatStore } from '@/stores/chatStore';
import { StackStatusView } from '@/components/shared/StackStatusView';
import { ChatView } from '@/components/chat/ChatView';
import { SessionList } from '@/components/sessions/SessionList';
import { ArtifactsPane } from '@/components/artifacts/ArtifactsPane';
import { NewChatIcon } from '@/components/icons/NewChatIcon';
import { SettingsGearIcon } from '@/components/icons/SettingsGearIcon';
import { SchismResolutionModal } from '@/components/chat/SchismResolutionModal';
import type { StackStatus } from '@/lib/types';

const DEGRADED: StackStatus[] = ['ollama_down', 'backend_down', 'no_model', 'provider_unreachable'];

/** Full window: history sidebar + shared chat surface + artifacts pane. On open
    it adopts the session handed over from the overlay (Expand). */
export function App() {
  const status = useStackStore((s) => s.status);
  const init = useStackStore((s) => s.init);
  const initAdaptivePathway = useAdaptivePathwayStore((s) => s.init);
  const messages = useChatStore((s) => s.messages);
  const exportSession = useChatStore((s) => s.exportSession);
  const newSession = useChatStore((s) => s.newSession);
  const [showArtifacts, setShowArtifacts] = useState(true);

  useEffect(() => {
    void init();
    void initAdaptivePathway();
    // This window's own one-time handoff, if Expand created it with one
    // (Feature 5: every Expand opens a brand-new window now, so there is no
    // "already open, re-adopt a later handoff" case to also subscribe to —
    // a fresh window only ever needs this single mount-time read).
    void (async () => {
      const info = await ipc.getPendingHandoff();
      if (info?.session_id) await useChatStore.getState().adoptSession(info);
    })();
    // Show/hide-artifacts is persisted (Round-3 item 6).
    void ipc.getConfig().then((c) => setShowArtifacts(c.show_artifacts));
  }, [init, initAdaptivePathway]);

  const toggleArtifacts = async () => {
    const next = !showArtifacts;
    setShowArtifacts(next);
    const cfg = await ipc.getConfig();
    await ipc.setConfig({ ...cfg, show_artifacts: next });
  };

  const degraded = DEGRADED.includes(status);

  return (
    <div className="main-window">
      <SessionList />
      <div className="main-center">
        <header className="main-header">
          <h1>Kitty</h1>
          <div style={{ display: 'flex', gap: 8 }}>
            {messages.length > 0 && (
              <button onClick={() => void exportSession()} title="Export this session as ChatML">
                Export
              </button>
            )}
            <button onClick={() => void toggleArtifacts()}>
              {showArtifacts ? 'Hide artifacts' : 'Show artifacts'}
            </button>
            <button onClick={() => ipc.openSettings()} title="Settings" aria-label="Settings">
              <SettingsGearIcon />
            </button>
            <button onClick={() => void newSession()} title="New chat" aria-label="New chat">
              <NewChatIcon />
            </button>
          </div>
        </header>
        <div className="main-body">
          {degraded ? <StackStatusView status={status} /> : <ChatView />}
        </div>
      </div>
      {showArtifacts && !degraded && <ArtifactsPane />}
      <SchismResolutionModal />
    </div>
  );
}
