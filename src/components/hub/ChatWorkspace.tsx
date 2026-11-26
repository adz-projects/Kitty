import { useEffect } from 'react';
import { createPortal } from 'react-dom';
import { ipc } from '@/lib/ipc';
import { useStackStore, selectBooting } from '@/stores/stackStore';
import { useAdaptivePathwayStore } from '@/stores/adaptivePathwayStore';
import { useChatStore } from '@/stores/chatStore';
import { listingFor, useArtifactsStore } from '@/stores/artifactsStore';
import { useRouteStore } from '@/stores/routeStore';
import { useMobileUiStore } from '@/stores/mobileUiStore';
import { isAndroid } from '@/lib/platform';
import { StackStatusView } from '@/components/shared/StackStatusView';
import { StartupSpinner } from '@/components/shared/StartupSpinner';
import { ChatView } from '@/components/chat/ChatView';
import { SessionList } from '@/components/sessions/SessionList';
import { ArtifactsPane } from '@/components/artifacts/ArtifactsPane';
import { ChatHeaderControls } from '@/components/chat/ChatHeaderControls';
import { HubBanners } from './HubBanners';
import { NewChatIcon } from '@/components/icons/NewChatIcon';
import { SettingsGearIcon } from '@/components/icons/SettingsGearIcon';
import { ExportIcon } from '@/components/icons/ExportIcon';
import { MenuIcon } from '@/components/icons/MenuIcon';
import type { StackStatus } from '@/lib/types';

const DEGRADED: StackStatus[] = ['backend_down'];

/** Full window: history sidebar + shared chat surface + artifacts pane. On open
    it adopts the session handed over from the overlay (Expand).

    On the desktop the chat's actions (export, artifacts, settings, new chat)
    sit in the window's title bar, rendered into `titleBarSlot` while the chat
    route is showing; on Android they stay in the header row above the chat. */
