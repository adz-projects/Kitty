import ReactMarkdown from 'react-markdown';
import remarkGfm from 'remark-gfm';
import rehypeHighlight from 'rehype-highlight';
import 'highlight.js/styles/github.css';
import { useChatStore, type Message } from '@/stores/chatStore';
import { ToolCallCard } from './ToolCallCard';

/** One chat message. User turns render as a plain bubble; assistant turns render
    markdown, with an optional collapsible reasoning block and tool cards. Hover
    actions: Branch from here, Regenerate (assistant), Copy as Markdown. */
export function MessageItem({ message, index }: { message: Message; index: number }) {
  const branch = useChatStore((s) => s.branch);
  const regenerate = useChatStore((s) => s.regenerate);

  const actions = (
    <div className="msg-actions">
      <button title="Branch a new session from here" onClick={() => void branch(index)}>
        Branch
      </button>
      {message.role === 'assistant' && (
        <>
          <button title="Regenerate this response" onClick={() => void regenerate(index)}>
            Regenerate
          </button>
          <button
            title="Copy as Markdown"
            onClick={() => void navigator.clipboard.writeText(message.text)}
          >
            Copy
          </button>
        </>
      )}
    </div>
  );

  if (message.role === 'user') {
    return (
      <div className="msg msg-user">
        <div className="bubble">{message.text}</div>
        {actions}
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
      {actions}
    </div>
  );
}
