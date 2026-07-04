import { useEffect, useRef } from 'react';
import { useChatStore } from '@/stores/chatStore';
import { MessageItem } from './MessageItem';
import { Composer } from './Composer';

/** The shared chat surface used by both the overlay and the full window
    (CLAUDE.md rule 5). The window wrapper supplies the surrounding chrome. */
export function ChatView() {
  const { messages, busy, error, cwd, mode, title, send, newSession, bindEvents } = useChatStore();
  const listRef = useRef<HTMLDivElement>(null);

  useEffect(() => {
    bindEvents();
  }, [bindEvents]);

  // Auto-scroll to the latest content while streaming.
  useEffect(() => {
    const el = listRef.current;
    if (el) el.scrollTop = el.scrollHeight;
  }, [messages]);

  const folder = cwd ? cwd.split('/').filter(Boolean).pop() : null;
  const lastAssistant = messages[messages.length - 1];
  const awaitingFirstToken =
    busy && lastAssistant?.role === 'assistant' && !lastAssistant.text && !lastAssistant.reasoning;

  return (
    <div className="chat">
      <div className="chat-header">
        <span className="pill" title={cwd ?? undefined}>
          📁 {folder ?? 'no session'}
          {mode ? ` · ${mode}` : ''}
        </span>
        <button onClick={() => void newSession()} title="Start a new session">
          New chat
        </button>
      </div>

      <div className="message-list" ref={listRef}>
        {messages.length === 0 && (
          <p className="muted">{title ?? 'New conversation. Ask Goose anything.'}</p>
        )}
        {messages.map((m) => (
          <MessageItem key={m.id} message={m} />
        ))}
        {awaitingFirstToken && <span className="typing">Thinking…</span>}
      </div>

      {error && <div className="chat-error">{error}</div>}
      <Composer onSend={(t) => void send(t)} disabled={busy} />
    </div>
  );
}
