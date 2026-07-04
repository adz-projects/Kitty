import { useState } from 'react';

/** Collapsible reasoning trace shown above the final answer (Phase 10). While
    the model is still reasoning (streaming, no answer yet) it auto-expands so the
    user can watch; once the final answer starts it auto-collapses — unless the
    user has manually pinned it. Always re-expandable later. */
export function ReasoningPanel({
  reasoning,
  streaming,
  hasAnswer,
}: {
  reasoning: string;
  streaming: boolean;
  hasAnswer: boolean;
}) {
  // null = follow the automatic behavior; true/false = user pinned open/closed.
  const [pinned, setPinned] = useState<boolean | null>(null);
  const auto = streaming && !hasAnswer;
  const open = pinned ?? auto;

  return (
    <div className={`reasoning${open ? ' open' : ''}`}>
      <button className="reasoning-summary" onClick={() => setPinned(!open)}>
        <span className="reasoning-caret">{open ? '▾' : '▸'}</span>
        {auto ? 'Reasoning…' : 'Reasoning'}
      </button>
      {open && <div className="reasoning-body">{reasoning}</div>}
    </div>
  );
}
