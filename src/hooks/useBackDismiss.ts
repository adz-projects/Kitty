import { useEffect, useRef } from 'react';
import { registerBackLayer } from '@/lib/backDismiss';

/** While `active`, Android's Back button calls `onDismiss` instead of leaving
    the app. See `lib/backDismiss.ts` for how the history entry is managed. */
export function useBackDismiss(active: boolean, onDismiss: () => void) {
  const cbRef = useRef(onDismiss);
  cbRef.current = onDismiss;

  useEffect(() => {
    if (!active) return;
    return registerBackLayer(() => cbRef.current());
  }, [active]);
}
