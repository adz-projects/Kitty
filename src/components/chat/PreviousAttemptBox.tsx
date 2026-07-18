import { useState } from 'react';
import { ThinkingBox } from './ThinkingBox';
import ReactMarkdown from 'react-markdown';
import remarkGfm from 'remark-gfm';
import type { Message } from '@/stores/chatStore';

/** A response superseded by `regenerate()` (see chatStore.ts) — collapsed by
    default, same disclosure convention as `ThinkingBox`, so a regenerated
    turn stays in the conversation (owner direction: regenerate must not
    branch into a new chat) without cluttering the transcript with the old
    answer front and center. */
export function PreviousAttemptBox({ message }: { message: Message }) {
  const [open, setOpen] = useState(false);

  return (
    <div className={`reasoning${open ? ' open' : ''}`}>
      <button className="reasoning-summary" onClick={() => setOpen(!open)}>
        <span className="reasoning-caret">{open ? '▾' : '▸'}</span>
        Previous attempt
      </button>
      {open && (
        <div className="reasoning-body">
          {(message.reasoning || message.toolCalls.length > 0) && (
            <ThinkingBox
              reasoning={message.reasoning}
              toolCalls={message.toolCalls}
              streaming={false}
              hasAnswer={message.text.length > 0}
            />
          )}
          {message.text && (
            <div className="bubble markdown">
              <ReactMarkdown remarkPlugins={[remarkGfm]}>{message.text}</ReactMarkdown>
            </div>
          )}
        </div>
      )}
    </div>
  );
}
