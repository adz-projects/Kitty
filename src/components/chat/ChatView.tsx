import { useEffect, useMemo, useState } from 'react';
import { ipc, onFileDrop, pickFolder } from '@/lib/ipc';
import { findHintToolCall, humanizeChatError, isChatMode, useChatStore } from '@/stores/chatStore';
import { useStackStore } from '@/stores/stackStore';
import { supportsReasoning } from '@/lib/reasoning_models';
import type { AdaptivePathwaySessionReflection } from '@/lib/types';
import { MessageList } from './MessageList';
import { useProgressStage } from './useProgressStage';
import { Composer } from './Composer';
import { ApprovalPrompt } from './ApprovalPrompt';
import { ModeToggle } from './ModeToggle';
import { ProviderBadge } from './ProviderBadge';
import { EffortDropdown } from './EffortDropdown';
import { ChatHeaderMenu } from './ChatHeaderMenu';
import { FileChips } from './FileChips';
import { AttachmentChips } from './AttachmentChips';
import { ClipboardImageChips } from './ClipboardImageChips';
import { PendingAttachmentChips } from './PendingAttachmentChips';
import { Modal } from '@/components/shared/Modal';
import { ErrorDetail } from '@/components/shared/ErrorDetail';
import { ChatBubbleIcon } from '@/components/icons/ChatBubbleIcon';
import { FolderIcon } from '@/components/icons/FolderIcon';
import { LightbulbIcon } from '@/components/icons/LightbulbIcon';

/** The shared chat surface used by both the overlay and the full window
    (CLAUDE.md rule 5). In chat mode (per-session `ModeToggle`, `isChatMode`) it
    hides the agent chrome and switches to a reading-friendly column. */
