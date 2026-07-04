import ReactMarkdown from 'react-markdown';
import remarkGfm from 'remark-gfm';
import rehypeHighlight from 'rehype-highlight';
import 'highlight.js/styles/github.css';
import type { Message } from '@/stores/chatStore';
import { ToolCallCard } from './ToolCallCard';

/** One chat message. User turns render as a plain bubble; assistant turns render
    markdown, with an optional collapsible reasoning block and tool cards.
    (The full reasoning panel is Phase 10 — here it is a simple collapsible.) */
export function MessageItem({ message }: { message: Message }) {
  if (message.role === 'user') {
    return (
      <div className="msg msg-user">
        <div className="bubble">{message.text}</div>
      </div>
    );
  }

  return (
    <div className="msg msg-assistant">
      {message.reasoning && (
        <details className="reasoning">
          <summary>Reasoning</summary>
          <div className="reasoning-body">{message.reasoning}</div>
        </details>
      )}
      {message.toolCalls.map((tc) => (
        <ToolCallCard key={tc.id} call={tc} />
      ))}
      {message.text && (
        <div className="bubble markdown">
          <ReactMarkdown remarkPlugins={[remarkGfm]} rehypePlugins={[rehypeHighlight]}>
            {message.text}
          </ReactMarkdown>
        </div>
      )}
    </div>
  );
}
