import { useEffect } from 'react';
import { ipc, onFileDrop, pickFolder } from '@/lib/ipc';
import { humanizeChatError, useChatStore } from '@/stores/chatStore';
import { useStackStore } from '@/stores/stackStore';
import { supportsReasoning } from '@/lib/reasoning_models';
import { MessageList } from './MessageList';
import { useProgressStage } from './useProgressStage';
import { Composer } from './Composer';
import { ApprovalPrompt } from './ApprovalPrompt';
import { isAndroid } from '@/lib/platform';
import { ChatHeaderControls } from './ChatHeaderControls';
import { FileChips } from './FileChips';
import { AttachmentChips } from './AttachmentChips';
import { ClipboardImageChips } from './ClipboardImageChips';
import { PendingAttachmentChips } from './PendingAttachmentChips';
import { ErrorDetail } from '@/components/shared/ErrorDetail';
import { FolderIcon } from '@/components/icons/FolderIcon';

/** The shared chat surface used by both the overlay and the full window
    (CLAUDE.md rule 5).
    There used to be a per-session chat/agentic mode here that hid the agent
    chrome and switched to a reading-friendly column. It's gone: the split cost
    a toggle, a dead "thought partner" pill, two system prompts and a fork in
    the drop/paste handling, all to express a distinction the daemon barely
    honored (it only ever widened the sandbox to include `cwd`). The two things
    the chat side did better — collapsing a long paste into a chip, and inlining
    a dropped file's *content* rather than its path — are now simply how the
    composer behaves. */
