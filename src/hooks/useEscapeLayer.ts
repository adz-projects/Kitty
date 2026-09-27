import { useEffect, useRef } from 'react';
import { registerEscapeLayer } from '@/lib/escapeStack';

/** While `active`, Escape calls `onEscape` (topmost layer first) instead of
    reaching the window. See `lib/escapeStack.ts`. */
export function useEscapeLayer(active: boolean, onEscape: () => void) {
  const cbRef = useRef(onEscape);
  cbRef.current = onEscape;

  useEffect(() => {
    if (!active) return;
    return registerEscapeLayer(() => cbRef.current());
  }, [active]);
}
