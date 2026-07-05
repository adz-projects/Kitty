import { useState } from 'react';
import { ToolCallCard } from './ToolCallCard';
import type { ToolCall } from '@/stores/chatStore';

/** One collapsible container for a turn's reasoning trace and tool calls
    (Round-3 item 23, replaces the separate ReasoningPanel + loose ToolCallCard
    list). Reasoning and tool calls have no shared timestamp in the streaming
    assembly, so this groups rather than truly interleaves them: reasoning
    narrative first, then every tool call in order. Closed by default — the box
    never auto-expands, even while streaming; the user opens it explicitly and
    it stays as they leave it. */
export function ThinkingBox({
  reasoning,
  toolCalls,
  streaming,
  hasAnswer,
}: {
  reasoning: string;
  toolCalls: ToolCall[];
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
        {streamingReasoning ? 'Thinking…' : 'Thinking'}
      </button>
      {open && (
        <div className="reasoning-body">
          {reasoning && <div className="reasoning-text">{reasoning}</div>}
          {toolCalls.length > 0 && (
            <div className="reasoning-tools">
              {toolCalls.map((tc) => (
                <ToolCallCard key={tc.id} call={tc} />
              ))}
            </div>
          )}
        </div>
      )}
    </div>
  );
}