export function ChatView() {
  // Individual slice selectors (not a whole-store `useChatStore()` call) so a
  // change to any one field — e.g. a streamed message delta — re-renders only
  // what consumes it, never the entire chat surface.
  const messages = useChatStore((s) => s.messages);
  const busy = useChatStore((s) => s.busy);
  const sessionConcluded = useChatStore((s) => s.sessionConcluded);
  const replaying = useChatStore((s) => s.replaying);
  const error = useChatStore((s) => s.error);
  const errorType = useChatStore((s) => s.errorType);
  const cwd = useChatStore((s) => s.cwd);
  const title = useChatStore((s) => s.title);
  const pendingApprovals = useChatStore((s) => s.pendingApprovals);
  const providerHost = useChatStore((s) => s.providerHost);
  const providerOffline = useChatStore((s) => s.providerOffline);
  const checkingConnection = useChatStore((s) => s.checkingConnection);
  const retryConnection = useChatStore((s) => s.retryConnection);
  const warning = useChatStore((s) => s.warning);
  const dismissWarning = useChatStore((s) => s.dismissWarning);
  const compactionNotice = useChatStore((s) => s.compactionNotice);
  const dismissCompactionNotice = useChatStore((s) => s.dismissCompactionNotice);
  const loopSuspected = useChatStore((s) => s.loopSuspected);
  const dismissLoopWarning = useChatStore((s) => s.dismissLoopWarning);
  const send = useChatStore((s) => s.send);
  const cancel = useChatStore((s) => s.cancel);
  const respondApproval = useChatStore((s) => s.respondApproval);
  const addDroppedPaths = useChatStore((s) => s.addDroppedPaths);
  const setWorkingDir = useChatStore((s) => s.setWorkingDir);
  const resetWorkingDir = useChatStore((s) => s.resetWorkingDir);
  const isDefaultFolder = useChatStore((s) => s.isDefaultFolder);
  const bindEvents = useChatStore((s) => s.bindEvents);
  const refreshProvider = useChatStore((s) => s.refreshProvider);
  const model = useChatStore((s) => s.model);
  const newSession = useChatStore((s) => s.newSession);
  const loadSession = useChatStore((s) => s.loadSession);
  // WS8 backgrounded-turn lifecycle (a chat this window left is still running):
  const backgroundSession = useChatStore((s) => s.backgroundSession);
  const backgroundTurnToast = useChatStore((s) => s.backgroundTurnToast);
  const dismissBackgroundToast = useChatStore((s) => s.dismissBackgroundToast);
  const startupPhase = useStackStore((s) => s.startupPhase);
  const starting = startupPhase !== 'ready';

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
    <div className="chat">
      {/* Android has no chat header at all: the folder pill points at a
          filesystem the user cannot browse, and the controls are hoisted into
          the window header by `ChatWorkspace` rather than costing a second
          header row on a phone-height screen. */}
      {!isAndroid() && (
        <div className="chat-header">
          {isDefaultFolder ? (
            // No project folder chosen — the "thought partner" state. No folder
            // icon; clicking the pill opens the picker to attach a working dir.
            <button
              className="pill pill-thought-partner"
              title="Thinking space — click to set a working directory"
              onClick={async () => {
                const dir = await pickFolder();
                if (dir) await setWorkingDir(dir);
              }}
            >
              Thought Partner
            </button>
          ) : (
            // A working folder is set: show it, plus an inline reset control
            // that returns the session to the default "thought partner" state
            // without opening the picker.
            <span className="pill pill-folder">
              <button
                className="pill-body"
                title={`Working directory: ${cwd} — click to change`}
                onClick={async () => {
                  const dir = await pickFolder();
                  if (dir) await setWorkingDir(dir);
                }}
              >
                <FolderIcon /> {folder ?? 'set folder'}
              </button>
              <button
                className="pill-reset"
                title="Return to thought partner (clear the working folder)"
                aria-label="Return to thought partner"
                onClick={() => void resetWorkingDir()}
              >
                ×
              </button>
            </span>
          )}
          <ChatHeaderControls />
        </div>
      )}

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

      {backgroundSession && (
        <div className="conflict-banner" role="status">
          <span className="status-dot ok" />
          <span style={{ flex: 1 }}>
            A chat you left is still running in the background — you&apos;ll get a notification when
            it&apos;s done.
          </span>
        </div>
      )}
      {backgroundTurnToast && (
        <div className="conflict-banner" role="status">
          <span className={`status-dot ${backgroundTurnToast.ok ? 'ok' : 'bad'}`} />
          <span style={{ flex: 1 }}>
            {backgroundTurnToast.ok
              ? backgroundTurnToast.title
                ? `"${backgroundTurnToast.title}" finished while you were away.`
                : 'Your previous chat finished in the background.'
              : backgroundTurnToast.title
                ? `"${backgroundTurnToast.title}" failed while you were away.`
                : 'Your previous chat failed in the background.'}
          </span>
          <button
            className="link"
            onClick={() => {
              void loadSession(
                backgroundTurnToast.sessionId,
                backgroundTurnToast.cwd,
                backgroundTurnToast.title ?? undefined
              );
              dismissBackgroundToast();
            }}
          >
            Open chat
          </button>
          <button className="link" onClick={dismissBackgroundToast}>
            Dismiss
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

      {pendingApprovals.map((a) => (
        <ApprovalPrompt key={a.tool_call_id} request={a} onRespond={respondApproval} />
      ))}
      {error && (
        <ErrorDetail
          summary={humanizeChatError(error, errorType ?? undefined)}
          raw={error}
          errorType={errorType ?? undefined}
          onNewSession={() => void newSession()}
          onOpenProviderSettings={() => void ipc.openSettings('providers')}
        />
      )}
      <AttachmentChips />
      <FileChips />
      <ClipboardImageChips />
      <PendingAttachmentChips />
      {starting && (
        <p className="muted startup-phase-banner">
          {startupPhase === 'warming_model' ? 'Warming model…' : 'Starting…'}
        </p>
      )}
      {/* Distinct from the disabled composer's own "Chat concluded."
          placeholder (release-fixes item 28) — that's easy to miss as a "why
          can't I type" signal on its own, especially scrolled past a long
          conversation. Shown regardless of whether an error card above also
          explains *why* (e.g. context_exceeded) — this confirms the current
          *state* right next to the input the user is looking at. */}
      {sessionConcluded && (
        <p className="chat-concluded-banner muted">
          This chat has ended.{' '}
          <button type="button" className="link" onClick={() => void newSession()}>
            Start a new chat
          </button>{' '}
          to continue.
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
