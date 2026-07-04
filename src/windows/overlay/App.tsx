import { useEffect } from 'react';
import { ipc, onNewSessionRequest } from '@/lib/ipc';
import { useStackStore } from '@/stores/stackStore';
import { useChatStore } from '@/stores/chatStore';
import { StackStatusView } from '@/components/StackStatusView';
import { ChatView } from '@/components/chat/ChatView';
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
    const unlisten = onNewSessionRequest(() => void useChatStore.getState().newSession());
    return () => {
      window.removeEventListener('keydown', onKey);
      void unlisten.then((fn) => fn());
    };
  }, [init]);

  const degraded = DEGRADED.includes(status);

  const expand = async () => {
    const s = useChatStore.getState();
    if (s.sessionId) {
      await ipc.setActiveSession({
        session_id: s.sessionId,
        cwd: s.cwd ?? '',
        current_mode: s.mode ?? 'auto',
        available_modes: s.availableModes,
      });
    }
    await ipc.openMain();
    await ipc.hideOverlay();
  };

  return (
    <div className="overlay-root">
      <div
        className="overlay-card"
        style={{ display: 'flex', flexDirection: 'column', height: '100%' }}
      >
        <div className="overlay-titlebar" data-tauri-drag-region>
          <strong>Goose</strong>
          <div style={{ display: 'flex', gap: 8 }}>
            <button onClick={() => void expand()} title="Expand to full window">
              Expand
            </button>
            <button onClick={() => ipc.openSettings()} title="Open settings">
              Settings
            </button>
          </div>
        </div>
        <div
          className="overlay-body"
          style={{ flex: 1, minHeight: 0, display: 'flex', flexDirection: 'column' }}
        >
          {degraded ? (
            <StackStatusView status={status} />
          ) : (
            <>
              {status === 'conflict_goose_desktop' && <StackStatusView status={status} />}
              <ChatView />
            </>
          )}
        </div>
      </div>
    </div>
  );
}
