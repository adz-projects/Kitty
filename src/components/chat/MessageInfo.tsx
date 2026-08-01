import { useState } from 'react';
import type { Message } from '@/stores/chatStore';
import { usePopoverPosition } from '@/lib/usePopoverPosition';

/** Hover-revealed info button (Round-4) for an assistant message: model,
    provider, tokens, and generation time. The values are captured once, at
    the moment the request was actually sent (see chatStore's `lastSentProvider`
    /`lastSentModel`) — not read live from the chat-pill/provider-badge state,
    which reflects whatever is *currently* active and can drift from what
    actually produced this specific message if the user switches providers
    mid-conversation. Replayed/resumed messages (`session/load`) never go
    through send_prompt's completion path, so they have no metadata and this
    renders nothing for them — expected, not a bug. */
export function MessageInfo({ message }: { message: Message }) {
  const [open, setOpen] = useState(false);
  const { triggerRef, popoverRef, style } = usePopoverPosition(open, () => setOpen(false));
  const hasData =
    message.model != null ||
    message.providerName != null ||
    message.durationMs != null ||
    message.inputTokens != null;
  if (!hasData) return null;

  return (
    <div style={{ position: 'relative', display: 'inline-block' }}>
      <button
        ref={triggerRef as React.Ref<HTMLButtonElement>}
        title="Generation info"
        aria-label="Generation info"
        aria-expanded={open}
        onClick={() => setOpen((o) => !o)}
      >
        ⓘ
      </button>
      {open && (
        <div
          ref={popoverRef}
          className="mode-popover msg-info-popover"
          role="tooltip"
          style={style}
        >
          {message.model && (
            <div>
              <span className="muted">Model:</span> {message.model}
            </div>
          )}
          {message.providerName && (
            <div>
              <span className="muted">Provider:</span> {message.providerName}
            </div>
          )}
          {message.durationMs != null && (
            <div>
              <span className="muted">Time:</span> {(message.durationMs / 1000).toFixed(1)}s
            </div>
          )}
          {message.inputTokens != null && message.outputTokens != null && (
            <div>
              <span className="muted">Tokens:</span> {message.inputTokens} in →{' '}
              {message.outputTokens} out
            </div>
          )}
          {message.ttftMs != null && (
            <div>
              <span className="muted">Time to first token:</span> {(message.ttftMs / 1000).toFixed(2)}s
            </div>
          )}
          {message.ttftMs != null &&
            message.durationMs != null &&
            message.outputTokens != null &&
            message.outputTokens > 0 &&
            message.durationMs > message.ttftMs && (
              <div>
                <span className="muted">Generation speed:</span>{' '}
                {(
                  (message.outputTokens / (message.durationMs - message.ttftMs)) *
                  1000
                ).toFixed(1)}{' '}
                tok/s
              </div>
            )}
        </div>
      )}
    </div>
  );
}
