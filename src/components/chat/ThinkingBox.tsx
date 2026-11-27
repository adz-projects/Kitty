import { useState } from 'react';
import { ToolCallCard } from './ToolCallCard';
import { MemoryRecallCard } from './MemoryRecallCard';
import { PROGRESS_LABELS, type ProgressStage } from './ThinkingIndicator';
import type { MemoryRecallTrace, ToolCall } from '@/stores/chatStore';

/** One collapsible container for a turn's reasoning trace, memory recall
    inspector, and tool calls (Round-3 item 23, replaces the separate
    ReasoningPanel + loose ToolCallCard list). Reasoning and tool calls have no
    shared timestamp in the streaming assembly, so this groups rather than
    truly interleaves them: reasoning narrative first, then memory recall and
    every tool call in order. Collapsed by default, including while reasoning
    streams — it only opens when the user explicitly clicks it (`pinned`),
    which then sticks for the rest of the turn. */
export function ThinkingBox({
  reasoning,
  toolCalls,
  memoryRecall,
  streaming,
  hasAnswer,
  draft,
  progressStage,
}: {
  reasoning: string;
  toolCalls: ToolCall[];
  memoryRecall?: MemoryRecallTrace;
  streaming: boolean;
  hasAnswer: boolean;
  /** An answer written before every specialist report was in — kept for the
      record, but in here with the working rather than in the answer bubble. */
  draft?: string;
  /** Live turn progress stage, folded into the thinking bar so the user
      doesn't see a second floating "Provider is thinking…" line below it. */
  progressStage?: ProgressStage | null;
}) {
  // null = follow the default; true/false = user explicitly opened/closed it.
  const [pinned, setPinned] = useState<boolean | null>(null);
  const streamingReasoning = streaming && !hasAnswer;
  const isLive = Boolean(progressStage && !hasAnswer);
  // Default: collapsed. A user's explicit toggle (`pinned`) overrides the
  // default from then on.
  const open = pinned ?? false;

  return (
    <div className={`reasoning${open ? ' open' : ''}${isLive ? ' reasoning-live' : ''}`}>
      <button
        className="reasoning-summary"
        onClick={() => setPinned(!open)}
        aria-label={
          isLive && progressStage ? `Thinking — ${PROGRESS_LABELS[progressStage]}` : undefined
        }
      >
        <span className="reasoning-summary-main">
          {isLive && <span className="reasoning-live-dot" aria-hidden="true" />}
          <span className="reasoning-caret">{open ? '▾' : '▸'}</span>
          <span>{isLive ? 'Thinking' : streamingReasoning ? 'Thinking…' : 'Thinking'}</span>
          {isLive && progressStage && (
            <span className="reasoning-live-stage progress-pulse">
              · {PROGRESS_LABELS[progressStage]}…
            </span>
          )}
        </span>
        {isLive && (
          <span className="reasoning-live-tag muted" aria-hidden="true">
            live
          </span>
        )}
      </button>
      {open && (
        <div className="reasoning-body">
          {reasoning && <div className="reasoning-text">{reasoning}</div>}
          {draft && (
            <div className="reasoning-draft">
              <div className="muted">Draft, written before all specialist reports were in</div>
              <div className="reasoning-text">{draft}</div>
            </div>
          )}
          {(memoryRecall || toolCalls.length > 0) && (
            <div className="reasoning-tools">
              {memoryRecall && <MemoryRecallCard recall={memoryRecall} />}
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
