import { memo, type ReactNode } from 'react';
import ReactMarkdown from 'react-markdown';
import remarkGfm from 'remark-gfm';
import rehypeHighlight from 'rehype-highlight';
import 'highlight.js/styles/github.css';
import { useChatStore, type Message } from '@/stores/chatStore';
import { ThinkingBox } from './ThinkingBox';
import { CodeBlock } from './CodeBlock';
import { MessageInfo } from './MessageInfo';
import { MessageAttachmentChips } from './MessageAttachmentChips';
import { ipc } from '@/lib/ipc';

/** Open markdown links with the OS default handler instead of navigating the
    Kitty window itself — Tauri's webview otherwise treats a bare `<a href>`
    as in-window navigation. `open_path` already opens both file paths and
    `https://` URLs via the OS default handler (same command the wizard's
    "View release" link uses). */
function ExternalLink({ href, children }: { href?: string; children?: ReactNode }) {
  return (
    <a
      href={href}
      onClick={(e) => {
        e.preventDefault();
        if (href) void ipc.openPath(href);
      }}
    >
      {children}
    </a>
  );
}

const MARKDOWN_COMPONENTS = { pre: CodeBlock, a: ExternalLink };

/** One chat message. User turns render as a plain bubble; assistant turns render
    markdown, with an optional collapsible reasoning block and tool cards. Hover
    actions: Branch from here, Regenerate (assistant), Copy as Markdown, and an
    info button (assistant only) surfacing model/provider/tokens/duration.

    Memoized (Round-7 perf fix): a session replay/live stream re-renders the
    parent list on every incoming event, but only ever changes one message at
    a time — without this, every already-rendered historical message (incl.
    its full markdown/syntax-highlight pass) re-executed on every single
    unrelated event too, an O(n²) cost across a long replay. */
export const MessageItem = memo(function MessageItem({
  message,
  index,
}: {
  message: Message;
  index: number;
}) {
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
          <MessageInfo message={message} />
        </>
      )}
    </div>
  );

  if (message.role === 'user') {
    return (
      <div className="msg msg-user">
        <div className="bubble">{message.text}</div>
        <MessageAttachmentChips files={message.attachedFiles} />
        {actions}
      </div>
    );
  }

  return (
    <div className="msg msg-assistant">
      {(message.reasoning || message.toolCalls.length > 0) && (
        <ThinkingBox
          reasoning={message.reasoning}
          toolCalls={message.toolCalls}
          streaming={message.streaming}
          hasAnswer={message.text.length > 0}
        />
      )}
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
      {actions}
    </div>
  );
});
