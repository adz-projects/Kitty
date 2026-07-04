import { useEffect } from 'react';
import { onFileDrop } from '@/lib/ipc';
import { useChatStore } from '@/stores/chatStore';
import { MessageList } from './MessageList';
import { Composer } from './Composer';
import { ApprovalPrompt } from './ApprovalPrompt';
import { ModeBadge } from './ModeBadge';
import { FileChips } from './FileChips';

/** The shared chat surface used by both the overlay and the full window
    (CLAUDE.md rule 5). The window wrapper supplies the surrounding chrome. */
export function ChatView() {
  const {
    messages,
    busy,
    error,
    cwd,
    title,
    pendingApprovals,
    send,
    cancel,
    newSession,
    respondApproval,
    addDroppedPaths,
    bindEvents,
  } = useChatStore();

  useEffect(() => {
    bindEvents();
  }, [bindEvents]);

  // File/folder drop onto this window becomes composer chips.
  useEffect(() => {
    const un = onFileDrop((paths) => void addDroppedPaths(paths));
    return () => void un.then((fn) => fn());
  }, [addDroppedPaths]);

  const folder = cwd ? cwd.split(/[\\/]/).filter(Boolean).pop() : null;
  const last = messages[messages.length - 1];
  const awaitingFirstToken =
    busy &&
    (!last || last.role === 'user' || (last.role === 'assistant' && !last.text && !last.reasoning));

  return (
    <div className="chat">
      <div className="chat-header">
        <span className="pill" title={cwd ?? undefined}>
          📁 {folder ?? 'no session'}
        </span>
        <div style={{ display: 'flex', gap: 8, alignItems: 'center' }}>
          <ModeBadge />
          <button onClick={() => void newSession()} title="Start a new session">
            New chat
          </button>
        </div>
      </div>

      <MessageList
        messages={messages}
        empty={title ?? 'New conversation. Ask Goose anything.'}
        typing={awaitingFirstToken}
      />

      {pendingApprovals.map((a) => (
        <ApprovalPrompt
          key={a.tool_call_id}
          request={a}
          onRespond={(tid, opt) => void respondApproval(tid, opt)}
        />
      ))}
      {error && <div className="chat-error">{error}</div>}
      <FileChips />
      <Composer onSend={(t) => void send(t)} onStop={() => void cancel()} disabled={busy} />
    </div>
  );
}
