import { useEffect, useState } from 'react';
import { ipc } from '@/lib/ipc';
import { useChatStore } from '@/stores/chatStore';
import { useAdaptivePathwayStore } from '@/stores/adaptivePathwayStore';
import { LightbulbIcon } from '@/components/icons/LightbulbIcon';
import { PauseIcon } from '@/components/icons/PauseIcon';

/** Per-session incognito toggle for Kitty's memory — a single control that
    pauses *both* memory engines for this session: behavioral (pathway) and
    factual (memorabilia). Paused means "don't recall or write memory for this
    session" — the pathway engine's `conversation_state.paused` flag via
    `ipc.setPathwaySessionPaused` and memorabilia's `session_pause` flag via
    `ipc.setMemorabiliaSessionPaused`. (Repurposed from the old tool-hint-era
    "pause suggestions" toggle — that hint-badge UI is retired, see
    `HintBadge`/`HintFeedbackButtons`/`NudgeConsentPrompt`'s deletion.)

    Visible whenever *either* memory engine's MCP server is connected+registered
    (`useAdaptivePathwayStore`), regardless of session state — the toggle
    action itself needs a session, so it's just disabled (not unmounted)
    during the gap before one lands (New Chat/session-load/mode-swap all
    pass through a `sessionId: null` moment).

    No GET for "is this session currently paused" exists, so `paused` is
    optimistic client-side state (new sessions start unpaused, matching the
    engine's own default) rather than synced from the server on mount. */
export function AdaptivePathwayToggle() {
  const sessionId = useChatStore((s) => s.sessionId);
  const available = useAdaptivePathwayStore((s) => s.available);
  const [paused, setPaused] = useState(false);

  // Reset local state whenever the session changes so a paused toggle from
  // a previous session doesn't stick around on the next one (this component
  // isn't remounted on New Chat, see the store's own doc comment).
  useEffect(() => {
    setPaused(false);
  }, [sessionId]);

  if (!available) return null;

  const toggle = async () => {
    if (!sessionId) return;
    const next = !paused;
    setPaused(next);
    // Pause both engines. Each is independent: one may be disabled (its
    // route returns a soft error), which must not stop the other from
    // pausing. Revert the optimistic flip only if *both* calls failed —
    // if either succeeded, the session is at least partly incognito and the
    // button should reflect the paused intent.
    const results = await Promise.allSettled([
      ipc.setPathwaySessionPaused(sessionId, next),
      ipc.setMemorabiliaSessionPaused(sessionId, next),
    ]);
    if (results.every((r) => r.status === 'rejected')) {
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
          ? "Incognito — this session won't be remembered. Click to resume."
          : 'Remembering this conversation. Click to go incognito.'
      }
    >
      {paused ? <PauseIcon /> : <LightbulbIcon />}{' '}
      <span className="ap-toggle-label">{paused ? 'Incognito' : 'Remembering'}</span>
    </button>
  );
}
