import { memo, useEffect, useRef, useState, type ReactNode } from 'react';
import ReactMarkdown from 'react-markdown';
import remarkGfm from 'remark-gfm';
import rehypeHighlight from 'rehype-highlight';
import 'highlight.js/styles/github.css';
import {
  isVisualizationToolCall,
  stripInternalMarkers,
  stripPromptPreamble,
  useChatStore,
  type Message,
} from '@/stores/chatStore';
import { ThinkingBox } from './ThinkingBox';
import { PreviousAttemptBox } from './PreviousAttemptBox';
import { CodeBlock } from './CodeBlock';
import { MessageInfo } from './MessageInfo';
import { MessageAttachmentChips } from './MessageAttachmentChips';
import { VisualizationCard } from './VisualizationCard';
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

  // Branch/Regenerate/Export each fire a round-trip to goosed (fork/prompt/
  // export). Latch while one is running so a double-click can't fork twice or
  // queue a duplicate prompt — the buttons visibly disable until it settles.
  const [busy, setBusy] = useState(false);
  const runOnce = (fn: () => Promise<unknown>) => () => {
    if (busy) return;
    setBusy(true);
    void Promise.resolve(fn()).finally(() => setBusy(false));
  };

  const [copied, setCopied] = useState(false);
  const copyTimerRef = useRef<ReturnType<typeof setTimeout> | null>(null);
  useEffect(
    () => () => {
      if (copyTimerRef.current) clearTimeout(copyTimerRef.current);
    },
    []
  );
  const copyMessage = () => {
    void navigator.clipboard
      .writeText(message.text)
      .then(() => {
        setCopied(true);
        if (copyTimerRef.current) clearTimeout(copyTimerRef.current);
        copyTimerRef.current = setTimeout(() => setCopied(false), 1200);
      })
      .catch(() => {
        /* clipboard may be unavailable */
      });
  };

  const actions = (
    <div className="msg-actions">
      <button
        title="Branch a new session from here"
        disabled={busy}
        onClick={runOnce(() => branch(index))}
      >
        Branch
      </button>
      <button
        title="Export the conversation up to here as ChatML"
        disabled={busy}
        onClick={runOnce(() => exportSession(index))}
      >
        Export from here
      </button>
      {message.role === 'assistant' && (
        <>
          <button
            title="Regenerate this response"
            disabled={busy}
            onClick={runOnce(() => regenerate(index))}
          >
            Regenerate
          </button>
          <button title="Copy as Markdown" onClick={copyMessage}>
            {copied ? 'Copied' : 'Copy'}
          </button>
          <MessageInfo message={message} />
        </>
      )}
    </div>
  );

  if (message.role === 'user') {
    // Defensive: the live-typed bubble and the replay path both already keep
    // this clean (see chatStore.ts's stripPromptPreamble), but stripping
    // again here at the single render chokepoint costs nothing on already-
    // clean text (the wrapper regexes simply won't match) and guarantees the
    // raw <system>/transcript preamble can never surface in the chat, no
    // matter which code path a message's text came from.
    const displayText = stripInternalMarkers(
      index === 0 ? stripPromptPreamble(message.text) : message.text
    );
    return (
      <div className="msg msg-user">
        <div className="bubble">{displayText}</div>
        <MessageAttachmentChips files={message.attachedFiles} />
        {actions}
      </div>
    );
  }

  if (message.superseded) {
    // A regenerated-away-from answer: collapsed, no actions — regenerating,
    // branching, etc. don't make sense against a superseded turn.
    return (
      <div className="msg msg-assistant">
        <PreviousAttemptBox message={message} />
      </div>
    );
  }

  // Visualizations render as their own always-visible card, the same way a
  // fenced code block renders inline rather than behind a click — everything
  // else stays in the collapsed Thinking tray.
  const vizCalls = message.toolCalls.filter(isVisualizationToolCall);
  const otherToolCalls = message.toolCalls.filter((c) => !isVisualizationToolCall(c));

  return (
    <div className="msg msg-assistant">
      {(message.reasoning || otherToolCalls.length > 0) && (
        <ThinkingBox
          reasoning={message.reasoning}
          toolCalls={otherToolCalls}
          streaming={message.streaming}
          hasAnswer={message.text.length > 0}
        />
      )}
      {vizCalls.map((call) => (
        <VisualizationCard key={call.id} call={call} />
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
      {actions}
    </div>
  );
});
