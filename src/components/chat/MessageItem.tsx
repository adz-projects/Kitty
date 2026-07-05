import ReactMarkdown from 'react-markdown';
import remarkGfm from 'remark-gfm';
import rehypeHighlight from 'rehype-highlight';
import 'highlight.js/styles/github.css';
import { useChatStore, type Message } from '@/stores/chatStore';
import { ToolCallCard } from './ToolCallCard';
import { ReasoningPanel } from './ReasoningPanel';
import { CodeBlock } from './CodeBlock';

const MARKDOWN_COMPONENTS = { pre: CodeBlock };

/** One chat message. User turns render as a plain bubble; assistant turns render
    markdown, with an optional collapsible reasoning block and tool cards. Hover
    actions: Branch from here, Regenerate (assistant), Copy as Markdown. */
export function MessageItem({ message, index }: { message: Message; index: number }) {
  const branch = useChatStore((s) => s.branch);
  const regenerate = useChatStore((s) => s.regenerate);
  const exportSession = useChatStore((s) => s.exportSession);

  const actions = (
    <div className="msg-actions">
      <button title="Branch a new session from here" onClick={() => void branch(index)}>
        Branch
      </button>
      <button
        title="Export the conversation up to here as ChatML"
        onClick={() => void exportSession(index)}
      >
        Export from here
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
        <ReasoningPanel
          reasoning={message.reasoning}
          streaming={message.streaming}
          hasAnswer={message.text.length > 0}
        />
      )}
      {message.toolCalls.map((tc) => (
        <ToolCallCard key={tc.id} call={tc} />
      ))}
      {message.text && (
        <div className="bubble markdown">
          <ReactMarkdown
            remarkPlugins={[remarkGfm]}
            rehypePlugins={[rehypeHighlight]}
            components={MARKDOWN_COMPONENTS}
          >
            {message.text}
          </ReactMarkdown>
        </div>
      )}
      {(message.durationMs != null || message.inputTokens != null) && (
        <div className="muted msg-meta">
          {message.durationMs != null && `⏱ ${(message.durationMs / 1000).toFixed(1)}s`}
          {message.inputTokens != null &&
            message.outputTokens != null &&
            ` · ${message.inputTokens}→${message.outputTokens} tok`}
          {message.providerName && ` · ${message.providerName}`}
        </div>
      )}
      {actions}
    </div>
  );
}
