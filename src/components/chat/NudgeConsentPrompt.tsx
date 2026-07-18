import { useState } from 'react';
import { ipc } from '@/lib/ipc';
import { findHintToolCall, parseHintOutput, useChatStore, type Message } from '@/stores/chatStore';

/** Exploration-consent prompt (Adaptive Pathway changelog): shown on the
    assistant message whose `decide` tool call offered to widen exploration
    for a few turns. Real entropy/concentration numbers aren't serialized
    back to the frontend — `_format_result()` only sends the bare
    `nudge_offered` boolean, the underlying stats stay in a server-side log
    string — so the copy below stays generic rather than fabricating figures,
    matching how the Schism Resolution modal avoids the original UI guide's
    illustrative numbers it can't actually back. */
export function NudgeConsentPrompt({ message }: { message: Message }) {
  const sessionId = useChatStore((s) => s.sessionId);
  const [acted, setActed] = useState(false);
  const call = findHintToolCall(message);
  const parsed = parseHintOutput(call);
  if (!parsed?.nudge_offered || !sessionId || acted) return null;

  const accept = async () => {
    setActed(true);
    try {
      await ipc.adaptivePathwayAcceptNudge(sessionId);
    } catch {
      /* best-effort — a failed accept just means no exploration boost this turn */
    }
  };

  const dismiss = async () => {
    setActed(true);
    try {
      await ipc.adaptivePathwayDismissNudge();
    } catch {
      /* best-effort */
    }
  };

  return (
    <div className="nudge-prompt">
      <p>
        I&apos;ve noticed we&apos;re circling similar approaches — want me to mix in some
        alternatives for the next few turns?
      </p>
      <div className="row">
        <button className="primary" onClick={() => void accept()}>
          Accept
        </button>
        <button onClick={() => void dismiss()}>Not now</button>
      </div>
    </div>
  );
}
