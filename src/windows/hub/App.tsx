import { useEffect } from 'react';
import { useRouteStore } from '@/stores/routeStore';
import { ChatWorkspace } from '@/components/hub/ChatWorkspace';
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

  useEffect(() => {
    void init();
  }, [init]);

  return (
    <>
      <div hidden={view !== 'chat'} className="hub-route">
        <ChatWorkspace />
      </div>
      {view === 'settings' && <SettingsView />}
      {view === 'wizard' && <WizardView />}
    </>
  );
}
