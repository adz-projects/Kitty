import { useState } from 'react';

/** Collapsible reasoning trace shown above the final answer (Phase 10). Closed
    by default — the panel never auto-expands, even while reasoning streams; the
    user opens it explicitly and it stays as they leave it. Always re-expandable. */
export function ReasoningPanel({
  reasoning,
  streaming,
  hasAnswer,
}: {
  reasoning: string;
  streaming: boolean;
  hasAnswer: boolean;
}) {
  // null = follow the default (closed); true/false = user opened/closed it.
  const [pinned, setPinned] = useState<boolean | null>(null);
  const streamingReasoning = streaming && !hasAnswer;
  const open = pinned ?? false;

  return (
    <div className={`reasoning${open ? ' open' : ''}`}>
      <button className="reasoning-summary" onClick={() => setPinned(!open)}>
        <span className="reasoning-caret">{open ? '▾' : '▸'}</span>
        {streamingReasoning ? 'Reasoning…' : 'Reasoning'}
      </button>
      {open && <div className="reasoning-body">{reasoning}</div>}
    </div>
  );
}
