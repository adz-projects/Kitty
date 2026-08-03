import { useEffect, useState } from 'react';
import { ipc } from '@/lib/ipc';
import { useChatStore } from '@/stores/chatStore';
import { useAdaptivePathwayStore } from '@/stores/adaptivePathwayStore';
import { LightbulbIcon } from '@/components/icons/LightbulbIcon';
import { PauseIcon } from '@/components/icons/PauseIcon';

/** Pause/resume header toggle for the Adaptive Pathway extension's hints
    (Round-C) — `ModeToggle`-style pill. Visible whenever the extension is
    enabled/reachable (`status === 'ok'`), regardless of session state — it
    used to also require a live `sessionId`, which flickered the button in
    and out on every New Chat/session-load/mode-swap (all of which pass
    through a `sessionId: null` gap before the real session lands). The
    toggle action itself still needs a session, so it's just disabled (not
    unmounted) during that gap, matching `EffortDropdown`/`ModeBadge`'s
    `creatingSession` convention.

    `status` reads from `useAdaptivePathwayStore` rather than owning its own
    local state — this component lives inside `ChatView`, which itself gets
    unmounted/remounted whenever the *unrelated* goosed/Ollama stack status
    flips into and out of a degraded state (`main/App.tsx`/`overlay/App.tsx`'s
    `degraded ? <StackStatusView/> : <ChatView/>` swap). Local state reset to
    its default on every such remount, flickering the button even though the
    Adaptive Pathway sidecar itself never went down — the store's status
    (and its one-time event subscription) survives that remount instead.

    The sidecar has no GET for "is this session currently paused", so
    `paused` is optimistic client-side state (new sessions start unpaused,
    matching the engine's own `suggestions_paused=False` default) rather than
    synced from the server on mount. */
export function AdaptivePathwayToggle() {
  const sessionId = useChatStore((s) => s.sessionId);
  const status = useAdaptivePathwayStore((s) => s.status);
  const [paused, setPaused] = useState(false);

  // No server GET for per-session pause state (see comment above), but new
  // sessions always start unpaused — reset local state whenever the session
  // changes so a paused toggle from a previous session doesn't stick around
  // on the next one (this component isn't remounted on New Chat, see above).
  useEffect(() => {
    setPaused(false);
  }, [sessionId]);

  if (status !== 'ok') return null;

  const toggle = async () => {
    if (!sessionId) return;
    const next = !paused;
    setPaused(next);
    try {
      await ipc.adaptivePathwayToggleSuggestions(sessionId, next);
    } catch {
      setPaused(!next);
    }
  };

  return (
    <button
      className="status-badge ap-toggle"
      onClick={() => void toggle()}
      disabled={!sessionId}
      title={
        paused
          ? 'Adaptive Pathway suggestions paused — click to resume'
          : 'Adaptive Pathway suggestions on — click to pause'
      }
    >
      {paused ? <PauseIcon /> : <LightbulbIcon />}{' '}
      <span className="ap-toggle-label">{paused ? 'Paused' : 'Hints on'}</span>
    </button>
  );
}
