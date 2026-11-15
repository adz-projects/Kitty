import { useEffect, useRef, useState } from 'react';
import { ipc, onNewSessionRequest } from '@/lib/ipc';
import { useStackStore, selectBooting } from '@/stores/stackStore';
import { useAdaptivePathwayStore } from '@/stores/adaptivePathwayStore';
import { useChatStore } from '@/stores/chatStore';
import { StackStatusView } from '@/components/shared/StackStatusView';
import { StartupSpinner } from '@/components/shared/StartupSpinner';
import { ConfirmHost } from '@/components/shared/ConfirmDialog';
import { ApprovalModal } from '@/components/chat/ApprovalModal';
import { ChatView } from '@/components/chat/ChatView';
import { RecentSessions } from '@/components/sessions/RecentSessions';
import { NewChatIcon } from '@/components/icons/NewChatIcon';
import { DoubleChevronIcon } from '@/components/icons/DoubleChevronIcon';
import { SettingsGearIcon } from '@/components/icons/SettingsGearIcon';
import type { StackStatus } from '@/lib/types';

const DEGRADED: StackStatus[] = ['backend_down'];

export function App() {
  const status = useStackStore((s) => s.status);
  const init = useStackStore((s) => s.init);
  const initAdaptivePathway = useAdaptivePathwayStore((s) => s.init);

  useEffect(() => {
    void init();
    void initAdaptivePathway();
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
  }, [init, initAdaptivePathway]);

  const sessionId = useChatStore((s) => s.sessionId);
  // The title bar's left-hand slot: the chat's header row (folder pill, model,
  // effort, ⋯) is drawn there rather than as a row of its own.
  const [headerSlot, setHeaderSlot] = useState<HTMLDivElement | null>(null);
  const booting = useStackStore(selectBooting);
  const degraded = DEGRADED.includes(status);
  // A first-connect `backend_down` during the grace window is a slow daemon
  // bind, not a failure — show a spinner instead of the hard panel (item 2).
  const bootSpinner = status === 'backend_down' && booting;

  // Latched: handOffToMain opens a brand-new chat window every time — a
  // double-click (or click while the first open is still in flight) must not
  // spawn two windows both adopting the same session.
  const expandInFlight = useRef(false);
  const expand = () => {
    if (expandInFlight.current) return;
    expandInFlight.current = true;
    void useChatStore
      .getState()
      .handOffToMain()
      .finally(() => {
        expandInFlight.current = false;
      });
  };

  return (
    <div className="overlay-root">
      <div
        className="overlay-card"
        style={{ display: 'flex', flexDirection: 'column', height: '100%' }}
      >
        <div className="overlay-titlebar" data-tauri-drag-region>
          <div className="overlay-header-slot" ref={setHeaderSlot} />
          <div className="overlay-titlebar-buttons">
            {/* Left to right after the chat's own controls: new chat and
                settings, the chat picker, then expand and hide. */}
            <button
              onClick={() => void useChatStore.getState().newSession()}
              title="New chat"
              aria-label="New chat"
            >
              <NewChatIcon />
            </button>
            <button onClick={() => ipc.openSettings()} title="Settings" aria-label="Settings">
              <SettingsGearIcon />
            </button>
            <RecentSessions />
            <button onClick={expand} title="Expand to full window" aria-label="Expand">
              <DoubleChevronIcon direction="up" />
            </button>
            <button
              onClick={() => void ipc.hideOverlay()}
              title="Hide overlay"
              aria-label="Hide overlay"
            >
              <DoubleChevronIcon direction="down" />
            </button>
          </div>
        </div>
        <div
          className="overlay-body"
          style={{ flex: 1, minHeight: 0, display: 'flex', flexDirection: 'column' }}
        >
          {bootSpinner ? (
            <StartupSpinner />
          ) : degraded ? (
            <StackStatusView status={status} />
          ) : (
            <ChatView headerSlot={headerSlot} />
          )}
        </div>
      </div>
      <ConfirmHost />
      <ApprovalModal onScreenSessionId={degraded || bootSpinner ? null : sessionId} />
    </div>
  );
}
