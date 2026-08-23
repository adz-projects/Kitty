import { useChatStore } from '@/stores/chatStore';
import { supportsReasoning } from '@/lib/reasoning_models';
import { MessageList } from './MessageList';
import { useProgressStage } from './useProgressStage';

/** The streaming half of the chat surface, split out of `ChatView` (perf item
    G).

    `flushDeltas` hands the store a new `messages` array on every animation
    frame, so anything subscribed to `s.messages` re-renders at frame rate
    while a reply streams. `ChatView` subscribed to it while using it only for
    this list and the progress indicator — which meant its whole body (every
    banner conditional, `pendingApprovals.map`, all four chip components and
    the `Composer`) was rebuilt 60 times a second for no reason.

    Everything that genuinely has to move per frame now lives here, and
    `ChatView` subscribes to a `hasMessages` boolean instead — the same trick
    `ChatWorkspace` already uses to keep the hub chrome off the streaming
    path. */
export function ChatTranscript() {
  const messages = useChatStore((s) => s.messages);
  const replaying = useChatStore((s) => s.replaying);
  const busy = useChatStore((s) => s.busy);
  const title = useChatStore((s) => s.title);
  const model = useChatStore((s) => s.model);

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

  if (replaying) {
    return (
      <div className="message-list message-list-loading">
        <p className="muted">Loading conversation…</p>
      </div>
    );
  }

  return (
    <MessageList
      messages={messages}
      empty={title ?? 'Start a new chat.'}
      stage={progressStage}
    />
  );
}