export function ChatWorkspace({
  titleBarSlot,
  searchSlot,
}: {
  titleBarSlot: HTMLElement | null;
  searchSlot: HTMLElement | null;
}) {
  const status = useStackStore((s) => s.status);
  const init = useStackStore((s) => s.init);
  const initAdaptivePathway = useAdaptivePathwayStore((s) => s.init);
  // Only this boolean is interesting — subscribing to the whole `messages`
  // array would re-render the header (and the un-memoized sidebar/pane) on
  // every streamed token, since each delta produces a fresh array.
  const hasMessages = useChatStore((s) => s.messages.length > 0);
  // The artifacts drawer (desktop) or sheet (Android). It never opens by
  // itself: `open` starts false on every launch and only the user flips it.
  const artifactsOpen = useArtifactsStore((s) => s.open);
  const setArtifactsOpen = useArtifactsStore((s) => s.setOpen);
  const toggleArtifacts = useArtifactsStore((s) => s.toggle);
  const unseen = useArtifactsStore((s) => s.unseen);
  const cwd = useChatStore((s) => s.cwd);
  const sessionId = useChatStore((s) => s.sessionId);
  const artifactCount = useArtifactsStore((s) => listingFor(s, cwd)?.length ?? 0);
  const exportSession = useChatStore((s) => s.exportSession);
  const newSession = useChatStore((s) => s.newSession);
  const goto = useRouteStore((s) => s.goto);
  const view = useRouteStore((s) => s.view);
  const openDrawer = useMobileUiStore((s) => s.setDrawerOpen);
  const android = isAndroid();
  useEffect(() => {
    void init();
    void initAdaptivePathway();
    let mounted = true;
    // This window's own one-time handoff, if Expand created it with one
    // (Feature 5: every Expand opens a brand-new window now, so there is no
    // "already open, re-adopt a later handoff" case to also subscribe to —
    // a fresh window only ever needs this single mount-time read).
    void (async () => {
      try {
        const info = await ipc.getPendingHandoff();
        if (!mounted || !info?.session_id) return;
        if (info.spectate) {
          // Watching a specialist, not resuming a conversation: load the
          // delegate's transcript read-only. The provider/model come from the
          // status event that offered the watch — they are the DELEGATE's host,
          // which is usually neither this window's nor the parent session's, and
          // are passed for labelling only (`spectateSession` never writes them
          // back to the running delegate).
          await useChatStore
            .getState()
            .spectateSession(
              info.session_id,
              String(info.specialist ?? 'specialist'),
              typeof info.provider_id === 'string' ? info.provider_id : undefined,
              typeof info.model === 'string' ? info.model : undefined
            );
          return;
        }
        if (info.overlay_run === true) {
          await useChatStore.getState().adoptOverlayRun({
            session_id: info.session_id,
            cwd: typeof info.cwd === 'string' ? info.cwd : '',
            thinking_effort: info.thinking_effort ?? null,
            is_default_folder:
              typeof info.is_default_folder === 'boolean' ? info.is_default_folder : true,
            provider_id: typeof info.provider_id === 'string' ? info.provider_id : null,
            model_id: typeof info.model_id === 'string' ? info.model_id : null,
          });
          return;
        }
        // The Expand path always hands over a complete snapshot; the cast is
        // narrowing `Partial` back to that, not inventing fields.
        const adopt = useChatStore.getState().adoptSession;
        await adopt(info as unknown as Parameters<typeof adopt>[0]);
      } catch {
        // No handoff (or backend briefly unreachable) — a plain chat window.
      }
    })();
    return () => {
      mounted = false;
    };
  }, [init, initAdaptivePathway]);

  // This window has an artifacts pane, so output cards can point at it.
  useEffect(() => {
    useArtifactsStore.getState().setHasPane(true);
    return () => useArtifactsStore.getState().setHasPane(false);
  }, []);

  // A different chat (or a new one): whatever was flagged unseen was about the
  // chat the user just left.
  useEffect(() => {
    useArtifactsStore.getState().resetForChat();
  }, [sessionId]);

  const booting = useStackStore(selectBooting);
  const degraded = DEGRADED.includes(status);
  // A first-connect `backend_down` during the grace window is a slow daemon
  // bind, not a failure — show a spinner instead of the hard panel (item 2).
  const bootSpinner = status === 'backend_down' && booting;
  const artifactsShown = artifactsOpen;

  // The chat's actions. Desktop: in the title bar (only while the chat route
  // is showing, since this component stays mounted under Settings). Android:
  // in the header row, after the menu button and the chat's own controls.
  const actions = (
    <div className={android ? 'main-header-actions' : 'titlebar-actions'}>
      {/* Export is desktop-only: on a phone the header is one crowded row
          and ChatML export is a workstation-shaped action (you're pulling
          a transcript into another file). Android keeps New chat + the
          artifacts toggle. */}
      {!android && hasMessages && (
        <button
          onClick={() => void exportSession()}
          title="Export this session as ChatML"
          aria-label="Export this session as ChatML"
        >
          <ExportIcon />
        </button>
      )}
      {/* Android: the sheet covers this button while open and closes
          itself (✕, swipe down, Back), so the button only ever opens.
          Desktop has its own toggle at the end of the row instead. */}
      {android && (
        <button
          className={unseen ? 'artifacts-unseen' : undefined}
          onClick={() => setArtifactsOpen(true)}
          aria-label={unseen ? 'Artifacts — something new' : 'Artifacts'}
        >
          Artifacts
        </button>
      )}
      {/* Routes within this hub rather than opening a window: with
          multiple hubs open (D21) a shared Settings window would be
          ambiguous about which one's session it configures.
          Desktop-only — Android reaches Settings from the menu drawer, so
          a second entry point here is clutter in an already narrow row. */}
      {!android && (
        <button onClick={() => goto('settings')} title="Settings" aria-label="Settings">
          <SettingsGearIcon />
        </button>
      )}
      <button onClick={() => void newSession()} title="New chat" aria-label="New chat">
        <NewChatIcon />
      </button>
      {/* Desktop: the artifacts drawer opens and closes with this button,
          last in the row so it sits on the side the drawer opens on. It is
          red while something new is waiting in it. */}
      {!android && (
        <button
          className={unseen ? 'artifacts-unseen' : undefined}
          onClick={toggleArtifacts}
          title={
            artifactsShown
              ? 'Hide artifacts'
              : unseen
                ? 'Show artifacts — something new was made'
                : 'Show artifacts'
          }
          aria-label={
            artifactsShown
              ? 'Hide artifacts'
              : unseen
                ? 'Show artifacts — something new was made'
                : 'Show artifacts'
          }
          aria-pressed={artifactsShown}
        >
          <MenuIcon size={16} />
          {/* With the drawer shut, the button says how much is behind it. */}
          {!artifactsShown && artifactCount > 0 && (
            <span className="titlebar-count">{artifactCount}</span>
          )}
        </button>
      )}
    </div>
  );

  return (
    <div className="main-window">
      {/* On Android the history lives in the menu drawer instead. */}
      {!android && <SessionList searchSlot={view === 'chat' ? searchSlot : null} />}
      <div className="main-center">
        {android ? (
          <header className="main-header">
            {/* The menu button: the drawer is the only way to saved chats and
                Settings now that the tab bar is gone. */}
            <button className="menu-button" onClick={() => openDrawer(true)} aria-label="Open menu">
              <MenuIcon />
            </button>
            <ChatHeaderControls />
            {actions}
          </header>
        ) : (
          titleBarSlot && view === 'chat' && createPortal(actions, titleBarSlot)
        )}
        <HubBanners />
        <div className="main-body">
          {bootSpinner ? (
            <StartupSpinner />
          ) : degraded ? (
            <StackStatusView status={status} />
          ) : (
            <ChatView />
          )}
        </div>
      </div>
      {artifactsShown && !degraded && <ArtifactsPane onClose={() => setArtifactsOpen(false)} />}
    </div>
  );
}
