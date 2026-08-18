import { useEffect, useState } from 'react';
import { ipc } from '@/lib/ipc';
import { useStackStore } from '@/stores/stackStore';
import { useAdaptivePathwayStore } from '@/stores/adaptivePathwayStore';
import { useChatStore } from '@/stores/chatStore';
import { useRouteStore } from '@/stores/routeStore';
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
  const [showArtifacts, setShowArtifacts] = useState(true);

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
        if (mounted && info?.session_id) await useChatStore.getState().adoptSession(info);
      } catch {
        // No handoff (or backend briefly unreachable) — a plain chat window.
      }
    })();
    // Show/hide-artifacts is persisted (Round-3 item 6).
    void ipc
      .getConfig()
      .then((c) => {
        if (mounted) setShowArtifacts(c.show_artifacts);
      })
      .catch(() => {
        // Keep the default (shown); the header toggle still works for this
        // window's lifetime even if the config read failed.
      });
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
  const android = isAndroid();

  return (
    <div className="main-window">
      <SessionList />
      <div className="main-center">
        <header className="main-header">
          {/* The mark always; the word only where there's room for it. On
              Android the header is one row that also carries the model picker,
              so the six characters of "Kitty" are the cheapest thing to give
              up. `KittyIcon` fills with `currentColor`, so it inherits `--text`
              and flips light-on-dark / dark-on-light with the theme for free.
              `app-mark` is needed on both now — a block `h1` misaligns the
              glyph against the neighbouring buttons (see base.css). */}
          <h1 className="app-mark">
            <KittyIcon size={24} />
            {!android && 'Kitty'}
          </h1>
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
            <button onClick={() => void toggleArtifacts()}>
              {/* Windows has room to spell it out; Android's header is one
                  crowded row shared with the model picker, so it keeps the
                  terser "Hide"/"Artifacts" wording. */}
              {android ? (showArtifacts ? 'Hide' : 'Artifacts') : showArtifacts ? 'Hide Artifacts' : 'Show Artifacts'}
            </button>
            {/* Routes within this hub rather than opening a window: with
                multiple hubs open (D21) a shared Settings window would be
                ambiguous about which one's session it configures.
                Desktop-only — Android reaches Settings from the tab bar, so a
                second entry point here is clutter in an already narrow row. */}
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
      {showArtifacts && !degraded && (
        <ArtifactsPane onClose={android ? () => void toggleArtifacts() : undefined} />
      )}
    </div>
  );
}
