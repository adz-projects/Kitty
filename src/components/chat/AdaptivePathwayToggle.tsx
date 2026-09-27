import { useEffect, useState } from 'react';
import { ipc } from '@/lib/ipc';
import { useChatStore } from '@/stores/chatStore';
import { useAdaptivePathwayStore } from '@/stores/adaptivePathwayStore';
import { useStackStore } from '@/stores/stackStore';
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

    The paused state is read back from the chat itself whenever the chat
    changes (#46), so a resumed, expanded or re-opened incognito chat still
    says so. */
export function AdaptivePathwayToggle() {
  const sessionId = useChatStore((s) => s.sessionId);
  const available = useAdaptivePathwayStore((s) => s.available);
  const [paused, setPaused] = useState(false);

  const init = useAdaptivePathwayStore((s) => s.init);

  // The chat's own state: new chats start unpaused, a resumed one says.
  useEffect(() => {
    setPaused(false);
    if (!sessionId) return;
    let live = true;
    void ipc
      .getSessionPause(sessionId)
      .then((p) => {
        if (live) setPaused(p);
      })
      .catch(() => {});
    return () => {
      live = false;
    };
  }, [sessionId]);

  // Whether memory is available at all is only knowable once the engine is
  // up — the overlay checks at startup, before it is (#46).
  useEffect(
    () =>
      useStackStore.subscribe((s, prev) => {
        if (s.status === 'ok' && prev.status !== 'ok') void init();
      }),
    [init]
  );

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
