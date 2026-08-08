import { useEffect, useState } from 'react';
import { ipc } from '@/lib/ipc';
import { useChatStore } from '@/stores/chatStore';
import { useAdaptivePathwayStore } from '@/stores/adaptivePathwayStore';
import { LightbulbIcon } from '@/components/icons/LightbulbIcon';
import { PauseIcon } from '@/components/icons/PauseIcon';

/** Per-session incognito toggle for the behavioral-memory (pathway) engine.
    Repurposed from the old tool-hint-era "pause suggestions" toggle
    (Round-C) — same button, same store-backed visibility gate, new meaning:
    paused now means "don't recall or record beliefs for this session" (the
    engine's `conversation_state.paused` flag via
    `ipc.setPathwaySessionPaused`), not "suppress hint badges" (that whole
    hint-badge UI is retired, see `HintBadge`/`HintFeedbackButtons`/
    `NudgeConsentPrompt`'s deletion).

    Visible whenever the pathway MCP server is actually connected+registered
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
    try {
      await ipc.setPathwaySessionPaused(sessionId, next);
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
          ? "Incognito — this session won't be remembered. Click to resume."
          : 'Remembering this conversation. Click to go incognito.'
      }
    >
      {paused ? <PauseIcon /> : <LightbulbIcon />}{' '}
      <span className="ap-toggle-label">{paused ? 'Incognito' : 'Remembering'}</span>
    </button>
  );
}
