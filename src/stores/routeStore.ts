// Which view a hub window is showing (docs/ANDROID.md §8.1).
//
// The hub replaced three separate Tauri windows — `main`, `settings`,
// `wizard` — with one window that routes between them. That is what lets the
// Android shell (§8.2) reuse the identical component tree behind bottom tabs
// instead of maintaining a second UI: on desktop the routes are reached from
// the header and the tray, on mobile from the tab bar, and neither knows about
// the other.
//
// **Routing away from chat does not interrupt anything.** Chat state and its
// `chat://*` listeners live in `chatStore`, bound once per window via
// `bindEvents`, not in the mounted components — so a stream started in the
// chat route keeps accumulating while the user reads Settings, and re-entering
// chat re-renders from store state. This is the property the whole
// three-windows-into-one move depends on; if it ever stops holding, switching
// tabs mid-reply would silently drop tokens.

import { create } from 'zustand';
import { onRouteGoto, ipc } from '@/lib/ipc';

/** The hub's routes.
 *
 * Deliberately does *not* include the overlay or the screenshot region
 * picker. Both are separate Tauri windows for reasons a route cannot satisfy:
 * the overlay is created once at startup and only ever shown/hidden because
 * summon latency is the product (CLAUDE.md rule 1), and the region picker is a
 * decorationless, transparent, always-on-top window sized to a specific
 * monitor's bounds. §8.1's note about folding the picker into the hub does not
 * survive contact with what that window actually is. */
export type HubView = 'chat' | 'settings' | 'wizard';

export interface RouteState {
  view: HubView;
  /** Settings section to open, for a deep link like "Fix this". */
  settingsSection: string | null;
  /** Element to briefly highlight within that section. */
  settingsHighlight: string | null;
  wizardMode: 'setup' | 'repair';
  goto: (view: HubView, opts?: RouteOptions) => void;
  /** Subscribe to `route://goto` and read this window's initial target.
      Idempotent — safe to call from an effect that may re-run. */
  init: () => Promise<void>;
}

export interface RouteOptions {
  section?: string | null;
  highlight?: string | null;
  mode?: 'setup' | 'repair';
}

/** Normalise a `route://goto` payload. Returns `null` for anything that isn't
    a route this build knows, rather than navigating somewhere blank — an
    unknown view means the Rust side and the bundle disagree, and staying put
    is the recoverable outcome. */
export function parseRouteTarget(payload: unknown): { view: HubView; opts: RouteOptions } | null {
  if (!payload || typeof payload !== 'object') return null;
  const p = payload as Record<string, unknown>;
  const view = p.view;
  if (view !== 'chat' && view !== 'settings' && view !== 'wizard') return null;
  return {
    view,
    opts: {
      section: typeof p.section === 'string' ? p.section : null,
      highlight: typeof p.highlight === 'string' ? p.highlight : null,
      mode: p.mode === 'repair' ? 'repair' : p.mode === 'setup' ? 'setup' : undefined,
    },
  };
}

let bound = false;

export const useRouteStore = create<RouteState>((set) => ({
  view: 'chat',
  settingsSection: null,
  settingsHighlight: null,
  wizardMode: 'setup',

  goto: (view, opts) =>
    set((s) => ({
      view,
      // A plain `goto('settings')` with no section clears any previous deep
      // link rather than silently reopening the last one — the user asked for
      // Settings, not for wherever a "Fix this" button last sent them.
      settingsSection: view === 'settings' ? (opts?.section ?? null) : s.settingsSection,
      settingsHighlight: view === 'settings' ? (opts?.highlight ?? null) : s.settingsHighlight,
      wizardMode: opts?.mode ?? s.wizardMode,
    })),

  init: async () => {
    if (bound) return;
    bound = true;
    // Register the listener before awaiting the one-shot initial target, and
    // track whether it already fired: a `route://goto` arriving while
    // `getRouteTarget()` is still in flight would otherwise be clobbered by
    // that now-stale initial value when it finally resolves. (Same hazard the
    // old settings window had, kept because the race is identical.)
    let navigated = false;
    void onRouteGoto((payload) => {
      const t = parseRouteTarget(payload);
      if (!t) return;
      navigated = true;
      useRouteStore.getState().goto(t.view, t.opts);
    });
    try {
      const initial = parseRouteTarget(await ipc.getRouteTarget());
      if (initial && !navigated) useRouteStore.getState().goto(initial.view, initial.opts);
    } catch {
      // No initial target is the normal case for a plain chat window.
    }
  },
}));
