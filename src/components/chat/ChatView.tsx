import { useEffect } from 'react';
import { onFileDrop } from '@/lib/ipc';
import { useChatStore } from '@/stores/chatStore';
import { supportsReasoning } from '@/lib/reasoning_models';
import { MessageList } from './MessageList';
import { Composer } from './Composer';
import { ApprovalPrompt } from './ApprovalPrompt';
import { ModeBadge } from './ModeBadge';
import { FileChips } from './FileChips';
import { AttachmentChips } from './AttachmentChips';

const TIER_LABEL: Record<string, string> = {
  personal: '🔒 private network',
  remote: '☁ remote',
};

/** The shared chat surface used by both the overlay and the full window
    (CLAUDE.md rule 5). In chat-only mode (tools_enabled:false) it hides the
    agent chrome and switches to a reading-friendly column. */
export function ChatView() {
  const {
    messages,
    busy,
    error,
    cwd,
    title,
    pendingApprovals,
    toolsEnabled,
    providerTier,
    providerHost,
    providerOffline,
    send,
    cancel,
    newSession,
    respondApproval,
    addDroppedPaths,
    bindEvents,
    refreshProvider,
    model,
  } = useChatStore();

  useEffect(() => {
    bindEvents();
    void refreshProvider();
  }, [bindEvents, refreshProvider]);

  useEffect(() => {
    const un = onFileDrop((paths) => void addDroppedPaths(paths));
    return () => void un.then((fn) => fn());
  }, [addDroppedPaths]);

  const folder = cwd ? cwd.split(/[\\/]/).filter(Boolean).pop() : null;
  const last = messages[messages.length - 1];
  const awaitingFirstToken =
    busy &&
    (!last || last.role === 'user' || (last.role === 'assistant' && !last.text && !last.reasoning));
  const chatOnly = !toolsEnabled;
  const tierBadge = providerTier && TIER_LABEL[providerTier];
  // Predictive hint for the thinking indicator (the reasoning panel itself is
  // content-driven). True if the model is known-reasoning or reasoning already began.
  const thinkingReasoning =
    supportsReasoning(model) || (!!last && last.role === 'assistant' && !!last.reasoning);

  return (
    <div className={`chat${chatOnly ? ' reading' : ''}`}>
      <div className="chat-header">
        <span className="pill" title={cwd ?? undefined}>
          {chatOnly ? '💬 thought partner' : `📁 ${folder ?? 'no session'}`}
        </span>
        <div style={{ display: 'flex', gap: 8, alignItems: 'center' }}>
          {tierBadge && (
            <span className="status-badge" title={providerHost ?? undefined}>
              {tierBadge}
              {providerHost ? `: ${providerHost}` : ''}
            </span>
          )}
          {!chatOnly && <ModeBadge />}
          <button onClick={() => void newSession()} title="Start a new session">
            New chat
          </button>
        </div>
      </div>

      {providerOffline && (
        <div className="conflict-banner" role="status">
          <span className="status-dot bad" />
          Can’t reach {providerHost ?? 'the provider'} — check Tailscale / your connection.
        </div>
      )}

      <MessageList
        messages={messages}
        empty={title ?? 'New conversation. Ask Goose anything.'}
        typing={awaitingFirstToken}
        thinkingReasoning={thinkingReasoning}
      />

      {!chatOnly &&
        pendingApprovals.map((a) => (
          <ApprovalPrompt
            key={a.tool_call_id}
            request={a}
            onRespond={(tid, opt) => void respondApproval(tid, opt)}
          />
        ))}
      {error && <div className="chat-error">{error}</div>}
      {chatOnly ? <AttachmentChips /> : <FileChips />}
      <Composer onSend={(t) => void send(t)} onStop={() => void cancel()} disabled={busy} />
    </div>
  );
}
