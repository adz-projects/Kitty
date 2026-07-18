import { useEffect, useState } from 'react';

const KEY = 'kitty.hintFeedbackSeenCount';
const THRESHOLD = 3;

/** Whether a hint-bearing message's feedback buttons (👍/👎/etc.) should stay
    lightly visible rather than fully hover-only (UX-simplification Batch 5,
    surface visibility) — true for roughly the first few times a hint-bearing
    message appears across the app's lifetime, then it settles into the same
    hover-only behavior as every other message action. Persisted in
    localStorage so it fades for good, not just for the current session. */
export function useHintFeedbackDiscoverable(hasHint: boolean): boolean {
  const [persistent, setPersistent] = useState(false);

  useEffect(() => {
    if (!hasHint) return;
    let count = 0;
    try {
      count = Number(localStorage.getItem(KEY) ?? '0');
    } catch {
      /* localStorage unavailable — just skip the intro treatment */
      return;
    }
    if (count < THRESHOLD) {
      setPersistent(true);
      try {
        localStorage.setItem(KEY, String(count + 1));
      } catch {
        /* ignore */
      }
    }
  }, [hasHint]);

  return persistent;
}