export function ChatView() {
  const {
    messages,
    busy,
    sessionConcluded,
    replaying,
    error,
    cwd,
    title,
    sessionId,
    pendingApprovals,
    providerHost,
    providerOffline,
    checkingConnection,
    retryConnection,
    warning,
    dismissWarning,
    compactionNotice,
    dismissCompactionNotice,
    loopSuspected,
    dismissLoopWarning,
    send,
    cancel,
    respondApproval,
    addDroppedPaths,
    setWorkingDir,
    bindEvents,
    refreshProvider,
    model,
  } = useChatStore();
  const chatOnly = useChatStore(isChatMode);
  const startupPhase = useStackStore((s) => s.startupPhase);
  const starting = startupPhase !== 'ready';
  // Adaptive Pathway session hint-count summary (Round-C) — cheap client-side
  // derivation from already-in-memory state, no new IPC needed.
  const hintCount = useMemo(
    () => messages.reduce((n, m) => (findHintToolCall(m) ? n + 1 : n), 0),
    [messages]
  );

  // Session reflection (Adaptive Pathway changelog) — refetched only when a
  // new hint appears, not on every render, since it's just for the "see the
  // roads not taken?" link's visibility + the modal's content.
  const [reflection, setReflection] = useState<AdaptivePathwaySessionReflection | null>(null);
  const [showReflection, setShowReflection] = useState(false);
  // Clear the reflection UI the moment the active session changes, so a prior
  // session's summary can't linger — either as a stale modal that pops open on
  // its own, or (if the new session's refetch below fails) as the wrong
  // session's "roads not taken?" data.
  useEffect(() => {
    setReflection(null);
    setShowReflection(false);
  }, [sessionId]);
  useEffect(() => {
    if (!sessionId || hintCount === 0) return;
    let cancelled = false;
    void ipc
      .adaptivePathwayGetSessionReflection(sessionId)
      .then((r) => {
        if (!cancelled) setReflection(r);
      })
      .catch(() => {
        if (!cancelled) setReflection(null);
      });
    return () => {
      cancelled = true;
    };
  }, [sessionId, hintCount]);

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
  const assistant = last && last.role === 'assistant' ? last : null;
  // Real-terms progress while awaiting the answer (Round-5 Batch 6): connecting
  // → thinking → formulating, derived from streaming signals + a client timer.
  const progressStage = useProgressStage(
    busy,
    assistant?.reasoning.length ?? 0,
    !!assistant?.text,
    supportsReasoning(model)
  );

  return (
    <div className={`chat${chatOnly ? ' reading' : ''}`}>
      <div className="chat-header">
        {chatOnly ? (
          <span className="pill pill-static">
            <ChatBubbleIcon /> thought partner
          </span>
        ) : (
          <button
            className="pill"
            title={
              cwd
                ? `Working directory: ${cwd} — click to change`
                : 'Click to set a working directory'
            }
            onClick={async () => {
              const dir = await pickFolder();
              if (dir) await setWorkingDir(dir);
            }}
          >
            <FolderIcon /> {folder ?? 'set folder'}
          </button>
        )}
        <div className="chat-header-controls">
          <ModeToggle />
          <ProviderBadge />
          <EffortDropdown />
          <ChatHeaderMenu chatOnly={chatOnly} />
        </div>
      </div>

      {providerOffline && (
        <div className="conflict-banner" role="status">
          <span className="status-dot bad" />
          <span style={{ flex: 1 }}>
            Can’t reach {providerHost ?? 'the provider'} — check Tailscale / your connection.
          </span>
          <button
            className="link"
            disabled={checkingConnection}
            onClick={() => void retryConnection()}
          >
            {checkingConnection ? 'Checking…' : 'Retry connection check'}
          </button>
        </div>
      )}

      {warning && (
        <div className="conflict-banner" role="status">
          <span className="status-dot warn" />
          <span style={{ flex: 1 }}>{warning}</span>
          <button className="link" onClick={dismissWarning}>
            Dismiss
          </button>
        </div>
      )}

      {compactionNotice && (
        <div className="conflict-banner" role="status">
          <span className="status-dot ok" />
          <span style={{ flex: 1 }}>{compactionNotice}</span>
          <button className="link" onClick={dismissCompactionNotice}>
            Dismiss
          </button>
        </div>
      )}

      {loopSuspected && (
        <div className="conflict-banner" role="status">
          <span className="status-dot warn" />
          <span style={{ flex: 1 }}>
            This response looks like it might be stuck repeating itself — want to cancel it?
          </span>
          <button className="link" onClick={() => void cancel()}>
            Cancel
          </button>
          <button className="link" onClick={dismissLoopWarning}>
            Keep waiting
          </button>
        </div>
      )}

      {replaying ? (
        <div className="message-list message-list-loading">
          <p className="muted">Loading conversation…</p>
        </div>
      ) : (
        <MessageList
          messages={messages}
          empty={title ?? 'Start a new chat.'}
          stage={progressStage}
        />
      )}

      {(!chatOnly || pendingApprovals.length > 0) &&
        pendingApprovals.map((a) => (
          <ApprovalPrompt
            key={a.tool_call_id}
            request={a}
            onRespond={(tid, opt) => void respondApproval(tid, opt)}
          />
        ))}
      {error && (
        <div className="chat-error">
          <ErrorDetail summary={humanizeChatError(error)} raw={error} />
        </div>
      )}
      {hintCount > 0 && (
        <div className="hint-summary muted">
          <LightbulbIcon /> {hintCount} suggestion{hintCount > 1 ? 's' : ''} this session
          {reflection?.has_untested && (
            <>
              {' · '}
              <button className="link" onClick={() => setShowReflection(true)}>
                see the roads not taken?
              </button>
            </>
          )}
        </div>
      )}
      {showReflection && reflection && (
        <Modal title="Session reflection">
          <p>{reflection.reflection}</p>
          <p>
            <span className="muted">Acceptance score:</span>{' '}
            {(reflection.acceptance_score * 100).toFixed(0)}%
          </p>
          {reflection.top_domains.length > 0 && (
            <p>
              <span className="muted">
                Top topic areas{' '}
                <span title="A domain is a topic area Kitty tracks preferences for separately, like coding vs. writing.">
                  (?)
                </span>
                :
              </span>{' '}
              {reflection.top_domains.map(([domain, count]) => `${domain} (${count})`).join(', ')}
            </p>
          )}
          <p>
            <span className="muted">Untested approaches available:</span>{' '}
            {reflection.unchosen_novel_edges}
          </p>
          <button onClick={() => setShowReflection(false)}>Close</button>
        </Modal>
      )}
      {chatOnly ? <AttachmentChips /> : <FileChips />}
      <ClipboardImageChips />
      <PendingAttachmentChips />
      {starting && (
        <p className="muted startup-phase-banner">
          {startupPhase === 'warming_model' ? 'Warming model…' : 'Starting…'}
        </p>
      )}
      <Composer
        onSend={(t) => void send(t)}
        onStop={() => void cancel()}
        disabled={busy}
        concluded={sessionConcluded}
        sendBlocked={starting}
      />
    </div>
  );
}
