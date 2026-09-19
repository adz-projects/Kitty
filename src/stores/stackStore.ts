import { create } from 'zustand';
import { ipc, onStackStatus, onStartupPhase } from '@/lib/ipc';
import type { StackStatus, StartupPhase } from '@/lib/types';

interface StackState {
  status: StackStatus;
  detail: string | null;
  /** One-time startup progress, separate from `status` (see types.ts). */
  startupPhase: StartupPhase;
  /** True once the backend has reported `ok` at least once this process life.
      Never reset — a backend that drops *after* connecting is a real failure,
      not a slow boot, so the grace window (below) only ever applies before the
      first successful connect. */
  everConnected: boolean;
  /** True once the startup grace window has elapsed. Until then a transient
      `backend_down` (the daemon is merely slow to bind its port on first start,
      and on Android every start) is shown as a spinner rather than the hard
      "engine isn't running" panel. Set by a one-shot timer in `init()`. */
  graceElapsed: boolean;
  /** Prime from the current status, then subscribe to changes. Idempotent. */
  init: () => Promise<void>;
}

/** How long a first-connect `backend_down` is treated as "still starting"
    (spinner) rather than a failure (hard panel). The daemon normally binds
    within a couple of seconds; this is generous headroom for a cold start. */
const GRACE_MS = 15000;

/** True while we should show a startup spinner for a `backend_down` rather than
    the degraded panel: we have never connected and the grace window is open.
    Selector so the time-based transition is driven by an actual state change
    (`graceElapsed` flipping) and re-renders subscribers. */
export const selectBooting = (s: StackState): boolean => !s.everConnected && !s.graceElapsed;

// Per-channel bind state so the two `init()` subscription paths can't leak or
// double-bind: `subscribed` guards "already successfully bound", `pending`
// dedupes concurrent in-flight binds (two StrictMode/concurrent init() calls
// would otherwise BOTH pass the `subscribed` check before either await
// resolves and attach duplicate listeners), and a failed bind stays pending-
// cleared so a later init() retries that channel without re-bounding the
// already-successful one.
const bound: { status: boolean; startupPhase: boolean } = { status: false, startupPhase: false };
const pending: { status: Promise<void> | null; startupPhase: Promise<void> | null } = {
  status: null,
  startupPhase: null,
};

// One-shot guard so repeated init() calls (StrictMode, multiple windows) don't
// schedule the grace timer more than once.
let graceTimerStarted = false;

export const useStackStore = create<StackState>((set) => ({
  status: 'starting',
  detail: null,
  startupPhase: 'spawning_backend',
  everConnected: false,
  graceElapsed: false,
  init: async () => {
    if (!graceTimerStarted) {
      graceTimerStarted = true;
      setTimeout(() => set({ graceElapsed: true }), GRACE_MS);
    }
    try {
      const status = await ipc.getStackStatus();
      set({ status, ...(status === 'ok' ? { everConnected: true } : null) });
    } catch {
      // Backend not ready yet; the health loop event will update us.
    }
    try {
      set({ startupPhase: await ipc.getStartupPhase() });
    } catch {
      // Backend not ready yet; the stack://startup-phase event will update us.
    }
    if (!bound.status && !pending.status) {
      pending.status = onStackStatus((p) =>
        set({
          status: p.status,
          detail: p.detail,
          ...(p.status === 'ok' ? { everConnected: true } : null),
        })
      )
        .then(() => {
          bound.status = true;
        })
        .catch(() => {
          // Leave `bound.status` false so a later init() retries this channel.
        })
        .finally(() => {
          pending.status = null;
        });
    }
    if (!bound.startupPhase && !pending.startupPhase) {
      pending.startupPhase = onStartupPhase((p) => set({ startupPhase: p.phase }))
        .then(() => {
          bound.startupPhase = true;
        })
        .catch(() => {
          // Leave `bound.startupPhase` false so a later init() retries it.
        })
        .finally(() => {
          pending.startupPhase = null;
        });
    }
    // Await any just-started binds so concurrent init() callers don't race
    // ahead of the flag flips; already-bound/pending calls short-circuit.
    await pending.status;
    await pending.startupPhase;
  },
}));
