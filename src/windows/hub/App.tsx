import { useEffect } from 'react';
import { useRouteStore } from '@/stores/routeStore';
import { isAndroid } from '@/lib/platform';
import { trackViewportHeight } from '@/lib/viewport';
import { useBackDismiss } from '@/hooks/useBackDismiss';
import { ChatWorkspace } from '@/components/hub/ChatWorkspace';
import { MobileDrawer } from '@/components/hub/MobileDrawer';
import { SessionList } from '@/components/sessions/SessionList';
import { SettingsView } from '@/components/settings/SettingsView';
import { WizardView } from '@/components/wizard/WizardView';

/** The hub window (docs/ANDROID.md §8.1): one window routing between chat,
    settings and setup, where there used to be three.
 *
 * Multiple hub instances can be open at once (D21) — `windows.rs` allocates
 * `chat-N` labels off the same bundle — and each is an independent viewer with
 * its own session, its own pinned model, and its own route. That independence
 * is why the route lives in a per-window zustand store rather than in Rust:
 * two hubs showing different things is the intended state, not drift.
 *
 * Chat stays mounted across route changes. `chatStore` owns the `chat://*`
 * subscriptions, so this is a display switch, not a teardown — but the
 * component is kept mounted anyway so scroll position and composer drafts
 * survive a trip to Settings, which store state alone would not preserve. */
export function App() {
  const view = useRouteStore((s) => s.view);
  const init = useRouteStore((s) => s.init);
  const goto = useRouteStore((s) => s.goto);
  const android = isAndroid();

  useEffect(() => {
    void init();
  }, [init]);

  // Android only: the soft keyboard shrinks the *visual* viewport without
  // resizing the layout, so the app has to follow it by hand. See
  // `lib/viewport.ts`. Desktop windows resize properly and need nothing.
  useEffect(() => {
    if (!android) return;
    return trackViewportHeight();
  }, [android]);

  // With no tab bar, Back is how a phone gets from Settings to the chat. The
  // wizard is excluded on purpose: first run shouldn't offer a way out into a
  // half-configured app.
  useBackDismiss(android && (view === 'settings' || view === 'sessions'), () => goto('chat'));

  return (
    <>
      <div hidden={view !== 'chat'} className="hub-route">
        <ChatWorkspace />
      </div>
      {/* `sessions` is a full-page list for anything that routes to it. On
          desktop the list lives in the chat sidebar and on Android in the menu
          drawer, so neither navigates here themselves — but rendering nothing
          for a reachable route would be a worse failure than a redundant
          page. */}
      {view === 'sessions' && (
        <div className="sessions-route">
          <SessionList />
        </div>
      )}
      {view === 'settings' && <SettingsView />}
      {view === 'wizard' && <WizardView />}
      {android && <MobileDrawer />}
    </>
  );
}
