import { useEffect, useState } from 'react';
import type { ProgressStage } from './ThinkingIndicator';

// "Connecting" holds until this long after send with nothing back (then we
// assume the request reached the provider and it's thinking/composing).
const CONNECTING_MS = 1800;
// Reasoning that stops growing for this long is treated as "done, now composing".
const REASONING_IDLE_MS = 1500;

/** Derive the pre-answer progress stage from streaming signals + one timer
    (Round-5 Batch 6). Returns `null` once the answer text starts (so the
    indicator settles and the answer bubble takes over) or the turn isn't
    active. Reasoning models go connecting → thinking → formulating; non-
    reasoning models go connecting → formulating (no reasoning phase). */
export function useProgressStage(
  active: boolean,
  reasoningLen: number,
  hasText: boolean,
  reasoningModel: boolean
): ProgressStage | null {
  const [stage, setStage] = useState<ProgressStage | null>(null);

  useEffect(() => {
    if (!active || hasText) {
      setStage(null);
      return;
    }
    if (reasoningLen === 0) {
      // Nothing back yet: connecting, then move to thinking (reasoning model)
      // or straight to formulating (non-reasoning model).
      setStage('connecting');
      const t = setTimeout(
        () => setStage(reasoningModel ? 'thinking' : 'formulating'),
        CONNECTING_MS
      );
      return () => clearTimeout(t);
    }
    // Reasoning is streaming (this effect re-runs on each delta as reasoningLen
    // grows, resetting the timer). When it goes idle, switch to formulating.
    setStage('thinking');
    const t = setTimeout(() => setStage('formulating'), REASONING_IDLE_MS);
    return () => clearTimeout(t);
  }, [active, reasoningLen, hasText, reasoningModel]);

  return stage;
}
