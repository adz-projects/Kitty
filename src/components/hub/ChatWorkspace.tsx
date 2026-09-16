import { useEffect, useState } from 'react';
import { ipc } from '@/lib/ipc';
import { useStackStore } from '@/stores/stackStore';
import { useAdaptivePathwayStore } from '@/stores/adaptivePathwayStore';
import { useChatStore } from '@/stores/chatStore';
import { useRouteStore } from '@/stores/routeStore';
import { useMobileUiStore } from '@/stores/mobileUiStore';
import { isAndroid } from '@/lib/platform';
import { StackStatusView } from '@/components/shared/StackStatusView';
import { ChatView } from '@/components/chat/ChatView';
import { SessionList } from '@/components/sessions/SessionList';
import { ArtifactsPane } from '@/components/artifacts/ArtifactsPane';
import { ChatHeaderControls } from '@/components/chat/ChatHeaderControls';
import { NewChatIcon } from '@/components/icons/NewChatIcon';
import { SettingsGearIcon } from '@/components/icons/SettingsGearIcon';
import { KittyIcon } from '@/components/icons/KittyIcon';
import { ExportIcon } from '@/components/icons/ExportIcon';
import { MenuIcon } from '@/components/icons/MenuIcon';
import type { StackStatus } from '@/lib/types';

const DEGRADED: StackStatus[] = ['backend_down', 'local_model_missing', 'provider_unreachable'];

/** Full window: history sidebar + shared chat surface + artifacts pane. On open
    it adopts the session handed over from the overlay (Expand). */
export function ChatWorkspace() {
  const status = useStackStore((s) => s.status);
  const init = useStackStore((s) => s.init);
  const initAdaptivePathway = useAdaptivePathwayStore((s) => s.init);
  // Only this boolean is interesting — subscribing to the whole `messages`
  // array would re-render the header (and the un-memoized sidebar/pane) on
  // every streamed token, since each delta produces a fresh array.
  const hasMessages = useChatStore((s) => s.messages.length > 0);
  const exportSession = useChatStore((s) => s.exportSession);
  const newSession = useChatStore((s) => s.newSession);
  const goto = useRouteStore((s) => s.goto);
  const openDrawer = useMobileUiStore((s) => s.setDrawerOpen);
  const android = isAndroid();
  const [showArtifacts, setShowArtifacts] = useState(true);
  // Android's artifacts sheet covers the conversation, so it always starts
  // closed and isn't persisted: the desktop `show_artifacts` preference
  // describes a side column, and honouring it here opened a sheet over the
  // chat on every launch.
  const [artifactsSheetOpen, setArtifactsSheetOpen] = useState(false);

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
        // The Expand path always hands over a complete snapshot; the cast is
        // narrowing `Partial` back to that, not inventing fields.
        const adopt = useChatStore.getState().adoptSession;
        await adopt(info as unknown as Parameters<typeof adopt>[0]);
      } catch {
        // No handoff (or backend briefly unreachable) — a plain chat window.
      }
    })();
    // Show/hide-artifacts is persisted (Round-3 item 6). Desktop only — see
    // `artifactsSheetOpen`.
    if (!isAndroid()) {
      void ipc
        .getConfig()
        .then((c) => {
          if (mounted) setShowArtifacts(c.show_artifacts);
        })
        .catch(() => {
          // Keep the default (shown); the header toggle still works for this
          // window's lifetime even if the config read failed.
        });
    }
    return () => {
      mounted = false;
    };
  }, [init, initAdaptivePathway]);

  const toggleArtifacts = async () => {
    const next = !showArtifacts;
    // Optimistically flip the UI, then persist — reading + writing the whole
    // config across two IPC calls opened a lost-update race with a concurrent
    // Settings save (getConfig's snapshot could clobber a newer write). A
    // dedicated `set` that only touches `show_artifacts` would be ideal, but
    // short of that, re-reading immediately before writing keeps the stale
    // window as small as possible; failures still surface as a console warning
    // rather than silently diverging the toggle from disk.
    setShowArtifacts(next);
    try {
      const cfg = await ipc.getConfig();
      await ipc.setConfig({ ...cfg, show_artifacts: next });
    } catch (e) {
      setShowArtifacts(!next); // revert the optimistic flip on failure
      console.warn('failed to persist show_artifacts', e);
    }
  };

  const degraded = DEGRADED.includes(status);
  const artifactsShown = android ? artifactsSheetOpen : showArtifacts;

  return (
    <div className="main-window">
      {/* On Android the history lives in the menu drawer instead. */}
      {!android && <SessionList />}
      <div className="main-center">
        <header className="main-header">
          {/* Desktop: the mark and wordmark. `KittyIcon` fills with
              `currentColor`, so it inherits `--text` and flips with the theme
              for free; `app-mark` keeps the glyph centred against the
              neighbouring buttons (see base.css).
              Android: the menu button takes that spot. The row is too narrow
              for a mark as well, and the drawer is the only way to saved chats
              and Settings now that the tab bar is gone. */}
          {android ? (
            <button className="menu-button" onClick={() => openDrawer(true)} aria-label="Open menu">
              <MenuIcon />
            </button>
          ) : (
            <h1 className="app-mark">
              <KittyIcon size={24} />
              Kitty
            </h1>
          )}
          {android && <ChatHeaderControls />}
          <div style={{ display: 'flex', gap: 8 }}>
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
                itself (✕, swipe down, Back), so the button only ever opens. */}
            <button
              onClick={() => (android ? setArtifactsSheetOpen(true) : void toggleArtifacts())}
            >
              {android ? 'Artifacts' : showArtifacts ? 'Hide Artifacts' : 'Show Artifacts'}
            </button>
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
          </div>
        </header>
        <div className="main-body">
          {degraded ? <StackStatusView status={status} /> : <ChatView />}
        </div>
      </div>
      {artifactsShown && !degraded && (
        <ArtifactsPane onClose={android ? () => setArtifactsSheetOpen(false) : undefined} />
      )}
    </div>
  );
}
